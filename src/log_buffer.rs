//! Shared in-memory log buffer with broadcast channel for SSE subscribers.

use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::{broadcast, RwLock};

const MAX_ENTRIES: usize = 500;

#[derive(Clone, serde::Serialize)]
pub struct LogEntry {
    pub timestamp: String,
    pub symbol: String,
    pub level: String,
    pub message: String,
}

/// Thread-safe log buffer that stores recent entries and broadcasts new ones to SSE subscribers.
#[derive(Clone)]
pub struct LogBuffer {
    entries: Arc<RwLock<VecDeque<LogEntry>>>,
    tx: broadcast::Sender<LogEntry>,
}

impl LogBuffer {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(256);
        Self {
            entries: Arc::new(RwLock::new(VecDeque::with_capacity(MAX_ENTRIES))),
            tx,
        }
    }

    /// Push a new log entry into the buffer and broadcast to SSE subscribers.
    pub async fn push(&self, symbol: &str, level: &str, message: String) {
        let entry = LogEntry {
            timestamp: chrono::Utc::now().format("%H:%M:%S").to_string(),
            symbol: symbol.to_string(),
            level: level.to_string(),
            message,
        };
        {
            let mut entries = self.entries.write().await;
            if entries.len() >= MAX_ENTRIES {
                entries.pop_front();
            }
            entries.push_back(entry.clone());
        }
        // Ignore send errors (no subscribers connected)
        let _ = self.tx.send(entry);
    }

    /// Get all current entries (for initial page load).
    pub async fn snapshot(&self) -> Vec<LogEntry> {
        self.entries.read().await.iter().cloned().collect()
    }

    /// Subscribe to new entries (for SSE stream).
    pub fn subscribe(&self) -> broadcast::Receiver<LogEntry> {
        self.tx.subscribe()
    }
}
