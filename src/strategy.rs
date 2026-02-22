//! 5m post-close sweep bot: BTC, ETH, SOL, XRP. Monitor live prices during round,
//! then sweep stale limit orders after market closes using FOK orders.

use crate::api::PolymarketApi;
use crate::chainlink::run_chainlink_multi_poller;
use crate::config::Config;
use crate::discovery::{current_5m_period_start, parse_price_to_beat_from_question, MarketDiscovery, MARKET_5M_DURATION_SECS};
use crate::log_buffer::LogBuffer;
use crate::orderbook_ws::OrderbookMirror;
use crate::paper_trade::PaperTradeLogger;
use crate::rtds::{LatestPriceCache, PriceCacheMulti};
use anyhow::Result;
use chrono::Utc;
use log::{debug, error, info, warn};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio::time::{sleep, Duration};

/// How long to wait for RTDS prices to arrive before giving up on a period (seconds).
const PRICE_WAIT_TIMEOUT_SECS: u64 = 45;
/// How often to re-check for RTDS prices while waiting (seconds).
const PRICE_POLL_INTERVAL_SECS: u64 = 3;

/// Per-symbol market info discovered for a period.
struct SymbolRound {
    symbol: String,
    condition_id: String,
    up_token: String,
    down_token: String,
    period_5: i64,
    price_to_beat: f64,
}

pub struct ArbStrategy {
    api: Arc<PolymarketApi>,
    config: Config,
    discovery: MarketDiscovery,
    /// symbol -> period_start -> price-to-beat (from RTDS Chainlink).
    price_cache_5: PriceCacheMulti,
    /// Latest RTDS price per symbol (for post-close sweep winner determination).
    latest_prices: LatestPriceCache,
    /// Paper trade logger.
    paper_trader: PaperTradeLogger,
    /// Web dashboard log buffer.
    log_buffer: LogBuffer,
    /// Single orderbook mirror shared across the unified loop.
    orderbook_mirror: Arc<OrderbookMirror>,
}

impl ArbStrategy {
    pub fn new(api: Arc<PolymarketApi>, config: Config, log_buffer: LogBuffer) -> Self {
        let latest_prices: LatestPriceCache = Arc::new(RwLock::new(HashMap::new()));
        let paper_trader = PaperTradeLogger::new(Arc::clone(&latest_prices), log_buffer.clone());
        Self {
            discovery: MarketDiscovery::new(api.clone()),
            api,
            config,
            price_cache_5: Arc::new(RwLock::new(HashMap::new())),
            latest_prices,
            paper_trader,
            log_buffer,
            orderbook_mirror: Arc::new(OrderbookMirror::new()),
        }
    }

    /// Discover market + price-to-beat for a single symbol in the current period.
    /// Returns None if the market or price is not available.
    async fn discover_symbol(&self, symbol: &str) -> Result<Option<SymbolRound>> {
        let period_5 = current_5m_period_start();
        let (m5_cid, question) = match self.discovery.get_5m_market(symbol, period_5).await? {
            Some(v) => v,
            None => {
                warn!("{} no market for period {}", symbol, period_5);
                return Ok(None);
            }
        };
        // Try RTDS WS cache first, fall back to parsing market question
        let price_to_beat = {
            let cache = self.price_cache_5.read().await;
            cache.get(symbol).and_then(|per_period| per_period.get(&period_5).copied())
        };
        let price_to_beat = match price_to_beat {
            Some(p) => p,
            None => {
                // Fallback: parse price-to-beat from market question text
                match parse_price_to_beat_from_question(&question) {
                    Some(p) => {
                        info!("{} PTB from market question: ${} (RTDS not yet available)", symbol, p);
                        p
                    }
                    None => {
                        warn!("{} no price-to-beat from RTDS or market question for period {}", symbol, period_5);
                        return Ok(None);
                    }
                }
            }
        };
        let (m5_up, m5_down) = self.discovery.get_market_tokens(&m5_cid).await?;
        debug!("{} period={} ptb=${} up={}.. down={}..)",
            symbol, period_5, price_to_beat,
            &m5_up[..m5_up.len().min(12)],
            &m5_down[..m5_down.len().min(12)],
        );
        self.log_buffer.push(symbol, "info", format!("period={} ptb=${}", period_5, price_to_beat)).await;
        Ok(Some(SymbolRound {
            symbol: symbol.to_string(),
            condition_id: m5_cid,
            up_token: m5_up,
            down_token: m5_down,
            period_5,
            price_to_beat,
        }))
    }

