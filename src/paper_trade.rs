//! Paper trade logger: after each 5m round, log what we *would* have traded
//! to paper_trade.md. Pure observation — no orders placed.

use crate::api::PolymarketApi;
use crate::config::StrategyConfig;
use crate::discovery::format_5m_period_et;
use crate::log_buffer::LogBuffer;
use crate::orderbook_ws::OrderbookMirror;
use crate::rtds::LatestPriceCache;
use chrono::Utc;
use log::{error, info};
use std::fmt::Write as FmtWrite;
use std::io::Write as IoWrite;
use std::sync::Arc;
use tokio::sync::Mutex;

/// Shared handle for paper trade logging across concurrent symbol loops.
#[derive(Clone)]
pub struct PaperTradeLogger {
    api: Arc<PolymarketApi>,
    latest_prices: LatestPriceCache,
    orderbook_mirror: Arc<OrderbookMirror>,
    file_mutex: Arc<Mutex<()>>,
    log_buffer: LogBuffer,
}

impl PaperTradeLogger {
    pub fn new(
        api: Arc<PolymarketApi>,
        latest_prices: LatestPriceCache,
        orderbook_mirror: Arc<OrderbookMirror>,
        log_buffer: LogBuffer,
    ) -> Self {
        // Create/touch paper_trade.md immediately so it exists from startup
        match std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open("paper_trade.md")
        {
            Ok(mut f) => {
                // Write header only if the file is empty (newly created)
                if f.metadata().map(|m| m.len() == 0).unwrap_or(false) {
                    let header = format!(
                        "# Paper Trade Log\n\nStarted: {}\n\n---\n\n",
                        Utc::now().format("%Y-%m-%d %H:%M:%S UTC")
                    );
                    let _ = f.write_all(header.as_bytes());
                }
                info!("paper_trade.md ready");
            }
            Err(e) => {
                error!("Failed to create paper_trade.md on startup: {}", e);
            }
        }
        Self {
            api,
            latest_prices,
            orderbook_mirror,
            file_mutex: Arc::new(Mutex::new(())),
            log_buffer,
        }
    }

