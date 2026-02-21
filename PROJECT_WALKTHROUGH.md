# Polymarket 5m Arbitrage Bot - Complete Code Walkthrough

> A course-style guide to understanding this Rust trading bot, file by file.
> Written for someone who doesn't know Rust well.

---

## Table of Contents

1. [Project Overview](#1-project-overview)
2. [Cargo.toml - The Project Blueprint](#2-cargotoml---the-project-blueprint)
3. [models.rs - Data Structures](#3-modelsrs---data-structures)
4. [config.rs - Configuration & CLI](#4-configrs---configuration--cli)
5. [main.rs - Entry Point](#5-mainrs---entry-point)
6. [discovery.rs - Market Discovery](#6-discoveryrs---market-discovery)
7. [ws.rs - WebSocket Market Data](#7-wsrs---websocket-market-data)
8. [rtds.rs - Real-Time Data Socket](#8-rtdsrs---real-time-data-socket)
9. [chainlink.rs - Chainlink Price Poller](#9-chainlinkrs---chainlink-price-poller)
10. [api.rs - Polymarket API Client](#10-apirs---polymarket-api-client)
11. [strategy.rs - The Trading Brain](#11-strategyrs---the-trading-brain)
12. [ecosystem.config.js - Deployment](#12-ecosystemconfigjs---deployment)
13. [How It All Fits Together](#13-how-it-all-fits-together)

---

## 1. Project Overview

### What does this bot do?

This bot trades on **Polymarket** prediction markets -- specifically **5-minute crypto price markets**. These markets ask things like:

> "Will Bitcoin be above $97,500 at 3:05 PM ET?"

The bot:
1. **Discovers** active 5-minute markets for BTC, ETH, SOL, and XRP
2. **Gets the price-to-beat** (the threshold price) from Polymarket's real-time data feed (RTDS Chainlink)
3. **Monitors live prices** via WebSocket (bid/ask for "Up" and "Down" tokens)
4. **Places pre-orders** -- limit buy orders on the side the market currently favors
5. **Polls for resolution** after the market ends to see who won

### Architecture at a Glance

```
                    +----------------+
                    |   main.rs      |  Entry point
                    +-------+--------+
                            |
                    +-------v--------+
                    |  strategy.rs   |  Trading brain (runs the loop)
                    +-------+--------+
                            |
          +-----------------+------------------+
          |                 |                  |
  +-------v------+  +------v-------+  +-------v-------+
  | discovery.rs |  |   ws.rs      |  | chainlink.rs  |
  | (find mkts)  |  | (live bids)  |  | (price feed)  |
  +-------+------+  +--------------+  +-------+-------+
          |                                    |
  +-------v------+                    +--------v------+
  |   api.rs     |                    |   rtds.rs     |
  | (HTTP + SDK) |                    | (WS Chainlink)|
  +--------------+                    +---------------+
          |
  +-------v------+    +---------------+
  |  models.rs   |    |  config.rs    |
  | (data types) |    | (settings)    |
  +--------------+    +---------------+
```

---

## 2. Cargo.toml - The Project Blueprint

**File:** `Cargo.toml`
**Purpose:** This is Rust's equivalent of `package.json` (Node.js) or `requirements.txt` (Python). It defines the project name, version, and all external libraries (called "crates") the project depends on.

### Full File Walkthrough

```toml
[package]
name = "polymarket-arbitrage-bot"
version = "0.1.0"
edition = "2021"
```

- **`name`** -- The name of the compiled binary. When you run `cargo build`, it produces a file called `polymarket-arbitrage-bot`.
- **`version`** -- Semantic versioning. `0.1.0` means it's early/experimental.
- **`edition = "2021"`** -- Rust has "editions" (2015, 2018, 2021). This determines which Rust language features are available. Think of it as a language version.

### Dependencies Explained (Grouped by Purpose)

#### Async Runtime & Concurrency
```toml
tokio = { version = "1.35", features = ["full"] }
futures-util = "0.3"
```
- **`tokio`** -- THE async runtime for Rust. In Python you just use `asyncio`, but Rust requires you to pick a runtime. `features = ["full"]` means "give me everything" (timers, networking, file I/O, etc.)
- **`futures-util`** -- Helper functions for working with async streams (like `.next()` on a WebSocket stream)

#### HTTP & Networking
```toml
reqwest = { version = "0.11", features = ["json"] }
tokio-tungstenite = { version = "0.21", features = ["native-tls"] }
url = "2.5"
```
- **`reqwest`** -- HTTP client (like `axios` in JS or `requests` in Python). `features = ["json"]` adds `.json()` method to automatically deserialize responses.
- **`tokio-tungstenite`** -- WebSocket client that works with tokio. Used for live price streaming. `native-tls` means it uses the OS's TLS library for secure connections.
- **`url`** -- URL parsing library

#### Serialization (Converting data to/from JSON)
```toml
serde = { version = "1.0", features = ["derive"] }
serde_json = "1.0"
```
- **`serde`** -- THE serialization framework in Rust. `derive` lets you add `#[derive(Serialize, Deserialize)]` to structs to auto-generate JSON conversion code.
- **`serde_json`** -- JSON-specific serde implementation

#### Cryptography & Signing
```toml
hmac = "0.12"
sha2 = "0.10"
hex = "0.4"
base64 = "0.21"
```
- **`hmac`** + **`sha2`** -- Used for HMAC-SHA256 signatures (authenticating API requests)
- **`hex`** -- Converting bytes to/from hexadecimal strings (blockchain addresses are hex)
- **`base64`** -- Base64 encoding/decoding (API secrets are often base64-encoded)

#### Numeric Precision
```toml
rust_decimal = "1.33"
rust_decimal_macros = "1.33"
```
- **`rust_decimal`** -- High-precision decimal numbers. Regular `f64` has floating-point errors (0.1 + 0.2 = 0.30000000000000004). When dealing with money, you NEED exact decimals. This crate gives you that.

#### Polymarket SDK & Blockchain
```toml
polymarket-client-sdk = { version = "0.4.2", features = ["clob"] }
alloy = { version = "1.3", features = ["full"] }
alloy-contract = "1.3"
alloy-sol-types = "1.5"
```
- **`polymarket-client-sdk`** -- Official Polymarket SDK for Rust. `clob` feature enables Central Limit Order Book operations (placing/canceling orders).
- **`alloy`** -- Modern Ethereum library for Rust (successor to `ethers-rs`). Used for signing transactions, interacting with smart contracts, and reading blockchain data.
- **`alloy-contract`** / **`alloy-sol-types`** -- Smart contract interaction helpers

#### Date/Time
```toml
chrono = { version = "0.4", features = ["serde"] }
chrono-tz = "0.9"
```
- **`chrono`** -- Date/time library (like `datetime` in Python). Critical here because markets align to Eastern Time.
- **`chrono-tz`** -- Timezone support. The bot needs to convert UTC to Eastern Time.

#### CLI & Config
```toml
clap = { version = "4.4", features = ["derive"] }
toml = "0.8"
```
- **`clap`** -- Command-Line Argument Parser. With `derive`, you can define CLI args as a struct.
- **`toml`** -- TOML file parser (though config actually uses JSON -- this might be leftover)

#### Utilities
```toml
anyhow = "1.0"
log = "0.4"
env_logger = "0.11"
walkdir = "2.5"
```
- **`anyhow`** -- Simplified error handling. Instead of defining custom error types, you just use `anyhow::Result` and `?` to propagate any error.
- **`log`** + **`env_logger`** -- Logging framework. `log` defines the macros (`info!`, `warn!`, `error!`), `env_logger` outputs them to the terminal.
- **`walkdir`** -- Directory traversal (likely unused/leftover)

---

## 3. models.rs - Data Structures

**File:** `src/models.rs`
**Purpose:** Defines all the data structures (types) that represent things the bot works with: markets, orders, prices, etc. This is the "vocabulary" of the project.

### Rust Concept: Structs

In Rust, a `struct` is like a class without methods (though you can add methods). It's a custom data type that groups related values together.

```rust
pub struct Market {
    pub condition_id: String,
    pub question: String,
    // ...
}
```

- **`pub`** = public (accessible from other files). Without `pub`, it's private.
- Each field has a name and a type.

### Rust Concept: Derive Macros

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
```

This line above a struct automatically generates code:
- **`Debug`** -- Lets you print the struct with `{:?}` format (like Python's `__repr__`)
- **`Clone`** -- Lets you create a copy with `.clone()`
- **`Serialize`** -- Can convert the struct TO JSON
- **`Deserialize`** -- Can create the struct FROM JSON

### Rust Concept: Serde Rename

```rust
#[serde(rename = "conditionId")]
pub condition_id: String,
```

The JSON from Polymarket API uses `camelCase` (like `conditionId`), but Rust convention is `snake_case` (`condition_id`). The `#[serde(rename = "...")]` attribute tells serde: "When reading/writing JSON, use this name instead."

### Struct-by-Struct Breakdown

#### `Market` (line 5-16)
```rust
pub struct Market {
    #[serde(rename = "conditionId")]
    pub condition_id: String,      // Unique market identifier on the blockchain
    #[serde(rename = "id")]
    pub market_id: Option<String>, // Optional internal market ID
    pub question: String,          // "Will Bitcoin be above $97,500 at 3:05 PM ET?"
    pub slug: String,              // URL-friendly name: "btc-updown-5m-1767726000"
    #[serde(rename = "endDateISO")]
    pub end_date_iso: Option<String>, // When the market closes
    pub active: bool,              // Is it currently tradeable?
    pub closed: bool,              // Has it already ended?
}
```

**What is `Option<String>`?** -- Rust has no `null` or `None` like other languages. Instead, it uses `Option<T>` which is either `Some(value)` or `None`. This forces you to handle the "missing" case explicitly. `Option<String>` means "might have a String, might not."

This struct represents a market fetched from the **Gamma API** (by slug).

#### `MarketDetails` (line 18-28)
```rust
pub struct MarketDetails {
    pub condition_id: String,
    pub question: String,
    pub tokens: Vec<MarketToken>,  // The tradeable tokens (Up and Down)
    pub active: bool,
    pub closed: bool,
    pub end_date_iso: String,
}
```

**What is `Vec<MarketToken>`?** -- `Vec` is Rust's dynamic array (like a Python list or JS array). This holds a list of `MarketToken` items.

This struct represents a market fetched from the **CLOB API** (by condition ID). Notice it includes `tokens` -- the actual things you buy/sell.

#### `MarketToken` (line 30-36)
```rust
pub struct MarketToken {
    pub outcome: String,    // "Up" or "Down"
    pub token_id: String,   // Long numeric ID used for trading
    pub winner: bool,       // Was this the winning outcome? (after resolution)
}
```

Each 5-minute market has exactly 2 tokens: "Up" and "Down." If BTC is above the price-to-beat at the end, the "Up" token wins and pays out $1. If not, "Down" wins.

#### `OrderBook` (line 38-48)
```rust
pub struct OrderBook {
    pub bids: Vec<OrderBookEntry>,  // Buy orders (people wanting to buy)
    pub asks: Vec<OrderBookEntry>,  // Sell orders (people wanting to sell)
}

pub struct OrderBookEntry {
    pub price: Decimal,  // Price in dollars (e.g. 0.65)
    pub size: Decimal,   // How many shares at this price
}
```

The order book is like an auction. **Bids** are what buyers are willing to pay. **Asks** are what sellers want to receive. The best bid is the highest; the best ask is the lowest.

**Why `Decimal` instead of `f64`?** -- Money needs exact precision. `0.1 + 0.2` with `f64` gives `0.30000000000000004`. With `Decimal`, it gives exactly `0.3`.

#### `OrderRequest` (line 50-58)
```rust
pub struct OrderRequest {
    pub token_id: String,
    pub side: String,       // "BUY" or "SELL"
    pub size: String,       // Number of shares (as a string for precision)
    pub price: String,      // Price per share (as a string)
    #[serde(rename = "type")]
    pub order_type: String, // "GTC" (Good Till Cancel), "FOK" (Fill Or Kill), etc.
}
```

This is what you send to Polymarket when you want to place an order.

Note: `#[serde(rename = "type")]` is needed because `type` is a **reserved keyword** in Rust -- you can't name a field `type`, so we call it `order_type` in Rust but it maps to `"type"` in JSON.

#### `OrderResponse` (line 60-65)
```rust
pub struct OrderResponse {
    pub order_id: Option<String>,
    pub status: String,
    pub message: Option<String>,
}
```

What Polymarket sends back after you place an order.

#### `OrderStatus` (line 67-75)
```rust
pub struct OrderStatus {
    pub id: Option<String>,
    pub status: Option<String>,
    pub original_size: Option<String>,
    pub size_matched: Option<String>,  // How much of your order was filled
}
```

Used to check if an order was filled. `size_matched` tells you how many shares actually got traded.

#### `RedeemResponse` (line 77-84)
```rust
pub struct RedeemResponse {
    pub success: bool,
    pub message: Option<String>,
    pub transaction_hash: Option<String>,  // Blockchain transaction ID
    pub amount_redeemed: Option<String>,
}
```

After a market resolves, winning tokens can be "redeemed" for USDC (a stablecoin). This struct is the result of that redemption.

#### `TokenPrice` (line 86-91)
```rust
pub struct TokenPrice {
    pub token_id: String,
    pub bid: Option<Decimal>,  // Best buy price (might not exist)
    pub ask: Option<Decimal>,  // Best sell price (might not exist)
}
```

Represents the current best bid/ask for a token.

#### `Fill` (line 93-103)
```rust
pub struct Fill {
    pub token_id: Option<String>,
    pub side: String,
    pub size: f64,
    pub price: f64,
    pub timestamp: u64,
    pub condition_id: Option<String>,
}
```

A "fill" is a completed trade. When your order matches someone else's, you get a fill.

---

## 4. config.rs - Configuration & CLI

**File:** `src/config.rs`
**Purpose:** Defines the command-line arguments the bot accepts and the configuration file format. Loads settings from `config.json`.

### Rust Concept: Clap Derive

```rust
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
```

**`clap`** with `derive` turns a struct into a CLI parser automatically:
- `#[derive(Parser)]` -- Makes this struct parseable from command-line args
- `#[command(...)]` -- Metadata for `--help` output
- `#[arg(short, long, default_value = "config.json")]` -- This field can be set with `-c config.json` or `--config config.json`. If not provided, defaults to `"config.json"`.
- `#[arg(long)]` -- Only `--redeem` (no short flag). It's a `bool`, so it's a flag (present = true, absent = false).
- `#[arg(long, requires = "redeem")]` -- `--condition-id` can only be used if `--redeem` is also present.

**`PathBuf`** is Rust's type for file system paths (cross-platform, handles `/` vs `\` automatically).

### Usage Examples
```bash
# Normal run with default config
./polymarket-arbitrage-bot

# Custom config file
./polymarket-arbitrage-bot --config /path/to/my-config.json

# Redeem mode (cash out winning positions)
./polymarket-arbitrage-bot --redeem

# Redeem specific condition
./polymarket-arbitrage-bot --redeem --condition-id 0xabc123...
```

### Config Structure

#### `Config` (line 18-22)
```rust
pub struct Config {
    pub polymarket: PolymarketConfig,  // API credentials + URLs
    pub strategy: StrategyConfig,      // Trading parameters
}
```

The root config has two sections: one for Polymarket connection details, one for trading strategy.

#### `StrategyConfig` (line 25-50)
```rust
pub struct StrategyConfig {
    #[serde(default = "default_symbols")]
    pub symbols: Vec<String>,           // ["btc", "eth", "sol", "xrp"]

    #[serde(default = "default_sum_threshold")]
    pub sum_threshold: f64,             // Max sum for arb trigger (0.99)

    #[serde(default = "default_verify_fill_secs")]
    pub verify_fill_secs: u64,          // Seconds to wait before checking fill

    #[serde(default)]
    pub simulation_mode: bool,          // If true, don't actually trade

    #[serde(default)]
    pub pre_order_enabled: bool,        // Enable/disable pre-ordering

    #[serde(default = "default_pre_order_size")]
    pub pre_order_size: String,         // "10" shares

    #[serde(default = "default_pre_order_side")]
    pub pre_order_side: String,         // "up", "down", or "favor"

    #[serde(default)]
    pub pre_order_improve_ticks: u32,   // How many cents to improve the price
}
```

**`#[serde(default = "default_symbols")]`** -- If this field is missing from the JSON, call the function `default_symbols()` to get a default value. This is how the bot can run with a minimal config file.

**Key strategy concepts:**
- **`pre_order_side: "favor"`** means "buy whichever side the market currently favors" (the side with the lower ask price).
- **`pre_order_improve_ticks`** -- If set to 2, the bot places its order 2 cents ($0.02) below the best ask. This means it gets a better price but might not fill immediately.

#### Default Functions (line 52-66)
```rust
fn default_symbols() -> Vec<String> {
    vec!["btc".into(), "eth".into(), "sol".into(), "xrp".into()]
}
fn default_sum_threshold() -> f64 { 0.99 }
fn default_verify_fill_secs() -> u64 { 10 }
fn default_pre_order_size() -> String { "10".to_string() }
fn default_pre_order_side() -> String { "favor".to_string() }
```

**`.into()`** -- Converts `"btc"` (a string slice `&str`) into a `String` (owned, heap-allocated). Rust distinguishes between borrowed strings (`&str`) and owned strings (`String`).

#### `PolymarketConfig` (line 68-87)
```rust
pub struct PolymarketConfig {
    pub gamma_api_url: String,         // https://gamma-api.polymarket.com
    pub clob_api_url: String,          // https://clob.polymarket.com
    pub api_key: Option<String>,       // Your API key
    pub api_secret: Option<String>,    // Your API secret
    pub api_passphrase: Option<String>,// Your API passphrase
    pub private_key: Option<String>,   // Ethereum private key for signing
    pub proxy_wallet_address: Option<String>, // Proxy/Safe wallet address
    pub signature_type: Option<u8>,    // 0=EOA, 1=Proxy, 2=GnosisSafe
    pub rpc_url: Option<String>,       // Polygon RPC URL
    pub ws_url: String,                // WebSocket URL for price data
    pub rtds_ws_url: String,           // RTDS WebSocket URL for Chainlink
}
```

All the `Option<String>` fields are optional -- you don't need them if you only want to monitor (not trade).

**Signature types explained:**
- **EOA (0)** -- Externally Owned Account. Your regular Ethereum wallet (MetaMask private key).
- **Proxy (1)** -- Polymarket's custom proxy wallet. Used by MagicLink/email login users.
- **GnosisSafe (2)** -- A multi-signature smart contract wallet. More secure but more complex redemption.

### Config Loading (line 127-139)
```rust
impl Config {
    pub fn load(path: &PathBuf) -> anyhow::Result<Self> {
        if path.exists() {
            let content = std::fs::read_to_string(path)?;
            Ok(serde_json::from_str(&content)?)
        } else {
            let config = Config::default();
            let content = serde_json::to_string_pretty(&config)?;
            std::fs::write(path, content)?;
            Ok(config)
        }
    }
}
```

**`impl Config`** -- Adds methods to the `Config` struct (like methods on a class).

**The `?` operator** -- This is Rust's error propagation. If the operation fails, it immediately returns the error to the caller. It's shorthand for:
```rust
let content = match std::fs::read_to_string(path) {
    Ok(c) => c,
    Err(e) => return Err(e.into()),
};
```

**Logic:** If config file exists, read and parse it. If it doesn't exist, create a default config and write it to disk. This way, the first run auto-generates a config file for you.

---

## 5. main.rs - Entry Point

**File:** `src/main.rs`
**Purpose:** The starting point of the program. Sets up logging, loads config, authenticates, and either runs the trading bot or the redeem-only mode.

### Module Declarations (line 1-8)
```rust
mod api;
mod chainlink;
mod config;
mod discovery;
mod models;
mod rtds;
mod strategy;
mod ws;
```

**`mod xyz;`** tells Rust: "Look for a file called `xyz.rs` in the same directory and include it as a module." This is how Rust organizes code into multiple files. Each `mod` statement makes that file's code available.

### Imports (line 10-16)
```rust
use anyhow::Result;
use clap::Parser;
use config::{Args, Config};
use std::io::Write;
use std::sync::Arc;
use api::PolymarketApi;
use strategy::ArbStrategy;
```

**`use`** brings items into scope so you don't have to write the full path every time.

**`Arc`** -- "Atomic Reference Counted" smart pointer. When multiple async tasks need to share the same data, you wrap it in `Arc`. It's like a shared pointer that automatically tracks how many things are using it and cleans up when the last one is done.

### The `main` Function (line 18-61)
```rust
#[tokio::main]
async fn main() -> Result<()> {
```

**`#[tokio::main]`** -- This attribute macro transforms your `main` function into an async runtime. Under the hood, it creates a tokio runtime and runs your async code on it. Without this, you can't use `await` in `main`.

**`async fn`** -- Makes this function asynchronous. It can `await` other async operations.

**`-> Result<()>`** -- The function returns either `Ok(())` (success, no value) or an error. The `()` is Rust's "void" or "unit type."

#### Logger Setup (line 20-23)
```rust
env_logger::Builder::from_default_env()
    .filter_level(log::LevelFilter::Info)
    .format(|buf, record| writeln!(buf, "{}", record.args()))
    .init();
```

Sets up logging to only show `Info` level and above (not `Debug` or `Trace`). The custom format strips timestamps and metadata -- just prints the message.

#### CLI Parsing & Config Loading (line 25-26)
```rust
let args = Args::parse();
let config = Config::load(&args.config)?;
```

`Args::parse()` reads command-line arguments. `Config::load` reads the config file. The `?` propagates any errors.

#### Creating the API Client (line 33-43)
```rust
let api = Arc::new(PolymarketApi::new(
    config.polymarket.gamma_api_url.clone(),
    config.polymarket.clob_api_url.clone(),
    // ... all the config values ...
));
```

**`Arc::new(...)`** wraps the API client in an `Arc` so multiple async tasks (one per symbol) can share the same client.

**`.clone()`** -- `String` in Rust is owned. When you pass it to a function, it *moves* (the original can't be used anymore). `.clone()` creates a copy so you can keep using the original. This is Rust's ownership system in action.

#### Redeem Mode (line 45-48)
```rust
if args.redeem {
    run_redeem_only(api.as_ref(), &config, args.condition_id.as_deref()).await?;
    return Ok(());
}
```

If `--redeem` flag is set, run redemption mode and exit. `as_ref()` converts `&Arc<T>` to `&T`. `as_deref()` converts `Option<String>` to `Option<&str>`.

#### Authentication (line 50-57)
```rust
if config.polymarket.private_key.is_some() {
    if let Err(e) = api.authenticate().await {
        log::error!("Authentication failed: {}", e);
        anyhow::bail!("Authentication failed. Please check your credentials.");
    }
} else {
    log::warn!("No private key provided. Bot can only monitor (no orders).");
}
```

**`if let Err(e) = ...`** -- Pattern matching. This checks if the result is an error, and if so, binds the error to `e`. It's a cleaner way to handle just the error case.

**`anyhow::bail!(...)`** -- Immediately returns an error with the given message. The program exits.

#### Running the Strategy (line 59-60)
```rust
let strategy = ArbStrategy::new(api, config);
strategy.run().await
```

Creates the strategy and runs it. `.await` pauses here until the strategy finishes (which is basically forever -- it loops continuously).

### Redeem Function (line 63-114)

```rust
async fn run_redeem_only(
    api: &PolymarketApi,
    config: &Config,
    condition_id: Option<&str>,
) -> Result<()> {
```

**`&PolymarketApi`** -- The `&` means "borrow." We don't own the API client; we just temporarily use it. Rust's borrow checker ensures we don't use it after it's freed.

**Key logic:**
1. If a specific `condition_id` is provided, redeem just that one
2. Otherwise, fetch ALL redeemable positions and redeem them one by one
3. Track success/failure counts

```rust
let cids: Vec<String> = if let Some(cid) = condition_id {
    // ...
    vec![cid]
} else {
    // Fetch all redeemable
    let list = api.get_redeemable_positions(proxy).await?;
    // ...
    list
};
```

**`if let Some(cid) = condition_id`** -- If `condition_id` is `Some(value)`, extract the value into `cid`. If it's `None`, take the `else` branch.

**`vec![cid]`** -- Creates a `Vec` with a single element. Macro shorthand.

---

## 6. discovery.rs - Market Discovery

**File:** `src/discovery.rs`
**Purpose:** Figures out which market to trade right now. Polymarket creates a new 5-minute market every 5 minutes, and the slug (URL-friendly name) includes the period start timestamp. This file calculates that timestamp and builds the slug.

### Constants (line 7-8)
```rust
pub const MARKET_15M_DURATION_SECS: i64 = 15 * 60; // 900
pub const MARKET_5M_DURATION_SECS: i64 = 5 * 60;   // 300
```

**`const`** -- Compile-time constant. Can't be changed at runtime. `i64` is a 64-bit signed integer.

### Period Start Calculation (line 11-26)

This is the core logic -- figuring out what 5-minute window we're currently in.

```rust
fn period_start_et_unix(minutes: i64) -> i64 {
    let utc_now = chrono::Utc::now();                    // Current time in UTC
    let et = New_York;                                    // Eastern timezone
    let now_et = utc_now.with_timezone(&et);              // Convert to ET
    let minute_floor = (now_et.minute() as i64 / minutes) * minutes;  // Round down
    let truncated_naive = now_et
        .date_naive()
        .and_hms_opt(now_et.hour(), minute_floor as u32, 0)
        .unwrap();
    let dt_et = et
        .from_local_datetime(&truncated_naive)
        .single()
        .or_else(|| et.from_local_datetime(&truncated_naive).earliest())
        .expect("ET period start");
    dt_et.timestamp()
}
```

**What this does step-by-step:**

1. Get current UTC time
2. Convert to Eastern Time
3. Round down to the nearest 5-minute (or 15-minute) boundary
   - If it's 3:07 PM ET, `minute_floor = (7 / 5) * 5 = 5`, so the period started at 3:05 PM
   - If it's 3:14 PM ET, `minute_floor = (14 / 5) * 5 = 10`, so the period started at 3:10 PM
4. Reconstruct the date-time at that floored minute with 0 seconds
5. Convert back to a Unix timestamp

**Why Eastern Time?** Polymarket aligns its markets to ET. The slug `btc-updown-5m-1767726000` uses the Unix timestamp of the period start in ET.

**`as i64`** -- Type casting. `now_et.minute()` returns `u32` (unsigned 32-bit), but we need `i64` for the division.

### Slug Building (line 29-36)
```rust
pub fn build_15m_slug(period_start_unix: i64) -> String {
    format!("btc-updown-15m-{}", period_start_unix)
}

pub fn build_5m_slug(symbol: &str, period_start_unix: i64) -> String {
    format!("{}-updown-5m-{}", symbol.to_lowercase(), period_start_unix)
}
```

**`format!`** -- Rust's string formatting macro (like Python's f-strings). `{}` is replaced with the value.

Examples:
- `build_5m_slug("btc", 1767726000)` returns `"btc-updown-5m-1767726000"`
- `build_5m_slug("ETH", 1767726300)` returns `"eth-updown-5m-1767726300"`

### Period Start for a Given Timestamp (line 49-67)
```rust
pub fn period_start_et_unix_for_timestamp(ts_sec: i64, minutes: i64) -> i64 {
```

Same logic as `period_start_et_unix` but for a specific timestamp instead of "now." This is used by the RTDS module to figure out which period a price update belongs to.

### Parsing Price from Question (line 76-100)
```rust
pub fn parse_price_to_beat_from_question(question: &str) -> Option<f64> {
    let q = question.to_lowercase();
    let idx = q.find("above ").or_else(|| q.find('$'))?;
    // ... extract the number after "above " or "$" ...
}
```

Given a question like "Will Bitcoin be above $97,500 at 3:05 PM ET?", this extracts `97500.0`.

**`?` on `Option`** -- When used on an `Option`, `?` returns `None` early if the value is `None`. It's like saying "if we can't find 'above' or '$', just return None immediately."

**The algorithm:**
1. Find "above " or "$" in the question
2. Extract the numeric characters after it (including `.` and `,`)
3. Remove commas (`97,500` -> `97500`)
4. Parse as `f64`

### `MarketDiscovery` Struct (line 102-158)

```rust
pub struct MarketDiscovery {
    api: Arc<PolymarketApi>,
}
```

Wraps the API client and provides convenience methods:

- **`get_market_tokens(condition_id)`** -- Given a market ID, returns the Up and Down token IDs
- **`get_15m_market(period_start)`** -- Finds the BTC 15m market for a period
- **`get_5m_market(symbol, period_start)`** -- Finds the 5m market for any symbol

```rust
pub async fn get_market_tokens(&self, condition_id: &str) -> Result<(String, String)> {
    let details = self.api.get_market(condition_id).await?;
    let mut up_token = None;
    let mut down_token = None;
    for token in details.tokens {
        let outcome = token.outcome.to_uppercase();
        if outcome.contains("UP") || outcome == "1" {
            up_token = Some(token.token_id);
        } else if outcome.contains("DOWN") || outcome == "0" {
            down_token = Some(token.token_id);
        }
    }
    let up = up_token.ok_or_else(|| anyhow::anyhow!("Up token not found"))?;
    let down = down_token.ok_or_else(|| anyhow::anyhow!("Down token not found"))?;
    Ok((up, down))
}
```

**`&self`** -- Like `self` in Python. Reference to the current instance.

**`(String, String)`** -- A tuple. Returns two strings: the Up token ID and the Down token ID.

**`ok_or_else`** -- Converts `Option<T>` to `Result<T, E>`. If it's `None`, calls the closure to create an error.

---

## 7. ws.rs - WebSocket Market Data

**File:** `src/ws.rs`
**Purpose:** Connects to Polymarket's WebSocket to get **live bid/ask prices** for specific tokens. This is how the bot sees real-time market prices.

### Module Documentation (line 1)
```rust
//! CLOB Market WebSocket: subscribe to asset_ids and stream best bid/ask updates.
```

**`//!`** -- Module-level doc comment. Describes the whole file.

### Data Structures for WebSocket Messages (line 14-48)

```rust
#[derive(Debug, Deserialize)]
struct WsBookLevel {
    price: String,
    size: String,
}

#[derive(Debug, Deserialize)]
struct WsBookMessage {
    event_type: Option<String>,
    asset_id: String,
    #[serde(default, alias = "bids")]
    buys: Vec<WsBookLevel>,
    #[serde(default, alias = "asks")]
    sells: Vec<WsBookLevel>,
}
```

These are **private** structs (no `pub`) because they're only used within this file to deserialize WebSocket messages. Notice these don't derive `Serialize` -- we only read them, never write them.

**`#[serde(default, alias = "bids")]`** -- Two things:
- `default` means if the field is missing from JSON, use a default value (empty `Vec` for `Vec<T>`)
- `alias = "bids"` means this field can be called either `buys` or `bids` in the JSON

The WebSocket sends two types of messages:
1. **`book`** -- Full order book snapshot (all bids and asks)
2. **`price_change`** -- Just the best bid/ask changed

### Shared State Type (line 57-58)
```rust
pub type PricesSnapshot = Arc<RwLock<HashMap<String, BestPrices>>>;
```

**`type`** -- Creates a type alias. `PricesSnapshot` is just a shorter name for this complex type.

Let's unpack this:
- **`HashMap<String, BestPrices>`** -- A hash map (dictionary) where keys are token IDs and values are `BestPrices` structs
- **`RwLock<...>`** -- A read-write lock. Multiple readers can read simultaneously, but writing requires exclusive access. This is needed because the WebSocket task writes prices while the strategy task reads them.
- **`Arc<...>`** -- Shared ownership. Both the WS task and strategy task hold a reference to the same data.

### Placeholder Detection (line 65-72)
```rust
fn is_placeholder_quote(bid: Option<f64>, ask: Option<f64>) -> bool {
    match (bid, ask) {
        (Some(b), Some(a)) => b < 0.05 && a > 0.95,
        (Some(b), None) => b < 0.05,
        (None, Some(a)) => a > 0.95,
        (None, None) => false,
    }
}
```

**Pattern matching on a tuple** -- Rust's `match` can match on combinations. `(Some(b), Some(a))` matches when both are `Some`, and extracts the inner values to `b` and `a`.

Sometimes the exchange sends "empty" quotes (bid=0.01, ask=0.99). These aren't real prices -- they mean the order book is empty. This function detects those.

### WebSocket Connection (line 78-149)

```rust
pub async fn run_market_ws(
    ws_base_url: &str,
    asset_ids: Vec<String>,
    prices: PricesSnapshot,
) -> Result<()> {
```

This is the main WebSocket function. Let me walk through the key parts:

#### Connecting (line 96-103)
```rust
let (ws_stream, _) = match connect_async(&url).await {
    Ok(s) => s,
    Err(e) => {
        error!("WebSocket connect failed: {}. Reconnecting in {}s.", e, WS_RECONNECT_DELAY_SECS);
        tokio::time::sleep(tokio::time::Duration::from_secs(WS_RECONNECT_DELAY_SECS)).await;
        continue;  // Goes back to the top of the loop to retry
    }
};
```

**`connect_async`** returns a WebSocket stream and a response. The `_` means "I don't care about the response."

The whole function is wrapped in a `loop` so if the connection drops, it reconnects automatically.

#### Subscribing (line 105-112)
```rust
let (mut write, mut read) = ws_stream.split();
let sub_msg = Message::Text(sub_body.clone());
write.send(sub_msg).await?;
```

**`.split()`** splits the WebSocket into a writer and reader, so you can send and receive independently.

The subscription message looks like:
```json
{"assets_ids": ["token_id_1", "token_id_2"], "type": "market"}
```

#### Message Processing Loop (line 115-140)
```rust
while let Some(msg) = read.next().await {
    match msg {
        Ok(Message::Text(text)) => { /* process text message */ }
        Ok(Message::Ping(data)) => { let _ = write.send(Message::Pong(data)).await; }
        Ok(Message::Close(_)) => { break; }
        Err(e) => { break; }
        _ => {}
    }
}
```

**`while let Some(msg) = read.next().await`** -- Reads messages from the WebSocket stream one by one. `next()` returns `Some(message)` when there's a message, `None` when the stream ends.

**Nested `match`** -- First match on `Ok` vs `Err`, then on the message type. `Message::Ping` requires a `Pong` response (WebSocket keepalive). `Message::Close` means the server is disconnecting.

### Message Processing (line 151-192)

```rust
async fn process_message(text: &str, prices: &PricesSnapshot) -> Result<()> {
    let v: serde_json::Value = serde_json::from_str(text).context("Parse JSON")?;
    let event_type = v.get("event_type").and_then(|t| t.as_str());

    if event_type == Some("book") {
        // Full order book update
        let book: WsBookMessage = serde_json::from_value(v).context("Parse book")?;
        let bid = book.buys.first().and_then(|b| parse_f64(&b.price));
        let ask = book.sells.first().and_then(|a| parse_f64(&a.price));
        if (bid.is_some() || ask.is_some()) && !is_placeholder_quote(bid, ask) {
            let mut w = prices.write().await;  // Get write lock
            let entry = w.entry(book.asset_id).or_default();  // Get or create entry
            if let Some(b) = bid { entry.bid = Some(b); }
            if let Some(a) = ask { entry.ask = Some(a); }
        }
    }
    // ... similar for price_change ...
}
```

**`prices.write().await`** -- Acquires a write lock on the `RwLock`. This blocks other writers and readers until the lock is released (when `w` is dropped).

**`.entry(book.asset_id).or_default()`** -- HashMap entry API. If the key exists, return a mutable reference. If not, insert a default value and return that.

---

## 8. rtds.rs - Real-Time Data Socket

**File:** `src/rtds.rs`
**Purpose:** Connects to Polymarket's RTDS (Real-Time Data Socket) to get **Chainlink oracle prices** for BTC, ETH, SOL, and XRP. The "price-to-beat" for each 5-minute market comes from here.

### What is "Price-to-Beat"?

When Polymarket creates a 5m market like "Will BTC be above $97,500?", the $97,500 is the **price-to-beat**. It's the BTC price from Chainlink at the moment the market period starts. RTDS streams this data in real-time.

### Custom Deserializers (line 29-53)

```rust
fn deser_ts<'de, D>(d: D) -> Result<i64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let v = serde_json::Value::deserialize(d)?;
    match v {
        serde_json::Value::Number(n) => n.as_i64().ok_or_else(|| D::Error::custom("bad ts")),
        serde_json::Value::String(s) => s.parse::<i64>().map_err(D::Error::custom),
        _ => Err(D::Error::custom("timestamp must be number or string")),
    }
}
```

**`<'de>`** -- This is a **lifetime parameter**. It's one of Rust's more advanced features. In simple terms, it tells the compiler: "this deserializer borrows data that lives for the lifetime `'de`."

**Why custom deserializers?** The RTDS API sometimes sends timestamps as numbers (`1767726000`) and sometimes as strings (`"1767726000"`). This deserializer handles both cases.

**`where D: serde::Deserializer<'de>`** -- A trait bound. "D must implement the Deserializer trait." This is generic programming -- the function works with any deserializer.

### `PriceCacheMulti` Type (line 64)

```rust
pub type PriceCacheMulti = Arc<RwLock<HashMap<String, HashMap<i64, f64>>>>;
```

Nested structure:
```
symbol ("btc") -> period_start (1767726000) -> price (97500.0)
```

Each symbol has a map of period timestamps to prices. This is shared between the RTDS reader and the strategy.

### RTDS Connection & Subscription (line 83-116)

```rust
pub async fn run_rtds_chainlink_multi(
    ws_url: &str,
    symbols: &[String],
    price_cache_15: PriceCacheMulti,
    price_cache_5: PriceCacheMulti,
) -> Result<()> {
```

**`&[String]`** -- A "slice" of Strings. Like an array reference. This is the "borrowed" view of a `Vec<String>`.

The subscription message format:
```json
{
    "action": "subscribe",
    "subscriptions": [
        {
            "topic": "crypto_prices_chainlink",
            "type": "*",
            "filters": "{\"symbol\":\"btc/usd\"}"
        }
    ]
}
```

### The Core Loop with `tokio::select!` (line 121-172)

```rust
loop {
    tokio::select! {
        Some(msg) = ws_stream.next() => {
            // Process incoming message
        }
        _ = ping.tick() => {
            // Send periodic ping to keep connection alive
            if ws_stream.send(Message::Ping(vec![])).await.is_err() {
                break;
            }
        }
    }
}
```

**`tokio::select!`** -- This is a powerful async primitive. It waits for EITHER:
1. A new WebSocket message to arrive, OR
2. The ping timer to fire (every 5 seconds)

Whichever happens first gets executed. This way, you can handle both incoming data AND send keepalive pings in the same loop without threads.

### Price-to-Beat Capture Logic (line 127-154)

This is the most important logic in this file:

```rust
let ts_sec = p.timestamp / 1000;  // Timestamp in seconds (RTDS sends millis)
let period_5 = period_start_et_unix_for_timestamp(ts_sec, 5);  // Which 5m period?
let in_capture_5 = ts_sec >= period_5 && ts_sec < period_5 + FEED_TS_CAPTURE_WINDOW_SECS;
```

**The capture window:** Only the first price within 2 seconds of a period start is used as the price-to-beat. If the period starts at 3:05:00 PM, only prices with timestamps between 3:05:00 and 3:05:02 PM are captured.

```rust
if in_capture_5 {
    let mut cache = price_cache_5.write().await;
    let per_symbol = cache.entry(key.clone()).or_default();
    if !per_symbol.contains_key(&period_5) {  // Only capture FIRST price
        per_symbol.insert(period_5, p.value);
        info!("RTDS Chainlink price-to-beat 5m {}: period {} -> {:.2} USD", ...);
    }
}
```

**`!per_symbol.contains_key(&period_5)`** -- Only insert if we don't already have a price for this period. This ensures the price-to-beat is the FIRST price at the period start, not a later one.

---

## 9. chainlink.rs - Chainlink Price Poller

**File:** `src/chainlink.rs`
**Purpose:** A thin wrapper that spawns the RTDS Chainlink stream in a background task with automatic reconnection.

### Full File (only 39 lines)

```rust
pub async fn run_chainlink_multi_poller(
    rtds_ws_url: String,
    symbols: Vec<String>,
    price_cache_15: PriceCacheMulti,
    price_cache_5: PriceCacheMulti,
) -> Result<()> {
    let cache_15 = Arc::clone(&price_cache_15);
    let cache_5 = Arc::clone(&price_cache_5);

    tokio::spawn(async move {
        loop {
            if let Err(e) = run_rtds_chainlink_multi(
                &rtds_ws_url, &symbols, cache_15.clone(), cache_5.clone(),
            ).await {
                warn!("RTDS Chainlink stream exited: {} (reconnecting in 5s)", e);
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    });

    tokio::time::sleep(Duration::from_secs(2)).await;
    Ok(())
}
```

**`tokio::spawn(async move { ... })`** -- Spawns a new async task (like starting a new thread, but lighter). The `move` keyword means the closure **takes ownership** of the variables it captures (`rtds_ws_url`, `symbols`, `cache_15`, `cache_5`).

**The retry loop:**
1. Run the RTDS connection
2. If it disconnects (returns an error), wait 5 seconds
3. Reconnect (loop back)

**`Arc::clone(&price_cache_15)`** -- Creates a new reference to the same shared data. This is cheap (just increments a counter). Both the spawned task and the caller keep a reference.

**`tokio::time::sleep(Duration::from_secs(2)).await`** -- Wait 2 seconds after spawning to give the connection time to establish before the strategy starts looking for prices.

---

## 10. api.rs - Polymarket API Client

**File:** `src/api.rs`
**Purpose:** The HTTP client that communicates with Polymarket's APIs. Handles authentication, placing orders, fetching market data, canceling orders, and redeeming winning positions on the blockchain.

This is the **largest and most complex file** in the project (~1200 lines).

### Solidity Interface Declaration (line 26-35)

```rust
sol! {
    interface IConditionalTokens {
        function redeemPositions(
            address collateralToken,
            bytes32 parentCollectionId,
            bytes32 conditionId,
            uint256[] indexSets
        ) external;
    }
}
```

**`sol!`** -- A macro from the `alloy` crate that parses Solidity (Ethereum smart contract language) and generates Rust types for calling that contract. This defines the `redeemPositions` function on Polymarket's Conditional Tokens Framework (CTF) contract.

**What this means:** To redeem winning tokens, the bot needs to call a function on an Ethereum smart contract. This declaration tells Rust what that function looks like so it can encode the call correctly.

### HMAC Type Alias (line 39)

```rust
type HmacSha256 = Hmac<Sha256>;
```

Creates a type alias for "HMAC using SHA-256 hash." HMAC (Hash-based Message Authentication Code) is used to sign API requests so Polymarket knows they're from you.

### `PolymarketApi` Struct (line 41-53)

```rust
pub struct PolymarketApi {
    client: Client,                              // HTTP client (reqwest)
    gamma_url: String,                           // Gamma API base URL
    clob_url: String,                            // CLOB API base URL
    api_key: Option<String>,
    api_secret: Option<String>,
    api_passphrase: Option<String>,
    private_key: Option<String>,                 // Ethereum private key
    proxy_wallet_address: Option<String>,
    signature_type: Option<u8>,
    rpc_url: Option<String>,
    authenticated: Arc<tokio::sync::Mutex<bool>>,  // Thread-safe bool
}
```

**`Arc<tokio::sync::Mutex<bool>>`** -- A thread-safe, async-aware boolean. `Mutex` is like `RwLock` but simpler (exclusive access only). This tracks whether we've successfully authenticated.

### Authentication (line 87-142)

```rust
pub async fn authenticate(&self) -> Result<()> {
    let private_key = self.private_key.as_ref()
        .ok_or_else(|| anyhow::anyhow!("Private key is required for authentication"))?;

    let signer = LocalSigner::from_str(private_key)
        .context("Failed to create signer from private key")?
        .with_chain_id(Some(POLYGON));
```

**`LocalSigner`** -- Creates an Ethereum signer from a private key. The private key is a 256-bit hex string that can sign transactions.

**`.with_chain_id(Some(POLYGON))`** -- Sets the blockchain network. `POLYGON` is the Polygon/Matic chain where Polymarket runs (not Ethereum mainnet -- Polygon is cheaper and faster).

**`.context("...")`** -- From the `anyhow` crate. Adds context to errors so when something fails, you get a helpful message like "Failed to create signer: invalid hex string" instead of just "invalid hex string."

#### Signature Type Handling (line 104-124)

```rust
let sig_type = match self.signature_type {
    Some(1) => SignatureType::Proxy,
    Some(2) => SignatureType::GnosisSafe,
    Some(0) | None => {
        warn!("Defaulting to Proxy.");
        SignatureType::Proxy
    },
    Some(n) => anyhow::bail!("Invalid signature_type: {}", n),
};
```

**`match`** with `Some(n)` -- Pattern matching on `Option<u8>`. `Some(1)` matches when the value is `Some` containing `1`. `Some(n)` with a variable captures any other `Some` value.

**`Some(0) | None`** -- The `|` means "or." Matches either `Some(0)` or `None`.

### HMAC Signature Generation (line 145-172)

```rust
fn generate_signature(&self, method: &str, path: &str, body: &str, timestamp: u64) -> Result<String> {
    let message = format!("{}{}{}{}", method, path, body, timestamp);

    let secret_bytes = match base64::decode(secret) {
        Ok(bytes) => bytes,
        Err(_) => secret.as_bytes().to_vec(),
    };

    let mut mac = HmacSha256::new_from_slice(&secret_bytes)?;
    mac.update(message.as_bytes());
    let result = mac.finalize();
    let signature = hex::encode(result.into_bytes());
    Ok(signature)
}
```

**How API authentication works:**
1. Concatenate the HTTP method + path + body + timestamp into one string
2. Sign that string using your API secret with HMAC-SHA256
3. Send the signature in the request headers

This proves to Polymarket that:
- You know the API secret (without sending it)
- The request wasn't tampered with
- The request is recent (timestamp prevents replay attacks)

### Placing Orders (line 376-501)

This is the most critical function. Let me highlight the key parts:

```rust
pub async fn place_order(&self, order: &OrderRequest) -> Result<OrderResponse> {
    // 1. Create signer from private key
    let signer = LocalSigner::from_str(private_key)?;

    // 2. Authenticate with CLOB client
    let client = ClobClient::new(&self.clob_url, ClobConfig::default())?
        .authentication_builder(&signer)
        .authenticate().await?;

    // 3. Parse order parameters
    let side = match order.side.as_str() {
        "BUY" => Side::Buy,
        "SELL" => Side::Sell,
        _ => anyhow::bail!("Invalid side"),
    };

    // 4. Build and sign the order
    let order_builder = client
        .limit_order()
        .token_id(token_id_u256)
        .size(size)
        .price(price)
        .side(side);

    let signed_order = client.sign(&signer, order_builder.build().await?).await?;

    // 5. Post to exchange
    let response = client.post_order(signed_order).await?;
```

**Why sign orders?** On a decentralized exchange, your orders are cryptographic commitments. The signature proves you authorized the trade without revealing your private key.

**`U256`** -- A 256-bit unsigned integer. Ethereum uses these for token IDs (they're massive numbers). `U256::from_str_radix("abc123", 16)` parses a hex string into a `U256`.

### Token Redemption (line 845-1135)

This is the most complex function -- 290 lines! It handles cashing out winning positions on the blockchain.

**Three paths depending on wallet type:**

#### Path 1: Gnosis Safe (signature_type = 2) -- line 915-1028
Most complex. A Gnosis Safe is a multi-signature wallet (smart contract).

```rust
// 1) Get Safe nonce (transaction counter)
// 2) Get transaction hash from Safe.getTransactionHash(...)
// 3) Sign the hash with EIP-191 personal sign
// 4) Encode execTransaction(...) with the signature
```

**EIP-191** is an Ethereum signing standard. The bot:
1. Gets the nonce (prevents replay attacks)
2. Asks the Safe what hash to sign
3. Signs with `\x19Ethereum Signed Message:\n32` + hash
4. Adjusts `v` value (+4 for Safe compatibility)
5. Encodes the full `execTransaction` call

#### Path 2: Proxy Wallet (signature_type = 1) -- line 1029-1063

```rust
// Execute via Proxy Wallet Factory
// factory.proxy([(typeCode=1, to=CTF, value=0, data=redeemCalldata)])
```

Simpler than Safe. Just wraps the redemption call in a proxy call.

#### Path 3: EOA (signature_type = 0) -- line 1064-1067

```rust
(ctf_address, redeem_calldata, 300_000, false)
```

Simplest. Sends the transaction directly from your private key to the CTF contract.

### Chainlink Price Fetcher (line 1138-1199)

```rust
pub async fn get_chainlink_btc_price_usd(
    client: &Client,
    rpc_url: &str,
    proxy_address: &str,
) -> Result<(f64, u64), String> {
```

**`Result<(f64, u64), String>`** -- Returns either a tuple of (price, timestamp) or an error string.

This function calls Chainlink's `latestRoundData()` directly via an Ethereum RPC call. Chainlink is a decentralized oracle that provides real-world price data on-chain.

**The response decoding (line 1188-1198):**

```rust
let raw = hex::decode(hex_result)?;
let answer_slice = raw.get(32..64)?;  // Second 32-byte word is the answer
let answer = i128::from_be_bytes(answer_slice[16..32].try_into()?);
let price = (answer as f64) / 100_000_000.0;  // Chainlink uses 8 decimal places
```

Chainlink's `latestRoundData()` returns 5 values packed in 160 bytes (5 x 32 bytes):
1. Round ID
2. **Answer (price)** <-- This is what we want
3. Started at timestamp
4. **Updated at timestamp** <-- And this
5. Answered in round

The price is in 8-decimal format: `9750000000000` = `$97,500.00`.

---

## 11. strategy.rs - The Trading Brain

**File:** `src/strategy.rs`
**Purpose:** The main trading logic. Orchestrates everything: finds markets, gets prices, monitors live data, places orders, and handles the lifecycle of each 5-minute trading round.

### Module Comment (line 1-2)
```rust
//! 5m pre-order trading bot: BTC, ETH, SOL, XRP. Monitor live prices, revert view, and optionally place pre-orders.
//! Price-to-beat from Polymarket RTDS Chainlink (crypto_prices_chainlink) per symbol.
```

### `price_key` Helper (line 23-25)
```rust
fn price_key(p: Option<f64>) -> u32 {
    p.map(|v| (v * 100.0).round().max(0.0) as u32).unwrap_or(0)
}
```

Converts a price like `0.6543` to `65` (cents). Used to avoid logging the same price over and over when it only changes by tiny fractions.

**`.map(|v| ...)`** on `Option` -- Applies a function to the inner value if it's `Some`, returns `None` if it's `None`.

### `ArbStrategy` Struct (line 27-33)

```rust
pub struct ArbStrategy {
    api: Arc<PolymarketApi>,
    config: Config,
    discovery: MarketDiscovery,
    price_cache_5: PriceCacheMulti,  // Shared cache of price-to-beat values
}
```

### Wait for Market & Price (line 47-81)

```rust
async fn wait_for_5m_market_and_price(&self, symbol: &str)
    -> Result<(String, String, String, i64, f64)>
{
    loop {
        // 1. Get current 5m period start timestamp
        let period_5 = current_5m_period_start();

        // 2. Try to find the market
        let (m5_cid, _) = match self.discovery.get_5m_market(symbol, period_5).await? {
            Some((cid, _)) => (cid, ()),
            None => {
                warn!("5m {} market not found. Sleeping 10s.", symbol);
                sleep(Duration::from_secs(10)).await;
                continue;
            }
        };

        // 3. Try to get the price-to-beat from RTDS cache
        let price_to_beat = {
            let cache = self.price_cache_5.read().await;
            cache.get(symbol).and_then(|per_period| per_period.get(&period_5).copied())
        };

        // 4. If no price yet, wait and retry
        let price_to_beat = match price_to_beat {
            Some(p) => p,
            None => {
                info!("Waiting for price-to-beat from RTDS Chainlink.");
                sleep(Duration::from_secs(10)).await;
                continue;
            }
        };

        // 5. Get token IDs (Up and Down)
        let (m5_up, m5_down) = self.discovery.get_market_tokens(&m5_cid).await?;

        return Ok((m5_cid, m5_up, m5_down, period_5, price_to_beat));
    }
}
```

**Return type `(String, String, String, i64, f64)`** -- Five values:
1. Condition ID
2. Up token ID
3. Down token ID
4. Period start timestamp
5. Price-to-beat

**The curly braces around the cache read:**
```rust
let price_to_beat = {
    let cache = self.price_cache_5.read().await;
    cache.get(symbol).and_then(|per_period| per_period.get(&period_5).copied())
};
```
This is a **block expression**. The `cache` lock is acquired inside the block and automatically released when the block ends (when `cache` goes out of scope). This minimizes how long the lock is held.

### Pre-Order Side Logic (line 84-92)

```rust
fn pre_order_side(&self, market_favors_up: bool) -> &'static str {
    let side = self.config.strategy.pre_order_side.to_lowercase();
    match side.as_str() {
        "up" => "up",
        "down" => "down",
        "favor" => if market_favors_up { "up" } else { "down" },
        _ => if market_favors_up { "up" } else { "down" },
    }
}
```

**`&'static str`** -- A string reference that lives forever. String literals like `"up"` are compiled into the binary and never freed, so they have the `'static` lifetime.

**"favor" strategy:** If the Up token's ask price is lower than Down's ask price, the market thinks "Up" is more likely. Buy what the market favors.

### Running a 5-Minute Round (line 95-183)

This is the core trading loop for one period:

```rust
async fn run_5m_round(&self, symbol: &str, _m5_cid: &str,
    m5_up: &str, m5_down: &str, period_5: i64, price_to_beat: f64,
) -> Result<()> {
    // 1. Set up shared price state
    let prices: PricesSnapshot = Arc::new(RwLock::new(HashMap::new()));

    // 2. Start WebSocket in background
    let ws_handle = tokio::spawn(async move {
        run_market_ws(&ws_url, asset_ids, prices_clone).await
    });

    // 3. Main monitoring loop
    let mut pre_order_placed = false;
    loop {
        // Exit if period is over
        if now >= period_5 + MARKET_5M_DURATION_SECS { break; }

        // Read current prices
        let snap = prices.read().await;
        let ask_up = snap.get(m5_up).and_then(|p| p.ask);
        let ask_down = snap.get(m5_down).and_then(|p| p.ask);

        // Log revert view
        let market_favors_up = ask_up < ask_down;
        info!("BTC Revert view: market favors Up | could revert to Down if BTC drops below $97,500");

        // Place pre-order (once per period)
        if pre_order_enabled && !pre_order_placed {
            let side = self.pre_order_side(market_favors_up);
            // ... build and place order ...
            pre_order_placed = true;
        }

        sleep(Duration::from_millis(100)).await;  // Poll every 100ms
    }

    // 4. Clean up
    ws_handle.abort();
    Ok(())
}
```

**`tokio::spawn`** -- Runs the WebSocket connection in a separate task. The main loop reads from the shared `prices` state that the WebSocket writes to.

**`ws_handle.abort()`** -- Forcefully stops the WebSocket task when the period is over.

**The "revert view"** concept: If the market currently favors "Up" (Up is cheaper), the bot shows what would cause a "revert" -- if the price drops below the price-to-beat, the "Up" token would lose value and "Down" would win.

### Pre-Order Placement (line 146-176)

```rust
if let Some(ask) = best_ask {
    let ticks = self.config.strategy.pre_order_improve_ticks as f64 * 0.01;
    let price = (ask - ticks).max(0.01);  // Improve by X ticks, minimum 1 cent
    let order = OrderRequest {
        token_id: token_id.to_string(),
        side: "BUY".to_string(),
        size,
        price: format!("{:.4}", price),
        order_type: "GTC".to_string(),
    };
    drop(snap);  // Release the read lock before making the API call!
    match self.api.place_order(&order).await {
        Ok(_) => { pre_order_placed = true; }
        Err(e) => { warn!("Pre-order failed: {}", e); }
    }
}
```

**`drop(snap)`** -- Manually releases the read lock. This is important! If we don't drop it, the lock would be held during the entire API call, blocking the WebSocket from writing new prices.

**`format!("{:.4}", price)`** -- Formats the price with 4 decimal places (e.g., `"0.6200"`).

**`"GTC"`** = Good Till Cancel. The order stays open until someone fills it or it's explicitly canceled.

### Polling for Resolution (line 186-226)

After the 5-minute period ends, the bot checks if the market has resolved:

```rust
async fn poll_until_5m_resolved(&self, symbol: &str, _m5_cid: &str) -> Option<String> {
    sleep(Duration::from_secs(60)).await;  // Initial wait: 1 minute
    loop {
        if started.elapsed().as_secs() >= 600 { return None; }  // Timeout: 10 minutes
        let m = self.api.get_market(_m5_cid).await?;
        let winner = m.tokens.iter()
            .find(|t| t.winner)  // Find the winning token
            .map(|t| if t.outcome.contains("UP") { "Up" } else { "Down" });
        if m.closed && winner.is_some() {
            return winner;
        }
        sleep(Duration::from_secs(45)).await;  // Check every 45 seconds
    }
}
```

**`.find(|t| t.winner)`** -- Iterator method that returns the first token where `winner == true`.

### Per-Symbol Loop (line 229-254)

```rust
async fn run_symbol_loop(api, config, price_cache_5, symbol) -> Result<()> {
    loop {
        // 1. Wait for market + price
        let (m5_cid, m5_up, m5_down, period_5, price_to_beat) =
            strategy.wait_for_5m_market_and_price(&symbol).await?;

        // 2. Trade during the period
        strategy.run_5m_round(&symbol, ...).await;

        // 3. Wait for resolution
        strategy.poll_until_5m_resolved(&symbol, &m5_cid).await;

        // 4. Brief pause, then start next period
        sleep(Duration::from_secs(5)).await;
    }
}
```

This is the per-symbol lifecycle. It runs forever, handling one 5-minute period at a time.

### Main `run` Method (line 256-289)

```rust
pub async fn run(&self) -> Result<()> {
    // 1. Start RTDS Chainlink poller (price-to-beat for all symbols)
    run_chainlink_multi_poller(rtds_url, symbols_rtds, cache_15, cache_5).await?;
    sleep(Duration::from_secs(2)).await;

    // 2. Spawn one task per symbol
    let mut handles = Vec::new();
    for symbol in symbols.clone() {
        handles.push(tokio::spawn(async move {
            Self::run_symbol_loop(api, config, price_cache_5, symbol).await
        }));
    }

    // 3. Wait for all tasks (runs forever)
    futures_util::future::try_join_all(handles).await?;
    Ok(())
}
```

**`try_join_all(handles)`** -- Waits for ALL spawned tasks to complete. Since each task loops forever, this effectively blocks forever.

**Concurrency model:** The bot runs:
- 1 RTDS Chainlink task (shared across all symbols)
- 1 task per symbol (BTC, ETH, SOL, XRP = 4 tasks)
- Each symbol task spawns a WebSocket task per 5m round

Total: ~6 concurrent async tasks during active trading.

---

## 12. ecosystem.config.js - Deployment

**File:** `ecosystem.config.js`
**Purpose:** PM2 process manager configuration for running the bot in production (on a server).

```javascript
module.exports = {
    apps: [{
        name: 'polymarket-bot',
        script: './target/release/polymarket-arbitrage-bot',  // The compiled binary
        cwd: '/root/rust-pro/polymarket-trading-bot-btc-5m-market',
        instances: 1,         // Only one instance
        autorestart: true,    // Restart if it crashes
        watch: false,         // Don't restart on file changes
        max_memory_restart: '1G',  // Restart if using > 1GB RAM
        env: {
            RUST_LOG: 'info'  // Logging level
        },
        error_file: './logs/pm2-error.log',
        out_file: './logs/pm2-out.log'
    }]
};
```

**PM2** is a Node.js process manager. You use it like:
```bash
pm2 start ecosystem.config.js
pm2 logs polymarket-bot
pm2 restart polymarket-bot
```

---

## 13. How It All Fits Together

### Startup Sequence

```
1. main.rs: Parse CLI args, load config.json
2. main.rs: Create PolymarketApi client
3. main.rs: Authenticate with Polymarket (if private key present)
4. main.rs: Create ArbStrategy, call strategy.run()
5. strategy.rs run(): Start RTDS Chainlink poller (→ chainlink.rs → rtds.rs)
6. strategy.rs run(): Wait 2s for initial price data
7. strategy.rs run(): Spawn one task per symbol (btc, eth, sol, xrp)
```

### Per-Symbol Trading Cycle (repeats every 5 minutes)

```
1. wait_for_5m_market_and_price():
   a. Calculate current 5m period start (discovery.rs)
   b. Fetch market by slug from Gamma API (api.rs)
   c. Check RTDS price cache for price-to-beat (rtds.rs wrote it)
   d. Get Up/Down token IDs from CLOB API (api.rs)

2. run_5m_round():
   a. Spawn WebSocket for live bid/ask (ws.rs)
   b. Every 100ms:
      - Read current prices from shared state
      - Log "revert view" (which side market favors)
      - If pre-orders enabled and not placed yet:
        → Place limit buy order on favored side (api.rs)
   c. When period ends, kill WebSocket

3. poll_until_5m_resolved():
   a. Wait 60s
   b. Poll CLOB API every 45s to check if market resolved
   c. Log winning outcome (Up or Down)
   d. Timeout after 10 minutes

4. Sleep 5s, then go back to step 1
```

### Data Flow

```
RTDS WebSocket (Chainlink)
    ↓ price-to-beat
    ↓ stored in PriceCacheMulti
    ↓
Strategy reads cache → knows threshold price
    ↓
Market WebSocket (CLOB)
    ↓ live bid/ask for Up/Down tokens
    ↓ stored in PricesSnapshot
    ↓
Strategy reads snapshot → decides order side
    ↓
Place limit order via SDK → Polymarket CLOB API
```

### Key Rust Concepts Used Throughout

| Concept | What it does | Where used |
|---------|-------------|------------|
| `Arc<T>` | Shared ownership across tasks | API client, price caches |
| `RwLock<T>` | Multiple readers, one writer | Price snapshots |
| `Mutex<T>` | Exclusive access | Authentication flag |
| `Option<T>` | Nullable values | Config fields, prices |
| `Result<T, E>` | Error handling | Every async function |
| `?` operator | Error propagation | Everywhere |
| `match` | Pattern matching | Side selection, message types |
| `tokio::spawn` | Background async tasks | WebSocket, per-symbol loops |
| `tokio::select!` | Wait for multiple futures | RTDS ping + message |
| `async/await` | Asynchronous operations | All I/O operations |
| `#[derive(...)]` | Auto-generate trait impls | Structs (Debug, Clone, etc.) |
| `serde` | JSON serialization | All API data types |
| Closures `\|x\| ...` | Anonymous functions | Iterators, spawned tasks |
| Lifetimes `'a` | Borrow duration tracking | Custom deserializers |

---

## Recommended Reading Order

If you're going through the code yourself, read in this order:

1. **`Cargo.toml`** -- Understand what libraries are available
2. **`models.rs`** -- Learn the data types (the "vocabulary")
3. **`config.rs`** -- Understand what the bot is configured with
4. **`main.rs`** -- See the high-level flow
5. **`discovery.rs`** -- How markets are found (pure logic, easy to follow)
6. **`ws.rs`** -- WebSocket basics, shared state pattern
7. **`rtds.rs`** -- Similar to ws.rs but with capture window logic
8. **`chainlink.rs`** -- Simple glue code
9. **`strategy.rs`** -- The trading logic (uses everything above)
10. **`api.rs`** -- Deep dive into API calls and blockchain interactions (hardest, save for last)

---

*Generated for learning purposes. This document covers every source file in the project.*
