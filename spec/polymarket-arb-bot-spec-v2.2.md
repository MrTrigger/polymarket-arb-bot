# Polymarket 15-Minute Crypto Arbitrage Bot

## Design & Implementation Specification

**Target Language:** Rust  
**Strategy:** Dutch Book / Pair Arbitrage with Timing Optimization  
**Markets:** BTC, ETH, SOL, XRP 15-minute Up/Down binary options  
**Version:** 2.2  
**Date:** January 2026

**v2.2 Refinements:**
- Added arbitrage opportunity visualization
- Optimized shadow bid signing with pre-hashing
- Granular inventory imbalance thresholds
- Asymmetric cross-asset correlation handling
- Improved Rust shared state architecture

---

## Table of Contents

1. [Executive Summary](#1-executive-summary)
2. [How The Strategy Works](#2-how-the-strategy-works)
3. [System Architecture](#3-system-architecture)
4. [Data Sources & APIs](#4-data-sources--apis)
5. [Market Discovery](#5-market-discovery)
6. [Price Monitoring & Toxic Flow Detection](#6-price-monitoring--toxic-flow-detection)
7. [Arbitrage Detection & Cross-Asset Signals](#7-arbitrage-detection--cross-asset-signals)
8. [Order Execution: Price Chasing & Shadow Bids](#8-order-execution-price-chasing--shadow-bids)
9. [Inventory Management](#9-inventory-management)
10. [Risk Management](#10-risk-management)
11. [Position Sizing](#11-position-sizing)
12. [Timing & Market Lifecycle](#12-timing--market-lifecycle)
13. [Error Handling & Recovery](#13-error-handling--recovery)
14. [Monitoring & Logging](#14-monitoring--logging)
15. [Configuration](#15-configuration)
16. [Rust Implementation Notes](#16-rust-implementation-notes)
17. [Testing Strategy](#17-testing-strategy)
18. [Deployment Considerations](#18-deployment-considerations)
19. [Phase 2: Market Scanner](#19-phase-2-market-scanner)

---

## 1. Executive Summary

This section provides a complete overview of what this bot does, why it works, and how we've designed it to succeed. After reading this summary, you should understand the strategy well enough to explain it to someone else.

---

### 1.1 The One-Sentence Explanation

**We buy both sides of a binary bet for less than the guaranteed payout, locking in risk-free profit regardless of the outcome.**

---

### 1.2 What This Bot Does

This bot generates consistent, low-risk profits by exploiting a mathematical guarantee in Polymarket's 15-minute crypto prediction markets.

**The Market Structure:**

Every 15 minutes, Polymarket runs binary markets asking questions like:
- "Will Bitcoin be up or down compared to 15 minutes ago?"
- "Will Ethereum be up or down compared to 15 minutes ago?"

You can buy two types of shares:
- **YES shares** — Pay $1.00 if the price goes UP, $0.00 if it goes DOWN
- **NO shares** — Pay $1.00 if the price goes DOWN, $0.00 if it goes UP

**The Mathematical Guarantee:**

Here's the key insight: **if you own one YES share and one NO share, you are guaranteed to receive exactly $1.00 regardless of the outcome.**

- If Bitcoin goes UP: Your YES pays $1.00, your NO pays $0.00. Total: $1.00
- If Bitcoin goes DOWN: Your YES pays $0.00, your NO pays $1.00. Total: $1.00

This means: **If you can buy a YES + NO pair for less than $1.00, you've locked in a guaranteed profit.**

---

### 1.3 A Concrete Example

Let's walk through a real trade to make this tangible:

**The Setup:**
```
Market: "Will Bitcoin be up in 15 minutes?"
Current prices:
  YES shares: $0.55 each (implied 55% chance BTC goes up)
  NO shares:  $0.43 each (implied 43% chance BTC goes down)
  Combined:   $0.98
```

**The Trade:**
```
Buy 100 YES shares @ $0.55 = $55.00
Buy 100 NO shares  @ $0.43 = $43.00
Total investment:           $98.00
```

**15 Minutes Later — Scenario A (Bitcoin went UP):**
```
100 YES shares pay $1.00 each = $100.00
100 NO shares pay $0.00 each  = $0.00
Total received:                $100.00
Profit: $100.00 - $98.00 =     $2.00 (2.04% return)
```

**15 Minutes Later — Scenario B (Bitcoin went DOWN):**
```
100 YES shares pay $0.00 each = $0.00
100 NO shares pay $1.00 each  = $100.00
Total received:                $100.00
Profit: $100.00 - $98.00 =     $2.00 (2.04% return)
```

**The Result:** We make $2.00 profit no matter what Bitcoin does. This is not speculation — it's arbitrage.

---

### 1.4 Visualizing the Opportunity: Retail Panic in Action

To understand when and why these opportunities appear, let's visualize how "implied probability" shifts during a period of retail panic.

**The Setup:** Bitcoin suddenly pumps 1.5% in 30 seconds. Retail traders rush to buy YES.

```
TIME        BTC SPOT    YES PRICE   NO PRICE    COMBINED    ARB MARGIN
─────────────────────────────────────────────────────────────────────────
14:00:00    $94,000     $0.50       $0.50       $1.00       0.0%  ← Equilibrium
14:00:15    $94,500     $0.58       $0.48       $1.06       -6.0% ← Overreaction!
14:00:30    $94,800     $0.62       $0.42       $1.04       -4.0% ← Still expensive
14:00:45    $95,000     $0.65       $0.38       $1.03       -3.0% ← Normalizing
14:01:00    $95,200     $0.68       $0.35       $1.03       -3.0%
14:01:15    $95,100     $0.66       $0.36       $1.02       -2.0%
14:01:30    $94,900     $0.60       $0.38       $0.98       +2.0% ← OPPORTUNITY!
14:01:45    $94,700     $0.55       $0.42       $0.97       +3.0% ← BETTER!
14:02:00    $94,600     $0.52       $0.46       $0.98       +2.0% ← Still good
14:02:15    $94,500     $0.50       $0.49       $0.99       +1.0% ← Closing
14:02:30    $94,500     $0.50       $0.50       $1.00       0.0%  ← Equilibrium
```

**What happened:**

1. **Overreaction (14:00:15):** Retail panic-bought YES, pushing it to $0.58. But NO didn't drop proportionally — it only fell to $0.48. Combined = $1.06 (no arb, actually overpriced).

2. **The Reversal (14:01:30):** BTC started pulling back. YES holders panicked out. NO was still cheap from before. Combined dropped to $0.98 — a 2% arb opportunity.

3. **Peak Opportunity (14:01:45):** The dislocation hit maximum at 3% margin.

4. **Equilibrium (14:02:30):** Market re-balanced. Opportunity closed.

**The Visual Pattern:**

```
Arb Margin Over Time During Retail Panic Event

+4% │                          ╭──╮
+3% │                        ╭─╯  ╰─╮
+2% │                      ╭─╯      ╰─╮
+1% │                    ╭─╯          ╰─╮
 0% │──╮              ╭──╯              ╰────
-1% │  │            ╭─╯
-2% │  │          ╭─╯
-3% │  ╰─╮      ╭─╯
-4% │    ╰─╮  ╭─╯
-5% │      ╰──╯
    └────────────────────────────────────────→ Time
         ↑           ↑           ↑
     BTC pumps   Overreaction  Correction
                  (no arb)     (ARB WINDOW)
```

**Key Insight:** The arb opportunity doesn't appear during the initial move — it appears during the *correction* when one side has reverted faster than the other.

---

### 1.5 Why This Opportunity Exists

In theory, YES + NO should always equal $1.00 since markets are efficient. In practice, several factors create persistent inefficiencies:

**1. Retail Emotion**

When Bitcoin suddenly pumps 2% in a minute, retail traders panic-buy YES shares, pushing the price up. But they forget about NO — nobody wants to buy "Bitcoin will go down" when it's clearly going up. This creates a window where YES + NO < $1.00.

**2. Information Asymmetry**

Polymarket prices update slower than spot prices on Binance and Coinbase. A bot watching spot data can see the direction 100-500ms before Polymarket participants adjust their orders. This latency creates opportunities.

**3. Market Microstructure**

The order book is fragmented. Different traders have different views and post at different prices. At any given moment, the best available prices on both sides might not add up to $1.00.

**4. Time Pressure**

As the 15-minute window closes, traders scramble. Some need to exit positions. Some make last-minute bets. This volatility creates mispricings.

**5. Complexity Barrier**

Most retail traders don't understand the arbitrage. They think in terms of "betting on Bitcoin going up" not "buying both sides for guaranteed profit." This leaves the opportunity for those who do understand.

---

### 1.6 Why It's Not Easy Money

If this were trivially easy, everyone would do it and the opportunity would disappear. Here are the real challenges:

**Challenge 1: Execution Risk (Leg Risk)**

You can't buy YES and NO simultaneously. You must place two separate orders. What happens if:
- You buy 100 YES at $0.55
- Before you can buy NO, the price jumps to $0.50
- Now YES ($0.55) + NO ($0.50) = $1.05 — you're losing money
- And you're stuck with unhedged YES shares

This is called "leg risk" — the risk that the market moves between your two trades.

**Challenge 2: Getting Filled**

Limit orders don't guarantee fills. If you place a buy order at $0.55 and the market moves to $0.57, you'll never get filled. You could miss opportunities entirely.

**Challenge 3: Fees**

Polymarket charges taker fees up to 3.15% on 15-minute markets at 50/50 odds. If your arb margin is 2% and you pay 3.15% in fees, you lose money. You MUST use limit orders (maker orders) which don't pay fees.

**Challenge 4: Adverse Selection (Toxic Flow)**

The orders sitting in the book might be traps. A sophisticated trader might place a fake large order at an attractive price to bait you. When you try to buy, they pull it. Or worse, they let you fill and then the price moves against you.

**Challenge 5: Competition**

Other bots are doing this too. When a good opportunity appears, multiple bots race to capture it. Speed matters. Intelligence matters. You're competing against professional trading firms.

**Challenge 6: Time Pressure**

Each market only lasts 15 minutes. You must accumulate positions AND balance them before the window closes. If you're unbalanced at settlement, you have directional risk you didn't want.

---

### 1.7 Our Solutions

This bot is designed specifically to overcome each challenge:

| Challenge | Our Solution |
|-----------|--------------|
| **Leg Risk** | **Shadow Bids** — We pre-compute the second order before placing the first. When the first fills, the second fires within 50ms. |
| **Getting Filled** | **Price Chasing** — We dynamically adjust limit order prices, walking up incrementally until filled, but never exceeding a ceiling that would break profitability. |
| **Fees** | **Maker Only** — We only use limit orders, which earn rebates instead of paying fees. We only cross the spread in emergencies. |
| **Toxic Flow** | **Detection Algorithm** — We analyze order book patterns (size, timing, cancel rates) to identify and avoid suspicious liquidity. |
| **Competition** | **Multiple Edges** — Speed alone isn't enough. We combine better data (NQ signals), smarter execution (shadow bids), and better market selection (scanner). |
| **Time Pressure** | **Phase-Based Strategy** — We adapt behavior based on time remaining: accumulate early, balance in the middle, emergency close if needed. |

---

### 1.8 The Cross-Asset Edge (Your NQ Advantage)

Here's something most crypto-only bots miss: **NQ futures lead crypto.**

When institutional traders de-risk, they hit the most liquid market first — that's often NQ (Nasdaq futures). BTC follows 50-200 milliseconds later because:
- Crypto exchanges are fragmented
- Less institutional infrastructure
- More retail-driven

Since you already trade NQ and have order flow data, we integrate that signal:

1. **When NQ dumps hard** → We know BTC is likely to follow → We're more aggressive buying NO shares
2. **When NQ rips hard** → We know BTC is likely to follow → We're more aggressive buying YES shares

We're still doing arbitrage (buying both sides), but we time our accumulation better. This isn't prediction — it's using a faster data source to act before slower markets adjust.

---

### 1.9 The Multi-Market Scanner (Phase 2)

Account88888 (the successful bot we analyzed) trades mostly BTC but also ETH, SOL, and occasionally XRP. He's not randomly choosing — he's systematically finding where the edge is best.

Our scanner scores all markets on five dimensions:

| Dimension | Weight | What It Measures |
|-----------|--------|------------------|
| Liquidity | 25% | Can we execute our target size? |
| Margin | 35% | How fat is the arb spread? |
| Volatility | 15% | Is there enough action for opportunities? |
| Competition | 15% | How many other bots are fighting for fills? |
| Correlation | 10% | Can we use NQ signals effectively? |

The scanner runs every 30 seconds and allocates capital to wherever the edge is fattest right now.

**Example output:**
```
RECOMMENDED ALLOCATION:
  ETH: 45% ($4,500)  ← Retail panic, 3.2% margins
  SOL: 35% ($3,500)  ← Volatility spike, fat margins
  BTC: 20% ($2,000)  ← Overcrowded, thin margins
  XRP: 0%            ← Liquidity too low
```

---

### 1.10 The 2026 Polymarket Landscape

The market has evolved. Here are the key constraints we work within:

| Constraint | Impact | Our Response |
|------------|--------|--------------|
| **Taker fees up to 3.15%** | Can't use market orders profitably | Limit orders only (maker) |
| **Maker rebates available** | Extra income for providing liquidity | Always be the maker |
| **Bot competition intensifying** | Margins are thinner, speed matters | Multiple edges, not just speed |
| **15-minute windows** | Hard deadlines, must balance | Phase-based lifecycle management |
| **Polygon network** | Low gas costs (~$0.01/trade) | Not a significant constraint |

---

### 1.11 Expected Performance

Based on analysis of Account88888 and market conditions:

| Metric | Conservative | Optimistic |
|--------|--------------|------------|
| Margin per pair | 1-2% | 3-4% |
| Windows traded per day | 48 | 96 |
| Capital deployed per window | $1,000 | $10,000 |
| Daily gross profit | $480-960 | $2,880-3,840 |
| Monthly gross profit | $15,000-30,000 | $90,000-120,000 |
| Win rate | 95%+ | 98%+ |

**Important caveats:**
- These numbers assume proper execution. Sloppy implementation will result in losses.
- Account88888's performance may include airdrop farming incentives (volume = tokens).
- Competition increases over time; margins may compress.
- Past performance doesn't guarantee future results.

---

### 1.12 What This Document Covers

This specification provides everything needed to build and deploy the bot:

| Section | What You'll Learn |
|---------|-------------------|
| **2. How The Strategy Works** | Deep conceptual understanding with examples |
| **3. System Architecture** | Component design, data flow, concurrency model |
| **4. Data Sources & APIs** | Polymarket, Binance, NQ integration details |
| **5. Market Discovery** | Finding and parsing 15-minute markets |
| **6. Price Monitoring** | Real-time data, toxic flow detection |
| **7. Arbitrage Detection** | Opportunity identification, cross-asset signals |
| **8. Order Execution** | Price chasing, shadow bids, fill handling |
| **9. Inventory Management** | Tracking positions, rebalancing logic |
| **10. Risk Management** | Limits, circuit breakers, leg risk handling |
| **11. Position Sizing** | How much to trade on each opportunity |
| **12. Timing & Lifecycle** | Phase-based behavior throughout the window |
| **13. Error Handling** | Recovery, retries, graceful degradation |
| **14. Monitoring & Logging** | Metrics, alerts, trade journal |
| **15. Configuration** | All tunable parameters |
| **16. Rust Implementation** | Language-specific guidance, key crates |
| **17. Testing Strategy** | Paper trading, shadow mode, validation |
| **18. Deployment** | Infrastructure, security, launch checklist |
| **19. Phase 2 Scanner** | Multi-market allocation system |

---

### 1.13 Key Terminology

Before diving in, here are terms you'll see throughout:

| Term | Definition |
|------|------------|
| **Dutch Book** | Betting strategy that guarantees profit regardless of outcome |
| **Arbitrage** | Profiting from price differences without directional risk |
| **Leg Risk** | Risk of one side of a paired trade executing while the other doesn't |
| **Maker Order** | Limit order that adds liquidity to the book (earns rebates) |
| **Taker Order** | Order that crosses the spread (pays fees) |
| **Shadow Bid** | Pre-computed second order ready to fire instantly |
| **Price Chasing** | Dynamically adjusting limit prices to ensure fills |
| **Toxic Flow** | Liquidity that appears attractive but leads to bad fills |
| **Window** | A single 15-minute market lifecycle |
| **Accumulation** | Phase of building matched YES/NO positions |
| **Balancing** | Phase of equalizing inventory before settlement |

---

### 1.14 How to Read This Document

**If you want to understand the strategy:**
- Read Section 1 (this summary) and Section 2 (How The Strategy Works)
- These give you the conceptual foundation

**If you want to implement the bot:**
- Read Sections 3-18 in order
- Each section builds on previous ones
- Code examples are provided but require context from explanatory text

**If you want to understand a specific component:**
- Use the Table of Contents to jump to that section
- Each section opens with Purpose and Design Philosophy
- Cross-references point to related sections

**If you want the quick reference:**
- Appendix A has the glossary
- Appendix B has key thresholds and parameters
- Section 15 has the complete configuration file

---

### 1.15 Success Criteria

The bot is working correctly when:

1. **Profitable:** Generating positive returns after all costs
2. **Balanced:** Ending each window with equal YES and NO shares
3. **Safe:** Never exceeding risk limits, circuit breakers working
4. **Reliable:** Running continuously without manual intervention
5. **Observable:** Clear logs, metrics, and alerts for all activity

If any of these aren't true, something needs fixing before deploying with real capital.

---

## 2. How The Strategy Works

This section explains the complete strategy conceptually before we dive into implementation details.

### 2.1 The Dutch Book: Core Concept

A "Dutch Book" is a set

---

## 3. System Architecture

### 3.1 Design Philosophy

The architecture is built around several core principles:

**Principle 1: Speed Without Sacrifice**

Prediction market arbitrage is time-sensitive. Opportunities last milliseconds to seconds. But we won't sacrifice safety for speed. Every fast path has a fallback. Every optimization is tested.

**Principle 2: Modularity**

Each component has a single responsibility. The price monitor doesn't know about orders. The executor doesn't know about strategy. This makes testing easier and bugs more isolated.

**Principle 3: Lock-Free Hot Paths**

The critical trading loop must not block on locks. We use atomic operations and lock-free data structures (DashMap) for all shared state accessed during trading.

**Principle 4: Fail Safe**

When something goes wrong, the bot should protect capital first. Circuit breakers halt trading. Unhedged positions get emergency-closed. We'd rather miss profits than take unexpected losses.

### 3.2 Component Overview

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                        POLYMARKET ARB BOT v2.1                              │
├─────────────────────────────────────────────────────────────────────────────┤
│                                                                             │
│  ┌───────────────────────────────────────────────────────────────────────┐ │
│  │                         PHASE 2: SCANNER                              │ │
│  │                                                                       │ │
│  │  Scores all markets on: liquidity, margin, volatility, competition,  │ │
│  │  and correlation with NQ. Outputs allocation percentages.             │ │
│  └───────────────────────────────────┬───────────────────────────────────┘ │
│                                      │                                     │
│                                      ▼ Allocation Targets                  │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐  ┌─────────────┐    │
│  │   DISCOVERY  │  │   MONITOR    │  │   STRATEGY   │  │  EXECUTOR   │    │
│  │              │  │              │  │              │  │             │    │
│  │ Finds active │  │ Streams      │  │ Detects arb  │  │ Places      │    │
│  │ 15-min       │  │ prices from  │  │ opportuni-   │  │ orders,     │    │
│  │ markets,     │  │ Polymarket,  │  │ ties,        │  │ chases      │    │
│  │ extracts     │  │ Binance,     │  │ calculates   │  │ prices,     │    │
│  │ token IDs    │  │ and NQ.      │  │ sizing,      │  │ fires       │    │
│  │              │  │ Detects      │  │ applies      │  │ shadow      │    │
│  │              │  │ toxic flow   │  │ cross-asset  │  │ bids        │    │
│  │              │  │              │  │ signals      │  │             │    │
│  └──────────────┘  └──────────────┘  └──────────────┘  └─────────────┘    │
│         │                 │                 │                 │            │
│         └────────────────┬┴─────────────────┴─────────────────┘            │
│                          │                                                 │
│                          ▼                                                 │
│              ┌─────────────────────────────────────────────┐               │
│              │         SHARED STATE (Lock-Free)            │               │
│              │                                             │               │
│              │  Uses DashMap and atomic types to avoid     │               │
│              │  lock contention. Hot path checks are       │               │
│              │  single memory reads (~1 nanosecond).       │               │
│              └─────────────────────────────────────────────┘               │
│                                                                             │
└─────────────────────────────────────────────────────────────────────────────┘
```

### 3.3 Component Responsibilities

**Discovery Module**

*Purpose:* Find tradeable markets.

Every 5 minutes, Discovery queries Polymarket's Gamma API to find active 15-minute up/down markets. It extracts the "token IDs" — unique identifiers for YES and NO shares that we need for trading. It filters to our target assets (BTC, ETH, SOL, XRP) and tracks market timing.

*Key challenge:* Markets are created dynamically. New windows appear every 15 minutes. We must discover them before they become tradeable.

**Monitor Module**

*Purpose:* Maintain real-time market state.

Monitor maintains WebSocket connections to:
- Polymarket CLOB (order book data)
- Binance (BTC/ETH/SOL spot prices)
- Your NQ feed (cross-asset signals)

It keeps the order book state current, calculates derived metrics (combined cost, arb margin, available liquidity), and runs toxic flow detection on every update.

*Key challenge:* Data must be fresh. Stale prices lead to bad trades. We track "last update" timestamps and refuse to trade on old data.

**Strategy Module**

*Purpose:* Decide what to trade.

Strategy receives price updates and decides:
1. Is there an arbitrage opportunity?
2. How confident are we? (margin size, liquidity, toxic warnings)
3. How much should we trade? (position sizing)
4. Should we bias toward YES or NO? (cross-asset signals)

It outputs trade requests to the Executor.

*Key challenge:* False positives are expensive. We must be confident an opportunity is real before acting.

**Executor Module**

*Purpose:* Turn decisions into trades.

Executor handles the mechanics:
1. Prepare shadow bids before primary orders
2. Place limit orders with appropriate pricing
3. Chase prices when orders don't fill
4. Fire shadow bids instantly on primary fills
5. Track fills and update inventory
6. Handle errors and retries

*Key challenge:* Execution is where theory meets reality. Slippage, rejected orders, partial fills — Executor handles all of it.

**Scanner Module (Phase 2)**

*Purpose:* Allocate capital across markets.

Scanner runs every 30 seconds, scoring all active markets on five dimensions:
1. Liquidity (can we execute size?)
2. Margin (how fat is the spread?)
3. Volatility (is there action?)
4. Competition (how many other bots?)
5. Correlation (can we use NQ signals?)

It outputs allocation percentages that cap how much capital each market can use.

*Key challenge:* Scoring must be accurate. Sending capital to a bad market is worse than not trading.

### 3.4 Data Flow

Understanding how data flows through the system helps clarify the design:

```
External Data Sources                    Internal Processing
─────────────────────                    ───────────────────

  ┌─────────────┐
  │ Gamma API   │──── Market IDs ────────────┐
  │ (REST)      │                            │
  └─────────────┘                            ▼
                                    ┌─────────────────┐
  ┌─────────────┐                   │    DISCOVERY    │
  │ Polymarket  │                   └────────┬────────┘
  │ CLOB (WS)   │──── Order Book ───────┐    │
  └─────────────┘                       │    │ Active Markets
                                        ▼    ▼
  ┌─────────────┐                   ┌─────────────────┐
  │ Binance     │──── Spot Prices ─▶│    MONITOR      │
  │ (WS)        │                   │                 │
  └─────────────┘                   │  • Order books  │
                                    │  • Spot prices  │
  ┌─────────────┐                   │  • Toxic flow   │
  │ NQ Futures  │──── Order Flow ──▶│  • Staleness    │
  │ (Your Feed) │                   └────────┬────────┘
  └─────────────┘                            │
                                             │ Price Updates
                                             ▼
                                    ┌─────────────────┐
                                    │    STRATEGY     │
                                    │                 │
                                    │  • Arb detect   │
                                    │  • Sizing       │
                                    │  • Cross-asset  │
                                    └────────┬────────┘
                                             │
                                             │ Trade Requests
                                             ▼
                                    ┌─────────────────┐
                                    │    EXECUTOR     │
                                    │                 │
                                    │  • Shadow bids  │
                                    │  • Price chase  │
                                    │  • Fill track   │
                                    └────────┬────────┘
                                             │
                                             │ Orders
                                             ▼
                                    ┌─────────────────┐
                                    │  Polymarket     │
                                    │  CLOB API       │
                                    └─────────────────┘
```

### 3.5 Concurrency Model

The bot runs multiple concurrent tasks:

```rust
// Simplified view of the runtime structure

#[tokio::main]
async fn main() {
    // Shared state - lock-free access
    let state = Arc::new(GlobalState::new());
    
    // Communication channels
    let (price_tx, price_rx) = mpsc::channel(1000);
    let (order_tx, order_rx) = mpsc::channel(100);
    let (fill_tx, fill_rx) = broadcast::channel(100);
    
    // Spawn concurrent tasks
    tokio::spawn(discovery_loop(state.clone()));
    tokio::spawn(price_monitor(state.clone(), price_tx));
    tokio::spawn(strategy_loop(state.clone(), price_rx, order_tx));
    tokio::spawn(order_executor(state.clone(), order_rx, fill_tx));
    tokio::spawn(market_scanner(state.clone()));
    
    // Wait forever (or until error/shutdown)
    loop { tokio::time::sleep(Duration::from_secs(60)).await; }
}
```

**Why this model?**

1. **Independence:** Each task runs at its own pace. Slow discovery doesn't block fast price updates.

2. **Channels:** Components communicate via async channels. No shared mutable state except through lock-free structures.

3. **Backpressure:** Bounded channels prevent memory explosions. If Strategy can't keep up with price updates, old updates are dropped.

4. **Error Isolation:** If one task crashes, others continue. The main loop can restart failed tasks.

---

## 4. Data Sources & APIs

### 4.1 Overview

The bot consumes data from multiple sources, each serving a specific purpose:

| Source | Type | Purpose | Latency Requirement |
|--------|------|---------|---------------------|
| Polymarket Gamma API | REST | Discover markets | <5 seconds |
| Polymarket CLOB | WebSocket | Order book, prices | <500ms |
| Binance | WebSocket | Spot prices | <200ms |
| Coinbase | WebSocket | Backup spot prices | <200ms |
| NQ Futures (yours) | Your feed | Cross-asset signals | <100ms |

**Why multiple spot sources?**

Binance is primary because it has the highest volume and fastest updates. But if Binance disconnects, we fall back to Coinbase. Having redundancy prevents us from trading blind.

**Why care about latency?**

In arbitrage, timing matters. If our data is 500ms stale, we're seeing yesterday's newspaper. The market has already moved. Our "opportunity" might be gone or reversed.

### 4.2 Polymarket APIs

#### 4.2.1 Gamma API (Market Discovery)

The Gamma API is Polymarket's REST API for market metadata. We use it to discover which markets exist and extract token IDs.

**Base URL:** `https://gamma-api.polymarket.com`

**What we need from it:**

1. **Event listings** — All active prediction markets
2. **Market details** — Questions, outcomes, timing
3. **Token IDs** — The unique identifiers for YES and NO shares

**Key endpoint:**

```
GET /events?active=true&closed=false&tag=crypto
```

**Response structure (simplified):**

```json
{
  "id": "event-uuid",
  "title": "Bitcoin Up or Down - January 9, 4AM ET",
  "endDate": "2026-01-09T09:00:00Z",
  "markets": [
    {
      "id": "market-uuid",
      "question": "Will Bitcoin go up?",
      "clobTokenIds": ["0xabc...", "0xdef..."],
      "outcomePrices": "[\"0.55\", \"0.45\"]"
    }
  ]
}
```

**Parsing notes:**

- `clobTokenIds[0]` is YES, `clobTokenIds[1]` is NO
- `endDate` tells us when the market settles
- We filter by title pattern: "Up or Down" + time + crypto name

#### 4.2.2 CLOB API (Order Book & Trading)

The CLOB (Central Limit Order Book) API is where trading happens. It provides real-time order book data and accepts trade orders.

**Base URL:** `https://clob.polymarket.com`

**WebSocket:** `wss://ws-subscriptions-clob.polymarket.com/ws/market`

**Key REST endpoints:**

| Endpoint | Method | Purpose |
|----------|--------|---------|
| `/book` | GET | Full order book snapshot |
| `/price` | GET | Best bid/ask |
| `/order` | POST | Place order |
| `/order/{id}` | DELETE | Cancel order |

**WebSocket subscription:**

```json
{
  "type": "subscribe",
  "channel": "market",
  "assets_ids": ["0xabc...", "0xdef..."]
}
```

Once subscribed, we receive real-time updates:

```json
{
  "type": "price_change",
  "asset_id": "0xabc...",
  "price": "0.55",
  "timestamp": 1736412000000
}
```

**Why WebSocket over REST?**

REST requires polling (repeatedly asking "what's the price?"). WebSocket pushes updates to us. For time-sensitive trading, push is essential — we learn of changes 10-100x faster.

#### 4.2.3 Authentication

Polymarket uses EIP-712 signed messages for authentication. Every order request must be signed by your wallet.

**Required headers for authenticated requests:**

```
POLY-ADDRESS: <your_wallet_address>
POLY-SIGNATURE: <eip712_signature>
POLY-TIMESTAMP: <unix_timestamp>
POLY-NONCE: <random_nonce>
```

**Signing process:**

1. Construct the order payload (token ID, price, size, side, expiration)
2. Create an EIP-712 typed data structure
3. Sign with your private key
4. Include signature in request headers

**Security note:**

Your private key is the "password" to your trading account. It must be stored securely (encrypted on disk, loaded into memory only when needed). Never log it. Never transmit it.

### 4.3 Spot Price Feeds

We need real-time crypto spot prices for two purposes:

1. **Cross-asset signals:** Knowing where BTC/ETH/SOL is trading helps predict market direction
2. **Latency arbitrage:** When spot moves before Polymarket adjusts, we can act

#### 4.3.1 Binance WebSocket

**URL:** `wss://stream.binance.com:9443/ws`

**Subscription (combined stream):**

```json
{
  "method": "SUBSCRIBE",
  "params": ["btcusdt@trade", "ethusdt@trade", "solusdt@trade"],
  "id": 1
}
```

**Trade update:**

```json
{
  "e": "trade",
  "s": "BTCUSDT",
  "p": "94523.50",
  "q": "0.001",
  "T": 1736412000123
}
```

**Why trade stream, not ticker?**

The trade stream shows every actual trade. The ticker is an aggregate. For latency-sensitive work, we want the raw trades — they reflect what's actually happening right now.

#### 4.3.2 Coinbase WebSocket (Backup)

**URL:** `wss://ws-feed.exchange.coinbase.com`

**Subscription:**

```json
{
  "type": "subscribe",
  "channels": [
    {
      "name": "ticker",
      "product_ids": ["BTC-USD", "ETH-USD", "SOL-USD"]
    }
  ]
}
```

**Why Coinbase as backup?**

Binance is the primary because it's the most liquid crypto exchange. But Binance occasionally has outages or connectivity issues. Coinbase is the second-most-liquid USD venue and provides a reliable fallback.

### 4.4 NQ Futures Feed

**Purpose:** Leading indicator for crypto direction.

You already have an NQ futures trading framework. We integrate with it to get:

1. **Order flow imbalance:** Are buyers or sellers more aggressive?
2. **Price momentum:** How much has NQ moved recently?

**Interface we expect:**

```rust
pub trait NqFeedProvider: Send + Sync {
    /// Get current order flow imbalance (-1.0 to 1.0)
    /// Negative = selling pressure, Positive = buying pressure
    fn order_flow_imbalance(&self) -> Decimal;
    
    /// Get price momentum over last N seconds (% change)
    fn price_momentum(&self, lookback_secs: u32) -> Decimal;
    
    /// Subscribe to real-time updates
    fn subscribe(&self, callback: Box<dyn Fn(NqUpdate) + Send>);
}
```

**Why NQ leads crypto:**

When institutional traders de-risk, they hit the most liquid markets first. NQ futures are extremely liquid with 24-hour trading. BTC is correlated (~0.7 historically) but moves slower because:
- Crypto exchanges are fragmented
- Less institutional infrastructure
- More retail-driven

By watching NQ, we see the "signal" before the crypto markets fully react.

### 4.5 Data Staleness and Fallbacks

**The staleness problem:**

Network issues, exchange outages, or bugs can cause our data to become stale. Trading on stale data is dangerous — we might think there's an opportunity when it's already gone.

**Our approach:**

1. **Track timestamps:** Every price update is timestamped. We track when we last received data from each source.

2. **Staleness thresholds:** If data is older than the threshold, we refuse to trade.

| Source | Staleness Threshold |
|--------|---------------------|
| Polymarket CLOB | 1 second |
| Binance | 500ms |
| Coinbase | 500ms |
| NQ Feed | 200ms |

3. **Fallback logic:**
   - If Binance is stale, switch to Coinbase
   - If both are stale, disable cross-asset signals (but continue arb trading)
   - If Polymarket is stale, halt trading entirely

4. **Reconnection:** When a WebSocket disconnects, we immediately attempt reconnection with exponential backoff.

---

## 5. Market Discovery

### 5.1 Purpose

Before we can trade, we need to know what markets exist. Polymarket creates new 15-minute markets continuously — a new window every 15 minutes for each asset. Discovery finds these markets and extracts the information we need.

### 5.2 What We Need to Discover

For each tradeable market, we need:

| Field | Description | Example |
|-------|-------------|---------|
| Event ID | Unique identifier for the market | "abc-123-def-456" |
| YES Token ID | Token ID for buying "Up" | "0xabc..." |
| NO Token ID | Token ID for buying "Down" | "0xdef..." |
| Asset | Which crypto this is for | BTC, ETH, SOL, XRP |
| Window Start | When trading opens | 2026-01-09T08:45:00Z |
| Window End | When market settles | 2026-01-09T09:00:00Z |
| Status | Current phase | Upcoming, Active, Closing |

### 5.3 Discovery Logic

Discovery runs every 5 minutes (configurable). Here's the logic:

**Step 1: Fetch all active crypto events**

```
GET /events?active=true&closed=false&tag=crypto
```

**Step 2: Filter to 15-minute up/down markets**

We look for specific patterns in the title:
- Contains "Up or Down" (case insensitive)
- Contains a time pattern like "4AM ET" or "3:15PM ET"
- Contains a crypto name (Bitcoin, Ethereum, Solana, XRP)

**Step 3: Extract token IDs**

The `clobTokenIds` field contains two hex strings. The first is YES (up), the second is NO (down).

**Step 4: Calculate timing**

From `endDate`, we derive:
- Window end = endDate
- Window start = endDate - 15 minutes
- Time remaining = endDate - now

**Step 5: Determine status**

Based on time remaining:
- More than 15 minutes: Upcoming (not tradeable yet)
- 2-15 minutes: Active (main trading window)
- 30 seconds - 2 minutes: Closing Soon (balance inventory)
- Less than 30 seconds: Emergency Close
- Past: Settled

### 5.4 Implementation

```rust
struct MarketWindow {
    event_id: String,
    yes_token_id: String,
    no_token_id: String,
    asset: CryptoAsset,
    window_start: DateTime<Utc>,
    window_end: DateTime<Utc>,
    status: MarketStatus,
}

enum MarketStatus {
    Upcoming,      // Not tradeable yet
    Active,        // Main trading window
    ClosingSoon,   // Focus on balancing
    Closing,       // Emergency close
    Settled,       // Done
}

enum CryptoAsset {
    BTC,
    ETH,
    SOL,
    XRP,
}

async fn discover_markets(gamma_client: &GammaClient) -> Result<Vec<MarketWindow>> {
    let events = gamma_client
        .get_events()
        .active(true)
        .closed(false)
        .tag("crypto")
        .fetch()
        .await?;

    let mut windows = Vec::new();

    for event in events {
        // Filter: must be 15-minute up/down market
        if !is_15min_updown_market(&event.title) {
            continue;
        }

        // Filter: must be a crypto we trade
        let asset = match parse_crypto_asset(&event.title) {
            Some(a) => a,
            None => continue,
        };

        // Filter: must be relevant timing (within 20 minutes of end)
        let time_to_end = event.end_date - Utc::now();
        if time_to_end < Duration::zero() || time_to_end > Duration::minutes(20) {
            continue;
        }

        // Extract token IDs
        let market = &event.markets[0];
        let token_ids: Vec<String> = serde_json::from_str(&market.clob_token_ids)?;

        windows.push(MarketWindow {
            event_id: event.id,
            yes_token_id: token_ids[0].clone(),
            no_token_id: token_ids[1].clone(),
            asset,
            window_start: event.end_date - Duration::minutes(15),
            window_end: event.end_date,
            status: calculate_status(event.end_date),
        });
    }

    Ok(windows)
}
```

### 5.5 Edge Cases

**Race condition on new markets:**

A new market might appear while we're in the middle of a discovery cycle. We re-discover frequently (every 5 minutes, or triggered by gaps) to catch these.

**Duplicate markets:**

Sometimes Polymarket creates duplicate events. We deduplicate by token IDs.

**Cancelled markets:**

Rarely, markets are cancelled before settlement. We check the `active` flag and remove cancelled markets from our state.

---

## 6. Price Monitoring & Toxic Flow Detection

### 6.1 Purpose

Price Monitoring keeps us informed of current market conditions in real-time. But raw prices aren't enough — we also need to detect when liquidity is "toxic" (likely to result in bad fills).

This module answers two questions:
1. What are the current prices and how much liquidity is available?
2. Is this liquidity safe to trade against?

### 6.2 Order Book State

We maintain a complete picture of the order book for each token:

**Why track the full book, not just best prices?**

1. **Depth matters:** If best ask is $0.55 but only for 10 shares, and we need 100 shares, we'll pay more
2. **Toxic detection:** Large orders appearing suddenly are suspicious
3. **Competition signals:** Order velocity indicates bot activity

**State structure:**

```rust
struct OrderBookSide {
    /// Price levels: price -> (size, order_count, first_seen, last_updated)
    levels: BTreeMap<Decimal, OrderLevel>,
    
    /// Best price (lowest ask or highest bid)
    best_price: Option<Decimal>,
    
    /// Total liquidity across all levels
    total_liquidity: Decimal,
}

struct OrderLevel {
    size: Decimal,              // Total shares available at this price
    order_count: u32,           // How many orders (for competition analysis)
    first_seen: Instant,        // When this level appeared
    last_updated: Instant,      // When it was last modified
}

struct MarketState {
    event_id: String,
    asset: CryptoAsset,
    
    yes_book: TokenOrderBook,
    no_book: TokenOrderBook,
    
    // Derived (recalculated on every update)
    yes_best_ask: Decimal,
    no_best_ask: Decimal,
    combined_cost: Decimal,         // yes_best_ask + no_best_ask
    arb_margin: Decimal,            // 1.0 - combined_cost
    min_available_pairs: Decimal,   // min(yes_liquidity, no_liquidity)
    
    last_update: Instant,
}
```

### 6.3 WebSocket Message Processing

We receive two types of updates:

**Book snapshots:** Complete order book state (received on connection and periodically)

**Incremental updates:** Single price/size changes

Processing logic:

```rust
async fn handle_clob_message(&mut self, msg: Message) {
    let update: ClobUpdate = serde_json::from_str(&msg.to_string())?;
    
    match update.update_type.as_str() {
        "book" => {
            // Full snapshot - replace entire book state
            self.replace_book(&update);
        }
        "price_change" => {
            // Incremental - update single level
            self.update_level(&update);
        }
        _ => {}
    }
    
    // Recalculate derived fields
    self.recalculate_derived();
    
    // Run toxic flow detection
    let toxic_warning = self.toxic_detector.analyze(&self.current_state);
    
    // Notify strategy module
    self.notify_strategy(toxic_warning);
}
```

### 6.4 Toxic Flow Detection

**What is toxic flow?**

Toxic flow is liquidity that looks attractive but will result in bad fills. Common patterns:

1. **Spoofing:** Large orders placed to manipulate prices, pulled before they're filled
2. **Whale traps:** Real large orders that, if you're first to fill, leave you holding the bag
3. **Wash trading:** Fake activity to make the market look active

**Why we care:**

If we fill against a spoof, the order might disappear before our second leg executes, leaving us unhedged. If we fill against a whale, the market might move against us immediately.

**Detection signals:**

| Signal | What It Indicates |
|--------|-------------------|
| Large order (50x+ average) | Possible whale or spoof |
| Sudden appearance (<500ms old) | Possible spoof |
| High cancel rate (>80%) | Active spoofing |
| Size far exceeds typical retail ($10-50) | Non-retail order |

**Implementation:**

```rust
struct ToxicFlowDetector {
    /// Rolling average order size (updates over time)
    avg_order_size: Decimal,
    
    /// How many times average is "suspicious"
    toxic_size_multiplier: Decimal,  // e.g., 50x
    
    /// Orders newer than this are "sudden"
    sudden_threshold: Duration,      // e.g., 500ms
    
    /// Order history for pattern detection
    history: VecDeque<OrderEvent>,
}

impl ToxicFlowDetector {
    fn analyze(&self, state: &MarketState) -> Option<ToxicFlowWarning> {
        // Check both YES and NO books
        for (book, side) in [(&state.yes_book, Side::Up), (&state.no_book, Side::Down)] {
            if let Some(warning) = self.check_book(book, side) {
                return Some(warning);
            }
        }
        None
    }
    
    fn check_book(&self, book: &TokenOrderBook, side: Side) -> Option<ToxicFlowWarning> {
        let best_level = book.asks.levels.first()?;
        
        // Check 1: Abnormally large order
        let size_ratio = best_level.size / self.avg_order_size;
        
        if size_ratio > self.toxic_size_multiplier {
            // Check 2: Did it appear suddenly?
            let age = best_level.first_seen.elapsed();
            
            if age < self.sudden_threshold {
                // Large + sudden = high probability of spoof
                return Some(ToxicFlowWarning {
                    severity: ToxicSeverity::High,
                    affected_side: side,
                    reason: format!(
                        "Large order ({:.0}x avg) appeared {:.0}ms ago",
                        size_ratio, age.as_millis()
                    ),
                    action: ToxicAction::WaitAndSee(Duration::from_secs(2)),
                });
            }
            
            // Large but not sudden - might be real whale
            return Some(ToxicFlowWarning {
                severity: ToxicSeverity::Medium,
                affected_side: side,
                reason: format!("Large order ({:.0}x avg)", size_ratio),
                action: ToxicAction::ReduceSize(dec!(0.5)),
            });
        }
        
        None
    }
}
```

**Response to toxic flow:**

| Severity | Response |
|----------|----------|
| High | Skip this opportunity entirely, or wait 2 seconds to see if order disappears |
| Medium | Reduce position size by 50% |
| Low | Proceed with caution, log for analysis |

### 6.5 Staleness Detection

Data goes stale when:
- WebSocket disconnects
- Exchange has issues
- Network problems

We track when each data source was last updated:

```rust
impl MarketState {
    fn is_stale(&self) -> bool {
        let now = Instant::now();
        let age = now.duration_since(self.last_update);
        
        age > Duration::from_millis(1000)  // 1 second threshold
    }
}
```

**If data is stale:** We refuse to trade. Strategy will skip this market until data is fresh.

---

## 7. Arbitrage Detection & Cross-Asset Signals

### 7.1 Purpose

This module answers: "Is there a profitable opportunity right now, and how confident are we?"

Detection is more nuanced than just checking if YES + NO < $1.00. We must consider:
- Net margin after costs
- Liquidity constraints
- Time remaining
- Toxic flow warnings
- Cross-asset signals

### 7.2 Core Detection Logic

**Step 1: Calculate raw margin**

```
gross_margin = 1.00 - (yes_best_ask + no_best_ask)
```

**Step 2: Subtract estimated costs**

```
estimated_slippage = 0.002  // 0.2%
gas_costs = 0.01 * 2        // ~$0.01 per trade on Polygon
net_margin = gross_margin - estimated_slippage - gas_costs
```

**Step 3: Compare to threshold**

The threshold is dynamic based on time remaining:

| Time Remaining | Minimum Margin | Reasoning |
|----------------|----------------|-----------|
| >5 minutes | 2.5% | Be selective, plenty of opportunities coming |
| 2-5 minutes | 1.5% | Time is ticking, lower standards |
| <2 minutes | 0.5% | Focus on balancing inventory, take what's available |

**Step 4: Assess confidence**

Confidence combines margin quality, liquidity depth, and toxic warnings:

```rust
fn calculate_confidence(
    margin: Decimal,
    liquidity: Decimal,
    toxic_warning: Option<&ToxicFlowWarning>
) -> ArbConfidence {
    // Start with margin-based confidence
    let base = if margin > dec!(0.03) && liquidity > dec!(100) {
        ArbConfidence::High
    } else if margin > dec!(0.02) && liquidity > dec!(50) {
        ArbConfidence::Medium
    } else if margin > dec!(0.01) {
        ArbConfidence::Low
    } else {
        ArbConfidence::None
    };
    
    // Downgrade if toxic
    match toxic_warning {
        Some(w) if w.severity == ToxicSeverity::High => ArbConfidence::None,
        Some(w) if w.severity == ToxicSeverity::Medium => base.downgrade(),
        _ => base
    }
}
```

### 7.3 Cross-Asset Signals

**The insight:**

NQ futures and BTC are correlated (~0.7). NQ moves first because it's more liquid and has more institutional flow. By watching NQ, we can anticipate BTC direction.

**How we use it:**

1. **Get NQ signal strength**
   - Order flow imbalance: Are buyers or sellers more aggressive?
   - Price momentum: How much has NQ moved in last 60 seconds?

2. **Calculate directional bias**
   - If NQ is dumping (negative order flow, negative momentum): Bias toward NO
   - If NQ is ripping (positive order flow, positive momentum): Bias toward YES
   - If neutral: No bias

3. **Apply to trading**
   - We still buy both sides (it's arbitrage, not directional)
   - But we can time our accumulation better
   - When NQ dumps, we're more aggressive buying NO (it's about to get more expensive)

**Correlation by asset:**

| Asset | Correlation with NQ | Signal Usefulness |
|-------|---------------------|-------------------|
| BTC | 0.70 | High |
| ETH | 0.60 | Medium-High |
| SOL | 0.50 | Medium |
| XRP | 0.30 | Low |

### 7.3.1 Critical: Asymmetric Correlation

**The standard correlation numbers above are misleading.** Correlation is NOT symmetric between up and down moves:

| Condition | BTC-NQ Correlation | Explanation |
|-----------|-------------------|-------------|
| **NQ Crashing** | **0.85 - 0.95** | Panic selling is contagious. When institutions dump, everything follows. |
| **NQ Rallying** | **0.40 - 0.50** | FOMO is less synchronized. Crypto may rally independently or lag. |
| **NQ Flat** | **0.30 - 0.40** | No signal to follow. Crypto moves on its own drivers. |

**Why this matters:**

During a sharp NQ selloff, BTC will almost certainly follow within 50-200ms. Our signal is extremely reliable.

During an NQ rally, BTC might follow, might not, might already be ahead. Our signal is less reliable.

**Implementation with volatility multiplier:**

```rust
struct CrossAssetAnalyzer {
    nq_feed: Arc<dyn NqFeedProvider>,
    
    /// Base correlations (used for rallies and flat markets)
    base_correlations: HashMap<CryptoAsset, Decimal>,
    
    /// Signal threshold before acting
    signal_threshold: Decimal,
    
    /// Multiplier for downside moves (panic correlation is higher)
    downside_multiplier: Decimal,  // 1.5x
}

impl CrossAssetAnalyzer {
    fn new(nq_feed: Arc<dyn NqFeedProvider>) -> Self {
        let mut base_correlations = HashMap::new();
        base_correlations.insert(CryptoAsset::BTC, dec!(0.50));  // Conservative base
        base_correlations.insert(CryptoAsset::ETH, dec!(0.40));
        base_correlations.insert(CryptoAsset::SOL, dec!(0.35));
        base_correlations.insert(CryptoAsset::XRP, dec!(0.20));
        
        Self {
            nq_feed,
            base_correlations,
            signal_threshold: dec!(0.02),
            downside_multiplier: dec!(1.5),
        }
    }
    
    fn get_bias(&self, asset: CryptoAsset) -> Option<Side> {
        let order_flow = self.nq_feed.order_flow_imbalance();
        let momentum = self.nq_feed.price_momentum(60);
        
        // Combine signals
        let raw_signal = order_flow * dec!(0.6) + momentum * dec!(0.4);
        
        // Get base correlation
        let base_correlation = self.base_correlations.get(&asset).copied()?;
        
        // Apply asymmetric correlation adjustment
        let effective_correlation = if raw_signal < Decimal::ZERO {
            // NQ is negative - apply downside multiplier
            // Panic correlation is much higher
            (base_correlation * self.downside_multiplier).min(dec!(0.95))
        } else {
            // NQ is positive or flat - use base correlation
            base_correlation
        };
        
        // Apply correlation to signal
        let adjusted_signal = raw_signal * effective_correlation;
        
        // Threshold check
        if adjusted_signal < -self.signal_threshold {
            Some(Side::Down)
        } else if adjusted_signal > self.signal_threshold {
            Some(Side::Up)
        } else {
            None
        }
    }
    
    /// Get signal strength for position sizing
    /// Returns higher values for downside signals (more reliable)
    fn get_signal_strength(&self, asset: CryptoAsset) -> SignalStrength {
        let order_flow = self.nq_feed.order_flow_imbalance();
        let momentum = self.nq_feed.price_momentum(60);
        let raw_signal = order_flow * dec!(0.6) + momentum * dec!(0.4);
        
        let base_correlation = self.base_correlations
            .get(&asset)
            .copied()
            .unwrap_or(Decimal::ZERO);
        
        // Downside signals are more reliable, so return higher strength
        if raw_signal < -self.signal_threshold {
            let effective_corr = (base_correlation * self.downside_multiplier).min(dec!(0.95));
            SignalStrength {
                direction: Side::Down,
                confidence: effective_corr,
                raw_signal: raw_signal.abs(),
            }
        } else if raw_signal > self.signal_threshold {
            SignalStrength {
                direction: Side::Up,
                confidence: base_correlation,  // Lower confidence for upside
                raw_signal: raw_signal.abs(),
            }
        } else {
            SignalStrength {
                direction: Side::Up,  // Arbitrary
                confidence: Decimal::ZERO,
                raw_signal: Decimal::ZERO,
            }
        }
    }
}

struct SignalStrength {
    direction: Side,
    confidence: Decimal,   // 0.0 - 1.0, higher for downside signals
    raw_signal: Decimal,
}
```

**Practical implication for trading:**

When NQ dumps:
- Signal confidence is HIGH (0.85+)
- We can be very aggressive buying NO
- Slightly larger position sizes are justified
- The shadow bid for NO should be priced more aggressively

When NQ rallies:
- Signal confidence is MODERATE (0.40-0.50)
- We still bias toward YES, but less aggressively
- Keep position sizes normal
- Don't overpay for YES based on signal alone

**Important note:**

Cross-asset signals are an **enhancement**, not the core strategy. We still do arbitrage (buy both sides). The signals help us:
- Time entries better
- Size positions more optimally
- Have slightly more conviction on one side

If NQ signals are unavailable (feed disconnected), we continue trading without them.

### 7.4 Opportunity Structure

When we detect an opportunity, we package all relevant information:

```rust
struct ArbOpportunity {
    // Market identification
    event_id: String,
    asset: CryptoAsset,
    
    // Prices
    yes_price: Decimal,
    no_price: Decimal,
    combined_cost: Decimal,
    
    // Margins
    gross_margin: Decimal,
    net_margin: Decimal,
    
    // Constraints
    max_pairs: Decimal,  // Limited by liquidity
    
    // Confidence
    confidence: ArbConfidence,
    
    // Enhancements
    cross_asset_bias: Option<Side>,
    toxic_warning: Option<ToxicFlowWarning>,
    
    // Metadata
    timestamp: Instant,
}
```

This structure flows to the Executor, which decides how to act on it.

---

## 8. Order Execution: Price Chasing & Shadow Bids

### 8.1 Purpose

This is where the rubber meets the road. Execution turns opportunities into profits — or into losses if done poorly.

The challenges:
1. **Getting filled:** Limit orders don't guarantee fills. Markets move.
2. **Leg risk:** Filling one side but not the other leaves us exposed.
3. **Speed:** Competition is fierce. Slow execution means missed opportunities.

Our solutions:
1. **Price Chasing:** Dynamically adjust limit prices to ensure fills
2. **Shadow Bids:** Pre-compute second leg orders for instant execution
3. **Maker-only:** Use limit orders to avoid 3.15% taker fees

### 8.2 Why Limit Orders Only

**The fee problem:**

Polymarket charges taker fees up to 3.15% on 15-minute markets when prices are near 50/50. If our arb margin is 2% and we pay 3.15% in fees, we lose money.

**The solution:**

Limit orders are "maker" orders — we provide liquidity to the market. Makers don't pay taker fees. In fact, Polymarket pays makers a small rebate.

**The tradeoff:**

Limit orders don't guarantee fills. We might place an order at $0.55 and the market moves to $0.57. We never get filled. This is why we need Price Chasing.

### 8.3 Price Chasing

**The concept:**

Instead of placing a static limit order and hoping, we actively manage the order:

1. Place initial order at aggressive limit price
2. Every 500ms, check if filled
3. If not filled, cancel and resubmit at a slightly higher price
4. Repeat until filled OR we hit the "ceiling" price

**The ceiling:**

We can't chase forever. There's a maximum price beyond which the trade becomes unprofitable. The ceiling is calculated dynamically:

```
ceiling = 1.00 - other_leg_price - minimum_margin
```

Example:
- We're trying to buy YES
- We already know we can buy NO at $0.06
- We want at least 1% margin
- Ceiling = 1.00 - 0.06 - 0.01 = $0.93

We'll chase up to $0.93 but not beyond.

**Implementation:**

```rust
struct PriceChaser {
    initial_price: Decimal,
    ceiling_price: Decimal,
    step_size: Decimal,         // How much to increase each step (e.g., $0.001)
    check_interval: Duration,   // How often to check (e.g., 500ms)
    max_chase_time: Duration,   // Give up after this long (e.g., 5 seconds)
}

impl PriceChaser {
    async fn chase(&self, executor: &mut OrderExecutor, order: Order) -> ChaseResult {
        let mut current_price = self.initial_price;
        let start = Instant::now();
        
        loop {
            // Wait for check interval
            tokio::time::sleep(self.check_interval).await;
            
            // Check if filled
            let status = executor.get_order_status(&order.id).await?;
            if status == OrderStatus::Filled {
                return ChaseResult::Filled(order);
            }
            
            // Check timeout
            if start.elapsed() > self.max_chase_time {
                return ChaseResult::Timeout(order);
            }
            
            // Check if we can still chase
            let next_price = current_price + self.step_size;
            if next_price > self.ceiling_price {
                return ChaseResult::CeilingReached(order);
            }
            
            // Cancel current order and place new one at higher price
            executor.cancel(&order.id).await?;
            order = executor.place_limit(next_price, order.remaining_size).await?;
            current_price = next_price;
        }
    }
}
```

**Tuning parameters:**

| Parameter | Typical Value | Tradeoff |
|-----------|---------------|----------|
| step_size | $0.001 | Smaller = more fills, higher cost |
| check_interval | 500ms | Faster = more responsive, more API calls |
| max_chase_time | 5 seconds | Longer = more fills, more capital locked |

### 8.4 Shadow Bids

**The problem:**

After the first leg fills, we need to fill the second leg fast. But:
- Building the order takes time (serialization, nonce generation)
- Signing takes time (~1-5ms)
- Network latency adds more time

Every millisecond the second leg isn't placed is a millisecond of leg risk.

**The solution:**

Before placing the first (primary) leg, we pre-compute the second (shadow) leg:

1. Calculate expected price for second leg
2. Build the order payload
3. Everything is ready except the signature

When the primary leg fills:
1. Sign the pre-built payload (~1-5ms)
2. Submit immediately
3. Total latency: <50ms

### 8.4.1 Micro-Optimization: Pre-Hashing for Sub-Millisecond Signing

**The bottleneck:**

In high-frequency order placement, the network is rarely the bottleneck — **signing is**. Generating an EIP-712 signature involves:
1. Hashing the domain separator (Keccak256)
2. Hashing the type structure (Keccak256)  
3. Hashing the message data (Keccak256)
4. Combining hashes (Keccak256)
5. ECDSA signature generation

Steps 1-4 each take ~0.5-1ms. Step 5 takes ~1-2ms. Total: 3-6ms.

**The optimization:**

Since `token_id`, `side`, and `expiration` are known when the market is discovered (not when we fire), we can pre-compute most of the hashing:

```rust
/// Pre-computed hash components for fast signing
struct PrehashedOrder {
    /// Domain separator hash (computed once per market)
    domain_separator_hash: [u8; 32],
    
    /// Type hash for the order struct (computed once, constant)
    type_hash: [u8; 32],
    
    /// Partial struct hash with known fields
    /// Only price and size need to be added
    partial_struct_hash: [u8; 32],
    
    /// The known fields
    token_id: String,
    side: OrderSide,
    expiration: u64,
    
    /// Fields that change per order
    price: Option<Decimal>,
    size: Option<Decimal>,
}

impl PrehashedOrder {
    /// Called when market is discovered - pre-compute everything possible
    fn new(token_id: String, side: OrderSide, expiration: u64, chain_id: u64) -> Self {
        // Domain separator is constant for the contract
        let domain_separator_hash = keccak256(encode_packed(&[
            keccak256(b"EIP712Domain(string name,uint256 chainId,address verifyingContract)"),
            keccak256(b"Polymarket"),
            chain_id.to_be_bytes(),
            CLOB_CONTRACT_ADDRESS,
        ]));
        
        // Type hash is constant for Order struct
        let type_hash = keccak256(
            b"Order(address maker,address taker,uint256 tokenId,uint256 makerAmount,uint256 takerAmount,uint256 expiration,uint256 nonce,uint256 feeRateBps,uint8 side,uint8 signatureType)"
        );
        
        // Pre-hash the known fields
        let partial_struct_hash = keccak256(encode_packed(&[
            type_hash,
            // token_id encoded
            pad_to_32_bytes(&token_id),
            // side encoded
            [side as u8; 32],
            // expiration encoded
            pad_to_32_bytes(&expiration.to_be_bytes()),
        ]));
        
        Self {
            domain_separator_hash,
            type_hash,
            partial_struct_hash,
            token_id,
            side,
            expiration,
            price: None,
            size: None,
        }
    }
    
    /// Called when we need to fire - only hash price and size
    fn finalize_and_sign(&self, price: Decimal, size: Decimal, wallet: &Wallet) -> Result<Signature> {
        // Only these two hashes happen at fire time
        let price_hash = keccak256(&price.to_string().as_bytes());
        let size_hash = keccak256(&size.to_string().as_bytes());
        
        // Combine with pre-computed partial
        let struct_hash = keccak256(encode_packed(&[
            self.partial_struct_hash,
            price_hash,
            size_hash,
        ]));
        
        // Final message hash
        let message_hash = keccak256(encode_packed(&[
            [0x19, 0x01],  // EIP-712 prefix
            self.domain_separator_hash,
            struct_hash,
        ]));
        
        // ECDSA sign (this is the unavoidable ~1-2ms)
        wallet.sign_hash(message_hash)
    }
}
```

**Performance improvement:**

| Operation | Without Pre-Hashing | With Pre-Hashing |
|-----------|---------------------|------------------|
| Domain separator hash | 0.8ms | 0ms (pre-computed) |
| Type hash | 0.5ms | 0ms (pre-computed) |
| Partial struct hash | 0.8ms | 0ms (pre-computed) |
| Final hashes (price, size) | 0.6ms | 0.6ms |
| ECDSA signature | 1.5ms | 1.5ms |
| **Total** | **4.2ms** | **2.1ms** |

**Result:** We shave ~2ms off shadow bid execution. In a competitive environment where multiple bots race to fill, 2ms is significant.

### 8.4.2 Shadow Order Implementation

**Basic implementation:**

```rust
struct ShadowOrder {
    token_id: String,
    side: OrderSide,
    price: Decimal,
    size: Decimal,
    
    /// Pre-built payload (everything except signature)
    unsigned_payload: OrderPayload,
    
    /// Pre-hashed components for fast signing
    prehashed: PrehashedOrder,
    
    /// When this shadow was created (for staleness check)
    created_at: Instant,
}

impl ShadowOrder {
    /// Create a shadow order ready to fire
    fn new(token_id: String, price: Decimal, size: Decimal) -> Self {
        let expiration = future_expiration();
        
        Self {
            token_id: token_id.clone(),
            side: OrderSide::Buy,
            price,
            size,
            unsigned_payload: OrderPayload {
                token_id: token_id.clone(),
                price: price.to_string(),
                size: size.to_string(),
                side: "BUY".to_string(),
                expiration,
                nonce: generate_nonce(),
            },
            prehashed: PrehashedOrder::new(token_id, OrderSide::Buy, expiration, POLYGON_CHAIN_ID),
            created_at: Instant::now(),
        }
    }
    
    /// Fire instantly when primary leg fills - uses pre-hashing
    async fn fire(&self, wallet: &Wallet, client: &ClobClient) -> Result<Order> {
        // Use pre-hashed signing for speed
        let signature = self.prehashed.finalize_and_sign(self.price, self.size, wallet)?;
        client.submit_presigned(self.unsigned_payload.clone(), signature).await
    }
}
```

### 8.4.3 Managing Shadows

We maintain a map of shadow orders keyed by (event_id, side):

```rust
struct ShadowManager {
    shadows: DashMap<(String, Side), ShadowOrder>,
}

impl ShadowManager {
    /// Prepare shadow before placing primary
    fn prepare(&self, event_id: &str, primary_side: Side, opposite_token: &str, price: Decimal, size: Decimal) {
        let shadow = ShadowOrder::new(opposite_token.to_string(), price, size);
        self.shadows.insert((event_id.to_string(), primary_side.opposite()), shadow);
    }
    
    /// Fire when primary fills
    async fn fire(&self, event_id: &str, filled_side: Side, filled_size: Decimal, wallet: &Wallet, client: &ClobClient) -> Result<Order> {
        let key = (event_id.to_string(), filled_side.opposite());
        let mut shadow = self.shadows.get_mut(&key).ok_or(Error::NoShadowOrder)?;
        
        // Adjust size if needed
        if shadow.size != filled_size {
            shadow.update_size(filled_size);
        }
        
        // Fire!
        shadow.fire(wallet, client).await
    }
}
```

### 8.5 The Complete Execution Flow

Here's how a complete pair execution works:

```
┌─────────────────────────────────────────────────────────────────────┐
│                    PAIR EXECUTION FLOW                              │
├─────────────────────────────────────────────────────────────────────┤
│                                                                     │
│  1. PREPARE                                                         │
│     ├── Calculate prices for YES and NO                            │
│     ├── Validate combined cost < threshold                          │
│     ├── Choose primary leg (better liquidity)                       │
│     └── Create shadow order for secondary leg                       │
│                                                                     │
│  2. EXECUTE PRIMARY                                                 │
│     ├── Place limit order                                           │
│     ├── Start price chasing loop                                    │
│     │   ├── Check fill status every 500ms                          │
│     │   ├── If not filled, bump price by $0.001                    │
│     │   └── Stop if: filled, ceiling reached, or timeout           │
│     └── Result: Filled, Partial, or Failed                         │
│                                                                     │
│  3. EXECUTE SECONDARY (if primary filled)                           │
│     ├── Fire shadow order immediately                               │
│     ├── Shadow executes in <50ms                                    │
│     └── Track fill status                                           │
│                                                                     │
│  4. HANDLE RESULT                                                   │
│     ├── If both filled: Update inventory, log success              │
│     ├── If partial: Fire shadow for filled amount, continue         │
│     └── If failed: Cancel orders, assess leg risk                   │
│                                                                     │
└─────────────────────────────────────────────────────────────────────┘
```

### 8.6 Execution Edge Cases

**Partial fills:**

Sometimes the primary leg partially fills (we wanted 100, got 60). We:
1. Fire shadow for the filled amount (60)
2. Continue chasing for the remainder (40)
3. If we get more fills, fire additional shadows

**Shadow staleness:**

If the shadow sits too long (>30 seconds), market conditions may have changed. We refresh it with current prices.

**Failed primary:**

If the primary leg never fills (timeout or ceiling), we have no risk — we never entered the trade. We just move on to the next opportunity.

**Failed shadow:**

If the shadow fails to fire or execute, we're in leg risk. The Risk Manager takes over (see Section 10).

---

## 9. Inventory Management

### 9.1 Purpose

Inventory Management tracks what we own and ensures we're balanced at settlement.

Key questions it answers:
1. How many YES and NO shares do we have?
2. What's our cost basis?
3. Are we balanced or exposed?
4. What should we do to rebalance?

### 9.2 Inventory State

For each active market, we track:

```rust
struct Inventory {
    event_id: String,
    asset: CryptoAsset,
    
    // Holdings
    yes_shares: Decimal,
    no_shares: Decimal,
    
    // Cost basis
    yes_cost: Decimal,  // Total $ spent on YES
    no_cost: Decimal,   // Total $ spent on NO
    
    // Derived (recalculated after every trade)
    total_pairs: Decimal,      // min(yes_shares, no_shares)
    imbalance: Decimal,        // abs(yes_shares - no_shares)
    imbalanced_side: Option<Side>,  // Which side has excess
    avg_pair_cost: Decimal,    // Average cost per matched pair
}
```

**Example:**

```
YES shares: 150 @ avg $0.58 = $87 cost
NO shares: 100 @ avg $0.06 = $6 cost

Total pairs: 100 (limited by NO)
Imbalance: 50 (excess YES)
Imbalanced side: Up (YES)
Avg pair cost: ($58 + $6) / 100 = $0.64 per pair
Guaranteed profit: 100 × ($1.00 - $0.64) = $36
Unhedged exposure: 50 YES @ $0.58 = $29 at risk
```

### 9.3 Updating Inventory

Every time a trade fills:

```rust
impl Inventory {
    fn on_fill(&mut self, side: Side, shares: Decimal, cost: Decimal) {
        match side {
            Side::Up => {
                self.yes_shares += shares;
                self.yes_cost += cost;
            }
            Side::Down => {
                self.no_shares += shares;
                self.no_cost += cost;
            }
        }
        self.recalculate();
    }
    
    fn recalculate(&mut self) {
        self.total_pairs = self.yes_shares.min(self.no_shares);
        
        let diff = self.yes_shares - self.no_shares;
        self.imbalance = diff.abs();
        
        self.imbalanced_side = if diff > Decimal::ZERO {
            Some(Side::Up)    // Excess YES
        } else if diff < Decimal::ZERO {
            Some(Side::Down)  // Excess NO
        } else {
            None              // Balanced
        };
        
        if self.total_pairs > Decimal::ZERO {
            // Calculate cost of matched pairs
            let yes_price = self.yes_cost / self.yes_shares;
            let no_price = self.no_cost / self.no_shares;
            self.avg_pair_cost = yes_price + no_price;
        }
    }
}
```

### 9.4 Rebalancing Logic

When placing new orders, we account for existing imbalance:

```rust
fn calculate_order_sizes(
    &self,
    opportunity: &ArbOpportunity,
    target_new_pairs: Decimal,
) -> (Decimal, Decimal) {
    match self.imbalanced_side {
        Some(Side::Up) => {
            // We have excess YES, need more NO
            let extra_no = self.imbalance.min(target_new_pairs);
            (target_new_pairs, target_new_pairs + extra_no)
        }
        Some(Side::Down) => {
            // We have excess NO, need more YES
            let extra_yes = self.imbalance.min(target_new_pairs);
            (target_new_pairs + extra_yes, target_new_pairs)
        }
        None => {
            // Balanced, buy equal
            (target_new_pairs, target_new_pairs)
        }
    }
}
```

### 9.5 P&L Calculation

**Guaranteed P&L (matched pairs):**

```
guaranteed_pnl = total_pairs × (1.00 - avg_pair_cost)
```

This is locked in. No matter what happens, we'll receive this much profit.

**Unhedged exposure:**

```
unhedged_cost = (excess_shares × avg_price_of_excess)
```

If the excess side loses, we lose this amount. If it wins, we gain (1.00 - avg_price) per share.

**Expected P&L at settlement:**

```
If our excess side wins: guaranteed_pnl + (excess_shares × (1.00 - excess_avg_price))
If our excess side loses: guaranteed_pnl - unhedged_cost
```

### 9.6 Inventory Limits and State Machine

Based on analysis of successful bots like Account88888, we use a granular inventory state machine rather than a single threshold:

**Inventory State Definitions:**

| State | Imbalance Ratio | Risk Level | Bot Behavior |
|-------|-----------------|------------|--------------|
| **Balanced** | < 5% | Neutral | Continue normal accumulation |
| **Skewed** | 5% - 15% | Moderate | Rebalance: lower bid on excess side, raise bid on weak side |
| **Exposed** | 15% - 30% | High | Stop new positions, only fill weak leg |
| **Crisis** | > 30% | Extreme | Emergency close (see Section 10.4) |

**Implementation:**

```rust
#[derive(Debug, Clone, Copy, PartialEq)]
enum InventoryState {
    Balanced,    // < 5% imbalance - all systems go
    Skewed,      // 5-15% - passive rebalancing
    Exposed,     // 15-30% - active rebalancing only
    Crisis,      // > 30% - emergency mode
}

impl Inventory {
    fn state(&self) -> InventoryState {
        if self.total_pairs == Decimal::ZERO {
            return InventoryState::Balanced;
        }
        
        let ratio = self.imbalance / self.total_pairs;
        
        if ratio < dec!(0.05) {
            InventoryState::Balanced
        } else if ratio < dec!(0.15) {
            InventoryState::Skewed
        } else if ratio < dec!(0.30) {
            InventoryState::Exposed
        } else {
            InventoryState::Crisis
        }
    }
    
    fn allowed_actions(&self) -> AllowedActions {
        match self.state() {
            InventoryState::Balanced => AllowedActions {
                can_accumulate_yes: true,
                can_accumulate_no: true,
                rebalance_urgency: Urgency::None,
            },
            InventoryState::Skewed => {
                // Can still accumulate, but bias toward weak side
                let weak_side = self.imbalanced_side.map(|s| s.opposite());
                AllowedActions {
                    can_accumulate_yes: weak_side != Some(Side::Down),
                    can_accumulate_no: weak_side != Some(Side::Up),
                    rebalance_urgency: Urgency::Low,
                }
            },
            InventoryState::Exposed => {
                // Only allow filling the weak side
                let weak_side = self.imbalanced_side.map(|s| s.opposite());
                AllowedActions {
                    can_accumulate_yes: weak_side == Some(Side::Up),
                    can_accumulate_no: weak_side == Some(Side::Down),
                    rebalance_urgency: Urgency::High,
                }
            },
            InventoryState::Crisis => AllowedActions {
                can_accumulate_yes: false,
                can_accumulate_no: false,
                rebalance_urgency: Urgency::Critical,
            },
        }
    }
}

struct AllowedActions {
    can_accumulate_yes: bool,
    can_accumulate_no: bool,
    rebalance_urgency: Urgency,
}
```

**State-Based Pricing Adjustments:**

When in **Skewed** state, we adjust our limit order prices to encourage rebalancing:

```rust
fn adjust_prices_for_rebalance(
    &self,
    base_yes_price: Decimal,
    base_no_price: Decimal,
    state: InventoryState,
    excess_side: Option<Side>,
) -> (Decimal, Decimal) {
    match (state, excess_side) {
        (InventoryState::Skewed, Some(Side::Up)) => {
            // Excess YES - be less aggressive on YES, more on NO
            (
                base_yes_price - dec!(0.005),  // Lower YES bid by 0.5¢
                base_no_price + dec!(0.005),   // Raise NO bid by 0.5¢
            )
        },
        (InventoryState::Skewed, Some(Side::Down)) => {
            // Excess NO - be less aggressive on NO, more on YES
            (
                base_yes_price + dec!(0.005),
                base_no_price - dec!(0.005),
            )
        },
        _ => (base_yes_price, base_no_price),
    }
}
```

**Hard Limits (unchanged):**

| Limit | Value | Purpose |
|-------|-------|---------|
| Max absolute imbalance | 100 shares | Hard cap regardless of ratio |
| Max position per market | $10,000 | Capital limit per market |

When any hard limit is approached, we prioritize rebalancing over accumulating.

---

## 10. Risk Management

### 10.1 Purpose

Risk Management protects capital. Its job is to prevent catastrophic losses, even if it means missing profitable opportunities.

Core principle: **It's better to miss a good trade than to take a bad one.**

### 10.2 Risk Categories

**Leg Risk (Primary)**

The risk of being filled on one side but not the other. This is our main risk.

Mitigation:
- Shadow bids for instant second-leg execution
- Strict position limits during accumulation
- Emergency close procedures when time runs out

**Execution Risk**

Orders might be rejected, delayed, or filled at worse prices than expected.

Mitigation:
- Price chasing with ceiling limits
- Retry logic with exponential backoff
- Graceful degradation when APIs are slow

**Market Risk (Minor)**

We have minimal market risk because we're balanced. But unhedged exposure exists during accumulation.

Mitigation:
- Strict imbalance limits
- Time-based urgency adjustments
- Emergency close at end of window

**Technical Risk**

System failures, network issues, bugs.

Mitigation:
- Circuit breakers halt trading on repeated failures
- Connection recovery logic
- Comprehensive logging for debugging

### 10.3 Pre-Trade Risk Checks

Before every trade, we verify safety:

```rust
fn check_pre_trade(
    &self,
    opportunity: &ArbOpportunity,
    proposed_size: Decimal,
    inventory: &Inventory,
    time_remaining: Duration,
) -> Result<RiskApproval> {
    
    // 1. Time check
    if time_remaining < Duration::from_secs(30) {
        return Err(Error::TooCloseToExpiry);
    }
    
    // 2. Position size check
    let proposed_exposure = proposed_size * opportunity.combined_cost;
    if proposed_exposure > self.config.max_position_per_market {
        return Err(Error::PositionLimitExceeded);
    }
    
    // 3. Total exposure check
    let new_total = self.current_exposure + proposed_exposure;
    if new_total > self.config.max_total_exposure {
        return Err(Error::TotalExposureExceeded);
    }
    
    // 4. Unhedged exposure check
    let projected_imbalance = inventory.imbalance + proposed_size;
    if projected_imbalance > self.config.max_unhedged_exposure {
        return Err(Error::ImbalanceLimitExceeded);
    }
    
    // 5. Daily loss check
    if self.daily_pnl < -self.config.max_daily_loss {
        return Err(Error::DailyLossLimitReached);
    }
    
    // 6. Toxic flow check
    if let Some(ref toxic) = opportunity.toxic_warning {
        if toxic.severity == ToxicSeverity::High {
            return Err(Error::ToxicFlowDetected);
        }
    }
    
    Ok(RiskApproval { approved_size: proposed_size })
}
```

### 10.4 Leg Risk Response

When leg risk occurs (one side filled, other not):

```rust
async fn handle_leg_risk(
    &self,
    filled_side: Side,
    filled_amount: Decimal,
    unfilled_order: &Order,
    time_remaining: Duration,
) -> Result<()> {
    let exposure = filled_amount * unfilled_order.price;
    
    log::warn!(
        "LEG RISK: {} filled {}, {} unfilled. Exposure: ${}",
        filled_side, filled_amount, filled_side.opposite(), exposure
    );
    
    // Response depends on severity and time
    if exposure > self.config.max_unhedged_exposure {
        // EMERGENCY: exposure too high
        self.emergency_close(unfilled_order.side, filled_amount).await?;
        
    } else if time_remaining < Duration::from_secs(60) {
        // TIME PRESSURE: be aggressive
        self.aggressive_chase(unfilled_order, Duration::from_secs(30)).await?;
        
    } else {
        // TIME AVAILABLE: normal chase
        self.normal_chase(unfilled_order).await?;
    }
    
    Ok(())
}

async fn emergency_close(&self, side: Side, size: Decimal) -> Result<()> {
    // Use IOC (Immediate-Or-Cancel) order
    // This is a TAKER order - we pay the 3.15% fee
    // But it guarantees fill, eliminating leg risk
    
    log::error!("EMERGENCY CLOSE: IOC order for {} {}", size, side);
    
    self.executor.place_ioc(side, size, dec!(0.95)).await?;
    
    Ok(())
}
```

### 10.5 Circuit Breakers

Circuit breakers halt trading when something is systematically wrong:

**Triggers:**

| Trigger | Threshold | Cooldown |
|---------|-----------|----------|
| Consecutive failures | 3 | 5 minutes |
| Slippage events | 5 per hour | 10 minutes |
| API errors | 10 per minute | 2 minutes |
| Leg risk events | 2 | 5 minutes |

**Implementation:**

```rust
struct CircuitBreaker {
    consecutive_failures: AtomicU32,
    is_tripped: AtomicBool,
    tripped_at: Mutex<Option<Instant>>,
    cooldown: Duration,
}

impl CircuitBreaker {
    fn record_failure(&self) {
        let failures = self.consecutive_failures.fetch_add(1, Ordering::SeqCst);
        
        if failures >= 3 {
            self.trip();
        }
    }
    
    fn record_success(&self) {
        self.consecutive_failures.store(0, Ordering::SeqCst);
    }
    
    fn trip(&self) {
        self.is_tripped.store(true, Ordering::SeqCst);
        *self.tripped_at.lock().unwrap() = Some(Instant::now());
        log::error!("CIRCUIT BREAKER TRIPPED");
    }
    
    fn can_trade(&self) -> bool {
        if !self.is_tripped.load(Ordering::Relaxed) {
            return true;
        }
        
        // Check if cooldown passed
        if let Some(tripped_at) = *self.tripped_at.lock().unwrap() {
            if tripped_at.elapsed() > self.cooldown {
                self.reset();
                return true;
            }
        }
        
        false
    }
}
```

**Hot path optimization:**

The `can_trade()` check happens on every iteration of the main loop. It must be fast.

```rust
// This compiles to a single atomic load - ~1 nanosecond
if !circuit_breaker.is_tripped.load(Ordering::Relaxed) {
    return true;  // Fast path - not tripped
}
// Slow path - check cooldown (only if tripped)
```

---

## 11. Position Sizing

### 11.1 Purpose

Position Sizing determines how much to trade on each opportunity. Goals:

1. **Maximize profit:** Larger positions = larger profits
2. **Limit risk:** Don't bet more than we can afford to lose
3. **Stay balanced:** Don't accumulate too much on one side
4. **Respect constraints:** Liquidity limits, capital limits

### 11.2 Sizing Factors

We consider multiple factors:

**Margin Quality**

Better margins deserve larger positions:

```
margin_score = (net_margin / 0.02)^2  // Non-linear, rewards fat margins
```

A 4% margin gets 4x the allocation of a 2% margin.

**Liquidity**

We can't trade more than the book supports:

```
max_by_liquidity = opportunity.min_available_pairs
```

**Time Remaining**

Earlier in the window, we're more aggressive. Later, we focus on balancing:

```
time_score = (minutes_remaining / 10).min(1.0)
```

**Scanner Allocation**

The Phase 2 scanner tells us what percentage of capital this market should get:

```
max_by_scanner = total_capital × scanner_allocation_pct
```

**Cross-Asset Signal**

Stronger signals justify slightly larger positions:

```
signal_multiplier = 1.0 + (signal_strength × 0.1)  // Up to 10% boost
```

### 11.3 Sizing Algorithm

```rust
fn calculate_size(
    &self,
    opportunity: &ArbOpportunity,
    available_capital: Decimal,
    inventory: &Inventory,
    time_remaining: Duration,
    scanner_allocation: Decimal,
) -> Decimal {
    // Start with scanner-allocated capital
    let allocated_capital = available_capital * scanner_allocation;
    
    // Adjust for margin quality (non-linear)
    let margin_score = (opportunity.net_margin / dec!(0.02)).powi(2);
    let margin_adjusted = allocated_capital * margin_score.min(dec!(2.0));
    
    // Adjust for liquidity
    let max_by_liquidity = opportunity.max_pairs * opportunity.combined_cost;
    let liquidity_adjusted = margin_adjusted.min(max_by_liquidity);
    
    // Adjust for time remaining
    let time_factor = (time_remaining.num_minutes() as f64 / 10.0).min(1.0);
    let time_adjusted = liquidity_adjusted * Decimal::from_f64(time_factor).unwrap();
    
    // Adjust for toxic flow
    let toxic_adjusted = match &opportunity.toxic_warning {
        Some(w) if w.severity == ToxicSeverity::Medium => time_adjusted * dec!(0.5),
        _ => time_adjusted,
    };
    
    // Apply cross-asset signal boost
    let signal_boost = match opportunity.cross_asset_bias {
        Some(_) => dec!(1.1),
        None => dec!(1.0),
    };
    let signal_adjusted = toxic_adjusted * signal_boost;
    
    // Apply limits
    let limited = signal_adjusted
        .min(self.config.max_trade_size)
        .min(available_capital);
    
    // Enforce minimum
    if limited < self.config.min_trade_size {
        return Decimal::ZERO;  // Not worth the gas cost
    }
    
    // Convert to shares
    (limited / opportunity.combined_cost).floor()
}
```

### 11.4 Conservative Sizing

The algorithm is intentionally conservative:

1. **Non-linear margin weighting:** Only fat margins get large allocations
2. **Multiple limits:** Every factor can reduce size, none can increase above base
3. **Time decay:** Less aggressive as window closes
4. **Toxic penalty:** Suspicious liquidity gets half size

This means we might miss some profit, but we also avoid large losses.

---

## 12. Timing & Market Lifecycle

### 12.1 Purpose

Every 15-minute market follows the same lifecycle. Our behavior must adapt to where we are in that cycle.

### 12.2 Lifecycle Phases

```
┌────────────────────────────────────────────────────────────────────────┐
│                         15-MINUTE MARKET LIFECYCLE                     │
├────────────────────────────────────────────────────────────────────────┤
│                                                                        │
│  TIME:    0        5        10       13       14.5     15              │
│           │        │        │        │        │        │               │
│           ▼        ▼        ▼        ▼        ▼        ▼               │
│       ┌───────────────────────────┐┌─────┐┌──────┐┌───────┐           │
│       │      ACCUMULATION         ││BALANC││CLOSE ││SETTLE │           │
│       │                           ││  ING ││      ││       │           │
│       │  • Hunt for opportunities ││• Stop ││• IOC ││• Payout│          │
│       │  • Build matched pairs    ││ new  ││ if   ││• Log   │          │
│       │  • Accept temp imbalance  ││ trades││ need-││ result│          │
│       │  • Be patient with fills  ││• Equal││ ed   ││       │          │
│       │                           ││ ize  ││      ││       │           │
│       └───────────────────────────┘└─────┘└──────┘└───────┘           │
│                                                                        │
│  URGENCY:  Low ───────────────────> Medium ─────> High ───> Critical  │
│                                                                        │
└────────────────────────────────────────────────────────────────────────┘
```

### 12.3 Phase-Specific Behavior

**Accumulation Phase (0-13 minutes)**

*Goals:* Find opportunities, build inventory, maximize profit potential

*Behavior:*
- Actively hunt for arb opportunities
- Place limit orders, be patient for fills
- Accept temporary imbalance (we'll balance later)
- Use normal price chasing (slower step increases)
- Full position sizing

*Margin threshold:* 2.5%+ (be selective)

**Balancing Phase (13-14.5 minutes)**

*Goals:* Equalize inventory, prepare for settlement

*Behavior:*
- Stop opening new positions
- Focus on filling the weaker side
- More aggressive price chasing (faster steps)
- Accept thinner margins to get balanced
- Cancel stale orders

*Margin threshold:* 1.5%+ (less selective)

**Closing Phase (14.5-15 minutes)**

*Goals:* Ensure balanced settlement at any cost

*Behavior:*
- Cancel all open limit orders
- If imbalanced: use IOC orders (pay taker fee)
- Accept losses to avoid larger losses
- Log final state

*Margin threshold:* 0.5%+ or just balance regardless

**Settlement (15 minutes)**

*Goals:* Collect profits, record results

*Behavior:*
- Verify settlement transaction
- Calculate final P&L
- Update daily statistics
- Clean up state

### 12.4 Implementation

```rust
enum WindowPhase {
    Accumulation { time_remaining: Duration },
    Balancing { time_remaining: Duration },
    Closing { time_remaining: Duration },
    Settled,
}

impl WindowPhase {
    fn from_time_remaining(remaining: Duration) -> Self {
        let seconds = remaining.num_seconds();
        
        if seconds > 120 {
            WindowPhase::Accumulation { time_remaining: remaining }
        } else if seconds > 30 {
            WindowPhase::Balancing { time_remaining: remaining }
        } else if seconds > 0 {
            WindowPhase::Closing { time_remaining: remaining }
        } else {
            WindowPhase::Settled
        }
    }
}

impl TradingLoop {
    async fn run_cycle(&mut self, market: &MarketWindow) -> Result<()> {
        let time_remaining = market.window_end - Utc::now();
        let phase = WindowPhase::from_time_remaining(time_remaining);
        
        match phase {
            WindowPhase::Accumulation { .. } => {
                // Normal trading: hunt opportunities, build positions
                if let Some(opp) = self.detect_opportunity(&market) {
                    if opp.confidence >= ArbConfidence::Medium {
                        self.execute_trade(&opp).await?;
                    }
                }
            }
            
            WindowPhase::Balancing { .. } => {
                // Rebalancing: focus on equalizing inventory
                let inventory = self.get_inventory(&market.event_id);
                if inventory.imbalance > dec!(10) {
                    self.rebalance(&inventory).await?;
                }
            }
            
            WindowPhase::Closing { .. } => {
                // Emergency: force balance at any cost
                self.emergency_close(&market.event_id).await?;
            }
            
            WindowPhase::Settled => {
                // Cleanup: log results, update stats
                self.finalize(&market.event_id).await?;
            }
        }
        
        Ok(())
    }
}
```

---

## 13. Error Handling & Recovery

### 13.1 Philosophy

Errors are inevitable. The question is how we respond. Our philosophy:

1. **Fail fast, fail safe:** When something's wrong, stop immediately. Don't compound errors.
2. **Protect capital first:** Cancel orders, close positions, then debug.
3. **Log everything:** We need data to understand what happened.
4. **Retry intelligently:** Some errors are transient. Retry with backoff.
5. **Circuit break on patterns:** Repeated failures indicate systemic issues.

### 13.2 Error Categories

**Transient Errors (Retry)**

- Network timeouts
- Rate limits
- Temporary exchange issues
- Order temporarily rejected (try again)

**Permanent Errors (Abort)**

- Authentication failure
- Invalid parameters
- Insufficient balance
- Market closed

**Systemic Errors (Circuit Break)**

- Repeated failures
- Data source unavailable
- Unexpected market behavior

### 13.3 Retry Logic

For transient errors, we retry with exponential backoff:

```rust
struct RetryPolicy {
    max_attempts: u32,      // e.g., 3
    base_delay: Duration,   // e.g., 100ms
    max_delay: Duration,    // e.g., 5 seconds
    backoff_factor: f64,    // e.g., 2.0
}

impl RetryPolicy {
    async fn execute<F, T, E>(&self, operation: F) -> Result<T, E>
    where
        F: Fn() -> Future<Output = Result<T, E>>,
        E: IsRetryable,
    {
        let mut attempt = 0;
        let mut delay = self.base_delay;
        
        loop {
            attempt += 1;
            
            match operation().await {
                Ok(result) => return Ok(result),
                Err(e) if attempt >= self.max_attempts => return Err(e),
                Err(e) if !e.is_retryable() => return Err(e),
                Err(e) => {
                    log::warn!("Attempt {}/{} failed: {:?}, retrying in {:?}",
                        attempt, self.max_attempts, e, delay);
                    
                    tokio::time::sleep(delay).await;
                    delay = (delay * self.backoff_factor).min(self.max_delay);
                }
            }
        }
    }
}
```

### 13.4 Connection Recovery

WebSocket connections break. When they do:

1. **Detect disconnection:** WebSocket library signals disconnection
2. **Cancel outstanding orders:** Safety first
3. **Attempt reconnection:** With exponential backoff
4. **Resubscribe:** Once connected, resubscribe to market feeds
5. **Resume trading:** Only after data is fresh

```rust
async fn handle_disconnection(&mut self, source: DataSource) {
    log::warn!("{:?} disconnected, initiating recovery", source);
    
    // Step 1: Safety - cancel orders
    if let Err(e) = self.cancel_all_orders().await {
        log::error!("Failed to cancel orders: {:?}", e);
        self.circuit_breaker.trip();
        return;
    }
    
    // Step 2: Reconnect with backoff
    let mut delay = Duration::from_millis(100);
    for attempt in 1..=10 {
        log::info!("Reconnection attempt {} for {:?}", attempt, source);
        
        match self.reconnect(source).await {
            Ok(_) => {
                log::info!("{:?} reconnected successfully", source);
                return;
            }
            Err(e) => {
                log::warn!("Reconnection failed: {:?}", e);
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(Duration::from_secs(30));
            }
        }
    }
    
    // Step 3: Give up, circuit break
    log::error!("Failed to reconnect {:?} after 10 attempts", source);
    self.circuit_breaker.trip();
}
```

---

## 14. Monitoring & Logging

### 14.1 Purpose

We need visibility into what the bot is doing. Monitoring and logging serve:

1. **Operational awareness:** Is the bot running? Making money?
2. **Debugging:** When something goes wrong, what happened?
3. **Optimization:** Where are we losing edge? How can we improve?
4. **Compliance:** Record of all trading activity

### 14.2 Metrics to Track

**Performance Metrics:**

| Metric | Description |
|--------|-------------|
| trades_executed | Total trades attempted |
| trades_successful | Trades that filled as expected |
| trades_failed | Trades that failed or were cancelled |
| gross_pnl | Profit before fees |
| net_pnl | Profit after fees |
| win_rate | Percentage of profitable windows |

**Execution Metrics:**

| Metric | Description |
|--------|-------------|
| order_latency_ms | Time from decision to order placed |
| fill_time_ms | Time from order placed to filled |
| shadow_latency_ms | Time to fire shadow after primary fills |
| price_chase_steps | How many price adjustments per order |

**Risk Metrics:**

| Metric | Description |
|--------|-------------|
| max_imbalance | Largest unhedged position |
| leg_risk_events | Times we had leg risk |
| circuit_breaker_trips | System halts |

**System Metrics:**

| Metric | Description |
|--------|-------------|
| ws_reconnects | WebSocket reconnection count |
| api_errors | Failed API calls |
| data_staleness_events | Times data was too old |

### 14.3 Structured Logging

We use structured logging (JSON format) for easy parsing:

```rust
use tracing::{info, warn, error, instrument};

#[instrument(skip(executor))]
async fn execute_trade(opportunity: &ArbOpportunity, executor: &Executor) -> Result<()> {
    info!(
        event_id = %opportunity.event_id,
        asset = ?opportunity.asset,
        yes_price = %opportunity.yes_price,
        no_price = %opportunity.no_price,
        margin = %opportunity.net_margin,
        "Executing trade"
    );
    
    // ... execution logic ...
    
    info!(
        event_id = %opportunity.event_id,
        yes_filled = %result.yes_filled,
        no_filled = %result.no_filled,
        latency_ms = %elapsed.as_millis(),
        "Trade completed"
    );
    
    Ok(())
}
```

**Log output:**

```json
{
  "timestamp": "2026-01-09T14:00:05.123Z",
  "level": "INFO",
  "target": "polymarket_bot::executor",
  "message": "Executing trade",
  "event_id": "abc-123",
  "asset": "BTC",
  "yes_price": "0.55",
  "no_price": "0.43",
  "margin": "0.02"
}
```

### 14.4 Trade Journal

Every completed trade is recorded in a structured journal:

```rust
struct TradeRecord {
    // Identification
    timestamp: DateTime<Utc>,
    event_id: String,
    asset: CryptoAsset,
    
    // Position
    yes_shares: Decimal,
    yes_avg_price: Decimal,
    no_shares: Decimal,
    no_avg_price: Decimal,
    
    // Execution details
    primary_leg: Side,
    shadow_latency_ms: u64,
    price_chase_steps: u32,
    
    // Signals used
    cross_asset_bias: Option<Side>,
    toxic_warning: Option<String>,
    
    // Outcome
    winning_side: Side,
    payout: Decimal,
    gross_pnl: Decimal,
    fees: Decimal,
    net_pnl: Decimal,
    
    // Risk events
    had_leg_risk: bool,
}
```

The journal is written to disk as JSONL (one JSON object per line) for easy parsing and analysis.

### 14.5 Alerting

Critical events trigger alerts:

| Event | Severity | Alert Channel |
|-------|----------|---------------|
| Circuit breaker tripped | Critical | Page on-call |
| Daily loss limit near | Warning | Slack |
| WebSocket disconnected >30s | Critical | Page on-call |
| Leg risk event | Warning | Slack |
| Unusual slippage | Warning | Slack |

---

## 15. Configuration

### 15.1 Configuration Philosophy

All tunable parameters should be in configuration, not code. This allows:

1. **Environment-specific settings:** Different values for paper vs production
2. **Easy tuning:** Adjust parameters without rebuilding
3. **Version control:** Track configuration changes
4. **Secrets separation:** Keep credentials out of code

### 15.2 Configuration File

```toml
# config.toml

#─────────────────────────────────────────────────────────────
# API Endpoints
#─────────────────────────────────────────────────────────────

[api]
polymarket_clob_url = "https://clob.polymarket.com"
polymarket_gamma_url = "https://gamma-api.polymarket.com"
polymarket_ws_url = "wss://ws-subscriptions-clob.polymarket.com/ws/market"
binance_ws_url = "wss://stream.binance.com:9443/ws"

#─────────────────────────────────────────────────────────────
# Wallet (loaded from environment variables for security)
#─────────────────────────────────────────────────────────────

[wallet]
address = "${POLY_WALLET_ADDRESS}"
private_key_path = "${POLY_PRIVATE_KEY_PATH}"

#─────────────────────────────────────────────────────────────
# Trading Parameters
#─────────────────────────────────────────────────────────────

[trading]
enabled_assets = ["BTC", "ETH", "SOL", "XRP"]
base_allocation_usd = 1000.0
min_trade_size_usd = 10.0
max_trade_size_usd = 5000.0

#─────────────────────────────────────────────────────────────
# Margin Thresholds (by time remaining)
#─────────────────────────────────────────────────────────────

[margins]
min_margin_early = 0.025      # 2.5% when >5 min left
min_margin_mid = 0.015        # 1.5% when 2-5 min left
min_margin_late = 0.005       # 0.5% when <2 min left

#─────────────────────────────────────────────────────────────
# Risk Limits
#─────────────────────────────────────────────────────────────

[risk]
max_position_per_market = 10000.0
max_total_exposure = 50000.0
max_unhedged_exposure = 500.0
max_unhedged_ratio = 0.10
max_loss_per_day = 1000.0
stop_trading_seconds_before_close = 30

#─────────────────────────────────────────────────────────────
# Price Chasing
#─────────────────────────────────────────────────────────────

[price_chasing]
step_size = 0.001             # $0.001 per step
check_interval_ms = 500       # Check every 500ms
max_chase_time_secs = 5       # Give up after 5s
min_amend_interval_ms = 200   # Rate limit amendments

#─────────────────────────────────────────────────────────────
# Toxic Flow Detection
#─────────────────────────────────────────────────────────────

[toxic_flow]
size_multiplier = 50.0        # 50x average = suspicious
sudden_appearance_ms = 500    # Orders <500ms old are "sudden"
skip_high_severity = true     # Skip trades on high toxic warning
reduce_size_on_medium = 0.5   # Reduce to 50% on medium warning

#─────────────────────────────────────────────────────────────
# Cross-Asset Signals (NQ Integration)
#─────────────────────────────────────────────────────────────

[cross_asset]
enabled = true
nq_signal_threshold = 0.02    # 2% move = significant
btc_correlation = 0.70
eth_correlation = 0.60
sol_correlation = 0.50
xrp_correlation = 0.30

#─────────────────────────────────────────────────────────────
# Phase 2 Scanner
#─────────────────────────────────────────────────────────────

[scanner]
enabled = true
scan_interval_secs = 30
rebalance_threshold = 0.10    # Rebalance if 10%+ off target
max_single_market_allocation = 0.60  # Max 60% to one market

[scanner.weights]
liquidity = 0.25
margin = 0.35
volatility = 0.15
competition = 0.15
correlation = 0.10

#─────────────────────────────────────────────────────────────
# Logging
#─────────────────────────────────────────────────────────────

[logging]
level = "info"
file = "logs/polymarket-bot.log"
trade_journal = "logs/trades.jsonl"
```

### 15.3 Environment Variables

Sensitive values are loaded from environment:

```bash
export POLY_WALLET_ADDRESS="0x..."
export POLY_PRIVATE_KEY_PATH="/secure/path/to/key.json"
export RUN_MODE="production"  # or "development" or "paper"
```

### 15.4 Configuration Loading

```rust
use config::{Config, Environment, File};

impl Settings {
    pub fn load() -> Result<Self> {
        let config = Config::builder()
            // Default values
            .add_source(File::with_name("config/default"))
            // Environment-specific overrides
            .add_source(File::with_name(&format!(
                "config/{}",
                std::env::var("RUN_MODE").unwrap_or("development".into())
            )).required(false))
            // Environment variable overrides (POLY_ prefix)
            .add_source(Environment::with_prefix("POLY").separator("__"))
            .build()?;
        
        config.try_deserialize()
    }
}
```

---

## 16. Rust Implementation Notes

### 16.1 Why Rust?

This bot has specific requirements that make Rust ideal:

1. **Performance:** Low latency is critical. Rust has no garbage collection pauses.
2. **Safety:** Financial software can't have memory bugs. Rust prevents them at compile time.
3. **Concurrency:** We run many concurrent tasks. Rust's async model is excellent.
4. **Ecosystem:** Great crates for WebSockets, HTTP, crypto, and finance.

### 16.2 Key Dependencies

```toml
[dependencies]
# Async runtime
tokio = { version = "1", features = ["full"] }
futures = "0.3"

# HTTP/WebSocket
reqwest = { version = "0.11", features = ["json"] }
tokio-tungstenite = { version = "0.20", features = ["native-tls"] }

# Serialization
serde = { version = "1", features = ["derive"] }
serde_json = "1"

# CRITICAL: Use Decimal, not f64, for all financial math
rust_decimal = { version = "1", features = ["serde"] }
rust_decimal_macros = "1"

# Time
chrono = { version = "0.4", features = ["serde"] }

# Crypto signing
ethers = { version = "2", features = ["legacy"] }

# Lock-free data structures
dashmap = "5"

# Logging
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["json"] }

# Error handling
thiserror = "1"
anyhow = "1"
```

### 16.3 Critical: Decimal Arithmetic

**NEVER use f64 for prices or financial calculations.**

Floating point has precision errors:

```rust
// BAD - floating point
let price: f64 = 0.55;
let shares: f64 = 100.0;
let cost = price * shares;  // Might be 54.99999999...

// GOOD - decimal
use rust_decimal_macros::dec;
let price = dec!(0.55);
let shares = dec!(100);
let cost = price * shares;  // Exactly 55.00
```

### 16.4 Lock-Free Shared State Architecture

The trading hot path must not block on locks. A single lock acquisition on the hot path can add 100ns-10μs of latency — unacceptable when competing with other bots.

**Core architecture pattern: Shared State + Message Bus**

```rust
use dashmap::DashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use tokio::sync::{mpsc, broadcast};

/// Shared market data - accessed by multiple tasks concurrently
/// Uses lock-free data structures exclusively
pub struct SharedMarketData {
    /// Current prices and order book state
    /// DashMap provides lock-free concurrent read/write
    pub prices: DashMap<String, MarketState>,
    
    /// Current inventory positions
    pub inventory: DashMap<String, Inventory>,
    
    /// Active shadow orders ready to fire
    pub shadows: DashMap<(String, Side), ShadowOrder>,
    
    /// Open orders we're tracking
    pub open_orders: DashMap<String, Order>,
    
    /// Scanner allocations (updated every 30s)
    pub allocations: DashMap<CryptoAsset, Decimal>,
}

/// Global control flags - atomic for zero-overhead access
pub struct ControlFlags {
    /// Master kill switch
    pub trading_enabled: AtomicBool,
    
    /// Circuit breaker state
    pub circuit_breaker_tripped: AtomicBool,
    
    /// NQ feed connected
    pub nq_feed_active: AtomicBool,
    
    /// Polymarket WS connected
    pub clob_connected: AtomicBool,
    
    /// Binance WS connected
    pub binance_connected: AtomicBool,
}

/// Metrics counters - atomic for lock-free updates
pub struct MetricsCounters {
    pub trades_executed: AtomicU64,
    pub trades_successful: AtomicU64,
    pub trades_failed: AtomicU64,
    pub leg_risk_events: AtomicU64,
    pub shadow_fires: AtomicU64,
    pub price_chases: AtomicU64,
}

/// The complete global state
pub struct GlobalState {
    pub data: SharedMarketData,
    pub flags: ControlFlags,
    pub metrics: MetricsCounters,
}

impl GlobalState {
    pub fn new() -> Self {
        Self {
            data: SharedMarketData {
                prices: DashMap::new(),
                inventory: DashMap::new(),
                shadows: DashMap::new(),
                open_orders: DashMap::new(),
                allocations: DashMap::new(),
            },
            flags: ControlFlags {
                trading_enabled: AtomicBool::new(true),
                circuit_breaker_tripped: AtomicBool::new(false),
                nq_feed_active: AtomicBool::new(false),
                clob_connected: AtomicBool::new(false),
                binance_connected: AtomicBool::new(false),
            },
            metrics: MetricsCounters {
                trades_executed: AtomicU64::new(0),
                trades_successful: AtomicU64::new(0),
                trades_failed: AtomicU64::new(0),
                leg_risk_events: AtomicU64::new(0),
                shadow_fires: AtomicU64::new(0),
                price_chases: AtomicU64::new(0),
            },
        }
    }
    
    /// Hot path check - compiles to TWO atomic loads (~2ns)
    #[inline(always)]
    pub fn can_trade(&self) -> bool {
        self.flags.trading_enabled.load(Ordering::Relaxed) &&
        !self.flags.circuit_breaker_tripped.load(Ordering::Relaxed)
    }
    
    /// Check if we have all data connections
    #[inline]
    pub fn data_healthy(&self) -> bool {
        self.flags.clob_connected.load(Ordering::Relaxed) &&
        self.flags.binance_connected.load(Ordering::Relaxed)
    }
    
    /// Record a trade result
    #[inline]
    pub fn record_trade(&self, success: bool) {
        self.metrics.trades_executed.fetch_add(1, Ordering::Relaxed);
        if success {
            self.metrics.trades_successful.fetch_add(1, Ordering::Relaxed);
        } else {
            self.metrics.trades_failed.fetch_add(1, Ordering::Relaxed);
        }
    }
}
```

**The Hot Path Task (Arbitrage Engine):**

```rust
/// The main trading loop - runs at maximum speed
/// All operations are lock-free
async fn arb_engine(
    state: Arc<GlobalState>,
    mut price_rx: mpsc::Receiver<PriceUpdate>,
    order_tx: mpsc::Sender<OrderRequest>,
    mut fill_rx: broadcast::Receiver<FillEvent>,
) {
    loop {
        tokio::select! {
            // Priority 1: Handle fills (fire shadow bids immediately)
            Ok(fill) = fill_rx.recv() => {
                // Single atomic check
                if !state.can_trade() { continue; }
                
                // Lock-free DashMap lookup
                if let Some(shadow) = state.data.shadows.get(&(fill.event_id.clone(), fill.side.opposite())) {
                    // Fire shadow bid
                    let request = OrderRequest::FireShadow {
                        event_id: fill.event_id,
                        side: fill.side.opposite(),
                        size: fill.filled_size,
                    };
                    order_tx.send(request).await.ok();
                    state.metrics.shadow_fires.fetch_add(1, Ordering::Relaxed);
                }
            }
            
            // Priority 2: Process price updates
            Some(update) = price_rx.recv() => {
                // Single atomic check
                if !state.can_trade() { continue; }
                
                // Lock-free read of market state
                if let Some(market) = state.data.prices.get(&update.event_id) {
                    // Check for arb opportunity
                    if market.arb_margin > dec!(0.02) {
                        // Lock-free read of inventory
                        let inventory = state.data.inventory
                            .get(&update.event_id)
                            .map(|i| i.clone());
                        
                        // Lock-free read of allocation
                        let allocation = state.data.allocations
                            .get(&market.asset)
                            .map(|a| *a)
                            .unwrap_or(Decimal::ZERO);
                        
                        if allocation > Decimal::ZERO {
                            // Generate order request
                            let request = create_order_request(&market, &inventory, allocation);
                            order_tx.send(request).await.ok();
                        }
                    }
                }
            }
        }
    }
}
```

**Message Types:**

```rust
/// Price updates from Monitor -> Strategy
struct PriceUpdate {
    event_id: String,
    asset: CryptoAsset,
    yes_price: Decimal,
    no_price: Decimal,
    combined_cost: Decimal,
    arb_margin: Decimal,
    toxic_warning: Option<ToxicFlowWarning>,
    timestamp: Instant,
}

/// Order requests from Strategy -> Executor
enum OrderRequest {
    /// Place a new paired arb order
    PlacePair {
        event_id: String,
        yes_price: Decimal,
        yes_size: Decimal,
        no_price: Decimal,
        no_size: Decimal,
    },
    
    /// Fire a pre-computed shadow bid
    FireShadow {
        event_id: String,
        side: Side,
        size: Decimal,
    },
    
    /// Cancel an order
    Cancel {
        order_id: String,
    },
    
    /// Emergency close all for a market
    EmergencyClose {
        event_id: String,
    },
}

/// Fill events from Executor -> Strategy (broadcast to all listeners)
struct FillEvent {
    event_id: String,
    order_id: String,
    side: Side,
    filled_size: Decimal,
    fill_price: Decimal,
    remaining_size: Decimal,
    timestamp: Instant,
}
```

**Complete Runtime Structure:**

```rust
#[tokio::main]
async fn main() -> Result<()> {
    // Shared state - lock-free access
    let state = Arc::new(GlobalState::new());
    
    // Communication channels
    let (price_tx, price_rx) = mpsc::channel::<PriceUpdate>(1000);
    let (order_tx, order_rx) = mpsc::channel::<OrderRequest>(100);
    let (fill_tx, _) = broadcast::channel::<FillEvent>(100);
    let (allocation_tx, allocation_rx) = mpsc::channel::<AllocationUpdate>(10);
    
    // Spawn all tasks
    let handles = vec![
        // Market discovery (runs every 5 minutes)
        tokio::spawn(discovery_loop(state.clone())),
        
        // Price monitoring (WebSocket handlers)
        tokio::spawn(clob_monitor(state.clone(), price_tx.clone())),
        tokio::spawn(binance_monitor(state.clone())),
        tokio::spawn(nq_monitor(state.clone())),
        
        // Core trading engine (hot path)
        tokio::spawn(arb_engine(
            state.clone(),
            price_rx,
            order_tx,
            fill_tx.subscribe(),
        )),
        
        // Order execution
        tokio::spawn(order_executor(
            state.clone(),
            order_rx,
            fill_tx,
        )),
        
        // Phase 2 scanner
        tokio::spawn(market_scanner(state.clone(), allocation_tx)),
        
        // Housekeeping
        tokio::spawn(metrics_reporter(state.clone())),
        tokio::spawn(connection_monitor(state.clone())),
    ];
    
    // Wait for any task to fail
    for handle in handles {
        if let Err(e) = handle.await {
            log::error!("Task failed: {:?}", e);
            break;
        }
    }
    
    Ok(())
}
```

**Why this architecture works:**

1. **Zero contention on hot path:** The arb engine never blocks on locks
2. **Predictable latency:** Every operation has bounded time complexity
3. **Backpressure:** Bounded channels prevent memory explosions
4. **Independence:** Each task can fail/restart without affecting others
5. **Testability:** Each component can be tested in isolation

### 16.5 Async Architecture

Each component runs as a separate Tokio task, communicating via channels:

```rust
// Channels are the "wires" between components
let (price_tx, price_rx) = mpsc::channel(1000);  // Buffer 1000 price updates
let (order_tx, order_rx) = mpsc::channel(100);   // Buffer 100 order requests

// Each component is a separate task
tokio::spawn(async move {
    price_monitor(price_tx).await;
});

tokio::spawn(async move {
    strategy_loop(price_rx, order_tx).await;
});

tokio::spawn(async move {
    order_executor(order_rx).await;
});
```

**Benefits:**

1. Components run independently
2. No shared mutable state (except lock-free structures)
3. Backpressure from bounded channels prevents memory issues
4. Easy to test components in isolation

---

## 17. Testing Strategy

### 17.1 Testing Philosophy

We can't test with real money until we're confident the bot works. Testing happens in phases:

| Phase | What | Duration | Risk |
|-------|------|----------|------|
| Unit Tests | Individual functions | Continuous | None |
| Integration Tests | Component interactions | Continuous | None |
| Paper Trading | Full system, simulated fills | 1 week | None |
| Shadow Mode | Real prices, logged only | 1 week | None |
| Small Capital | Real trading, $100-500 | 1 week | Low |
| Production | Gradual scale-up | Ongoing | Managed |

### 17.2 Unit Testing

Test individual functions with mock data:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_arb_detection_finds_opportunity() {
        let detector = ArbDetector::default();
        let state = mock_market_state("0.55", "0.43");  // Combined: 0.98
        
        let opp = detector.detect(&state, Duration::minutes(10), None);
        
        assert!(opp.is_some());
        assert_eq!(opp.unwrap().gross_margin, dec!(0.02));
    }
    
    #[test]
    fn test_no_arb_when_expensive() {
        let detector = ArbDetector::default();
        let state = mock_market_state("0.55", "0.46");  // Combined: 1.01
        
        let opp = detector.detect(&state, Duration::minutes(10), None);
        
        assert!(opp.is_none());
    }
    
    #[test]
    fn test_price_ceiling_calculation() {
        let ceiling = PriceChaser::calculate_ceiling(dec!(0.05), dec!(0.01));
        assert_eq!(ceiling, dec!(0.94));  // 1.00 - 0.05 - 0.01
    }
}
```

### 17.3 Paper Trading

Run the full system with simulated order execution:

```rust
struct PaperExecutor {
    fill_probability: f64,      // 0.95 = 95% of limits fill
    fill_delay_ms: u64,         // Simulate network latency
}

impl PaperExecutor {
    async fn place_order(&self, order: &Order) -> Result<Order> {
        // Simulate network delay
        tokio::time::sleep(Duration::from_millis(self.fill_delay_ms)).await;
        
        // Simulate fill probability
        if rand::random::<f64>() < self.fill_probability {
            Ok(Order { status: Filled, ..order.clone() })
        } else {
            Ok(Order { status: Open, ..order.clone() })
        }
    }
}
```

Paper trading validates:
- End-to-end flow works
- Strategy logic is correct
- Error handling works
- Performance is acceptable

### 17.4 Shadow Mode

Connect to real markets, but only log — don't execute:

```rust
if config.shadow_mode {
    log::info!(
        "SHADOW: Would execute {} {} at {}",
        opportunity.asset,
        order.size,
        order.price
    );
} else {
    executor.place_order(order).await?;
}
```

Shadow mode validates:
- Real data feeds work
- Opportunities exist as expected
- Timing assumptions are correct

### 17.5 Small Capital Testing

Real trading with minimal capital ($100-500):

- Start with single market (BTC only)
- Use minimum position sizes
- Run for full trading day (96 windows)
- Compare results to paper trading

---

## 18. Deployment Considerations

### 18.1 Infrastructure

**Location:**

The bot should run close to Polymarket's infrastructure for low latency. Polymarket uses Polygon (Ethereum L2) which has validators globally, but the API servers are likely US-based.

Recommended: US East (NYC or Virginia) VPS

**Specifications:**

| Component | Minimum | Recommended |
|-----------|---------|-------------|
| CPU | 2 cores | 4 cores |
| RAM | 4 GB | 8 GB |
| Storage | 20 GB SSD | 50 GB SSD |
| Network | 100 Mbps | 1 Gbps |

**Providers:**

- AWS EC2 (us-east-1)
- DigitalOcean (NYC)
- Vultr (New Jersey)

### 18.2 Wallet Security

Your private key is critical:

1. **Never hardcode:** Load from environment or encrypted file
2. **Never log:** Exclude from all logging
3. **Limit exposure:** Only load into memory when needed
4. **Separate keys:** Use different wallets for testing vs production
5. **Limited funds:** Only keep what's needed in the trading wallet

### 18.3 Deployment Checklist

**Pre-Launch:**

```
[ ] Wallet funded with USDC on Polygon
[ ] MATIC for gas (~$10 worth)
[ ] Configuration reviewed
[ ] Paper trading successful
[ ] Shadow mode successful
[ ] Monitoring set up
[ ] Alerting configured
[ ] Backup/recovery plan documented
```

**Launch:**

```
[ ] Start in shadow mode
[ ] Monitor for 24 hours
[ ] Enable trading with minimum size
[ ] Monitor for 24 hours
[ ] Gradually increase size
```

**Ongoing:**

```
[ ] Daily P&L review
[ ] Weekly parameter review
[ ] Monthly strategy review
[ ] Keep dependencies updated
[ ] Monitor for market changes
```

---

## 19. Phase 2: Market Scanner

### 19.1 Purpose

The Scanner answers: "Which markets should I trade right now?"

Account88888 (the bot we analyzed) trades mostly BTC but also ETH, SOL, and occasionally XRP. He's not randomly choosing — he's systematically finding where the edge is fattest.

**The insight:**

- BTC is most liquid but also most competitive
- Sometimes ETH has fatter margins when BTC is crowded
- SOL/XRP can have 4%+ margins during volatility
- The best market changes throughout the day

### 19.2 Scoring Dimensions

The Scanner scores each market on five dimensions:

**1. Liquidity (Weight: 25%)**

*Question:* Can we execute our target size without moving the market?

*Scoring:*
- Check order book depth at multiple levels
- Look at both YES and NO sides
- Use the thinner side as the constraint
- Score 0 if below minimum ($1,000), 1.0 if ideal ($10,000+)

**2. Margin (Weight: 35%)**

*Question:* How fat is the arbitrage spread?

*Scoring:*
- Calculate current YES + NO cost
- Compare to thresholds: 4%+ = excellent, 2% = good, 1% = marginal
- Non-linear scoring (reward fat margins disproportionately)
- Bonus for improving trend

**3. Volatility (Weight: 15%)**

*Question:* Is there enough action to create opportunities?

*Scoring:*
- Measure spot price volatility over last 5 minutes
- Sweet spot: ~0.5% volatility = good retail panic
- Too quiet (no opportunities) or too volatile (chaos) = lower score

**4. Competition (Weight: 15%)**

*Question:* How many other bots are we competing against?

*Scoring:*
- Measure order velocity (orders per minute)
- Check cancel/order ratio (high = bots cancelling)
- Tight spreads (near 1.00) = heavily arbed
- More competition = lower score

**5. Correlation (Weight: 10%)**

*Question:* Can we use NQ signals effectively?

*Scoring:*
- Look up historical correlation with NQ
- If we have strong NQ signal, prefer correlated assets
- BTC (0.70) scores higher than XRP (0.30) when NQ is moving

### 19.3 Allocation Algorithm

After scoring all markets:

1. **Filter:** Remove markets scoring below 0.3 (not worth trading)
2. **Normalize:** Calculate proportion of total score
3. **Cap:** No single market gets more than 60%
4. **Output:** Allocation percentages

**Example output:**

```
┌────────┬───────────┬────────┬──────────┬───────────┬───────────┬─────────┐
│ Asset  │ Liquidity │ Margin │ Volatil. │ Competit. │ Correlat. │ TOTAL   │
├────────┼───────────┼────────┼──────────┼───────────┼───────────┼─────────┤
│ ETH    │ 0.85      │ 0.92   │ 0.78     │ 0.65      │ 0.60      │ 0.81 ★  │
│ SOL    │ 0.55      │ 0.88   │ 0.85     │ 0.80      │ 0.50      │ 0.73    │
│ BTC    │ 0.95      │ 0.45   │ 0.42     │ 0.35      │ 0.70      │ 0.54    │
│ XRP    │ 0.30      │ 0.70   │ 0.60     │ 0.90      │ 0.30      │ 0.52    │
└────────┴───────────┴────────┴──────────┴───────────┴───────────┴─────────┘

ALLOCATION:
  ETH: 45% ($4,500)
  SOL: 35% ($3,500)
  BTC: 20% ($2,000)
  XRP: 0%  (liquidity below threshold)
```

### 19.4 Dynamic Rebalancing

The Scanner runs every 30 seconds and adjusts allocations:

```rust
impl Scanner {
    async fn run(&self) {
        let mut interval = tokio::time::interval(Duration::from_secs(30));
        
        loop {
            interval.tick().await;
            
            // Score all markets
            let scores = self.score_all_markets();
            
            // Calculate allocations
            let allocations = self.calculate_allocations(&scores);
            
            // Check for significant changes
            for (asset, new_alloc) in &allocations {
                let current = self.current_allocations.get(asset);
                let drift = (new_alloc - current).abs();
                
                if drift > self.rebalance_threshold {
                    log::info!(
                        "SCANNER: {} allocation {:.1}% -> {:.1}%",
                        asset, current * 100, new_alloc * 100
                    );
                    self.update_allocation(asset, *new_alloc);
                }
            }
        }
    }
}
```

### 19.5 Integration with Trading

The Scanner's allocations flow to position sizing:

```rust
fn calculate_position_size(
    &self,
    opportunity: &ArbOpportunity,
    available_capital: Decimal,
) -> Decimal {
    // Get allocation from Scanner
    let allocation = self.scanner.get_allocation(opportunity.asset);
    
    // This market can use at most this much capital
    let max_for_market = available_capital * allocation;
    
    // Apply other sizing factors within this budget
    self.size_within_budget(opportunity, max_for_market)
}
```

---

## Appendix A: Glossary

| Term | Definition |
|------|------------|
| **Arbitrage** | Profiting from price differences without taking directional risk |
| **Dutch Book** | Betting strategy that guarantees profit regardless of outcome |
| **Leg Risk** | Risk of one side of a paired trade failing |
| **Maker** | Order that provides liquidity (limit order sitting on book) |
| **Taker** | Order that takes liquidity (crossing the spread) |
| **CLOB** | Central Limit Order Book |
| **Shadow Bid** | Pre-computed order ready to fire instantly |
| **Price Chasing** | Dynamically adjusting limit prices to get filled |
| **Toxic Flow** | Liquidity designed to trap other traders |
| **Circuit Breaker** | Safety mechanism that halts trading on repeated failures |

---

## Appendix B: Quick Reference

**Key Thresholds:**

| Parameter | Value |
|-----------|-------|
| Minimum margin (early) | 2.5% |
| Minimum margin (late) | 0.5% |
| Max unhedged exposure | $500 |
| Stop trading before close | 30 seconds |
| Data staleness threshold | 1 second |
| Price chase step | $0.001 |

**Lifecycle Phases:**

| Phase | Time Remaining | Behavior |
|-------|----------------|----------|
| Accumulation | >2 min | Normal trading |
| Balancing | 30s - 2 min | Focus on equalizing |
| Closing | <30s | Emergency close |

**Command Line:**

```bash
# Paper trading
cargo run --release -- --paper

# Shadow mode (real prices, no execution)
cargo run --release -- --shadow

# Production
cargo run --release
```

---

*Document Version: 2.1*  
*Last Updated: January 2026*
