//! Paper trade logger: prediction accuracy tracker for 5m rounds.
//! Logs compact prediction records and resolution results.

use crate::discovery::{format_5m_period_et, parse_price_to_beat_from_question};
use crate::log_buffer::LogBuffer;
use crate::rtds::LatestPriceCache;
use chrono::Utc;
use log::{info, warn};
use std::fmt::Write as FmtWrite;
use tokio::fs::OpenOptions;
use tokio::io::AsyncWriteExt;

const PAPER_TRADE_FILE: &str = "paper_trade.md";
const PREDICTIONS_CSV: &str = "predictions.csv";

/// A single prediction for one symbol in one 5m period.
pub struct PredictionRecord {
    pub symbol: String,
    pub period_5: i64,
    pub period_str: String,
    pub price_to_beat: f64,
    pub close_price: f64,
    pub prediction: String,
    pub condition_id: String,
    pub close_rtds_ts_ms: i64,
    pub system_read_ts_ms: i64,
    pub age_s: i64,
    pub diff: f64,
    pub diff_pct: f64,
    pub raw_rtds_json: String,
}

/// Shared handle for paper trade logging across concurrent symbol loops.
#[derive(Clone)]
pub struct PaperTradeLogger {
    latest_prices: LatestPriceCache,
    log_buffer: LogBuffer,
}

impl PaperTradeLogger {
    pub fn new(latest_prices: LatestPriceCache, log_buffer: LogBuffer) -> Self {
        Self {
            latest_prices,
            log_buffer,
        }
    }

    /// Log a prediction after a 5m round closes.
    /// Returns the prediction record if a close price was available.
    pub async fn log(
        &self,
        symbol: &str,
        period_5: i64,
        price_to_beat: f64,
        condition_id: &str,
    ) -> Option<PredictionRecord> {
        info!("Paper trade: {} period={} ptb=${}", symbol, period_5, price_to_beat);
        let system_read_ts_ms = Utc::now().timestamp_millis();
        let period_str = format_5m_period_et(period_5);

        // Get close price from RTDS WS
        let rtds_result = {
            let cache = self.latest_prices.read().await;
            cache.get(symbol).cloned()
        };

        let (close_price, close_rtds_ts_ms, raw_json) = match rtds_result {
            Some((p, ts, raw)) => (p, ts, raw),
            None => {
                let md = format!("## {} | {}\n\n- PTB: ${} | Close: unavailable\n---\n\n", symbol.to_uppercase(), period_str, price_to_beat);
                self.append_file(PAPER_TRADE_FILE, &md).await;
                self.log_buffer.push(symbol, "warn", format!("{} | no close price", period_str)).await;
                return None;
            }
        };

        let age_s = (system_read_ts_ms - close_rtds_ts_ms) / 1000;
        let diff = close_price - price_to_beat;
        let diff_pct = if price_to_beat > 0.0 { (diff / price_to_beat).abs() * 100.0 } else { 0.0 };
        let prediction = if diff > 0.0 { "Up" } else { "Down" };

        let record = PredictionRecord {
            symbol: symbol.to_string(),
            period_5,
            period_str: period_str.clone(),
            price_to_beat,
            close_price,
            prediction: prediction.to_string(),
            condition_id: condition_id.to_string(),
            close_rtds_ts_ms,
            system_read_ts_ms,
            age_s,
            diff,
            diff_pct,
            raw_rtds_json: raw_json.clone(),
        };

        // Write compact markdown
        let mut md = String::new();
        let _ = writeln!(md, "## {} | {}\n", symbol.to_uppercase(), period_str);
        let _ = writeln!(md, "- PTB: ${}", price_to_beat);
        let _ = writeln!(md, "- Close: ${}", close_price);
        let _ = writeln!(md, "- Prediction: {}", prediction);
        let _ = writeln!(md, "- Diff: {}${} ({}%)", if diff >= 0.0 { "+" } else { "-" }, diff.abs(), format!("{:.3}", diff_pct));
        let _ = writeln!(md, "- Close RTDS ts: {}", close_rtds_ts_ms);
        let _ = writeln!(md, "- System read: {}", system_read_ts_ms);
        let _ = writeln!(md, "- Age: {}s", age_s);
        let _ = writeln!(md, "- Raw RTDS: {}", raw_json);

        self.append_file(PAPER_TRADE_FILE, &md).await;

        let summary = format!(
            "{} | {} ptb=${} close=${} diff={}${} ({}%)",
            period_str, prediction, price_to_beat, close_price,
            if diff >= 0.0 { "+" } else { "-" }, diff.abs(),
            format!("{:.3}", diff_pct),
        );
        self.log_buffer.push(symbol, "info", summary).await;

        Some(record)
    }

