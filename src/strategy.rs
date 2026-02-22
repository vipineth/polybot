//! 5m post-close sweep bot: BTC, ETH, SOL, XRP. Monitor live prices during round,
//! then sweep stale limit orders after market closes using FOK orders.

use crate::api::PolymarketApi;
use crate::chainlink::run_chainlink_multi_poller;
use crate::config::Config;
use crate::discovery::{current_5m_period_start, MarketDiscovery, MARKET_5M_DURATION_SECS};
use crate::paper_trade::PaperTradeLogger;
use crate::rtds::{LatestPriceCache, PriceCacheMulti};
use anyhow::Result;
use chrono::Utc;
use log::{error, info, warn};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio::time::{sleep, Duration};

pub struct ArbStrategy {
    api: Arc<PolymarketApi>,
    config: Config,
    discovery: MarketDiscovery,
    /// symbol -> period_start -> price-to-beat (from RTDS Chainlink).
    price_cache_5: PriceCacheMulti,
    /// Latest RTDS price per symbol (for post-close sweep winner determination).
    latest_prices: LatestPriceCache,
    /// Paper trade logger (shared across symbol loops).
    paper_trader: PaperTradeLogger,
}

impl ArbStrategy {
    pub fn new(api: Arc<PolymarketApi>, config: Config) -> Self {
        let latest_prices: LatestPriceCache = Arc::new(RwLock::new(HashMap::new()));
        let paper_trader = PaperTradeLogger::new(api.clone(), Arc::clone(&latest_prices));
        Self {
            discovery: MarketDiscovery::new(api.clone()),
            api,
            config,
            price_cache_5: Arc::new(RwLock::new(HashMap::new())),
            latest_prices,
            paper_trader,
        }
    }

    /// Wait until we have the current 5m market and its price-to-beat for the given symbol.
    /// Returns (m5_cid, m5_up_token, m5_down_token, period_5, price_to_beat).
    async fn wait_for_5m_market_and_price(&self, symbol: &str) -> Result<(String, String, String, i64, f64)> {
        loop {
            let period_5 = current_5m_period_start();
            let m5_cid = match self.discovery.get_5m_market(symbol, period_5).await? {
                Some((cid, _)) => cid,
                None => {
                    warn!("{} no market for period {}, skipping", symbol, period_5);
                    let remaining = (period_5 + MARKET_5M_DURATION_SECS) - Utc::now().timestamp();
                    if remaining > 0 {
                        sleep(Duration::from_secs(remaining as u64)).await;
                    }
                    continue;
                }
            };
            // Price-to-beat: the on-chain Chainlink price at the start of this period.
            // 1. RTDS WS capture (exact period-start price, only works for BTC)
            // 2. Chainlink RPC (on-chain eth_call, works for all symbols)
            let rtds_capture = {
                let cache = self.price_cache_5.read().await;
                cache.get(symbol).and_then(|per_period| per_period.get(&period_5).copied())
            };
            let (price_to_beat, source) = if let Some(p) = rtds_capture {
                (p, "RTDS WS")
            } else {
                match self.api.get_chainlink_price_rpc(symbol).await {
                    Ok((p, _)) => (p, "Chainlink RPC"),
                    Err(e) => {
                        warn!("{} no price-to-beat: RTDS empty, Chainlink RPC failed: {}", symbol, e);
                        let remaining = (period_5 + MARKET_5M_DURATION_SECS) - Utc::now().timestamp();
                        if remaining > 0 {
                            sleep(Duration::from_secs(remaining as u64)).await;
                        }
                        continue;
                    }
                }
            };
            let (m5_up, m5_down) = self.discovery.get_market_tokens(&m5_cid).await?;
            info!("{} period={} price-to-beat=${} ({})", symbol, period_5, price_to_beat, source);
            return Ok((m5_cid, m5_up, m5_down, period_5, price_to_beat));
        }
    }

