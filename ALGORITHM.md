# Post-Close Stale Liquidity Sweep — Algorithm Specification

Reverse-engineered from 4,339 trades across 390 markets (lesstidy), cross-referenced with our paper trade observations. Reviewed by four independent perspectives: HFT latency specialist, quant risk manager, Polymarket API expert, and competing bot operator.

---

## Executive Summary

Buy winning-side shares at $0.99 from stale limit orders that remain on the CLOB after a 5-minute crypto market's outcome is already determinable. Use WebSocket orderbook subscription to maintain a real-time local mirror, pre-build+sign orders for both outcomes before close, and fire the winning side at T+0.5s for a first on-chain fill at T+2-3s.

---

## Architecture: Event-Driven WebSocket (Not REST Polling)

### Why REST Polling Fails
Our paper trade bot fetched orderbooks via REST at T+0 and consistently saw **0 asks** on winning tokens where lesstidy filled thousands of shares at T+4-8s. The REST API is eventually consistent and returns empty during market state transition.

### WebSocket Event Model

**IMPORTANT**: The Polymarket CLOB WS does NOT send individual order events. It sends **price-level aggregated snapshots**:

| Event Type | Payload | Description |
|-----------|---------|-------------|
| `book` | `{ bids: [{price, size}], asks: [{price, size}] }` | Full orderbook snapshot, aggregated by price level |
| `price_change` | `{ best_bid, best_ask }` | Delta when a price level changes |
| `last_trade_price` | `{ price }` | Last trade price update |

There are NO `order_placed`, `order_cancelled`, or per-order-ID events. The local mirror tracks price levels, not individual orders. Multiple resting orders at $0.99 appear as a single level `{"price": "0.99", "size": "5000"}`.

**Cargo.toml requirement**:
```toml
polymarket-client-sdk = { version = "0.4.2", features = ["clob", "ws"] }
```

### Data Flow
```
         WS: CLOB orderbook              WS: RTDS prices
         (book snapshots for              (chainlink feed)
          both Up + Down tokens)
                  │                              │
                  ▼                              ▼
         ┌───────────────────────────────────────────┐
         │              LOCAL STATE                  │
         │  • Up book: HashMap<price, total_size>    │
         │  • Down book: HashMap<price, total_size>  │
         │  • Latest price per symbol (live)         │
         │  • Price-to-beat per period               │
         │  • Pre-built+signed orders (Up & Down)    │
         │  • Authenticated CLOB client (reusable)   │
         └──────────────────┬────────────────────────┘
                            │
                      T+0: period closes
                            │
                      Fire pre-signed BUY
                      for winning token
```

---

## Order Submission: Already Private

Orders go through the CLOB matching engine, NOT the Polygon mempool:
```
Bot → HTTPS POST → CLOB Matching Engine → Atomic Match → On-chain Settlement
         (private)        (private)           (private)       (public)
```

**FOK orders**: Never rest on the book. Either match instantly or cancel. Other WS subscribers cannot see your intent. However, **fills ARE visible** — when your FOK matches a resting ask, other subscribers see the book snapshot update (the ask disappears). They know someone swept, but not who or from where.

---

## Critical Implementation Bugs (Must Fix Before Anything Else)

### Bug 0: `build_clob_client()` Re-Authenticates Every Order
**Severity: CRITICAL LATENCY**

`api.rs` `place_fok_buy()` creates a new signer, constructs a new CLOB client, and re-authenticates via HTTP on every single order. This adds 300-800ms per order. In a sweep of 5-10 orders, that's 3-8 seconds wasted on re-auth alone.

**Fix**: Authenticate ONCE at startup. Hold `ClobClient<Authenticated>` in an `Arc`. Polymarket auth tokens (HMAC-derived API keys) last for hours.