    /// Post-close sweep: determine winner from latest RTDS WS price,
    /// then buy winning tokens from stale limit orders using FOK orders.
    async fn sweep_stale_asks(
        &self,
        symbol: &str,
        price_to_beat: f64,
        m5_up: &str,
        m5_down: &str,
    ) -> Result<(u32, f64, f64)> {
        let cfg = &self.config.strategy;
        let now_ms = Utc::now().timestamp_millis();

        let rtds_result = {
            let cache = self.latest_prices.read().await;
            cache.get(symbol).copied()
        };

        let latest_price = match rtds_result {
            Some((p, ts)) => {
                let age = (now_ms - ts) / 1000;
                debug!("Sweep {} RTDS WS: ${} (age={}s)", symbol, p, age);
                p
            }
            None => {
                warn!("Sweep {}: no RTDS WS price available, skipping.", symbol);
                return Ok((0, 0.0, 0.0));
            }
        };

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

        let diff = latest_price - price_to_beat;

        if diff == 0.0 {
            debug!("Sweep {}: diff=0 (tied), skipping.", symbol);
            return Ok((0, 0.0, 0.0));
        }

        let min_margin_abs = cfg.sweep_min_margin_pct * price_to_beat;
        if diff.abs() < min_margin_abs {
            debug!(
                "Sweep {}: diff ${} < min margin ${} ({}%), skipping.",
                symbol, diff.abs(), min_margin_abs, cfg.sweep_min_margin_pct * 100.0
            );
            return Ok((0, 0.0, 0.0));
        }

        let (winner, winning_token) = if diff > 0.0 {
            ("Up", m5_up)
        } else {
            ("Down", m5_down)
        };
        info!(
            "Sweep {}: winner={} | price=${} ptb=${} diff={}",
            symbol, winner, latest_price, price_to_beat, diff
        );
        self.log_buffer.push(symbol, "info", format!("sweep winner={} (price=${}, ptb=${}, diff={})", winner, latest_price, price_to_beat, diff)).await;

        let sweep_start = std::time::Instant::now();
        let timeout = Duration::from_secs(cfg.sweep_timeout_secs);
        let mut total_orders: u32 = 0;
        let mut total_shares: f64 = 0.0;
        let mut total_cost: f64 = 0.0;
        let mut consecutive_empty_passes: u32 = 0;

        while sweep_start.elapsed() < timeout {
            if total_cost >= cfg.max_sweep_cost {
                debug!("Sweep {}: reached max_sweep_cost ${}, stopping.", symbol, cfg.max_sweep_cost);
                break;
            }

            let orderbook = match self.orderbook_mirror.get_orderbook(winning_token).await {
                Some(ob) => ob,
                None => {
                    debug!("Sweep {}: no orderbook in WS mirror, waiting...", symbol);
                    self.orderbook_mirror.wait_for_update(Duration::from_secs(3)).await;
                    continue;
                }
            };

            let mut eligible_asks: Vec<_> = orderbook
                .asks
                .iter()
                .filter(|a| {
                    let p = a.price.to_string().parse::<f64>().unwrap_or(1.0);
                    p <= cfg.sweep_max_price
                })
                .collect();
            eligible_asks.sort_by(|a, b| b.price.cmp(&a.price));

            if eligible_asks.is_empty() {
                consecutive_empty_passes += 1;
                if consecutive_empty_passes >= 3 {
                    break;
                }
                self.orderbook_mirror.wait_for_update(Duration::from_secs(3)).await;
                continue;
            }

            let mut filled_any = false;
            for ask in &eligible_asks {
                if sweep_start.elapsed() >= timeout {
                    break;
                }
                if total_cost >= cfg.max_sweep_cost {
                    break;
                }

                let price_str = format!("{}", ask.price);
                let ask_price: f64 = price_str.parse().unwrap_or(1.0);
                let ask_size: f64 = ask.size.to_string().parse().unwrap_or(0.0);

                let remaining_budget = cfg.max_sweep_cost - total_cost;
                let max_affordable = if ask_price > 0.0 {
                    remaining_budget / ask_price
                } else {
                    0.0
                };
                let order_size = ask_size.min(max_affordable);
                let order_size = (order_size * 100.0).floor() / 100.0;
                if order_size < 0.01 {
                    continue;
                }
                let size_str = format!("{:.2}", order_size);

                info!("Sweep {}: FOK BUY {} @ {} (ask size={})", symbol, size_str, price_str, ask.size);

                match self.api.place_fok_buy(winning_token, &size_str, &price_str).await {
                    Ok(Some(resp)) => {
                        total_orders += 1;
                        total_shares += order_size;
                        total_cost += order_size * ask_price;
                        filled_any = true;
                        info!(
                            "Sweep {}: FILLED #{} (id={}) +{} @ {} (cost=${})",
                            symbol, total_orders,
                            resp.order_id.as_deref().unwrap_or("?"),
                            order_size, price_str, total_cost
                        );
                    }
                    Ok(None) => {
                        debug!("Sweep {}: FOK not fillable @ {}", symbol, price_str);
                    }
                    Err(e) => {
                        error!("Sweep {}: FOK network error, halting: {}", symbol, e);
                        break;
                    }
                }

                sleep(Duration::from_millis(cfg.sweep_inter_order_delay_ms)).await;
            }

            if filled_any {
                consecutive_empty_passes = 0;
            } else {
                consecutive_empty_passes += 1;
                if consecutive_empty_passes >= 3 {
                    break;
                }
                self.orderbook_mirror.wait_for_update(Duration::from_secs(3)).await;
            }
        }

        info!("Sweep {} complete: {} orders, {} shares, ${} cost", symbol, total_orders, total_shares, total_cost);
        self.log_buffer.push(symbol, "info", format!("sweep done: {} orders, {} shares, ${} cost", total_orders, total_shares, total_cost)).await;
        Ok((total_orders, total_shares, total_cost))
    }

