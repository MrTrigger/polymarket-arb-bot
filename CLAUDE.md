# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Polymarket 15-Minute Arbitrage Bot - HFT-optimized trading bot for crypto binary options arbitrage. Exploits mispricings when YES + NO shares sum to less than $1.00 on Polymarket's 15-minute up/down markets (BTC, ETH, SOL, XRP).

## Architecture

Two separate binaries in a Cargo workspace:

| Binary | Purpose |
|--------|---------|
| `poly-collect` | Data collection: live (WebSocket) or historical (API). Binance spot prices, Polymarket orderbooks & price history |
| `poly-bot` | Trading strategy execution with multiple modes (live, paper, shadow, backtest) |

Shared types and utilities live in `poly-common` crate.

### Key Design Principles

- **Lock-free hot path**: DashMap and atomics for shared state, no Mutex on trading path
- **Zero-copy parsing**: Minimize allocations in WebSocket message handling
- **Pre-hashed signing**: EIP-712 shadow bids must fire <2ms after primary fill
- **Fire-and-forget observability**: <10ns overhead on hot path via bounded channels

### Performance Targets

- WS message to state update: <10μs
- Arb detection: <1μs
- Circuit breaker check: <10ns
- Shadow bid fire: <2ms

## Build Commands

```bash
cargo build                    # Build all crates
cargo build -p poly-collect    # Build single crate
cargo check                    # Type check without building
cargo clippy -- -D warnings    # Lint (treat warnings as errors)
cargo test                     # Run all tests
cargo test -p poly-bot         # Test single crate
cargo test strategy::arb       # Run specific test
```

## Project Structure

```
crates/
├── poly-common/          # Shared types, ClickHouse client
│   └── src/
│       ├── types.rs      # CryptoAsset, Side, OrderBookLevel, MarketWindow
│       ├── clickhouse.rs # ClickHouse client wrapper
│       └── schema.sql    # Table definitions
├── poly-collect/         # Data collection binary
│   └── src/
│       ├── discovery.rs  # Market discovery via Gamma API
│       ├── binance.rs    # Binance WebSocket capture
│       ├── clob.rs       # Polymarket CLOB WebSocket
│       └── history.rs    # Historical data collection
└── poly-bot/             # Trading bot binary
    └── src/
        ├── config.rs       # BotConfig, ObservabilityConfig
        ├── state.rs        # GlobalState (DashMaps, atomics)
        ├── types.rs        # OrderBook, MarketState, Inventory
        ├── data_source/    # DataSource trait (live, replay)
        ├── executor/       # Executor trait + implementations
        │   ├── mod.rs            # Executor trait, PositionSnapshot
        │   ├── position_manager.rs # Common position/risk management
        │   ├── simulated.rs      # Paper trading & backtesting
        │   ├── live.rs           # Real Polymarket API execution
        │   ├── shadow.rs         # Shadow bid manager
        │   └── chase.rs          # Price chasing logic
        ├── strategy/       # Trading strategy
        │   ├── arb.rs      # Arbitrage detection
        │   ├── toxic.rs    # Toxic flow detection
        │   └── sizing.rs   # Position sizing
        ├── risk/           # Risk management
        │   └── circuit_breaker.rs
        ├── observability/  # Fire-and-forget capture
        └── mode/           # Live, paper, shadow, backtest modes
```

## Executor Architecture

The executor module uses a **common code** pattern for position and risk management:

```
┌─────────────────────────────────────────────────────────────────────┐
│                    EXECUTOR TRAIT (Common Interface)                 │
│  • place_order(), cancel_order(), settle_market()                   │
│  • market_exposure(), total_exposure(), remaining_capacity()        │
│  • get_position() -> PositionSnapshot                               │
└─────────────────────────────────────────────────────────────────────┘
                                │
┌───────────────────────────────┴───────────────────────────────────────┐
│                    POSITION MANAGER (Common Code)                     │
│  Single source of truth for position tracking and risk limits:       │
│  • check_limits() - enforce max_market_exposure, max_total_exposure  │
│  • record_fill() - track position (yes_shares, no_shares, cost_basis)│
│  • settle_market() - calculate PnL at market expiry                  │
└───────────────────────────────────────────────────────────────────────┘
                                │
            ┌───────────────────┴───────────────────┐
            ▼                                       ▼
┌─────────────────────────┐           ┌─────────────────────────┐
│   SimulatedExecutor     │           │     LiveExecutor        │
│   (THIN wrapper)        │           │     (THIN wrapper)      │
│   • Simulated fills     │           │   • EIP-712 signing     │
│   • Balance tracking    │           │   • REST API calls      │
│   • Stats (win/loss)    │           │   • Shadow bid firing   │
└─────────────────────────┘           └─────────────────────────┘
```

**Key principle**: Position/risk management is COMMON code. Only order placement differs between live and simulated execution.

## Critical Rules

### Financial Math
- **NEVER use f64 for prices or quantities** - use `rust_decimal::Decimal`
- All money math must be exact decimal arithmetic

### Concurrency
- Hot path must not acquire Mutex locks - use DashMap or atomics
- `can_trade()` check must be a single atomic load (~10ns)
- Observability uses `try_send()` to avoid blocking

### External Dependencies

| Crate | Purpose |
|-------|---------|
| `polymarket-client-sdk` | Official Polymarket client (auth, discovery, orders) |
| `tokio-tungstenite` | Custom WebSocket for Binance |
| `dashmap` | Lock-free concurrent hash maps |
| `rust_decimal` | Financial math |
| `clickhouse` | Data storage and replay |

## Data Storage (ClickHouse)

Tables: `orderbook_snapshots`, `orderbook_deltas`, `spot_prices`, `market_windows`, `price_history`, `trade_history`, `decisions`

## Task Tracking

Active tasks are in `tasks/tasklist.json`. Progress and learnings go in `tasks/progress.txt`.

**Priority**: Phase 1 (poly-collect) is urgent - start collecting data ASAP. Every day of delay = less backtest data.