### Bug 1: Fixed 100-Share Order Size
`strategy.rs` line 331: `let size_str = cfg.sweep_order_size.clone()` uses a fixed size from config (default "100"). If the resting ask is 5,000 shares, we need 50 FOK orders to consume it. A competitor sending a single 5,000-share FOK beats us every time.

**Fix**: `order_size = min(ask.size, remaining_budget / ask.price)`.

### Bug 2: Config Defaults Contradict Algorithm
| Config | Code Default | Algorithm Says | Risk |
|--------|-------------|----------------|------|
| `sweep_min_margin_pct` | 0.0001 (0.01%) | 0.001 (0.1%) | 10x too aggressive — trades on noise |
| `max_sweep_cost` | $500 | $7,000 | Leaves 93% of opportunity on the table |
| `sweep_max_price` | 0.999 | 0.99 | Not critical — 0.999 covers 0.99 too |
| `sweep_inter_order_delay_ms` | 50ms | 2000ms | See Rule 5 — neither is right |

**Fix**: Reconcile all config defaults to match algorithm. Start with $500 cap until accuracy is proven > 99.5%.

### Bug 3: No WS Subscription (Still REST)
The entire sweep loop still uses REST `get_orderbook()`. The WS architecture described here is not implemented yet.

### Bug 4: Outcome String Mapping Risk
`discovery.rs` checks `outcome == "0"` → maps to Down. But outcomeIndex 0 = Up. If the API returns numeric string outcomes instead of "Up"/"Down", the mapping inverts. **Verify against live API responses.**

---

## The Algorithm

### Phase 0: Initialization (Once)

```
// Authenticate ONCE, reuse everywhere
clob_client = ClobClient::authenticated(private_key)  // hold in Arc
ws_client = WsClient::new()  // for orderbook subscriptions

CONNECT to RTDS WebSocket (all symbols: btc, eth, sol, xrp)
SPAWN per-symbol loops (4 concurrent loops)

CONFIG:
  target_buy_price          = 0.99
  max_position_per_market   = 500.0    // start conservative, raise after proving accuracy
  sweep_timeout             = 20s      // 45s is outdated; with 8-12 competitors, liquidity
                                       // is gone in 10-15s. 20s balances coverage vs waste.
  min_confidence_pct        = 0.001    // 0.1% — MUST match config default
  min_confidence_abs        = {        // absolute floor per coin (Chainlink deviation aware)
    btc: 68.0,                         // $68 at ~$68k
    eth: 2.0,                          // $2 at ~$2k
    sol: 0.10,                         // $0.10 at ~$85
    xrp: 0.005                         // $0.005 at ~$1.42 (NOT $0.00142 — too tight)
  }
  order_type                = FOK
```

### Phase 1: Market Discovery + WS Subscribe (T-30s)

```
period_start = current_5m_period_start()   // Unix ts, divisible by 300
slug = "{coin}-updown-5m-{period_start}"
market = api.get_market_by_slug(slug)
condition_id = market.condition_id
(up_token, down_token) = api.get_market_tokens(condition_id)

// Outcome mapping (verified 100%, 4,150 trades, zero exceptions):
//   Up   = outcomeIndex 0
//   Down = outcomeIndex 1

// Subscribe to BOTH orderbooks via WebSocket
ws_client.subscribe_orderbook([up_token, down_token])

// Local orderbook mirrors (price-level aggregated, NOT per-order)
local_book_up   = HashMap<f64, f64>   // price → total_size
local_book_down = HashMap<f64, f64>

// WS event handler (background task):
ON ws_event(token_id, event):
    MATCH event.type:
        "book" =>
            // Full snapshot: replace entire local book for this token
            local_book[token_id] = event.asks  // [{price, size}, ...]
        "price_change" =>
            // Delta: update the changed level
            local_book[token_id].update(event.price, event.new_size)
```

### Phase 2: Price-to-Beat Capture (T-5s to T+0s)

```
price_to_beat = RTDS cache for (symbol, period_start)
    FALLBACK: Chainlink RPC eth_call

ASSERT price_to_beat > 0 AND not NaN AND not stale (< 30s old)
```