    /// Run one 5m round: wait for the period to end. No orders placed during the round.
    async fn run_5m_round(
        &self,
        symbol: &str,
        period_5: i64,
    ) -> Result<()> {
        let remaining = (period_5 + MARKET_5M_DURATION_SECS) - Utc::now().timestamp();
        if remaining > 0 {
            info!("{} waiting {}s for period to close", symbol, remaining);
            sleep(Duration::from_secs(remaining as u64)).await;
        }
        info!("{} period {} closed", symbol, period_5);
        Ok(())
    }

    /// Post-close sweep: determine winner from latest price (RTDS + Chainlink RPC),
    /// then buy winning tokens from stale limit orders using FOK orders.
    /// Returns (total_orders, total_shares, total_cost).
    async fn sweep_stale_asks(
        &self,
        symbol: &str,
        price_to_beat: f64,
        m5_up: &str,
        m5_down: &str,
    ) -> Result<(u32, f64, f64)> {
        let cfg = &self.config.strategy;
        let now_ms = Utc::now().timestamp_millis();

        // --- Source 1: RTDS WS (cached from WebSocket) ---
        let rtds_start = std::time::Instant::now();
        let rtds_result = {
            let cache = self.latest_prices.read().await;
            cache.get(symbol).copied()
        };
        let rtds_elapsed = rtds_start.elapsed();

        let (rtds_price, rtds_age_s) = match rtds_result {
            Some((p, ts)) => {
                let age = (now_ms - ts) / 1000;
                info!(
                    "Sweep {} RTDS WS: ${} (age={}s, {:.1}ms)",
                    symbol, p, age, rtds_elapsed.as_secs_f64() * 1000.0
                );
                (Some(p), age)
            }
            None => {
                info!(
                    "Sweep {} RTDS WS: unavailable",
                    symbol
                );
                (None, i64::MAX)
            }
        };

        // --- Source 2: Chainlink RPC (on-chain eth_call) ---
        let rpc_start = std::time::Instant::now();
        let rpc_result = self.api.get_chainlink_price_rpc(symbol).await;
        let rpc_elapsed = rpc_start.elapsed();

        let (rpc_price, rpc_age_s) = match &rpc_result {
            Ok((p, updated_at)) => {
                let age = (now_ms / 1000) as i64 - (*updated_at as i64);
                info!(
                    "Sweep {} Chainlink RPC: ${} (age={}s, {:.0}ms)",
                    symbol, p, age, rpc_elapsed.as_secs_f64() * 1000.0
                );
                (Some(*p), age)
            }
            Err(e) => {
                warn!(
                    "Sweep {} Chainlink RPC: failed ({}, {:.0}ms)",
                    symbol, e, rpc_elapsed.as_secs_f64() * 1000.0
                );
                (None, i64::MAX)
            }
        };

        // --- Pick the best (freshest) available price ---
        let (latest_price, source, age_secs) = match (rtds_price, rpc_price) {
            (Some(rp), Some(cp)) => {
                if rtds_age_s <= rpc_age_s {
                    (rp, "RTDS WS", rtds_age_s)
                } else {
                    (cp, "Chainlink RPC", rpc_age_s)
                }
            }
            (Some(rp), None) => (rp, "RTDS WS", rtds_age_s),
            (None, Some(cp)) => (cp, "Chainlink RPC", rpc_age_s),
            (None, None) => {
                warn!("Sweep {}: no price from RTDS WS or Chainlink RPC, skipping.", symbol);
                return Ok((0, 0.0, 0.0));
            }
        };

        info!(
            "Sweep {} using {} price: ${} (age={}s)",
            symbol, source, latest_price, age_secs
        );

        // Price sanity validation
        if latest_price.is_nan() || latest_price.is_infinite() || latest_price <= 0.0
            || latest_price < 0.001 || latest_price > 1_000_000.0
        {
            warn!("Sweep {}: latest_price {} fails sanity check, skipping.", symbol, latest_price);
            return Ok((0, 0.0, 0.0));
        }
        if price_to_beat.is_nan() || price_to_beat.is_infinite() || price_to_beat <= 0.0
            || price_to_beat < 0.001 || price_to_beat > 1_000_000.0
        {
            warn!("Sweep {}: price_to_beat {} fails sanity check, skipping.", symbol, price_to_beat);
            return Ok((0, 0.0, 0.0));
        }

        // Check staleness
        if age_secs > cfg.sweep_timeout_secs as i64 {
            warn!(
                "Sweep {}: best price is {}s old (max {}s), skipping.",
                symbol, age_secs, cfg.sweep_timeout_secs
            );
            return Ok((0, 0.0, 0.0));
        }

        // Compute diff = latest_price - price_to_beat
        let diff = latest_price - price_to_beat;
        info!(
            "Sweep {}: latest_price={}, price_to_beat={}, diff={}",
            symbol, latest_price, price_to_beat, diff
        );

        // Guard: zero diff (tied prices)
        if diff == 0.0 {
            info!("Sweep {}: diff=0 (tied prices), skipping.", symbol);
            return Ok((0, 0.0, 0.0));
        }

        // Cross-validate RTDS vs RPC prices when both available
        if let (Some(rp), Some(cp)) = (rtds_price, rpc_price) {
            let source_disagreement = (rp - cp).abs();
            if source_disagreement > diff.abs() {
                warn!(
                    "Sweep {}: source disagreement (${}) > price movement (${}), skipping. RTDS=${}, RPC=${}",
                    symbol, source_disagreement, diff.abs(), rp, cp
                );
                return Ok((0, 0.0, 0.0));
            }
        }

        // Check minimum margin (percentage of price_to_beat)
        let min_margin_abs = cfg.sweep_min_margin_pct * price_to_beat;
        if diff.abs() < min_margin_abs {
            info!(
                "Sweep {}: diff ${} < min margin ${} ({}% of ${}), skipping.",
                symbol, diff.abs(), min_margin_abs, cfg.sweep_min_margin_pct * 100.0, price_to_beat
            );
            return Ok((0, 0.0, 0.0));
        }

        // Determine winner: diff > 0 -> Up, diff < 0 -> Down
        let (winner, winning_token) = if diff > 0.0 {
            ("Up", m5_up)
        } else {
            ("Down", m5_down)
        };
        info!(
            "Sweep {}: winner={} (diff={}), sweeping token {}",
            symbol,
            winner,
            diff,
            &winning_token[..winning_token.len().min(20)]
        );

        if self.config.strategy.simulation_mode {
            info!("Sweep {}: SIMULATION MODE - would sweep {} token, skipping actual orders.", symbol, winner);
            return Ok((0, 0.0, 0.0));
        }

        // 6. Sweep loop (until timeout)
        let sweep_start = std::time::Instant::now();
        let timeout = Duration::from_secs(cfg.sweep_timeout_secs);
        let mut total_orders: u32 = 0;
        let mut total_shares: f64 = 0.0;
        let mut total_cost: f64 = 0.0;
        let mut consecutive_empty_passes: u32 = 0;

        while sweep_start.elapsed() < timeout {
            // Max cost cap
            if total_cost >= cfg.max_sweep_cost {
                info!("Sweep {}: reached max_sweep_cost ${}, stopping.", symbol, cfg.max_sweep_cost);
                break;
            }

            // a. Fetch orderbook for winning token
            let orderbook = match self.api.get_orderbook(winning_token).await {
                Ok(ob) => ob,
                Err(e) => {
                    warn!("Sweep {}: orderbook fetch failed: {}", symbol, e);
                    break;
                }
            };

            // b. Collect asks where price <= sweep_max_price, sorted most expensive first.
            //    Target top-of-book (0.99) first — that's where real stale fills happen.
            //    Cheap phantom asks (0.01-0.30) are tried last if budget remains.
            let mut eligible_asks: Vec<_> = orderbook
                .asks
                .iter()
                .filter(|a| {
                    let p = a.price.to_string().parse::<f64>().unwrap_or(1.0);
                    p >= cfg.sweep_min_price && p <= cfg.sweep_max_price
                })
                .collect();
            eligible_asks.sort_by(|a, b| b.price.cmp(&a.price));

            if eligible_asks.is_empty() {
                consecutive_empty_passes += 1;
                if consecutive_empty_passes >= 3 {
                    info!("Sweep {}: {} consecutive empty passes, stopping.", symbol, consecutive_empty_passes);
                    break;
                }
                info!("Sweep {}: no eligible asks, empty pass {}/3, retrying in 500ms...", symbol, consecutive_empty_passes);
                sleep(Duration::from_millis(500)).await;
                continue;
            }

            let mut filled_any = false;
            // c. For each ask: place FOK buy at that price/size (cheapest first)
            for ask in &eligible_asks {
                if sweep_start.elapsed() >= timeout {
                    break;
                }
                if total_cost >= cfg.max_sweep_cost {
                    info!("Sweep {}: reached max_sweep_cost ${} mid-pass, stopping.", symbol, cfg.max_sweep_cost);
                    break;
                }

                let price_str = format!("{}", ask.price);
                let size_str = cfg.sweep_order_size.clone();

                info!(
                    "Sweep {}: FOK BUY {} @ {} (ask size={})",
                    symbol, size_str, price_str, ask.size
                );

                match self.api.place_fok_buy(winning_token, &size_str, &price_str).await {
                    Ok(Some(resp)) => {
                        total_orders += 1;
                        let shares: f64 = size_str.parse().unwrap_or(0.0);
                        let price: f64 = price_str.parse().unwrap_or(0.0);
                        total_shares += shares;
                        total_cost += shares * price;
                        filled_any = true;
                        info!(
                            "Sweep {}: FILLED order #{} (id={}) +{} shares @ {} (total_cost=${})",
                            symbol,
                            total_orders,
                            resp.order_id.as_deref().unwrap_or("?"),
                            shares,
                            price_str,
                            total_cost
                        );
                    }
                    Ok(None) => {
                        info!("Sweep {}: FOK not fillable @ {}, skipping.", symbol, price_str);
                    }
                    Err(e) => {
                        // Network error — order may have been placed, halt sweep
                        error!("Sweep {}: FOK network error, halting sweep: {}", symbol, e);
                        break;
                    }
                }

                // d. Sleep inter_order_delay between orders
                sleep(Duration::from_millis(cfg.sweep_inter_order_delay_ms)).await;
            }

            if filled_any {
                consecutive_empty_passes = 0;
            } else {
                consecutive_empty_passes += 1;
                if consecutive_empty_passes >= 3 {
                    info!("Sweep {}: {} consecutive empty passes, stopping.", symbol, consecutive_empty_passes);
                    break;
                }
                info!("Sweep {}: no fills this pass ({}/3), retrying in 500ms...", symbol, consecutive_empty_passes);
                sleep(Duration::from_millis(500)).await;
            }

            // e. Re-fetch orderbook and repeat
        }

        info!(
            "Sweep {} complete: {} orders, {} shares, ${} cost",
            symbol, total_orders, total_shares, total_cost
        );
        Ok((total_orders, total_shares, total_cost))
    }

