# Polymarket Trader Analysis: @lesstidy

## Wallet
`0xd4583c4704a8c2e416f0e7fa5b763f92f0291733`

---

## Strategy

**Post-close oracle latency arbitrage.** Lesstidy runs an automated bot that exploits stale order book liquidity across Polymarket's prediction markets. The mechanism:

1. A crypto time window closes (e.g., "BTC Up or Down 10:35AM-10:40AM ET") and the real-world outcome is already known
2. Polymarket's order book still has uncancelled limit orders at ~$0.99 for the winning outcome
3. The bot sweeps that stale liquidity — buying shares at $0.99 that resolve to $1.00
4. Profit = ~$0.01/share x thousands of shares per market

This same pattern applies across **every category** — crypto, sports, esports. In sports/esports markets, the bot buys the winning team's shares at $0.999 after the game result is known but before market resolution.

The strategy is not prediction — it is guaranteed-outcome arbitrage exploiting the delay between real-world results and order book cleanup.

---

## Key Stats

| Metric | Value |
|--------|-------|
| Markets traded | 428 |
| Individual orders | 4317 |
| Win rate | 99.8% (427/428) |
| Total volume | $1,543,281 |
| Total profit | $16,000.10 |
| ROI | 1.04% |
| Avg profit/market | $37.38 |

| Trading period | 2025-11-28 to 2026-02-21 |

---

## Category Breakdown

| Category | Markets | Volume | Profit | Win Rate |
|----------|---------|--------|--------|----------|
| Crypto 5-Min | 280 | $1,142,528 | $15,589.49 | 100.0% |
| Crypto Hourly | 103 | $247,647 | $257.37 | 99.0% |
| NBA | 21 | $75,876 | $75.93 | 100.0% |
| NFL | 9 | $28,539 | $28.57 | 100.0% |
| College Football | 4 | $14,501 | $14.51 | 100.0% |
| Soccer | 2 | $12,738 | $12.75 | 100.0% |
| Esports | 5 | $11,311 | $11.33 | 100.0% |
| NHL | 3 | $7,560 | $7.57 | 100.0% |
| UFC | 1 | $2,579 | $2.58 | 100.0% |

---

## Timing Analysis

| Delay After Close | Markets |
|---|---|
| 0-5s | 6 |
| 5-10s | 52 |
| 10-30s | 20 |
| 30-60s | 8 |
| 1-2min | 9 |
| 2-5min | 1 |
| 10min+ | 185 |

---

## Detected Patterns

- **post_close_trading**: Majority of entries occur after market window closes
- **high_win_rate**: Exceptionally high win rate of 99.8%
- **price_near_one**: Average buy price 0.9902 — buying near-certain outcomes
- **multi_order_sweeping**: Average 10.1 orders per market — aggressive liquidity sweeping

---

## Crypto Coin Breakdown

### 5-Min Markets (the profit engine — 97% of total profit)

| Coin | Markets | Profit | Avg Price | Avg Entry Speed | Up/Down |
|------|---------|--------|-----------|-----------------|---------|
| BTC | 172 | $10,411 | 0.9878 | 328s | 92/80 |
| ETH | 49 | $1,513 | 0.9897 | 530s | 24/25 |
| SOL | 33 | $730 | 0.9896 | 558s | 19/14 |
| XRP | 28 | $3,041 | 0.9771 | 594s | 13/15 |

BTC dominates in volume, but XRP has the highest profit-per-market due to finding deeper stale liquidity (avg price 0.977 vs 0.988 for BTC).

### Hourly Markets

| Coin | Markets | Profit | Avg Price | W/L |
|------|---------|--------|-----------|-----|
| BTC | 47 | $142 | 0.9889 | 46/1 |
| ETH | 18 | $49 | 0.9990 | 18/0 |
| SOL | 21 | $32 | 0.9990 | 21/0 |
| XRP | 17 | $34 | 0.9990 | 17/0 |

Hourly markets are far less profitable ($2.50/market vs $55.66 for 5-min) — likely less stale liquidity available.

---

## Golden Trades — Deeply Stale Liquidity

Occasionally the bot finds limit orders at prices far below $0.99, resulting in outsized returns:

| Market | Buy Price | Profit | Date |
|--------|-----------|--------|------|
| XRP Jan 25, 9:30AM | $0.757 | **$1,599** | 2026-01-25 |
| XRP Feb 16, 12:00PM | $0.877 | **$753** | 2026-02-16 |
| BTC Feb 21, 10:35AM | $0.921 | **$569** | 2026-02-21 |
| BTC Jan 25, 1:45AM | $0.932 | **$417** | 2026-01-25 |
| BTC Jan 13, 1:00AM | $0.922 | **$395** | 2026-01-13 |

These 5 trades alone account for **$3,734 (23% of total profit)**. The lower the stale order price, the more profit per share.

---

## Order Sweeping Analysis

The bot aggressively sweeps order books, placing many small orders to consume all available stale liquidity:

| Orders | Market | Cost | Profit |
|--------|--------|------|--------|
| 164 | BTC Feb 14, 3:00PM | $6,455 | $65 |
| 84 | BTC Feb 5, 1:30PM | $6,697 | $68 |
| 78 | BTC Feb 11, 3:15AM | $5,802 | $59 |
| 72 | BTC Feb 16, 10:20AM | $5,966 | $60 |
| 70 | BTC Feb 15, 9:55PM | $7,753 | $78 |

Average orders per 5-min crypto market: **13.0**. Max: **164 orders** in a single market.