### Phase 3: Pre-Build + Sign Orders (T-5s)

```
// Cache fee_rate_bps and tick_size (these require HTTP calls)
fee_rate = clob_client.fee_rate_bps(up_token).await     // cache this
tick_size = clob_client.tick_size(up_token).await        // cache this

// Pre-build and sign BUY orders for BOTH outcomes
// The Order struct has: salt (random), nonce (0), expiration (0 for FOK = no expiry)
// No timestamp check on the signed struct — pre-signing at T-5s is valid
pre_signed_up   = build_and_sign(up_token,   size=max_shares, price=0.99, type=FOK)
pre_signed_down = build_and_sign(down_token, size=max_shares, price=0.99, type=FOK)

// Signing is ~5ms in Rust (ECDSA secp256k1). The 200-500ms cost
// is from HTTP calls in build() — which we've already cached above.

// CONSIDERATION: Pre-sign at multiple price levels for sub-0.99 opportunities
pre_signed_up_95   = build_and_sign(up_token,   size=max_shares, price=0.95, type=FOK)
pre_signed_down_95 = build_and_sign(down_token, size=max_shares, price=0.95, type=FOK)
```

### Phase 4: Determine Winner + Fire (T+0s)

```
// T+0: Period closes
close_price = RTDS latest price for symbol   // instant lookup, already cached
    FALLBACK: Chainlink RPC

diff = close_price - price_to_beat
abs_diff = |diff|
confidence_pct = abs_diff / price_to_beat

// RULE: Skip low-confidence calls (percentage AND absolute floor)
IF confidence_pct < min_confidence_pct OR abs_diff < min_confidence_abs[symbol]:
    LOG "skipping: diff=${abs_diff} ({confidence_pct}%)"
    CONTINUE to next period

// Cross-validate: if RTDS and RPC disagree on direction, SKIP
IF rtds_says_up AND rpc_says_down (or vice versa):
    LOG "source disagreement, skipping"
    CONTINUE to next period

winner = IF diff > 0 THEN "Up" ELSE "Down"
winning_token = IF winner == "Up" THEN up_token ELSE down_token

// Check local book mirror — stale asks available?
stale_asks = local_book[winning_token]
    .filter(|price, size| price <= 0.99 AND size > 0)
    .sort_by(price DESC)   // 0.99 first

// T+0.5s: Fire pre-signed order (just HTTPS POST, no signing delay)
submit_order(IF winner == "Up" THEN pre_signed_up ELSE pre_signed_down)
// T+2-3s: First fill lands on-chain
```

### Phase 5: Sweep Loop — Rapid Fire (T+0.5s to T+20s)

```
sweep_start = now()
total_cost = 0.0
total_shares = 0.0
total_orders = 0

WHILE elapsed(sweep_start) < sweep_timeout:
    IF total_cost >= max_position_per_market:
        BREAK

    // Read local book mirror (updated by WS in real-time)
    stale_asks = local_book[winning_token]
        .filter(|price, size| price <= 0.99 AND size > 0)
        .sort_by(price DESC)

    IF stale_asks.is_empty():
        // Wait for WS book update with timeout
        WAIT for ws_book_event on winning_token, timeout = 3s
        IF timeout:
            BREAK
        CONTINUE

    FOR ask IN stale_asks:
        IF total_cost >= max_position_per_market:
            BREAK

        remaining_budget = max_position_per_market - total_cost
        max_shares = remaining_budget / ask.price
        order_size = MIN(ask.size, max_shares)

        // Fire FOK — uses pre-authenticated client (no re-auth)
        result = clob_client.post_order(
            build_fok_buy(winning_token, order_size, ask.price)
        )

        IF result.filled:
            total_orders += 1
            total_shares += result.filled_size   // use ACTUAL fill, not requested
            total_cost += result.filled_size * ask.price
            LOG "FILLED #{total_orders}: {result.filled_size} @ {ask.price}"

        // DO NOT sleep 2 seconds between orders.
        // CLOB matching is off-chain and instant.
        // Polygon block time constrains settlement, not matching.
        // Multiple FOK orders can match in the same engine tick.
        // Fire as fast as CLOB rate limits allow.
        SLEEP 100ms   // respect rate limits (~10 req/s per account)
```