    /// Poll until 5m market is closed and resolved; returns winning outcome (e.g. "Up" or "Down").
    async fn poll_until_5m_resolved(&self, symbol: &str, m5_cid: &str) -> Option<String> {
        const INITIAL_DELAY_SECS: u64 = 60;
        const POLL_INTERVAL_SECS: u64 = 45;
        const MAX_WAIT_SECS: u64 = 600;
        info!("{} polling for resolution...", symbol);
        sleep(Duration::from_secs(INITIAL_DELAY_SECS)).await;
        let started = std::time::Instant::now();
        loop {
            if started.elapsed().as_secs() >= MAX_WAIT_SECS {
                warn!("{} resolution timeout", symbol);
                return None;
            }
            let m = match self.api.get_market(m5_cid).await {
                Ok(m) => m,
                Err(e) => {
                    warn!("{} poll failed: {}", symbol, e);
                    sleep(Duration::from_secs(POLL_INTERVAL_SECS)).await;
                    continue;
                }
            };
            let winner = m
                .tokens
                .iter()
                .find(|t| t.winner)
                .map(|t| {
                    if t.outcome.to_uppercase().contains("UP") || t.outcome == "1" {
                        "Up".to_string()
                    } else {
                        "Down".to_string()
                    }
                });
            if m.closed && winner.is_some() {
                info!("{} resolved: {}", symbol, winner.as_deref().unwrap_or("?"));
                return winner;
            }
            sleep(Duration::from_secs(POLL_INTERVAL_SECS)).await;
        }
    }

