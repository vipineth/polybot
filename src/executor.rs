//! Order Executor — standalone safety gate between strategies and Polymarket.
//!
//! Accepts `OrderIntent` from any strategy, validates against safety rules,
//! executes via CLOB API, and returns `ExecutionResult`.
//!
//! The executor owns all order-related safety:
//! - Budget limits (per-round, per-strategy)
//! - Order size / price sanity checks
//! - Rate limiting between orders
//! - Network error handling (halt on ambiguous failures)
//! - Execution logging

use crate::api::PolymarketApi;
use anyhow::Result;
use log::{error, info, warn};
use std::sync::Arc;
use tokio::time::{sleep, Duration};

// ── Types ──────────────────────────────────────────────────────────────

/// What a strategy wants to trade.
#[derive(Debug, Clone)]
pub struct OrderIntent {
    pub token_id: String,
    pub side: Side,
    pub price: f64,
    pub size: f64,
    pub order_type: IntentOrderType,
    /// Which strategy created this intent.
    pub strategy: String,
    /// Human-readable reason (e.g. "UP won, diff=+$42.50").
    pub reason: String,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Side {
    Buy,
    Sell,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum IntentOrderType {
    /// Fill-or-Kill: fill entire size immediately or cancel.
    FOK,
    /// Good-til-Cancelled: leave on book until filled or cancelled.
    GTC,
}

/// Result of attempting to execute an OrderIntent.
#[derive(Debug, Clone)]
pub struct ExecutionResult {
    pub intent: OrderIntent,
    pub status: FillStatus,
    pub filled_size: f64,
    pub filled_price: f64,
    pub order_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FillStatus {
    Filled,
    /// Order was valid but not fillable at this price/size.
    NotFillable,
    /// API or validation rejected the order.
    Rejected,
    /// Network error — order may or may not have been placed.
    /// Executor halts further orders when this happens.
    NetworkError,
}

// ── Safety Config ──────────────────────────────────────────────────────

/// Safety limits the executor enforces on every round.
#[derive(Debug, Clone)]
pub struct ExecutorConfig {
    /// Max total USD cost per execute_batch call.
    pub max_batch_cost: f64,
    /// Max price allowed for any single order (e.g. 0.999).
    pub max_price: f64,
    /// Min size for an order to be worth sending.
    pub min_size: f64,
    /// Delay between consecutive orders (rate limit).
    pub inter_order_delay: Duration,
    /// Max consecutive not-fillable results before stopping the batch.
    pub max_consecutive_misses: u32,
    /// Whether to actually send orders (false = paper/dry-run mode).
    pub live: bool,
}

impl Default for ExecutorConfig {
    fn default() -> Self {
        Self {
            max_batch_cost: 500.0,
            max_price: 0.999,
            min_size: 0.01,
            inter_order_delay: Duration::from_millis(50),
            max_consecutive_misses: 3,
            live: false,
        }
    }
}

// ── Executor ───────────────────────────────────────────────────────────

pub struct OrderExecutor {
    api: Arc<PolymarketApi>,
    config: ExecutorConfig,
}

impl OrderExecutor {
    pub fn new(api: Arc<PolymarketApi>, config: ExecutorConfig) -> Self {
        Self { api, config }
    }

    /// Execute a batch of intents with safety limits.
    ///
    /// Processes intents in order, tracking cumulative cost. Stops early if:
    /// - Budget exhausted
    /// - Network error (ambiguous — can't safely continue)
    /// - Too many consecutive misses (market dried up)
    pub async fn execute_batch(&self, intents: Vec<OrderIntent>) -> Vec<ExecutionResult> {
        if intents.is_empty() {
            return vec![];
        }

        let strategy_name = intents.first().map(|i| i.strategy.as_str()).unwrap_or("?");
        info!(
            "Executor: {} intents from [{}], budget=${}, live={}",
            intents.len(),
            strategy_name,
            self.config.max_batch_cost,
            self.config.live,
        );

        let mut results = Vec::with_capacity(intents.len());
        let mut total_cost: f64 = 0.0;
        let mut consecutive_misses: u32 = 0;

        for intent in intents {
            // ── Pre-flight checks ──
            if let Some(rejection) = self.validate(&intent, total_cost) {
                info!("Executor: REJECTED {} — {}", self.intent_summary(&intent), rejection);
                results.push(ExecutionResult {
                    intent,
                    status: FillStatus::Rejected,
                    filled_size: 0.0,
                    filled_price: 0.0,
                    order_id: None,
                });
                continue;
            }

            // Cap size to remaining budget
            let remaining_budget = self.config.max_batch_cost - total_cost;
            let affordable_size = if intent.price > 0.0 {
                remaining_budget / intent.price
            } else {
                0.0
            };
            let actual_size = intent.size.min(affordable_size);
            let actual_size = (actual_size * 100.0).floor() / 100.0; // round down to 2dp

            if actual_size < self.config.min_size {
                info!("Executor: SKIP {} — capped size {:.2} below min", self.intent_summary(&intent), actual_size);
                results.push(ExecutionResult {
                    intent,
                    status: FillStatus::Rejected,
                    filled_size: 0.0,
                    filled_price: 0.0,
                    order_id: None,
                });
                continue;
            }

            // ── Execute ──
            let result = if self.config.live {
                self.execute_live(&intent, actual_size).await
            } else {
                self.execute_paper(&intent, actual_size)
            };

            match result.status {
                FillStatus::Filled => {
                    total_cost += result.filled_size * result.filled_price;
                    consecutive_misses = 0;
                    info!(
                        "Executor: FILLED {} — {:.2} @ {:.4} (total_cost=${:.2}, id={})",
                        self.intent_summary(&intent),
                        result.filled_size,
                        result.filled_price,
                        total_cost,
                        result.order_id.as_deref().unwrap_or("paper"),
                    );
                }
                FillStatus::NotFillable => {
                    consecutive_misses += 1;
                    info!(
                        "Executor: NOT FILLABLE {} (miss {}/{})",
                        self.intent_summary(&intent),
                        consecutive_misses,
                        self.config.max_consecutive_misses,
                    );
                }
                FillStatus::NetworkError => {
                    error!(
                        "Executor: NETWORK ERROR on {} — halting batch (order may be placed)",
                        self.intent_summary(&intent),
                    );
                    results.push(result);
                    break;
                }
                FillStatus::Rejected => {
                    warn!("Executor: REJECTED by API — {}", self.intent_summary(&intent));
                }
            }

            results.push(result);

            // ── Post-order checks ──
            if total_cost >= self.config.max_batch_cost {
                info!("Executor: budget exhausted (${:.2} >= ${})", total_cost, self.config.max_batch_cost);
                break;
            }
            if consecutive_misses >= self.config.max_consecutive_misses {
                info!("Executor: {} consecutive misses, stopping batch", consecutive_misses);
                break;
            }

            // Rate limit
            sleep(self.config.inter_order_delay).await;
        }

        let filled_count = results.iter().filter(|r| r.status == FillStatus::Filled).count();
        info!(
            "Executor: batch done — {}/{} filled, ${:.2} total cost",
            filled_count,
            results.len(),
            total_cost,
        );

        results
    }

    /// Validate an intent against safety rules. Returns Some(reason) if rejected.
    fn validate(&self, intent: &OrderIntent, current_cost: f64) -> Option<String> {
        if intent.price <= 0.0 || intent.price.is_nan() || intent.price.is_infinite() {
            return Some(format!("invalid price: {}", intent.price));
        }
        if intent.size <= 0.0 || intent.size.is_nan() || intent.size.is_infinite() {
            return Some(format!("invalid size: {}", intent.size));
        }
        if intent.price > self.config.max_price {
            return Some(format!("price {} > max {}", intent.price, self.config.max_price));
        }
        if intent.size < self.config.min_size {
            return Some(format!("size {} < min {}", intent.size, self.config.min_size));
        }
        if current_cost >= self.config.max_batch_cost {
            return Some(format!("budget exhausted: ${:.2} >= ${}", current_cost, self.config.max_batch_cost));
        }
        if intent.token_id.is_empty() {
            return Some("empty token_id".to_string());
        }
        // Only FOK buy supported for now
        if intent.side != Side::Buy {
            return Some("only Buy side supported currently".to_string());
        }
        if intent.order_type != IntentOrderType::FOK {
            return Some("only FOK order type supported currently".to_string());
        }
        None
    }

    /// Execute a single order against the live CLOB API.
    async fn execute_live(&self, intent: &OrderIntent, actual_size: f64) -> ExecutionResult {
        let size_str = format!("{:.2}", actual_size);
        let price_str = format!("{}", intent.price);

        match self.api.place_fok_buy(&intent.token_id, &size_str, &price_str).await {
            Ok(Some(resp)) => ExecutionResult {
                intent: intent.clone(),
                status: FillStatus::Filled,
                filled_size: actual_size,
                filled_price: intent.price,
                order_id: resp.order_id,
            },
            Ok(None) => ExecutionResult {
                intent: intent.clone(),
                status: FillStatus::NotFillable,
                filled_size: 0.0,
                filled_price: 0.0,
                order_id: None,
            },
            Err(e) => {
                let err_str = e.to_string().to_lowercase();
                let is_network = err_str.contains("network")
                    || err_str.contains("timeout")
                    || err_str.contains("connection");
                ExecutionResult {
                    intent: intent.clone(),
                    status: if is_network { FillStatus::NetworkError } else { FillStatus::Rejected },
                    filled_size: 0.0,
                    filled_price: 0.0,
                    order_id: None,
                }
            }
        }
    }

    /// Paper execution — always "fills" at the requested price.
    fn execute_paper(&self, intent: &OrderIntent, actual_size: f64) -> ExecutionResult {
        info!(
            "Executor [PAPER]: {} {:.2} @ {:.4} ({}) — {}",
            if intent.side == Side::Buy { "BUY" } else { "SELL" },
            actual_size,
            intent.price,
            intent.token_id[..intent.token_id.len().min(16)].to_string(),
            intent.reason,
        );
        ExecutionResult {
            intent: intent.clone(),
            status: FillStatus::Filled,
            filled_size: actual_size,
            filled_price: intent.price,
            order_id: None,
        }
    }

    fn intent_summary(&self, intent: &OrderIntent) -> String {
        format!(
            "{} {:.2}@{:.4} {}..  ({})",
            if intent.side == Side::Buy { "BUY" } else { "SELL" },
            intent.size,
            intent.price,
            &intent.token_id[..intent.token_id.len().min(12)],
            intent.reason,
        )
    }
}