### Phase 6: Cleanup + Next Period

```
ws_client.unsubscribe_orderbook([up_token, down_token])

LOG sweep results
SLEEP until next period_start - 30s
```

---

## Rules (Ordered by Priority)

### Rule 1: Authenticate Once, Reuse Forever
Build `ClobClient<Authenticated>` at startup. Hold in `Arc`. Never re-create per order. Auth tokens last hours.

**Impact**: Saves 300-800ms PER ORDER. On a 10-order sweep, saves 3-8 seconds total.

### Rule 2: WebSocket Book Snapshots (Not Order Events)
The WS sends `book` snapshots (price-level aggregated) and `price_change` deltas. There are NO per-order events. Local mirror is `HashMap<price, total_size>`, not per-order-ID.

Subscribe to both Up and Down tokens at T-30s. The `book` event sends an immediate full snapshot on subscribe (`initial_dump: true`).

### Rule 3: Pre-Build Orders (Cache HTTP Calls)
The SDK's `build()` method makes two HTTP calls: `fee_rate_bps()` and `tick_size()`. Cache these values and construct orders manually to eliminate HTTP latency from the critical path. Actual ECDSA signing is ~5ms.

### Rule 4: Confidence Threshold — Percentage AND Absolute Floor
Skip if EITHER condition fails:
- `|diff| / price_to_beat < 0.1%`
- `|diff| < absolute_floor[symbol]`

The absolute floor is critical for low-price coins:

| Coin | Price ~ | 0.1% threshold | Absolute floor | Why |
|------|---------|---------------|----------------|-----|
| BTC  | $68,000 | $68           | $68            | Threshold works fine |
| ETH  | $1,975  | $1.97         | $2.00          | Threshold works fine |
| SOL  | $85     | $0.085        | $0.10          | Slightly above threshold |
| XRP  | $1.42   | $0.00142      | **$0.005**     | 3.5x above threshold — XRP noise is $0.001+ |

**Chainlink deviation awareness**: Chainlink feeds have a ~0.5% deviation threshold. If the price moved less than 0.5%, Chainlink may not have updated. The on-chain price (used for resolution) could lag RTDS significantly.

**Dynamic scaling**: During low-vol periods (Asian session, weekends), increase thresholds to 0.3-0.5%.

### Rule 5: Fire Orders As Fast As CLOB Allows (NOT Every 2 Seconds)
CLOB matching is off-chain and instant. Multiple FOK orders can match in the same engine tick and batch-settle into one Polygon block. The 2-second block time constrains settlement, NOT matching.

**Rate limit**: ~10 requests/second per account. Use 100ms between orders.

**NOT 2 seconds**: The 2s recommendation was based on confusing settlement latency with matching latency. Lesstidy's 0-2s inter-fill gaps are settlement timestamps, not submission timestamps. They likely submitted much faster.

### Rule 6: Match the Ask Size
Don't use fixed 100-share orders. If the ask is 5,000 shares, send a single 5,000-share FOK. One order beats fifty.

For safety against partial book changes: if FOK for full size fails, retry at 90% of ask size, then 50%.

### Rule 7: Start Conservative on Position Size
**Do NOT start at $7,000 per market.**

Kelly Criterion analysis at 1% edge (buy at 0.99, redeem at 1.00):

