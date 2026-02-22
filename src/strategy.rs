//! 5m post-close sweep bot: BTC, ETH, SOL, XRP. Monitor live prices during round,
//! then sweep stale limit orders after market closes using FOK orders.

use crate::api::PolymarketApi;
use crate::chainlink::run_chainlink_multi_poller;
use crate::config::Config;
use crate::discovery::{current_5m_period_start, MarketDiscovery, MARKET_5M_DURATION_SECS};
use crate::log_buffer::LogBuffer;
use crate::orderbook_ws::OrderbookMirror;
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
    /// Web dashboard log buffer.
    log_buffer: LogBuffer,
    /// WebSocket orderbook mirror (shared across symbol loops).
    orderbook_mirror: Arc<OrderbookMirror>,
}

impl ArbStrategy {
    pub fn new(api: Arc<PolymarketApi>, config: Config, log_buffer: LogBuffer) -> Self {
        let latest_prices: LatestPriceCache = Arc::new(RwLock::new(HashMap::new()));
        let orderbook_mirror = Arc::new(OrderbookMirror::new());
        let paper_trader = PaperTradeLogger::new(api.clone(), Arc::clone(&latest_prices), Arc::clone(&orderbook_mirror), log_buffer.clone());
        Self {
            discovery: MarketDiscovery::new(api.clone()),
            api,
            config,
            price_cache_5: Arc::new(RwLock::new(HashMap::new())),
            latest_prices,
            paper_trader,
            log_buffer,
            orderbook_mirror,
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
            // Price-to-beat: RTDS WS Chainlink capture at period start.
            let price_to_beat = {
                let cache = self.price_cache_5.read().await;
                cache.get(symbol).and_then(|per_period| per_period.get(&period_5).copied())
            };
            let price_to_beat = match price_to_beat {
                Some(p) => p,
                None => {
                    warn!("{} no price-to-beat from RTDS WS for period {}, waiting...", symbol, period_5);
                    let remaining = (period_5 + MARKET_5M_DURATION_SECS) - Utc::now().timestamp();
                    if remaining > 0 {
                        sleep(Duration::from_secs(remaining as u64)).await;
                    }
                    continue;
                }
            };
            let (m5_up, m5_down) = self.discovery.get_market_tokens(&m5_cid).await?;
            info!("{} period={} price-to-beat=${} (RTDS WS)", symbol, period_5, price_to_beat);
            self.log_buffer.push(symbol, "info", format!("period={} price-to-beat=${} (RTDS WS)", period_5, price_to_beat)).await;
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
        self.log_buffer.push(symbol, "info", format!("period {} closed", period_5)).await;
        Ok(())
    }

    /// Post-close sweep: determine winner from latest RTDS WS price,
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

        // --- RTDS WS (cached from WebSocket) ---
        let rtds_result = {
            let cache = self.latest_prices.read().await;
            cache.get(symbol).copied()
        };

        let (latest_price, age_secs) = match rtds_result {
            Some((p, ts)) => {
                let age = (now_ms - ts) / 1000;
                info!("Sweep {} RTDS WS: ${} (age={}s)", symbol, p, age);
                (p, age)
            }
            None => {
                warn!("Sweep {}: no RTDS WS price available, skipping.", symbol);
                return Ok((0, 0.0, 0.0));
            }
        };

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
        self.log_buffer.push(symbol, "info", format!("sweep winner={} (price=${}, ptb=${}, diff={})", winner, latest_price, price_to_beat, diff)).await;

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

            // a. Read orderbook from WS mirror (instant), fall back to REST
            let orderbook = if let Some(ob) = self.orderbook_mirror.get_orderbook(winning_token).await {
                ob
            } else {
                match self.api.get_orderbook(winning_token).await {
                    Ok(ob) => ob,
                    Err(e) => {
                        warn!("Sweep {}: orderbook fetch failed: {}", symbol, e);
                        break;
                    }
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
                info!("Sweep {}: no eligible asks, empty pass {}/3, waiting for WS update...", symbol, consecutive_empty_passes);
                self.orderbook_mirror.wait_for_update(Duration::from_secs(3)).await;
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
                let ask_price: f64 = price_str.parse().unwrap_or(1.0);
                let ask_size: f64 = ask.size.to_string().parse().unwrap_or(0.0);

                // Dynamic sizing: match the ask size, capped by remaining budget
                let remaining_budget = cfg.max_sweep_cost - total_cost;
                let max_affordable = if ask_price > 0.0 {
                    remaining_budget / ask_price
                } else {
                    0.0
                };
                let order_size = ask_size.min(max_affordable);
                // Round down to 2 decimal places (SDK LOT_SIZE_SCALE)
                let order_size = (order_size * 100.0).floor() / 100.0;
                if order_size < 0.01 {
                    info!("Sweep {}: order_size too small ({:.2}), skipping ask @ {}", symbol, order_size, price_str);
                    continue;
                }
                let size_str = format!("{:.2}", order_size);

                info!(
                    "Sweep {}: FOK BUY {} @ {} (ask size={})",
                    symbol, size_str, price_str, ask.size
                );

                match self.api.place_fok_buy(winning_token, &size_str, &price_str).await {
                    Ok(Some(resp)) => {
                        total_orders += 1;
                        total_shares += order_size;
                        total_cost += order_size * ask_price;
                        filled_any = true;
                        info!(
                            "Sweep {}: FILLED order #{} (id={}) +{} shares @ {} (total_cost=${})",
                            symbol,
                            total_orders,
                            resp.order_id.as_deref().unwrap_or("?"),
                            order_size,
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
                info!("Sweep {}: no fills this pass ({}/3), waiting for WS update...", symbol, consecutive_empty_passes);
                self.orderbook_mirror.wait_for_update(Duration::from_secs(3)).await;
            }

            // e. Re-fetch orderbook and repeat
        }

        info!(
            "Sweep {} complete: {} orders, {} shares, ${} cost",
            symbol, total_orders, total_shares, total_cost
        );
        self.log_buffer.push(symbol, "info", format!("sweep complete: {} orders, {} shares, ${} cost", total_orders, total_shares, total_cost)).await;
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
        log_buffer: LogBuffer,
        orderbook_mirror: Arc<OrderbookMirror>,
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
            log_buffer,
            orderbook_mirror,
        };
        loop {
            let (m5_cid, m5_up, m5_down, period_5, price_to_beat) =
                strategy.wait_for_5m_market_and_price(&symbol).await?;

            // Subscribe to WS orderbook for both tokens (~5min of data before sweep)
            if let Err(e) = strategy.orderbook_mirror.subscribe(&[&m5_up, &m5_down]) {
                warn!("{} WS orderbook subscribe failed: {}", symbol, e);
            }

            // Pre-warm SDK order cache (fee_rate_bps + tick_size) for both tokens
            for token in [&m5_up, &m5_down] {
                if let Err(e) = strategy.api.warm_order_cache(token).await {
                    warn!("{} cache warm failed for {}: {}", symbol, &token[..token.len().min(20)], e);
                }
            }

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

            // Clean up WS subscriptions for this period
            strategy.orderbook_mirror.unsubscribe_all().await;

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
            let log_buffer = self.log_buffer.clone();
            let orderbook_mirror = Arc::clone(&self.orderbook_mirror);
            handles.push(tokio::spawn(async move {
                if let Err(e) = Self::run_symbol_loop(api, config, price_cache_5, latest_prices, paper_trader, log_buffer, orderbook_mirror, symbol.clone()).await {
                    error!("Symbol loop {} failed: {}", symbol, e);
                }
            }));
        }
        futures_util::future::try_join_all(handles).await?;
        Ok(())
    }
}
