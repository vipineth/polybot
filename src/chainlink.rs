//! Price-to-beat from Polymarket RTDS Chainlink (crypto_prices_chainlink) for multiple symbols.
//! Per docs: https://docs.polymarket.com/developers/RTDS/RTDS-crypto-prices
//! Single WS connection subscribes to all symbols with type: "*" and filters: "".
//! Price-to-beat is set when we receive a message whose feed_ts is in [period_start, period_start+2).

use crate::rtds::{run_rtds_chainlink_all, LatestPriceCache, PriceCacheMulti};
use anyhow::Result;
use log::warn;
use std::sync::Arc;
use tokio::time::Duration;

/// Spawn RTDS Chainlink stream for all symbols on a single connection.
pub async fn run_chainlink_multi_poller(
    rtds_ws_url: String,
    symbols: Vec<String>,
    price_cache_5: PriceCacheMulti,
    latest_prices: LatestPriceCache,
) -> Result<()> {
    let cache_5 = Arc::clone(&price_cache_5);
    let latest = Arc::clone(&latest_prices);

    tokio::spawn(async move {
        loop {
            if let Err(e) = run_rtds_chainlink_all(
                &rtds_ws_url,
                &symbols,
                cache_5.clone(),
                latest.clone(),
            )
            .await
            {
                warn!("RTDS WS stream exited: {} (reconnecting in 5s)", e);
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    });

    tokio::time::sleep(Duration::from_secs(2)).await;
    Ok(())
}
