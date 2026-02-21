//! Paper trade logger: after each 5m round, log what we *would* have traded
//! to paper_trade.md. Pure observation — no orders placed.

use crate::api::PolymarketApi;
use crate::config::StrategyConfig;
use crate::rtds::LatestPriceCache;
use chrono::Utc;
use chrono_tz::America::New_York;
use log::error;
use std::fmt::Write as FmtWrite;
use std::io::Write as IoWrite;
use std::sync::Arc;
use tokio::sync::Mutex;

/// Shared handle for paper trade logging across concurrent symbol loops.
#[derive(Clone)]
pub struct PaperTradeLogger {
    api: Arc<PolymarketApi>,
    latest_prices: LatestPriceCache,
    file_mutex: Arc<Mutex<()>>,
}

impl PaperTradeLogger {
    pub fn new(api: Arc<PolymarketApi>, latest_prices: LatestPriceCache) -> Self {
        Self {
            api,
            latest_prices,
            file_mutex: Arc::new(Mutex::new(())),
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
        let now_ms = Utc::now().timestamp_millis();
        let now_et = Utc::now().with_timezone(&New_York);
        let time_str = now_et.format("%Y-%m-%d %H:%M ET").to_string();

        // --- Source 1: RTDS WS (cached from Polymarket WebSocket) ---
        let rtds_start = std::time::Instant::now();
        let rtds_result = {
            let cache = self.latest_prices.read().await;
            cache.get(symbol).copied()
        };
        let rtds_elapsed = rtds_start.elapsed();

        let (rtds_price, rtds_age_s, rtds_line) = match rtds_result {
            Some((p, ts)) => {
                let age = (now_ms - ts) / 1000;
                let line = format!(
                    "- **RTDS WS**: ${:.2} (age={}s, {:.1}ms)",
                    p,
                    age,
                    rtds_elapsed.as_secs_f64() * 1000.0
                );
                (Some(p), age, line)
            }
            None => (None, i64::MAX, "- **RTDS WS**: unavailable".to_string()),
        };

        // --- Source 2: Chainlink RPC (on-chain eth_call) ---
        let rpc_start = std::time::Instant::now();
        let rpc_result = self.api.get_chainlink_price_rpc(symbol).await;
        let rpc_elapsed = rpc_start.elapsed();

        let (rpc_price, rpc_age_s, rpc_line) = match &rpc_result {
            Ok((p, updated_at)) => {
                let age = (now_ms / 1000) as i64 - (*updated_at as i64);
                let line = format!(
                    "- **Chainlink RPC**: ${:.2} (age={}s, {:.0}ms)",
                    p,
                    age,
                    rpc_elapsed.as_secs_f64() * 1000.0
                );
                (Some(*p), age, line)
            }
            Err(_) => (None, i64::MAX, "- **Chainlink RPC**: unavailable".to_string()),
        };

        // Build the markdown entry
        let mut md = String::new();
        let _ = writeln!(
            md,
            "## {} | {} | period={}\n",
            time_str,
            symbol.to_uppercase(),
            period_5
        );
        let _ = writeln!(md, "- **Price-to-beat**: ${:.2}", price_to_beat);
        let _ = writeln!(md, "{}", rtds_line);
        let _ = writeln!(md, "{}", rpc_line);

        // Speed comparison when both sources available
        let rtds_ms = rtds_elapsed.as_secs_f64() * 1000.0;
        let rpc_ms = rpc_elapsed.as_secs_f64() * 1000.0;
        if let (Some(rp), Some(cp)) = (rtds_price, rpc_price) {
            let price_diff = (rp - cp).abs();
            let faster = if rtds_ms < rpc_ms { "RTDS WS" } else { "Chainlink RPC" };
            let speed_diff = (rtds_ms - rpc_ms).abs();
            let fresher = if rtds_age_s < rpc_age_s {
                "RTDS WS"
            } else if rpc_age_s < rtds_age_s {
                "Chainlink RPC"
            } else {
                "tied"
            };
            let _ = writeln!(
                md,
                "- **Speed**: {} faster by {:.0}ms (RTDS WS {:.1}ms vs RPC {:.0}ms) | fresher: {} | price diff: ${:.2}",
                faster, speed_diff, rtds_ms, rpc_ms, fresher, price_diff
            );
        }

        // Pick best available price (freshest)
        let best = match (rtds_price, rpc_price) {
            (Some(rp), Some(cp)) => {
                if rtds_age_s <= rpc_age_s {
                    let _ = writeln!(md, "- **Best source**: RTDS WS");
                    Some(rp)
                } else {
                    let _ = writeln!(md, "- **Best source**: Chainlink RPC");
                    Some(cp)
                }
            }
            (Some(rp), None) => {
                let _ = writeln!(md, "- **Best source**: RTDS");
                Some(rp)
            }
            (None, Some(cp)) => {
                let _ = writeln!(md, "- **Best source**: Chainlink RPC");
                Some(cp)
            }
            (None, None) => None,
        };

        let latest_price = match best {
            Some(p) => p,
            None => {
                let _ = writeln!(md, "- **NO CLOSE PRICE** - cannot determine winner\n");
                let _ = writeln!(md, "---\n");
                self.append(&md).await;
                return;
            }
        };

        // Determine winner
        let diff = latest_price - price_to_beat;
        let (winner, winning_token) = if diff > 0.0 {
            ("Up", m5_up)
        } else {
            ("Down", m5_down)
        };

        let margin_ok = diff.abs() >= cfg.sweep_min_margin;
        let _ = writeln!(
            md,
            "- **Winner**: {} (diff={}{:.2}, margin {})\n",
            winner,
            if diff >= 0.0 { "+$" } else { "-$" },
            diff.abs(),
            if margin_ok { "OK" } else { "BELOW MIN" }
        );

        // Fetch orderbook for winning token
        match self.api.get_orderbook(winning_token).await {
            Ok(orderbook) => {
                let _ = writeln!(md, "### Winning token orderbook ({})", winner);
                let _ = writeln!(md, "| Price | Size | USD Value |");
                let _ = writeln!(md, "|-------|------|-----------|");

                let mut sweepable_levels = 0u32;
                let mut sweepable_shares = 0.0f64;
                let mut sweepable_cost = 0.0f64;

                for ask in &orderbook.asks {
                    let p: f64 = ask.price.to_string().parse().unwrap_or(1.0);
                    let s: f64 = ask.size.to_string().parse().unwrap_or(0.0);
                    let usd = p * s;
                    let _ = writeln!(md, "| {:.3}  | {:.0}  | ${:.2}   |", p, s, usd);

                    if p <= cfg.sweep_max_price {
                        sweepable_levels += 1;
                        sweepable_shares += s;
                        sweepable_cost += usd;
                    }
                }

                let _ = writeln!(md);
                let _ = writeln!(
                    md,
                    "- **Sweepable asks** (<= {:.3}): {} levels, {:.0} shares, ${:.2} cost",
                    cfg.sweep_max_price, sweepable_levels, sweepable_shares, sweepable_cost
                );

                if sweepable_shares > 0.0 {
                    let avg_price = sweepable_cost / sweepable_shares;
                    let profit = sweepable_shares * (1.0 - avg_price);
                    let _ = writeln!(
                        md,
                        "- **Hypothetical P&L**: buy {:.0} @ avg {:.3} -> profit ${:.2} ({:.0} * (1.0 - {:.3}))\n",
                        sweepable_shares, avg_price, profit, sweepable_shares, avg_price
                    );
                } else {
                    let _ = writeln!(md, "- **Hypothetical P&L**: no sweepable asks\n");
                }
            }
            Err(e) => {
                let _ = writeln!(md, "### Winning token orderbook ({})\n", winner);
                let _ = writeln!(md, "- **Orderbook fetch failed**: {}\n", e);
            }
        }

        let _ = writeln!(md, "---\n");
        self.append(&md).await;
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
                }
            }
            Err(e) => {
                error!("Failed to open paper_trade.md: {}", e);
            }
        }
    }
}