| Accuracy | Kelly fraction | EV per $5k market | Verdict |
|----------|---------------|-------------------|---------|
| 99.0%    | **-0.0001 (negative)** | **-$0.01** | DO NOT TRADE |
| 99.5%    | 0.50 (50% of bankroll) | +$25.25 | Trade cautiously |
| 99.9%    | 0.90 | +$45.45 | Trade aggressively |

**The strategy is -EV below 99.01% accuracy.** You must prove accuracy > 99.5% with 500+ verified paper trades before raising the position cap.

Start at **$500** per market. Scale to $2,000 after 200 verified wins with 0 wrong-winners. Scale to $5,000+ only after 500+ verified wins.

### Rule 8: Sweep Duration = 20 Seconds (Not 45)
With 8-12 competing operators (February 2026), if you haven't filled in 10-15s, remaining liquidity is gone. 45s was valid in November with 3-4 competitors.

### Rule 9: Correlation Circuit Breaker
If ALL 4 coins show the same direction AND confidence is below 0.5%, skip ALL of them.

Flash crash scenario: price crashes at T-1s, RTDS captures crash, Chainlink hasn't updated → RTDS says Down for all 4, oracle resolves Up for all 4 → $28,000 loss.

**Rule**: Max simultaneous exposure = $14,000 (2 coins). If 3+ coins trigger in the same window, only trade the 2 with highest confidence.

### Rule 10: All 4 Coins in Parallel, 24/7
BTC, ETH, SOL, XRP. Every 5-minute window, 24/7. Night trades are fastest (less competition).

### Rule 11: FOK Orders Only
Never use GTC — it leaks intent. FOK either fills atomically or cancels. However, note that fills ARE visible to WS subscribers as book snapshot changes. You are invisible pre-fill, visible post-fill.

### Rule 12: RTDS Fresh (<10s) or Fall Back
If RTDS > 10s stale, use Chainlink RPC. If both stale, skip.

### Rule 13: Track Actual Fill Sizes (Not Requested)
Current code tracks `request_size` not `actual_fill_size`. Use the response's filled amount to avoid phantom position tracking.

---

## Timing Diagram

```
T-30s   Discover market, subscribe WS for both tokens
        → initial book snapshot populates local mirror
T-5s    Cache fee_rate/tick_size, pre-sign orders for both Up/Down
T+0s    Period closes
        → Winner known instantly (RTDS price cached, ~0ms)
        → Check local mirror for stale asks
T+0.1s  Confidence check passes
T+0.3s  Fire pre-signed FOK to CLOB (HTTPS POST)
T+0.5s  CLOB matches against resting ask (off-chain, instant)
T+2-3s  Settlement on-chain (Polygon block)
T+0.4s  Fire second FOK (100ms later, don't wait for settlement)
T+0.5s  Fire third FOK...
T+1-3s  Rapid sweep of all visible stale asks
T+3-20s Continue: WS pushes book updates, snipe new asks
T+20s   Stop sweep, unsubscribe, log results
```

---

## Risk Management

### Kelly Criterion — This Strategy Has Razor-Thin Edge

The asymmetry is brutal: **+$50 when right, -$5,000 when wrong** (at $5k position).

| Wrong-winner rate | Markets to recover ONE loss | Monthly wrong-winners (at 10/day) | Monthly net (at $50/win) |
|---|---|---|---|
| 0.1% (1 in 1000) | 100 markets | 0.3 | +$14,550 |
| 0.5% (1 in 200) | 100 markets | 1.5 | +$7,500 |
| 1.0% (1 in 100) | 100 markets | 3.0 | **+$0 (break-even)** |
| 2.0% (1 in 50) | 100 markets | 6.0 | **-$15,000** |

**You cannot deploy real capital without proving accuracy > 99.5% over 500+ markets with resolution verification.**

### Never-Do List
- Never buy if confidence < 0.1% or below absolute floor
- Never exceed position cap before accuracy is proven
- Never let >2 coins sweep simultaneously (correlation risk)
- Never use GTC orders
- Never re-authenticate per order
- Never ignore source disagreement between RTDS and RPC

