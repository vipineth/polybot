use clap::Parser;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
pub struct Args {
    #[arg(short, long, default_value = "config.json")]
    pub config: PathBuf,

    #[arg(long)]
    pub redeem: bool,

    #[arg(long, requires = "redeem")]
    pub condition_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub polymarket: PolymarketConfig,
    pub strategy: StrategyConfig,
}

/// 5m post-close sweep: symbols to trade, sweep parameters.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StrategyConfig {
    /// 5m market symbols (e.g. btc, eth, sol, xrp). Slug format: {symbol}-updown-5m-{period}.
    #[serde(default = "default_symbols")]
    pub symbols: Vec<String>,
    /// Enable post-close sweep: buy winning tokens from stale limit orders after market closes.
    #[serde(default)]
    pub sweep_enabled: bool,
    /// Max ask price to buy winning tokens (e.g. 0.999 = pay at most 99.9c for a $1 token).
    #[serde(default = "default_sweep_max_price")]
    pub sweep_max_price: f64,
    /// Seconds to sweep before giving up.
    #[serde(default = "default_sweep_timeout_secs")]
    pub sweep_timeout_secs: u64,
    /// Milliseconds between FOK orders.
    #[serde(default = "default_sweep_inter_order_delay_ms")]
    pub sweep_inter_order_delay_ms: u64,
    /// Minimum price margin as a percentage of price_to_beat.
    /// E.g., 0.0001 = 0.01% → BTC@$68k requires ~$6.80 move to sweep.
    #[serde(default = "default_sweep_min_margin_pct")]
    pub sweep_min_margin_pct: f64,
    /// Maximum total cost (USD) per sweep. Safety cap to limit exposure on wrong-winner.
    #[serde(default = "default_max_sweep_cost")]
    pub max_sweep_cost: f64,
}

fn default_symbols() -> Vec<String> {
    vec!["btc".into(), "eth".into(), "sol".into(), "xrp".into()]
}
fn default_sweep_max_price() -> f64 {
    0.999
}
fn default_sweep_timeout_secs() -> u64 {
    30
}
fn default_sweep_inter_order_delay_ms() -> u64 {
    50
}
fn default_sweep_min_margin_pct() -> f64 {
    0.0001
}
fn default_max_sweep_cost() -> f64 {
    500.0
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolymarketConfig {
    pub gamma_api_url: String,
    pub clob_api_url: String,
    pub private_key: Option<String>,
    pub proxy_wallet_address: Option<String>,
    pub signature_type: Option<u8>,
    /// Polygon RPC URLs (tried in order as fallbacks for redemption).
    #[serde(default = "default_rpc_urls")]
    pub rpc_urls: Vec<String>,
    /// WebSocket base URL for market channel (e.g. wss://ws-subscriptions-clob.polymarket.com).
    #[serde(default = "default_ws_url")]
    pub ws_url: String,
    /// RTDS WebSocket URL for Chainlink BTC price (price-to-beat). Topic: crypto_prices_chainlink, symbol: btc/usd.
    #[serde(default = "default_rtds_ws_url")]
    pub rtds_ws_url: String,
}

fn default_rpc_urls() -> Vec<String> {
    vec![
        "https://1rpc.io/matic".to_string(),
        "https://poly.api.pocket.network".to_string(),
    ]
}

fn default_ws_url() -> String {
    "wss://ws-subscriptions-clob.polymarket.com".to_string()
}

fn default_rtds_ws_url() -> String {
    "wss://ws-live-data.polymarket.com".to_string()
}

impl Default for Config {
    fn default() -> Self {
        Self {
            polymarket: PolymarketConfig {
                gamma_api_url: "https://gamma-api.polymarket.com".to_string(),
                clob_api_url: "https://clob.polymarket.com".to_string(),
                private_key: None,
                proxy_wallet_address: None,
                signature_type: None,
                rpc_urls: default_rpc_urls(),
                ws_url: default_ws_url(),
                rtds_ws_url: default_rtds_ws_url(),
            },
            strategy: StrategyConfig {
                symbols: default_symbols(),
                sweep_enabled: false,
                sweep_max_price: default_sweep_max_price(),
                sweep_timeout_secs: default_sweep_timeout_secs(),
                sweep_inter_order_delay_ms: default_sweep_inter_order_delay_ms(),
                sweep_min_margin_pct: default_sweep_min_margin_pct(),
                max_sweep_cost: default_max_sweep_cost(),
            },
        }
    }
}

impl Config {
    pub fn load(path: &PathBuf) -> anyhow::Result<Self> {
        let mut config = if path.exists() {
            let content = std::fs::read_to_string(path)?;
            serde_json::from_str(&content)?
        } else {
            Config::default()
        };

        // Override from environment variables (for Railway / VPS deployments)
        if let Ok(v) = std::env::var("PRIVATE_KEY") {
            config.polymarket.private_key = Some(v);
        }
        if let Ok(v) = std::env::var("PROXY_WALLET_ADDRESS") {
            config.polymarket.proxy_wallet_address = Some(v);
        }
        if let Ok(v) = std::env::var("SIGNATURE_TYPE") {
            config.polymarket.signature_type = v.parse().ok();
        }
        if let Ok(v) = std::env::var("SWEEP_ENABLED") {
            config.strategy.sweep_enabled = v == "true" || v == "1";
        }

        Ok(config)
    }
}