    /// Unified loop: discover all symbols, subscribe at T-5s, sweep after close.
    pub async fn run(&self) -> Result<()> {
        let symbols = &self.config.strategy.symbols;
        let cfg = &self.config.strategy;
        info!("5m bot started | symbols: {:?} | sweep={}", symbols, cfg.sweep_enabled);

        // Start RTDS price feed
        let rtds_url = self.config.polymarket.rtds_ws_url.clone();
        let cache_5 = Arc::clone(&self.price_cache_5);
        let latest = Arc::clone(&self.latest_prices);
        let symbols_rtds = symbols.clone();
        if let Err(e) = run_chainlink_multi_poller(rtds_url, symbols_rtds, cache_5, latest).await {
            warn!("RTDS WS poller start failed: {}", e);
        }
        sleep(Duration::from_secs(2)).await;

        loop {
            // === Phase 1: Discover all markets early in the period ===
            // Retry discovery with a timeout to wait for RTDS prices to arrive.
            let period_5 = current_5m_period_start();
            let mut rounds: Vec<SymbolRound> = Vec::new();
            let discovery_deadline = std::time::Instant::now() + Duration::from_secs(PRICE_WAIT_TIMEOUT_SECS);

            loop {
                rounds.clear();
                for symbol in symbols {
                    match self.discover_symbol(symbol).await {
                        Ok(Some(round)) => rounds.push(round),
                        Ok(None) => {}
                        Err(e) => error!("{} discovery error: {}", symbol, e),
                    }
                }

                if !rounds.is_empty() {
                    break;
                }

                // Check if we're still in the same period and have time to retry
                if current_5m_period_start() != period_5 {
                    warn!("Period rolled over from {} before prices arrived", period_5);
                    break;
                }
                if std::time::Instant::now() >= discovery_deadline {
                    warn!("Timed out waiting {}s for RTDS prices for period {}", PRICE_WAIT_TIMEOUT_SECS, period_5);
                    break;
                }

                debug!("Waiting for RTDS prices for period {} (retrying in {}s)...", period_5, PRICE_POLL_INTERVAL_SECS);
                sleep(Duration::from_secs(PRICE_POLL_INTERVAL_SECS)).await;
            }

            if rounds.is_empty() {
                warn!("No markets discovered for period {}, sleeping until next", period_5);
                let remaining = (period_5 + MARKET_5M_DURATION_SECS) - Utc::now().timestamp();
                if remaining > 0 {
                    sleep(Duration::from_secs(remaining as u64)).await;
                }
                continue;
            }

            info!("Discovered {}/{} markets for period {}", rounds.len(), symbols.len(), period_5);

            // === Phase 2: Pre-warm order cache ===
            if self.api.is_authenticated() {
                for round in &rounds {
                    for token in [&round.up_token, &round.down_token] {
                        if let Err(e) = self.api.warm_order_cache(token).await {
                            debug!("{} cache warm failed for {}..): {}", round.symbol, &token[..token.len().min(12)], e);
                        }
                    }
                }
            }

            // === Phase 3: Wait for period close ===
            let close_time = period_5 + MARKET_5M_DURATION_SECS;
            let remaining = close_time - Utc::now().timestamp();
            if remaining > 0 {
                debug!("Waiting {}s until close", remaining);
                sleep(Duration::from_secs(remaining as u64)).await;
            }
            info!("Period {} closed", period_5);

            // === Phase 6: Paper trade + sweep each symbol ===
            for round in &rounds {
                // Paper trade log
                self.paper_trader
                    .log(
                        &self.config.strategy, &round.symbol, round.period_5,
                        round.price_to_beat, &round.up_token, &round.down_token,
                        &round.condition_id, &self.orderbook_mirror,
                    )
                    .await;

                // Sweep
                if cfg.sweep_enabled {
                    if let Err(e) = self
                        .sweep_stale_asks(&round.symbol, round.price_to_beat, &round.up_token, &round.down_token)
                        .await
                    {
                        error!("Sweep {} error: {}", round.symbol, e);
                    }
                }
            }

            // === Phase 7: Cleanup ===
            self.orderbook_mirror.unsubscribe_all().await;

            // Poll resolution for all markets (in parallel)
            let mut resolution_handles = Vec::new();
            for round in &rounds {
                let api = Arc::clone(&self.api);
                let symbol = round.symbol.clone();
                let cid = round.condition_id.clone();
                resolution_handles.push(tokio::spawn(async move {
                    // Inline poll logic (can't use &self across spawn)
                    const INITIAL_DELAY: u64 = 60;
                    const POLL_INTERVAL: u64 = 45;
                    const MAX_WAIT: u64 = 600;
                    debug!("{} polling for resolution...", symbol);
                    sleep(Duration::from_secs(INITIAL_DELAY)).await;
                    let started = std::time::Instant::now();
                    loop {
                        if started.elapsed().as_secs() >= MAX_WAIT {
                            debug!("{} resolution timeout", symbol);
                            break;
                        }
                        match api.get_market(&cid).await {
                            Ok(m) => {
                                let winner = m.tokens.iter().find(|t| t.winner).map(|t| {
                                    if t.outcome.to_uppercase().contains("UP") || t.outcome == "1" {
                                        "Up"
                                    } else {
                                        "Down"
                                    }
                                });
                                if m.closed && winner.is_some() {
                                    info!("{} resolved: {}", symbol, winner.unwrap_or("?"));
                                    break;
                                }
                            }
                            Err(e) => debug!("{} resolution poll failed: {}", symbol, e),
                        }
                        sleep(Duration::from_secs(POLL_INTERVAL)).await;
                    }
                }));
            }
            // Wait for all resolutions
            for handle in resolution_handles {
                let _ = handle.await;
            }

            sleep(Duration::from_secs(5)).await;
        }
    }
}
