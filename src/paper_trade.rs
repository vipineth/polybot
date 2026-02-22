//! Paper trade logger: after each 5m round, log what we *would* have traded.
//! Takes two orderbook snapshots: at close and after a post-close monitoring window.

use crate::config::StrategyConfig;
use crate::discovery::format_5m_period_et;
use crate::log_buffer::LogBuffer;
use crate::orderbook_ws::OrderbookMirror;
use crate::rtds::LatestPriceCache;
use chrono::Utc;
use log::{info, warn};
use std::fmt::Write as FmtWrite;
use tokio::fs::OpenOptions;
use tokio::io::AsyncWriteExt;
use tokio::time::{sleep, Duration};

const PAPER_TRADE_FILE: &str = "paper_trade.md";

/// How long to monitor the orderbook after close (total window).
const POST_CLOSE_MONITOR_SECS: u64 = 20;
/// How often to dump the orderbook during post-close monitoring.
const OB_DUMP_INTERVAL_SECS: u64 = 2;

/// Shared handle for paper trade logging across concurrent symbol loops.
#[derive(Clone)]
pub struct PaperTradeLogger {
    latest_prices: LatestPriceCache,
    log_buffer: LogBuffer,
}

impl PaperTradeLogger {
    pub fn new(
        latest_prices: LatestPriceCache,
        log_buffer: LogBuffer,
    ) -> Self {
        Self {
            latest_prices,
            log_buffer,
        }
    }