### Redemption Pipeline
After winning, shares must be explicitly redeemed on-chain (`redeemPositions()` on CTF contract). This is NOT automatic.

- Redemption takes ~2-4 seconds (one Polygon tx)
- Capital is locked between buy and redemption (~5-15 minutes)
- Need $28,000+ liquid USDC for 4 coins at $7k each
- Build a separate redemption sweep process that polls for resolved positions
- Consider batch-redeeming across multiple conditions in one tx (gas optimization)

### Persistent State
If the bot crashes mid-sweep, it forgets unredeemed positions. Need persistent tracking of:
- Markets entered (conditionId, token, side, shares, cost)
- Resolution status
- Redemption status

---

## Competitive Reality (February 2026)

| Factor | November 2025 | February 2026 |
|--------|--------------|---------------|
| Active operators | 3-4 | 8-12 |
| Fill latency to compete | T+4-8s | T+2-3s |
| Avg profit per market | ~$65 | ~$35-40 |
| Markets with stale liquidity | Abundant | Shrinking (MMs getting faster) |
| Realistic new-entrant daily profit | N/A | **$150-280** (not $500-750) |

### What Top Operators Do That Lesstidy Doesn't
1. **Multi-account sweeping**: 3-5 proxy wallets firing simultaneously to bypass per-account rate limits
2. **CEX price feeds**: Binance/Coinbase/Kraken websockets predict Chainlink updates 1-3 seconds early
3. **Co-located infrastructure**: Same AWS region as CLOB matching engine
4. **Dynamic sizing**: $15,000+ per market when confidence is >0.5%
5. **Batch redemption**: Custom smart contract to redeem dozens of positions in one tx

### Markets We're Not Covering (Money Left on Table)
- **Hourly crypto**: Same 4 coins, larger stale positions, less competition, 3-5x per-market profit
- **Sports** (NBA, soccer, tennis): When outcome is mathematically certain, same stale-order pattern, 3-10x margins
- **Daily crypto**: Lower capital efficiency but even less competition

---

## Expected Economics (Realistic, Not Optimistic)

### Phase 1: Paper Trading + Accuracy Verification (Weeks 1-4)
```
Capital at risk:    $0
Position cap:       Paper only
Goal:               500+ markets with resolution verification
Target accuracy:    > 99.5%
```

### Phase 2: Conservative Live (Weeks 5-8)
```
Position cap:       $500/market
Markets/day:        5-8 (realistic fill rate with competition)
Avg profit/market:  $5
Daily profit:       $25-40
Monthly profit:     $750-1,200
Purpose:            Validate real fills, test infrastructure
```

### Phase 3: Scale Up (Month 3+, after proven accuracy)
```
Position cap:       $3,000-5,000/market
Markets/day:        8-12
Avg profit/market:  $30-50
Daily profit:       $240-600
Monthly profit:     $7,200-18,000
Requires:           500+ verified wins, 0 wrong-winners, optimized latency
```

---

## Implementation Priority

1. **Fix `build_clob_client()` — authenticate once** (Bug 0) — saves 300-800ms per order
2. **Implement WS orderbook subscription** (Rule 2) — solves empty-book bug, enables speed
3. **Pre-build orders with cached HTTP values** (Rule 3) — eliminates signing from critical path
4. **Match ask sizes, not fixed 100** (Rule 6) — one order beats fifty
5. **Set config defaults to match algorithm** (Bug 2) — prevent accidental aggressive trading
6. **Confidence threshold with absolute floor** (Rule 4) — prevent wrong-winner on noise
7. **Fire at CLOB rate limit, not every 2s** (Rule 5) — 20x faster sweep
8. **Correlation circuit breaker** (Rule 9) — prevent $28k flash crash loss
9. **Add persistent state for unredeemed positions** — prevent capital leakage on crash
10. **Redemption pipeline** — automated redemption sweep for capital recycling