    /// Per-symbol loop: wait for market + price, run round, sweep, poll resolution, repeat.
    async fn run_symbol_loop(
        api: Arc<PolymarketApi>,
        config: Config,
        price_cache_5: PriceCacheMulti,
        latest_prices: LatestPriceCache,
        paper_trader: PaperTradeLogger,
        symbol: String,
    ) -> Result<()> {
        let discovery = MarketDiscovery::new(api.clone());
        let strategy = Self {
            api,
            config,
            discovery,
            price_cache_5,
            latest_prices,
            paper_trader,
        };
        loop {
            let (m5_cid, m5_up, m5_down, period_5, price_to_beat) =
                strategy.wait_for_5m_market_and_price(&symbol).await?;

            if let Err(e) = strategy
                .run_5m_round(&symbol, period_5)
                .await
            {
                error!("5m {} round error: {}", symbol, e);
            }

            // Paper trade log — always runs (pure observation, no orders)
            strategy
                .paper_trader
                .log(&strategy.config.strategy, &symbol, period_5, price_to_beat, &m5_up, &m5_down)
                .await;

            // Post-close sweep if enabled
            if strategy.config.strategy.sweep_enabled {
                if let Err(e) = strategy
                    .sweep_stale_asks(&symbol, price_to_beat, &m5_up, &m5_down)
                    .await
                {
                    error!("Sweep {} error: {}", symbol, e);
                }
            }

            let _ = strategy.poll_until_5m_resolved(&symbol, &m5_cid).await;
            sleep(Duration::from_secs(5)).await;
        }
    }