    /// Log a paper trade entry after a 5m round ends.
    /// Takes orderbook snapshot at close, waits 10s, takes another snapshot,
    /// and logs whether any favorable asks appeared in the post-close window.
    pub async fn log(
        &self,
        cfg: &StrategyConfig,
        symbol: &str,
        period_5: i64,
        price_to_beat: f64,
        m5_up: &str,
        m5_down: &str,
        orderbook_mirror: &OrderbookMirror,
    ) {
        info!("Paper trade: {} period={} ptb=${}", symbol, period_5, price_to_beat);
        let now_ms = Utc::now().timestamp_millis();
        let period_str = format_5m_period_et(period_5);

        // Get close price from RTDS WS
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

        let mut md = String::new();
        let _ = writeln!(md, "## {} | {}\n", symbol.to_uppercase(), period_str);
        let _ = writeln!(md, "- **Price-to-beat**: ${}", price_to_beat);
        match latest_price_opt {
            Some(p) => { let _ = writeln!(md, "- **Close price (RTDS WS)**: ${} (age={}s)", p, age_s); }
            None => { let _ = writeln!(md, "- **Close price**: unavailable"); }
        }

        let latest_price = match latest_price_opt {
            Some(p) => p,
            None => {
                let _ = writeln!(md, "- **NO CLOSE PRICE** — cannot determine winner\n---\n");
                self.log_entry(symbol, &md).await;
                self.log_buffer.push(symbol, "warn", format!("{} | no close price", period_str)).await;
                return;
            }
        };

        let diff = latest_price - price_to_beat;

        if diff == 0.0 {
            let _ = writeln!(md, "- **Winner**: NONE (tied) — diff=0\n---\n");
            self.log_entry(symbol, &md).await;
            self.log_buffer.push(symbol, "info", format!("{} | tied", period_str)).await;
            return;
        }

        let min_margin_abs = cfg.sweep_min_margin_pct * price_to_beat;
        if diff.abs() < min_margin_abs {
            let _ = writeln!(
                md, "- **BELOW MARGIN** — diff ${} < min ${} ({}%)\n---\n",
                diff.abs(), min_margin_abs, cfg.sweep_min_margin_pct * 100.0
            );
            self.log_entry(symbol, &md).await;
            self.log_buffer.push(symbol, "info", format!("{} | below margin (diff=${})", period_str, diff.abs())).await;
            return;
        }

        let (winner, winning_token) = if diff > 0.0 {
            ("Up", m5_up)
        } else {
            ("Down", m5_down)
        };

        let _ = writeln!(
            md, "- **Winner**: {} (diff={}{})\n",
            winner, if diff >= 0.0 { "+$" } else { "-$" }, diff.abs(),
        );

        // Subscribe to winning token orderbook after determining winner
        info!("{} subscribing to winning token orderbook...", symbol);
        if let Err(e) = orderbook_mirror.subscribe(&[winning_token]).await {
            warn!("{} orderbook subscribe failed: {}", symbol, e);
        }

        // Dump orderbook every 2s so we can see how it evolves post-close
        let _ = writeln!(md, "### Winning side ({}) orderbook snapshots", winner);
        let start = std::time::Instant::now();
        let mut snapshot_num = 0u32;
        let mut last_summary = OrderbookSnapshot { md: String::new(), capped_shares: 0.0, avg_price: 0.0, profit: 0.0 };
        loop {
            let elapsed = start.elapsed().as_secs();
            if elapsed >= POST_CLOSE_MONITOR_SECS {
                break;
            }
            // Wait for orderbook data (first iteration) or next update (subsequent)
            if snapshot_num == 0 {
                // Give WS a moment to deliver initial snapshot
                sleep(Duration::from_millis(500)).await;
            }

            let ob_opt = orderbook_mirror.get_orderbook(winning_token).await;
            if let Some(ref ob) = ob_opt {
                // Only dump if there are sweepable asks (price <= sweep_max_price)
                let sweepable_asks: Vec<_> = {
                    let mut sorted = ob.asks.clone();
                    sorted.retain(|a| {
                        let p: f64 = a.price.to_string().parse().unwrap_or(1.0);
                        p <= cfg.sweep_max_price
                    });
                    sorted.sort_by(|a, b| a.price.cmp(&b.price));
                    sorted
                };

                if sweepable_asks.is_empty() {
                    continue;
                }

                snapshot_num += 1;
                let elapsed_s = start.elapsed().as_secs();
                let ts = Utc::now().format("%H:%M:%S");
                let asks_summary: Vec<String> = sweepable_asks.iter()
                    .take(5)
                    .map(|a| format!("{}x{}", a.price, a.size))
                    .collect();
                info!(
                    "[OB {} {} T+{}s {}] asks: {}",
                    symbol, winner, elapsed_s, ts,
                    asks_summary.join(", "),
                );

                // Detailed snapshot for paper_trade.md
                let _ = writeln!(md, "\n**T+{}s ({}) snapshot #{}:**", elapsed_s, ts, snapshot_num);
                last_summary = self.snapshot_orderbook(cfg, winning_token, winner, orderbook_mirror).await;
                let _ = write!(md, "{}", last_summary.md);
            }

            sleep(Duration::from_secs(OB_DUMP_INTERVAL_SECS)).await;
        }

        let post_summary = last_summary;

        // === Verdict ===
        let favorable = post_summary.capped_shares > 0.0;
        if favorable {
            let _ = writeln!(
                md, "**FAVORABLE** — {:.2} sweepable shares found post-close (avg {:.4}, P&L ${:.2})\n",
                post_summary.capped_shares, post_summary.avg_price, post_summary.profit
            );
        } else {
            let _ = writeln!(md, "**NO FAVORABLE ASKS** found in post-close window\n");
        }
        let _ = writeln!(md, "---\n");

        self.log_entry(symbol, &md).await;

        // Log buffer summary
        let summary = if favorable {
            format!(
                "{} | {} ptb=${} close=${} diff={}{} | post-close: {:.2}sh @ {:.4} -> P&L ${:.2}",
                period_str, winner, price_to_beat, latest_price,
                if diff >= 0.0 { "+$" } else { "-$" }, diff.abs(),
                post_summary.capped_shares, post_summary.avg_price, post_summary.profit
            )
        } else {
            format!(
                "{} | {} ptb=${} close=${} diff={}{} | no favorable asks post-close",
                period_str, winner, price_to_beat, latest_price,
                if diff >= 0.0 { "+$" } else { "-$" }, diff.abs(),
            )
        };
        self.log_buffer.push(symbol, "info", summary).await;
    }

