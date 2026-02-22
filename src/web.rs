//! Built-in web dashboard: serves a single HTML page with live-updating logs via SSE.

use crate::log_buffer::LogBuffer;
use axum::extract::State;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::Html;
use axum::routing::get;
use axum::Router;
use futures_util::stream::Stream;
use log::info;
use std::convert::Infallible;

/// Spawn the web dashboard server as a background task.
pub async fn spawn_dashboard(log_buffer: LogBuffer) {
    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(3000);

    let app = Router::new()
        .route("/", get(index_handler))
        .route("/events", get(sse_handler))
        .route("/snapshot", get(snapshot_handler))
        .with_state(log_buffer);

    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{}", port))
        .await
        .expect("failed to bind dashboard port");
    info!("Dashboard running on http://0.0.0.0:{}", port);
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
}

async fn index_handler() -> Html<&'static str> {
    Html(DASHBOARD_HTML)
}

async fn snapshot_handler(State(buf): State<LogBuffer>) -> axum::Json<Vec<crate::log_buffer::LogEntry>> {
    axum::Json(buf.snapshot().await)
}

async fn sse_handler(State(buf): State<LogBuffer>) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let rx = buf.subscribe();
    let stream = async_stream::stream! {
        let mut rx = rx;
        loop {
            match rx.recv().await {
                Ok(entry) => {
                    let data = serde_json::to_string(&entry).unwrap_or_default();
                    yield Ok(Event::default().data(data));
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    let msg = format!("{{\"timestamp\":\"\",\"symbol\":\"SYS\",\"level\":\"warn\",\"message\":\"skipped {} events\"}}",n);
                    yield Ok(Event::default().data(msg));
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    };
    Sse::new(stream).keep_alive(KeepAlive::default())
}

use tokio::sync::broadcast;

const DASHBOARD_HTML: &str = r##"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width, initial-scale=1.0">
<title>Polymarket 5m Bot Dashboard</title>
<style>
  :root {
    --bg: #0d1117; --surface: #161b22; --border: #30363d;
    --text: #e6edf3; --text-dim: #8b949e;
    --btc: #f7931a; --eth: #627eea; --sol: #9945ff; --xrp: #8a8a8a;
    --info: #e6edf3; --warn: #d29922; --error: #f85149; --green: #3fb950;
  }
  * { margin: 0; padding: 0; box-sizing: border-box; }
  body {
    background: var(--bg); color: var(--text);
    font-family: 'SF Mono', 'Cascadia Code', 'Fira Code', monospace;
    font-size: 13px; height: 100vh; display: flex; flex-direction: column;
  }
  header {
    background: var(--surface); border-bottom: 1px solid var(--border);
    padding: 12px 20px; display: flex; align-items: center; gap: 16px;
    flex-shrink: 0;
  }
  header h1 { font-size: 15px; font-weight: 600; }
  .status { display: flex; align-items: center; gap: 6px; font-size: 12px; color: var(--text-dim); }
  .status-dot { width: 8px; height: 8px; border-radius: 50%; background: var(--green); }
  .status-dot.disconnected { background: var(--error); }
  .filters {
    display: flex; gap: 6px; margin-left: auto;
  }
  .filter-btn {
    background: var(--surface); color: var(--text-dim); border: 1px solid var(--border);
    border-radius: 6px; padding: 4px 10px; cursor: pointer; font-size: 12px;
    font-family: inherit; transition: all 0.15s;
  }
  .filter-btn:hover { border-color: var(--text-dim); }
  .filter-btn.active { color: var(--text); border-color: var(--text-dim); background: var(--border); }
  .filter-btn[data-symbol="BTC"].active { color: var(--btc); border-color: var(--btc); }
  .filter-btn[data-symbol="ETH"].active { color: var(--eth); border-color: var(--eth); }
  .filter-btn[data-symbol="SOL"].active { color: var(--sol); border-color: var(--sol); }
  .filter-btn[data-symbol="XRP"].active { color: var(--xrp); border-color: var(--xrp); }
  #log-container {
    flex: 1; overflow-y: auto; padding: 8px 0;
    scrollbar-width: thin; scrollbar-color: var(--border) transparent;
  }
  .log-entry {
    padding: 3px 20px; display: flex; gap: 10px; line-height: 1.5;
    transition: background 0.1s;
  }
  .log-entry:hover { background: var(--surface); }
  .log-entry.hidden { display: none; }
  .log-time { color: var(--text-dim); flex-shrink: 0; min-width: 62px; }
  .log-symbol {
    flex-shrink: 0; min-width: 36px; font-weight: 600; text-align: center;
  }
  .log-symbol.BTC { color: var(--btc); }
  .log-symbol.ETH { color: var(--eth); }
  .log-symbol.SOL { color: var(--sol); }
  .log-symbol.XRP { color: var(--xrp); }
  .log-symbol.SYS { color: var(--text-dim); }
  .log-msg { white-space: pre-wrap; word-break: break-word; }
  .log-entry.warn .log-msg { color: var(--warn); }
  .log-entry.error .log-msg { color: var(--error); }
  footer {
    background: var(--surface); border-top: 1px solid var(--border);
    padding: 6px 20px; font-size: 11px; color: var(--text-dim);
    display: flex; justify-content: space-between; flex-shrink: 0;
  }
</style>
</head>
<body>
<header>
  <h1>5m Bot</h1>
  <div class="status">
    <div class="status-dot" id="status-dot"></div>
    <span id="status-text">connecting...</span>
  </div>
  <div class="filters">
    <button class="filter-btn active" data-symbol="ALL" onclick="toggleFilter(this)">All</button>
    <button class="filter-btn active" data-symbol="BTC" onclick="toggleFilter(this)">BTC</button>
    <button class="filter-btn active" data-symbol="ETH" onclick="toggleFilter(this)">ETH</button>
    <button class="filter-btn active" data-symbol="SOL" onclick="toggleFilter(this)">SOL</button>
    <button class="filter-btn active" data-symbol="XRP" onclick="toggleFilter(this)">XRP</button>
  </div>
</header>
<div id="log-container"></div>
<footer>
  <span id="entry-count">0 entries</span>
  <span>Polymarket 5m Arbitrage Bot</span>
</footer>
<script>
const container = document.getElementById('log-container');
const statusDot = document.getElementById('status-dot');
const statusText = document.getElementById('status-text');
const entryCount = document.getElementById('entry-count');
let activeFilters = new Set(['BTC','ETH','SOL','XRP','SYS','']);
let totalEntries = 0;
let autoScroll = true;

container.addEventListener('scroll', function() {
  var sh = container.scrollHeight, st = container.scrollTop, ch = container.clientHeight;
  autoScroll = sh - st - ch < 60;
});

function toggleFilter(btn) {
  var sym = btn.dataset.symbol;
  if (sym === 'ALL') {
    var allActive = btn.classList.contains('active');
    document.querySelectorAll('.filter-btn').forEach(function(b) {
      if (allActive) b.classList.remove('active'); else b.classList.add('active');
    });
    if (allActive) activeFilters.clear();
    else activeFilters = new Set(['BTC','ETH','SOL','XRP','SYS','']);
  } else {
    btn.classList.toggle('active');
    if (btn.classList.contains('active')) activeFilters.add(sym);
    else activeFilters.delete(sym);
    var allBtn = document.querySelector('[data-symbol="ALL"]');
    var symbolBtns = document.querySelectorAll('.filter-btn:not([data-symbol="ALL"])');
    var allOn = true;
    symbolBtns.forEach(function(b) { if (!b.classList.contains('active')) allOn = false; });
    if (allOn) allBtn.classList.add('active'); else allBtn.classList.remove('active');
  }
  applyFilters();
}

function applyFilters() {
  document.querySelectorAll('.log-entry').forEach(function(el) {
    var sym = el.dataset.symbol || '';
    el.classList.toggle('hidden', !activeFilters.has(sym) && !activeFilters.has(''));
  });
}

function addEntry(entry) {
  var div = document.createElement('div');
  div.className = 'log-entry ' + (entry.level || 'info');
  div.dataset.symbol = entry.symbol || '';
  var sym = entry.symbol || '';
  var visible = activeFilters.has(sym) || activeFilters.has('');
  if (!visible) div.classList.add('hidden');

  var timeSpan = document.createElement('span');
  timeSpan.className = 'log-time';
  timeSpan.textContent = entry.timestamp;

  var symSpan = document.createElement('span');
  symSpan.className = 'log-symbol ' + sym;
  symSpan.textContent = sym || '-';

  var msgSpan = document.createElement('span');
  msgSpan.className = 'log-msg';
  msgSpan.textContent = entry.message;

  div.appendChild(timeSpan);
  div.appendChild(symSpan);
  div.appendChild(msgSpan);

  container.appendChild(div);
  totalEntries++;
  entryCount.textContent = totalEntries + ' entries';
  if (autoScroll) div.scrollIntoView({ block: 'end' });
}

// Load existing entries
fetch('/snapshot')
  .then(function(r) { return r.json(); })
  .then(function(entries) { entries.forEach(addEntry); })
  .catch(function() {});

// SSE connection with auto-reconnect
function connect() {
  var es = new EventSource('/events');
  es.onopen = function() {
    statusDot.classList.remove('disconnected');
    statusText.textContent = 'connected';
  };
  es.onmessage = function(e) {
    try { addEntry(JSON.parse(e.data)); } catch(err) {}
  };
  es.onerror = function() {
    statusDot.classList.add('disconnected');
    statusText.textContent = 'reconnecting...';
    es.close();
    setTimeout(connect, 3000);
  };
}
connect();
</script>
</body>
</html>
"##;