    pub async fn run(&self) -> Result<()> {
        let symbols = &self.config.strategy.symbols;
        let cfg = &self.config.strategy;
        info!("--- 5m bot | symbols: {:?} | sweep={} | sim={} ---",
            symbols, cfg.sweep_enabled, cfg.simulation_mode
        );

        let rtds_url = self.config.polymarket.rtds_ws_url.clone();
        let cache_5 = Arc::clone(&self.price_cache_5);
        let latest = Arc::clone(&self.latest_prices);
        let symbols_rtds = symbols.clone();
        if let Err(e) = run_chainlink_multi_poller(rtds_url, symbols_rtds, cache_5, latest).await {
            warn!("RTDS WS poller start failed: {}", e);
        }
        sleep(Duration::from_secs(2)).await;

        let mut handles = Vec::new();
        for symbol in symbols.clone() {
            let api = Arc::clone(&self.api);
            let config = self.config.clone();
            let price_cache_5 = Arc::clone(&self.price_cache_5);
            let latest_prices = Arc::clone(&self.latest_prices);
            let paper_trader = self.paper_trader.clone();
            handles.push(tokio::spawn(async move {
                if let Err(e) = Self::run_symbol_loop(api, config, price_cache_5, latest_prices, paper_trader, symbol.clone()).await {
                    error!("Symbol loop {} failed: {}", symbol, e);
                }
            }));
        }
        futures_util::future::try_join_all(handles).await?;
        Ok(())
    }
}