---

## Sports & Esports Deep Dive

All sports/esports trades use an identical pattern: buy the winning outcome at **$0.999** after the result is known.

### NBA (21 markets, $76 profit)
Teams traded: 76ers (3), Hornets (3), Cavaliers (2), Magic (2), Timberwolves (2), and 10 others. Heaviest sweeps on Hawks vs. Pistons (17 ord), Cavaliers vs. Knicks (22 ord), Lakers vs. Raptors (19 ord).

### NFL (9 markets, $29 profit)
Teams: Texans (2), Steelers (2), Bills (2), Broncos, Saints, 49ers. Mostly single orders.

### College Football (4 markets, $15 profit)
Navy, Jacksonville State, Minnesota, Ole Miss — all single-order wins.

### NHL (3 markets, $8 profit)
Predators, Wild, Blues. Blackhawks vs. Blues had 11 orders.

### Soccer (2 markets, $13 profit)
Stade Rennais (bet No, won), Chelsea (bet Yes, won).

### UFC (1 market, $3 profit)
Dvalishvili vs. Yan — bet on Yan at $0.999.

### Esports (5 markets, $11 profit)
All Counter-Strike: Spirit (2), Passion UA, 3DMAX, TheMongolz.

---

## The One Loss

| Field | Value |
|-------|-------|
| Market | Bitcoin Up or Down on November 29? |
| Category | Crypto Hourly |
| Outcome bet | BTC **Up** |
| Result | BTC went **Down** |
| Buy prices | $0.48 and $0.57 (avg $0.525) |
| Cost (lost) | $10.50 |
| Date | 2025-11-28 |

This was lesstidy's **very first trade** — a speculative bet, not arbitrage. After losing $10.50, they never made another speculative trade. Every subsequent trade was the $0.99+ arb strategy. This loss is how the strategy was likely discovered.

---

## Bot vs. Human: Behavioral Evidence

**Verdict: Automated bot.**

| Signal | Evidence |
|--------|----------|
| 24/7 operation | Trades at every hour including 2AM, 4AM, 5AM |
| No day-of-week bias | 51-72 trades per weekday, weekends included |
| Order sweeping speed | Up to 164 orders per market |
| Fastest entry | 3 seconds after window close |
| Consistent pricing | $0.99 buy price with no variation in execution |

### Hourly Distribution
Activity is spread across all 24 hours (10-28 trades per hour), confirming automated 24/7 operation.

### Daily Distribution
| Day | Trades |
|-----|--------|
| Friday | 72 |
| Sunday | 71 |
| Wednesday | 62 |
| Saturday | 61 |
| Thursday | 59 |
| Monday | 54 |
| Tuesday | 51 |

---

## Scaling Trajectory

The bot has been aggressively ramping up position sizes and frequency:

| Period | Avg Trades/Day | Avg Profit/Day | Max Position |
|--------|---------------|----------------|--------------|
| Nov 2025 | 3-5 | $5-12 | ~$2,000 |
| Dec 2025 | 3-5 | $30-60 | ~$4,000 |
| Jan 2026 | 4-7 | $100-200 | ~$5,700 |
| Feb 2026 | 8-22 | $400-1,600 | ~$7,800 |

Peak day: **Feb 15 — 22 trades, $1,117 profit**. Largest single position: **$7,840** (BTC Feb 13).

---

## Structural Assessment

### Is an order book vulnerability being exploited?
**Yes.** Buy price of $0.99+ combined with post-close timing = buying shares whose outcome is already determined from stale, uncancelled limit orders.

### Which market types have this vulnerability?
**All of them.** Crypto 5-min, Crypto Hourly, NBA, NFL, NHL, CFB, Soccer, UFC, Esports — identical $0.999 pattern across every category.

### How much stale liquidity exists?
The bot hits ~3.3 crypto 5-min markets per day out of potentially hundreds of daily windows across BTC/ETH/SOL/XRP. **Current coverage is estimated at 5-10%** — significant untapped liquidity likely remains.

### Could a faster/broader bot compete?
**Yes.** Key gaps:
- **Timing**: 68% of entries are 10min+ after close — a faster bot could front-run on every window
- **Coverage**: Only ~3 of hundreds of daily windows are captured
- **Coin bias**: 61% BTC, underexploiting ETH/SOL/XRP
- **Sports/esports**: Minimal coverage, likely manually identified

### Risks
- Polymarket adds auto-cancellation of stale orders after window close (kills strategy instantly)
- Competing bots discover the same edge, compressing margins
- Oracle edge cases where on-chain resolution differs from expected
- Market makers stop placing limit orders on these markets

### Expected Lifespan
Strategy has survived 85+ days and is still scaling. The rapidly growing position sizes ($7,800/trade in Feb vs $2,000 in Nov) suggest the operator knows this is temporary and is maximizing extraction before the window closes.

---

## Bottom Line

Lesstidy is an automated post-close arbitrage bot exploiting stale order book liquidity across Polymarket. After real-world outcomes are known but before market resolution, it sweeps uncancelled limit orders at $0.99, capturing guaranteed $0.01/share profit at scale. $16,106 profit at 99.8% win rate over 85 days, with aggressive recent scaling. The single loss was a speculative bet from day one — before the arbitrage system existed.

---

## Data Sources

- Polymarket Data API: `https://data-api.polymarket.com/activity`
- Polymarket Gamma API: `https://gamma-api.polymarket.com/public-search`
- Generated by Polymarket Trader Analyzer