    /// Log a paper trade entry after a 5m round ends.
    /// Fetches prices from both sources, determines winner, fetches orderbook,
    /// and appends a formatted markdown section to paper_trade.md.
    pub async fn log(
        &self,
        cfg: &StrategyConfig,
        symbol: &str,
        period_5: i64,
        price_to_beat: f64,
        m5_up: &str,
        m5_down: &str,
    ) {
        info!("Paper trade logging: {} period={} ptb=${}", symbol, period_5, price_to_beat);
        let now_ms = Utc::now().timestamp_millis();
        let period_str = format_5m_period_et(period_5);

        // --- RTDS WS (cached from Polymarket WebSocket) ---
        let rtds_result = {
            let cache = self.latest_prices.read().await;
            cache.get(symbol).copied()
        };

        let (latest_price_opt, age_s) = match rtds_result {
            Some((p, ts)) => {
                let age = (now_ms - ts) / 1000;
                (Some(p), age)
            }
            None => (None, i64::MAX),
        };

        // Build the markdown entry
        let mut md = String::new();
        let _ = writeln!(
            md,
            "## {} | {}\n",
            symbol.to_uppercase(),
            period_str
        );
        let _ = writeln!(md, "- **Price-to-beat**: ${}", price_to_beat);
        match latest_price_opt {
            Some(p) => { let _ = writeln!(md, "- **RTDS WS**: ${} (age={}s)", p, age_s); }
            None => { let _ = writeln!(md, "- **RTDS WS**: unavailable"); }
        }

        let best_age_s = age_s;
        let latest_price = match latest_price_opt {
            Some(p) => p,
            None => {
                let _ = writeln!(md, "- **NO CLOSE PRICE** - cannot determine winner\n");
                let _ = writeln!(md, "---\n");
                self.append(&md).await;
                self.log_buffer.push(symbol, "warn", format!("{} | no close price available", period_str)).await;
                return;
            }
        };

        // Staleness check (consistency with real strategy)
        if best_age_s > cfg.sweep_timeout_secs as i64 {
            let _ = writeln!(
                md,
                "- **STALE** — price is {}s old (max {}s), would skip in live mode\n",
                best_age_s, cfg.sweep_timeout_secs
            );
            let _ = writeln!(md, "---\n");
            self.append(&md).await;
            self.log_buffer.push(symbol, "warn", format!("{} | stale price ({}s old)", period_str, best_age_s)).await;
            return;
        }

        // Determine winner
        let diff = latest_price - price_to_beat;

        // Zero diff (tied prices)
        if diff == 0.0 {
            let _ = writeln!(md, "- **Winner**: NONE (tied) — diff=0, skipping\n");
            let _ = writeln!(md, "---\n");
            self.append(&md).await;
            self.log_buffer.push(symbol, "info", format!("{} | tied (ptb=${}, close=${})", period_str, price_to_beat, latest_price)).await;
            return;
        }

        // Minimum margin check (percentage of price_to_beat)
        let min_margin_abs = cfg.sweep_min_margin_pct * price_to_beat;
        if diff.abs() < min_margin_abs {
            let _ = writeln!(
                md,
                "- **BELOW MARGIN** — diff ${} < min margin ${} ({}% of ${}), would skip in live mode\n",
                diff.abs(), min_margin_abs, cfg.sweep_min_margin_pct * 100.0, price_to_beat
            );
            let _ = writeln!(md, "---\n");
            self.append(&md).await;
            self.log_buffer.push(symbol, "info", format!("{} | below margin (diff=${})", period_str, diff.abs())).await;
            return;
        }

        let (winner, winning_token) = if diff > 0.0 {
            ("Up", m5_up)
        } else {
            ("Down", m5_down)
        };

        let _ = writeln!(
            md,
            "- **Winner**: {} (diff={}{})\n",
            winner,
            if diff >= 0.0 { "+$" } else { "-$" },
            diff.abs(),
        );

        // Fetch orderbook for winning token (WS mirror first, REST fallback)
        let (orderbook_result, ob_source) =
            if let Some(ob) = self.orderbook_mirror.get_orderbook(winning_token).await {
                (Ok(ob), "WS mirror")
            } else {
                (self.api.get_orderbook(winning_token).await, "REST")
            };
        match orderbook_result {
            Ok(orderbook) => {
                let _ = writeln!(md, "### Winning token orderbook ({}) [source: {}]", winner, ob_source);
                let _ = writeln!(md, "| Price | Size | USD Value |");
                let _ = writeln!(md, "|-------|------|-----------|");

                let mut sweepable_levels = 0u32;
                let mut sweepable_shares = 0.0f64;
                let mut sweepable_cost = 0.0f64;
                let mut capped_cost = 0.0f64;
                let mut capped_shares = 0.0f64;
                let mut capped_levels = 0u32;

                // Sort asks descending (most expensive first) to match sweep order
                let mut sorted_asks = orderbook.asks.clone();
                sorted_asks.sort_by(|a, b| b.price.cmp(&a.price));

                for ask in &sorted_asks {
                    let p: f64 = ask.price.to_string().parse().unwrap_or(1.0);
                    let s: f64 = ask.size.to_string().parse().unwrap_or(0.0);
                    let usd = p * s;
                    let in_range = p >= cfg.sweep_min_price && p <= cfg.sweep_max_price;
                    let marker = if in_range && capped_cost < cfg.max_sweep_cost { " *" } else { "" };
                    let _ = writeln!(md, "| {}  | {}  | ${}  |{}", p, s, usd, marker);

                    if in_range {
                        sweepable_levels += 1;
                        sweepable_shares += s;
                        sweepable_cost += usd;
                        // Track what we'd actually buy within max_sweep_cost budget
                        if capped_cost < cfg.max_sweep_cost {
                            let remaining_budget = cfg.max_sweep_cost - capped_cost;
                            let buyable_usd = usd.min(remaining_budget);
                            let buyable_shares = if p > 0.0 { buyable_usd / p } else { 0.0 };
                            capped_cost += buyable_usd;
                            capped_shares += buyable_shares;
                            capped_levels += 1;
                        }
                    }
                }

                let _ = writeln!(md);
                let _ = writeln!(
                    md,
                    "- **Sweepable asks** (<= {}): {} levels, {} shares, ${} cost",
                    cfg.sweep_max_price, sweepable_levels, sweepable_shares, sweepable_cost
                );

                if capped_shares > 0.0 {
                    let avg_price = capped_cost / capped_shares;
                    let profit = capped_shares * (1.0 - avg_price);
                    let _ = writeln!(
                        md,
                        "- **Within budget** (${}): {} levels, {:.2} shares, ${:.2} cost, avg {:.4}",
                        cfg.max_sweep_cost, capped_levels, capped_shares, capped_cost, avg_price
                    );
                    let _ = writeln!(
                        md,
                        "- **Hypothetical P&L**: buy {:.2} shares @ avg {:.4} -> profit ${:.2}\n",
                        capped_shares, avg_price, profit
                    );
                    let _ = writeln!(md, "---\n");
                    self.append(&md).await;
                    self.log_buffer.push(
                        symbol,
                        "info",
                        format!(
                            "{} | winner={} ptb=${} close=${} diff={}{} | {:.2} shares @ avg {:.4} -> P&L ${:.2}",
                            period_str, winner, price_to_beat, latest_price,
                            if diff >= 0.0 { "+$" } else { "-$" }, diff.abs(),
                            capped_shares, avg_price, profit
                        ),
                    ).await;
                } else {
                    let _ = writeln!(md, "- **Hypothetical P&L**: no sweepable asks\n");
                    let _ = writeln!(md, "---\n");
                    self.append(&md).await;
                    self.log_buffer.push(
                        symbol,
                        "info",
                        format!(
                            "{} | winner={} ptb=${} close=${} diff={}{} | no sweepable asks",
                            period_str, winner, price_to_beat, latest_price,
                            if diff >= 0.0 { "+$" } else { "-$" }, diff.abs(),
                        ),
                    ).await;
                }
            }
            Err(e) => {
                let _ = writeln!(md, "### Winning token orderbook ({})\n", winner);
                let _ = writeln!(md, "- **Orderbook fetch failed**: {}\n", e);
                let _ = writeln!(md, "---\n");
                self.append(&md).await;
                self.log_buffer.push(symbol, "error", format!("{} | orderbook failed: {}", period_str, e)).await;
            }
        }
    }

    /// Append content to paper_trade.md, guarded by mutex.
    async fn append(&self, content: &str) {
        let _guard = self.file_mutex.lock().await;
        match std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open("paper_trade.md")
        {
            Ok(mut f) => {
                if let Err(e) = f.write_all(content.as_bytes()) {
                    error!("Failed to write paper_trade.md: {}", e);
                } else {
                    info!("paper_trade.md updated ({} bytes)", content.len());
                }
            }
            Err(e) => {
                error!("Failed to open paper_trade.md: {}", e);
            }
        }
    }
}