    /// Take a snapshot of the winning token's orderbook and return formatted summary.
    async fn snapshot_orderbook(
        &self,
        cfg: &StrategyConfig,
        winning_token: &str,
        _winner: &str,
        orderbook_mirror: &OrderbookMirror,
    ) -> OrderbookSnapshot {
        let orderbook_opt = orderbook_mirror.get_orderbook(winning_token).await;
        let mut md = String::new();

        match orderbook_opt {
            Some(orderbook) => {
                let _ = writeln!(md, "| Price | Size | USD |");
                let _ = writeln!(md, "|-------|------|-----|");

                let mut sweepable_levels = 0u32;
                let mut sweepable_shares = 0.0f64;
                let mut sweepable_cost = 0.0f64;
                let mut capped_cost = 0.0f64;
                let mut capped_shares = 0.0f64;
                let mut capped_levels = 0u32;

                let mut sorted_asks = orderbook.asks.clone();
                sorted_asks.sort_by(|a, b| b.price.cmp(&a.price));

                for ask in &sorted_asks {
                    let p: f64 = ask.price.to_string().parse().unwrap_or(1.0);
                    let s: f64 = ask.size.to_string().parse().unwrap_or(0.0);
                    let usd = p * s;
                    let in_range = p <= cfg.sweep_max_price;
                    let marker = if in_range && capped_cost < cfg.max_sweep_cost { " *" } else { "" };
                    let _ = writeln!(md, "| {} | {} | ${:.2} |{}", p, s, usd, marker);

                    if in_range {
                        sweepable_levels += 1;
                        sweepable_shares += s;
                        sweepable_cost += usd;
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
                    md, "Sweepable (<= {}): {} levels, {:.2} shares, ${:.2}",
                    cfg.sweep_max_price, sweepable_levels, sweepable_shares, sweepable_cost
                );

                let avg_price = if capped_shares > 0.0 { capped_cost / capped_shares } else { 0.0 };
                let profit = if capped_shares > 0.0 { capped_shares * (1.0 - avg_price) } else { 0.0 };

                if capped_shares > 0.0 {
                    let _ = writeln!(
                        md, "Within budget (${}): {} levels, {:.2} shares, ${:.2} cost, avg {:.4}",
                        cfg.max_sweep_cost, capped_levels, capped_shares, capped_cost, avg_price
                    );
                    let _ = writeln!(
                        md, "Hypothetical P&L: {:.2} shares @ {:.4} -> ${:.2}\n",
                        capped_shares, avg_price, profit
                    );
                } else {
                    let _ = writeln!(md, "No sweepable asks within budget\n");
                }

                OrderbookSnapshot { md, capped_shares, avg_price, profit }
            }
            None => {
                let _ = writeln!(md, "No orderbook data in WS mirror\n");
                OrderbookSnapshot { md, capped_shares: 0.0, avg_price: 0.0, profit: 0.0 }
            }
        }
    }

    /// Log entry to both stdout and paper_trade.md file.
    async fn log_entry(&self, symbol: &str, content: &str) {
        info!("Paper trade [{}]:\n{}", symbol, content);

        match OpenOptions::new()
            .create(true)
            .append(true)
            .open(PAPER_TRADE_FILE)
            .await
        {
            Ok(mut file) => {
                if let Err(e) = file.write_all(content.as_bytes()).await {
                    warn!("Failed to write paper_trade.md: {}", e);
                }
            }
            Err(e) => {
                warn!("Failed to open paper_trade.md: {}", e);
            }
        }
    }
}

struct OrderbookSnapshot {
    md: String,
    capped_shares: f64,
    avg_price: f64,
    profit: f64,
}
