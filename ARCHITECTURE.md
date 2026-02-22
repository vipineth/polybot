# Architecture

## Core Idea

```
[Data Sources]          [Strategies]              [Order Executor]
  PriceFeed   ──┐
  Orderbook   ──┼──▶  SweepArb        ──┐
  Discovery   ──┘     FutureStrategy   ──┼──▶  OrderExecutor  ──▶  Polymarket
                      FutureStrategy   ──┘
```

## Data Sources (lego blocks, build once)
- `PriceFeed` — RTDS WebSocket, Chainlink fallback
- `Orderbook` — WebSocket book mirror
- `Discovery` — market finding, period alignment

## Strategy Layer (where iteration happens)
- Each strategy declares what data it needs
- Accepts a `RoundContext` (prices, orderbook, market info)
- Outputs `Vec<OrderIntent>` — what it wants to trade
- Does NOT execute orders, does NOT manage timing

```rust
trait Strategy {
    fn name(&self) -> &str;
    async fn evaluate(&self, ctx: &RoundContext) -> Vec<OrderIntent>;
}
```

## Order Executor (safety gate)
- Receives `OrderIntent` from any strategy
- Validates: budget, position limits, sanity checks
- Executes: FOK orders via CLOB API
- Reports: `ExecutionResult` back

```rust
struct OrderIntent {
    token_id: String,
    side: Side,
    price: f64,
    size: f64,
    order_type: OrderType,  // FOK, GTC, etc.
    strategy: String,       // who requested this
    reason: String,         // why
}

struct ExecutionResult {
    intent: OrderIntent,
    status: FillStatus,     // Filled, Rejected, NetworkError
    filled_size: f64,
    filled_price: f64,
    order_id: Option<String>,
}
```

## Orchestrator (timing loop)
- Manages the lifecycle: discover → subscribe → wait → evaluate → execute
- Calls strategies at the right time
- Feeds data sources into strategy context
- Separate from strategy logic

## Refactor Order
1. Build OrderExecutor (standalone, no dependency on current code)
2. Define Strategy trait + OrderIntent/ExecutionResult
3. Extract current sweep logic into SweepArb strategy
4. Split api.rs (clob vs gamma vs redeem)
5. Extract orchestrator from ArbStrategy::run()