    /// Log resolution result after polling completes.
    pub async fn log_resolution(&self, record: &PredictionRecord, actual: Option<&str>, api_question: Option<&str>) {
        let api_ptb_str = api_question
            .and_then(parse_price_to_beat_from_question)
            .map(|p| format!(" | API PTB: ${}", p))
            .unwrap_or_default();

        let md = match actual {
            Some(winner) => {
                let correct = winner == record.prediction;
                let resolved_at = Utc::now().to_rfc3339();
                if correct {
                    format!(
                        "- **{}** Resolution: {} \u{2705} | PTB: ${} | Close: ${} |{} Resolved at: {}\n---\n\n",
                        record.symbol.to_uppercase(), winner,
                        record.price_to_beat, record.close_price,
                        api_ptb_str, resolved_at
                    )
                } else {
                    format!(
                        "- **{}** Resolution: {} \u{274C} (predicted {}) | PTB: ${} | Close: ${} |{} Resolved at: {}\n---\n\n",
                        record.symbol.to_uppercase(), winner, record.prediction,
                        record.price_to_beat, record.close_price,
                        api_ptb_str, resolved_at
                    )
                }
            }
            None => {
                format!(
                    "- **{}** Resolution: TIMEOUT (predicted {}) | PTB: ${} | Close: ${}\n---\n\n",
                    record.symbol.to_uppercase(), record.prediction,
                    record.price_to_beat, record.close_price
                )
            }
        };

        self.append_file(PAPER_TRADE_FILE, &md).await;

        // Write CSV row
        let correct = actual.map(|a| a == record.prediction).unwrap_or(false);
        let actual_str = actual.unwrap_or("TIMEOUT");
        self.write_csv_row(record, actual_str, correct).await;

        let log_msg = match actual {
            Some(w) if w == record.prediction => format!("{} | {} CORRECT", record.period_str, record.symbol),
            Some(w) => format!("{} | {} WRONG (predicted {} actual {})", record.period_str, record.symbol, record.prediction, w),
            None => format!("{} | {} TIMEOUT", record.period_str, record.symbol),
        };
        self.log_buffer.push(&record.symbol, "info", log_msg).await;
    }

    /// Append a row to predictions.csv (creating with header if needed).
    async fn write_csv_row(&self, record: &PredictionRecord, actual: &str, correct: bool) {
        let file_exists = tokio::fs::metadata(PREDICTIONS_CSV).await.is_ok();

        let mut content = String::new();
        if !file_exists {
            let _ = writeln!(content, "date,period,symbol,condition_id,ptb,close_price,prediction,actual,correct,close_rtds_ts,system_read_ts,age_s,diff,diff_pct");
        }
        let date = Utc::now().format("%Y-%m-%d");
        let _ = writeln!(
            content,
            "{},{},{},{},{},{},{},{},{},{},{},{},{},{}",
            date, record.period_5, record.symbol, record.condition_id,
            record.price_to_beat, record.close_price,
            record.prediction, actual, correct,
            record.close_rtds_ts_ms, record.system_read_ts_ms,
            record.age_s, record.diff, format!("{:.3}", record.diff_pct),
        );

        self.append_file(PREDICTIONS_CSV, &content).await;
    }

    /// Append content to a file.
    async fn append_file(&self, path: &str, content: &str) {
        match OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .await
        {
            Ok(mut file) => {
                if let Err(e) = file.write_all(content.as_bytes()).await {
                    warn!("Failed to write {}: {}", path, e);
                }
            }
            Err(e) => {
                warn!("Failed to open {}: {}", path, e);
            }
        }
    }
}
