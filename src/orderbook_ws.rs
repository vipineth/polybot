use crate::models::{OrderBook, OrderBookEntry};
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::{Notify, RwLock};
use tokio::task::JoinHandle;
use tokio::time::Duration;
use futures_util::StreamExt;
use log::{debug, warn};
use alloy::primitives::U256;
use polymarket_client_sdk::clob::ws::Client as WsClient;

pub struct OrderbookMirror {
    books: Arc<RwLock<HashMap<String, OrderBook>>>,
    notify: Arc<Notify>,
    active_tasks: std::sync::Mutex<Vec<JoinHandle<()>>>,
    update_count: Arc<AtomicU64>,
    subscribe_time: Arc<RwLock<Option<std::time::Instant>>>,
}

impl OrderbookMirror {
    pub fn new() -> Self {
        Self {
            books: Arc::new(RwLock::new(HashMap::new())),
            notify: Arc::new(Notify::new()),
            active_tasks: std::sync::Mutex::new(Vec::new()),
            update_count: Arc::new(AtomicU64::new(0)),
            subscribe_time: Arc::new(RwLock::new(None)),
        }
    }

    fn parse_token_id(token_id: &str) -> Result<U256> {
        if token_id.starts_with("0x") {
            U256::from_str_radix(token_id.trim_start_matches("0x"), 16)
        } else {
            U256::from_str_radix(token_id, 10)
        }
        .context(format!("Failed to parse token_id as U256: {}", token_id))
    }

    /// Subscribe to orderbook updates for the given token IDs via WebSocket.
    /// Spawns a background task that continuously updates the local mirror.
    pub async fn subscribe(&self, token_ids: &[&str]) -> Result<()> {
        let asset_ids: Vec<U256> = token_ids
            .iter()
            .map(|id| Self::parse_token_id(id))
            .collect::<Result<Vec<_>>>()?;

        // Build mapping from U256 string repr back to original token_id strings
        let token_id_map: HashMap<String, String> = token_ids
            .iter()
            .zip(asset_ids.iter())
            .map(|(orig, u256)| (u256.to_string(), orig.to_string()))
            .collect();

        // The SDK's subscribe_orderbook() captures &self lifetime in the returned Stream
        // due to Rust 2024 impl Trait capture rules, even though the stream is internally
        // self-contained. Leak the WsClient (small config struct) to get a 'static ref.
        let ws_client: &'static _ = Box::leak(Box::new(WsClient::default()));
        let stream = ws_client
            .subscribe_orderbook(asset_ids)
            .context("Failed to subscribe to orderbook WS")?;

        // Reset tracking for this subscription cycle
        self.update_count.store(0, Ordering::Relaxed);
        *self.subscribe_time.write().await = Some(std::time::Instant::now());

        let books = Arc::clone(&self.books);
        let notify = Arc::clone(&self.notify);
        let update_count = Arc::clone(&self.update_count);

        debug!("Orderbook WS subscribed to {} tokens", token_ids.len());

        let handle = tokio::spawn(async move {
            let mut stream = Box::pin(stream);
            while let Some(result) = stream.next().await {
                match result {
                    Ok(book_update) => {
                        update_count.fetch_add(1, Ordering::Relaxed);
                        let asset_id_str = book_update.asset_id.to_string();
                        let token_id = token_id_map
                            .get(&asset_id_str)
                            .cloned()
                            .unwrap_or(asset_id_str);

                        let orderbook = OrderBook {
                            bids: book_update
                                .bids
                                .iter()
                                .map(|l| OrderBookEntry {
                                    price: l.price,
                                    size: l.size,
                                })
                                .collect(),
                            asks: book_update
                                .asks
                                .iter()
                                .map(|l| OrderBookEntry {
                                    price: l.price,
                                    size: l.size,
                                })
                                .collect(),
                        };

                        let bid_count = orderbook.bids.len();
                        let ask_count = orderbook.asks.len();

                        {
                            let mut books = books.write().await;
                            books.insert(token_id.clone(), orderbook);
                        }

                        debug!(
                            "WS orderbook update: {} ({} bids, {} asks)",
                            &token_id[..token_id.len().min(20)],
                            bid_count,
                            ask_count
                        );
                        notify.notify_waiters();
                    }
                    Err(e) => {
                        warn!("WS orderbook stream error: {}", e);
                    }
                }
            }
            warn!("WS orderbook stream ended");
        });

        let mut tasks = self.active_tasks.lock().unwrap();
        tasks.push(handle);
        Ok(())
    }

    /// Read the latest orderbook snapshot from the local mirror (instant, no network).
    pub async fn get_orderbook(&self, token_id: &str) -> Option<OrderBook> {
        let books = self.books.read().await;
        books.get(token_id).cloned()
    }

    /// Block until the next WS book update arrives or timeout expires.
    /// Returns true if an update was received, false on timeout.
    pub async fn wait_for_update(&self, timeout: Duration) -> bool {
        tokio::select! {
            _ = self.notify.notified() => true,
            _ = tokio::time::sleep(timeout) => false,
        }
    }

    /// Abort all background subscription tasks and clear the book mirror.
    pub async fn unsubscribe_all(&self) {
        {
            let mut tasks = self.active_tasks.lock().unwrap();
            for handle in tasks.drain(..) {
                handle.abort();
            }
        }
        let mut books = self.books.write().await;
        books.clear();
    }
}
