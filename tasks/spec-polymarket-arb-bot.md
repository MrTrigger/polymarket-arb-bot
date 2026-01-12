# Spec: Polymarket 15-Minute Arbitrage Bot

## Source
`spec/polymarket-arb-bot-spec-v2.2.md`

## Project Context

### Stack
- **Language:** Rust
- **Runtime:** Tokio async
- **Target:** HFT-optimized, lock-free hot path

### Project Structure (Workspace)
```
polymarket-arb/
├── Cargo.toml                    # Workspace root
├── crates/
│   ├── poly-common/              # Shared types, ClickHouse schema
│   ├── poly-collect/             # Data collector (PRIORITY 1)
│   ├── poly-import/              # Historical data importer
│   └── poly-bot/                 # Trading bot
├── config/
└── spec/
```

### External Dependencies
- `polymarket-client-sdk` - Official Polymarket Rust client
- `tokio-tungstenite` - Custom WebSocket (minimal overhead)
- `dashmap` - Lock-free concurrent hash maps
- `rust_decimal` - Financial math (no floats)
- `clickhouse` - Data storage and replay

### Key Performance Targets
| Operation | Target |
|-----------|--------|
| WS message → state | <10μs |
| Arb detection | <1μs |
| Circuit breaker check | <10ns |
| Shadow bid fire | <2ms |
| Observability overhead | <10ns (fire-and-forget) |

---

## Priority Order

```
WEEK 1: poly-collect running → Start collecting data
        ─────────────────────────────────────────────
              ↓ (runs continuously)

WEEK 2-4: poly-bot development (in parallel)
          ───────────────────────────────────
              ↓

WEEK 4+: Backtest with collected data → Live trading
         ────────────────────────────────────────────
```

---

## PHASE 1: Data Collector (`poly-collect`) - PRIORITY

**Goal:** Start capturing live market data ASAP. Every day of delay = less backtest data.

### 1.1 Workspace and shared crate setup
- **What:** Create Cargo workspace, `poly-common` crate with shared types
- **Where:** `Cargo.toml`, `crates/poly-common/`
- **Acceptance:** `cargo check` passes for workspace

##### Subtasks:
- [ ] Create workspace Cargo.toml
- [ ] Create `poly-common` crate
- [ ] Define shared types: `CryptoAsset`, `Side`, `OrderBookLevel`, `MarketWindow`
- [ ] Add dependencies: `rust_decimal`, `chrono`, `serde`, `clickhouse`

### 1.2 ClickHouse schema and client
- **What:** Define tables for raw data capture, implement client wrapper
- **Where:** `crates/poly-common/src/clickhouse.rs`
- **Acceptance:** Can connect to ClickHouse, create tables

##### Subtasks:
- [ ] Define `orderbook_snapshots` table schema
- [ ] Define `orderbook_deltas` table schema
- [ ] Define `spot_prices` table schema
- [ ] Define `market_windows` table schema (metadata)
- [ ] Implement `ClickHouseClient` wrapper with batch insert
- [ ] Add connection config (host, port, database)
- [ ] Add table creation/migration helper

**ClickHouse Schema:**
```sql
-- Order book snapshots (periodic, every 100ms)
CREATE TABLE orderbook_snapshots (
    timestamp DateTime64(3),
    event_id String,
    asset LowCardinality(String),
    token_id String,
    side LowCardinality(String),  -- 'yes' or 'no'
    asks Array(Tuple(price Decimal64(6), size Decimal64(4))),
    bids Array(Tuple(price Decimal64(6), size Decimal64(4))),
) ENGINE = MergeTree()
PARTITION BY toYYYYMMDD(timestamp)
ORDER BY (event_id, side, timestamp);

-- Order book deltas (every update from WebSocket)
CREATE TABLE orderbook_deltas (
    timestamp DateTime64(3),
    event_id String,
    asset LowCardinality(String),
    token_id String,
    side LowCardinality(String),
    price Decimal64(6),
    size Decimal64(4),  -- 0 = removed
) ENGINE = MergeTree()
PARTITION BY toYYYYMMDD(timestamp)
ORDER BY (event_id, side, timestamp);

-- Spot prices from Binance
CREATE TABLE spot_prices (
    timestamp DateTime64(3),
    asset LowCardinality(String),
    price Decimal64(4),
    quantity Decimal64(8),
) ENGINE = MergeTree()
PARTITION BY toYYYYMMDD(timestamp)
ORDER BY (asset, timestamp);

-- Market window metadata
CREATE TABLE market_windows (
    event_id String,
    asset LowCardinality(String),
    yes_token_id String,
    no_token_id String,
    window_start DateTime64(0),
    window_end DateTime64(0),
    discovered_at DateTime64(3),
) ENGINE = ReplacingMergeTree()
ORDER BY (event_id);
```

### 1.3 Market discovery for collector
- **What:** Find active 15-min markets to subscribe to
- **Where:** `crates/poly-collect/src/discovery.rs`
- **Builds on:** `polymarket-client-sdk` Gamma API
- **Acceptance:** Discovers BTC/ETH/SOL 15-min markets, extracts token IDs

##### Subtasks:
- [ ] Query Gamma API for active crypto events
- [ ] Filter to 15-minute up/down markets
- [ ] Extract YES/NO token IDs
- [ ] Track window timing (start, end)
- [ ] Store metadata to ClickHouse `market_windows` table
- [ ] Re-discover every 5 minutes

### 1.4 Binance WebSocket capture
- **What:** Minimal WebSocket client capturing spot prices
- **Where:** `crates/poly-collect/src/binance.rs`
- **Acceptance:** Captures BTC/ETH/SOL trades to ClickHouse

##### Subtasks:
- [ ] Connect to `wss://stream.binance.com:9443/ws`
- [ ] Subscribe to trade streams: `btcusdt@trade`, `ethusdt@trade`, `solusdt@trade`
- [ ] Parse trade messages (price, quantity, timestamp)
- [ ] Buffer writes (batch 100-1000 records)
- [ ] Flush to ClickHouse `spot_prices` table
- [ ] Implement reconnection with backoff
- [ ] Log connection status

### 1.5 Polymarket CLOB WebSocket capture
- **What:** Capture order book updates from Polymarket
- **Where:** `crates/poly-collect/src/clob.rs`
- **Acceptance:** Captures order book snapshots and deltas to ClickHouse

##### Subtasks:
- [ ] Connect to Polymarket CLOB WebSocket
- [ ] Subscribe to discovered market token IDs
- [ ] Handle book snapshot messages → `orderbook_snapshots`
- [ ] Handle delta/update messages → `orderbook_deltas`
- [ ] Periodic full snapshot capture (every 100ms or on interval)
- [ ] Buffer and batch writes
- [ ] Implement reconnection with backoff
- [ ] Re-subscribe when new markets discovered

### 1.6 Collector main binary
- **What:** Wire everything together, run continuously
- **Where:** `crates/poly-collect/src/main.rs`
- **Acceptance:** Runs 24/7, captures all data, handles disconnects

##### Subtasks:
- [ ] Parse CLI args (--assets, --clickhouse-url)
- [ ] Load config from TOML
- [ ] Spawn discovery task
- [ ] Spawn Binance capture task
- [ ] Spawn CLOB capture task
- [ ] Spawn flush task (periodic batch writes)
- [ ] Implement graceful shutdown (SIGTERM)
- [ ] Add health logging (records/sec, connection status)

**CLI:**
```bash
# Start collecting
poly-collect --assets BTC,ETH,SOL --config config/collect.toml

# With explicit ClickHouse URL
poly-collect --assets BTC --clickhouse-url http://localhost:8123
```

### 1.7 Deployment config
- **What:** Kubernetes manifests, config files
- **Where:** `deploy/`, `config/collect.toml`
- **Acceptance:** Can deploy to k8s cluster

##### Subtasks:
- [ ] Create `config/collect.toml` with defaults
- [ ] Create Dockerfile for poly-collect
- [ ] Create k8s Deployment manifest
- [ ] Add resource limits (memory, CPU)
- [ ] Add health check endpoint (optional HTTP)
- [ ] Document deployment steps

---

## PHASE 2: Historical Data Importer (`poly-import`)

**Goal:** Download available historical data from Polymarket API

### 2.1 Price history importer
- **What:** Download historical price data from Polymarket
- **Where:** `crates/poly-import/src/prices.rs`
- **Acceptance:** Can download and store price history

##### Subtasks:
- [ ] Implement Polymarket price history API client
- [ ] Parse date range from CLI
- [ ] Download price history for specified tokens
- [ ] Store to ClickHouse `price_history` table
- [ ] Handle pagination/rate limits
- [ ] Show progress

### 2.2 Trade history importer
- **What:** Download historical trades from Polymarket
- **Where:** `crates/poly-import/src/trades.rs`
- **Acceptance:** Can download and store trade history

##### Subtasks:
- [ ] Implement trade history API client
- [ ] Download trades for date range
- [ ] Store to ClickHouse `trade_history` table
- [ ] Handle pagination

### 2.3 Import CLI
- **What:** Main binary for importing historical data
- **Where:** `crates/poly-import/src/main.rs`
- **Acceptance:** Can run one-shot imports

##### Subtasks:
- [ ] Parse CLI: `poly-import prices --start X --end Y`
- [ ] Parse CLI: `poly-import trades --start X --end Y`
- [ ] Progress reporting
- [ ] Error handling and resume capability

---

## PHASE 3: Bot Core Infrastructure (`poly-bot`)

**Goal:** Shared state, types, and infrastructure for trading bot

### 3.1 Bot configuration system
- **What:** TOML config with env var overrides
- **Where:** `crates/poly-bot/src/config.rs`
- **Acceptance:** Loads config, validates, secrets from env

##### Subtasks:
- [ ] Define `BotConfig` struct (all parameters from spec section 15)
- [ ] Define `ObservabilityConfig` with enable/disable flags
- [ ] Create `config/bot.toml` with defaults
- [ ] Implement loading with env var overrides
- [ ] Add validation for required fields

### 3.2 Shared state (lock-free)
- **What:** `GlobalState` with DashMaps and atomics
- **Where:** `crates/poly-bot/src/state.rs`
- **Acceptance:** Lock-free reads on hot path

##### Subtasks:
- [ ] Define `GlobalState` struct
- [ ] Define `SharedMarketData` with DashMaps (prices, books, inventory, shadows)
- [ ] Define `ControlFlags` with AtomicBool (trading_enabled, circuit_breaker)
- [ ] Define `MetricsCounters` with AtomicU64
- [ ] Implement `#[inline(always)] can_trade()` - two atomic loads
- [ ] Unit tests for concurrent access

### 3.3 Market state types
- **What:** Order book, market window, inventory types
- **Where:** `crates/poly-bot/src/types.rs`
- **Acceptance:** Types match spec, use Decimal not f64

##### Subtasks:
- [ ] Define `OrderBook` with bid/ask levels
- [ ] Define `MarketState` with derived fields (combined_cost, arb_margin)
- [ ] Define `Inventory` struct (yes/no shares, cost basis)
- [ ] Define `InventoryState` enum (Balanced, Skewed, Exposed, Crisis)
- [ ] Implement inventory state transitions

### 3.4 Data source trait
- **What:** Abstract data source for live/replay modes
- **Where:** `crates/poly-bot/src/data_source.rs`
- **Acceptance:** Same strategy code works with live or replay data

##### Subtasks:
- [ ] Define `DataSource` trait with `async fn next_event()`
- [ ] Define `MarketEvent` enum (PriceUpdate, BookUpdate, Fill, etc.)
- [ ] Implement `LiveDataSource` (WebSocket connections)
- [ ] Implement `ReplayDataSource` (reads from ClickHouse)

### 3.5 Executor trait
- **What:** Abstract executor for live/paper/backtest modes
- **Where:** `crates/poly-bot/src/executor.rs`
- **Acceptance:** Same strategy code works with real or simulated execution

##### Subtasks:
- [ ] Define `Executor` trait with `place_order()`, `cancel_order()`
- [ ] Define `OrderResult` enum (Filled, PartialFill, Rejected, etc.)
- [ ] Stub `LiveExecutor` (will implement in Phase 5)
- [ ] Implement `PaperExecutor` (simulates fills with configurable probability)
- [ ] Implement `BacktestExecutor` (simulates against historical book state)

---

## PHASE 4: Strategy Engine

**Goal:** Arbitrage detection and decision making

### 4.1 Arbitrage detection
- **What:** Core arb detection: YES + NO < threshold
- **Where:** `crates/poly-bot/src/strategy/arb.rs`
- **Acceptance:** Detects opportunities with time-based thresholds

##### Subtasks:
- [ ] Implement margin calculation
- [ ] Implement time-based threshold selection (2.5% early, 1.5% mid, 0.5% late)
- [ ] Implement confidence scoring
- [ ] Create `ArbOpportunity` struct
- [ ] Unit tests with mock market states

### 4.2 Toxic flow detection (basic)
- **What:** Flag suspicious order book patterns
- **Where:** `crates/poly-bot/src/strategy/toxic.rs`
- **Acceptance:** Detects large sudden orders

##### Subtasks:
- [ ] Track rolling average order size
- [ ] Detect orders >50x average
- [ ] Detect sudden appearance (<500ms)
- [ ] Return `ToxicFlowWarning` with severity

### 4.3 Position sizing
- **What:** Calculate trade size based on opportunity quality
- **Where:** `crates/poly-bot/src/strategy/sizing.rs`
- **Acceptance:** Sizes respect limits, scale with margin quality

##### Subtasks:
- [ ] Implement basic fixed sizing for MVP
- [ ] Respect max position per market
- [ ] Respect max total exposure
- [ ] Account for inventory imbalance

### 4.4 Strategy loop
- **What:** Main strategy task processing market events
- **Where:** `crates/poly-bot/src/strategy/mod.rs`
- **Acceptance:** Processes events, generates trade decisions

##### Subtasks:
- [ ] Implement event loop receiving from DataSource
- [ ] Check `can_trade()` (single atomic check)
- [ ] Run arb detection
- [ ] Run toxic flow check
- [ ] Calculate size
- [ ] Send decisions to executor
- [ ] Fire-and-forget to observability channel

---

## PHASE 5: Order Execution

**Goal:** Execute trades with shadow bids and price chasing

### 5.1 Shadow bid manager
- **What:** Pre-computed second leg orders
- **Where:** `crates/poly-bot/src/executor/shadow.rs`
- **Acceptance:** Shadow fires within 2ms of primary fill

##### Subtasks:
- [ ] Define `ShadowOrder` struct
- [ ] Implement `PrehashedOrder` for fast signing (spec 8.4.1)
- [ ] Pre-compute hashes at shadow creation time
- [ ] Implement `fire()` with fast signing path
- [ ] Store shadows in DashMap by (event_id, side)

### 5.2 Price chasing
- **What:** Dynamic limit price adjustment
- **Where:** `crates/poly-bot/src/executor/chase.rs`
- **Acceptance:** Chases price up to ceiling, handles timeout

##### Subtasks:
- [ ] Implement ceiling calculation
- [ ] Implement chase loop (check, bump, repeat)
- [ ] Handle partial fills
- [ ] Configurable step size, interval, max time

### 5.3 Live executor
- **What:** Real order submission via polymarket-client-sdk
- **Where:** `crates/poly-bot/src/executor/live.rs`
- **Builds on:** `polymarket-client-sdk`
- **Acceptance:** Can place/cancel real orders

##### Subtasks:
- [ ] Initialize polymarket-client-sdk with wallet
- [ ] Implement `place_order()` with limit orders
- [ ] Implement `cancel_order()`
- [ ] Implement fill tracking (WebSocket or polling)
- [ ] Fire shadow on primary fill
- [ ] Update inventory on fills

### 5.4 Fill handling
- **What:** Process fills, update state
- **Where:** `crates/poly-bot/src/executor/mod.rs`
- **Acceptance:** Fills update inventory, trigger shadows

##### Subtasks:
- [ ] Parse fill events
- [ ] Update inventory
- [ ] Fire shadow bid
- [ ] Handle partial fills
- [ ] Send to observability channel

---

## PHASE 6: Risk Management

**Goal:** Protect capital with limits and circuit breakers

### 6.1 Pre-trade risk checks
- **What:** Validate before execution
- **Where:** `crates/poly-bot/src/risk/checks.rs`
- **Acceptance:** Rejects trades exceeding limits

##### Subtasks:
- [ ] Time remaining check (>30s)
- [ ] Position size check
- [ ] Total exposure check
- [ ] Imbalance check
- [ ] Daily loss check
- [ ] Toxic severity check

### 6.2 Circuit breakers
- **What:** Halt trading on repeated failures
- **Where:** `crates/poly-bot/src/risk/circuit_breaker.rs`
- **Acceptance:** Trips after 3 failures, auto-resets

##### Subtasks:
- [ ] Atomic failure counter
- [ ] `trip()` sets atomic flag
- [ ] `can_trade()` is single atomic load
- [ ] Cooldown timer for auto-reset

### 6.3 Leg risk handling
- **What:** Handle one-leg-filled situations
- **Where:** `crates/poly-bot/src/risk/leg_risk.rs`
- **Acceptance:** Emergency close or aggressive chase

##### Subtasks:
- [ ] Detect leg risk condition
- [ ] Calculate exposure
- [ ] Emergency IOC close if exposure > limit
- [ ] Aggressive chase if time pressure

---

## PHASE 7: Observability (Zero Hot-Path Impact)

**Goal:** Decision capture without affecting performance

### 7.1 Decision snapshot types
- **What:** Minimal struct for hot path capture
- **Where:** `crates/poly-bot/src/observability/types.rs`
- **Acceptance:** All primitive types, no allocations

##### Subtasks:
- [ ] Define `DecisionSnapshot` with only primitives (u64, u8, etc.)
- [ ] Pre-hash event_id to u64 for hot path
- [ ] Define `DecisionContext` (full version, built async)
- [ ] Define `Counterfactual` for post-analysis

### 7.2 Fire-and-forget capture
- **What:** Non-blocking channel send from hot path
- **Where:** `crates/poly-bot/src/observability/capture.rs`
- **Acceptance:** <10ns overhead, drops if buffer full

##### Subtasks:
- [ ] Create bounded mpsc channel
- [ ] Implement `try_send()` wrapper (non-blocking)
- [ ] Config flag to enable/disable
- [ ] Inline the hot path check

### 7.3 Async enrichment and storage
- **What:** Background task that enriches and stores decisions
- **Where:** `crates/poly-bot/src/observability/processor.rs`
- **Acceptance:** Batches writes to ClickHouse

##### Subtasks:
- [ ] Receive from channel
- [ ] Enrich snapshot with full context (strings, etc.)
- [ ] Buffer records
- [ ] Batch write to ClickHouse
- [ ] Handle backpressure (drop oldest)

### 7.4 Counterfactual analysis
- **What:** Post-settlement analysis of decisions
- **Where:** `crates/poly-bot/src/observability/counterfactual.rs`
- **Acceptance:** Calculates what-if for skipped opportunities

##### Subtasks:
- [ ] Track skipped opportunities
- [ ] After window settles, calculate hypothetical P&L
- [ ] Store counterfactuals to ClickHouse
- [ ] Flag bad decisions for review

### 7.5 Anomaly detection
- **What:** Flag unusual situations for review
- **Where:** `crates/poly-bot/src/observability/anomaly.rs`
- **Acceptance:** Captures edge cases

##### Subtasks:
- [ ] Define `AnomalyType` enum
- [ ] Detect extreme margins, flash crashes, latency spikes
- [ ] Store anomalies with full context
- [ ] Optional alerting (log, webhook)

**ClickHouse Schema for Observability:**
```sql
-- Full decision context (L2)
CREATE TABLE decisions (
    timestamp DateTime64(3),
    decision_id UUID,
    event_id String,
    asset LowCardinality(String),

    -- Market state
    yes_ask Decimal64(6),
    no_ask Decimal64(6),
    combined_cost Decimal64(6),
    gross_margin Decimal64(6),
    spot_price Decimal64(4),
    time_remaining_secs UInt32,

    -- Bot state
    inventory_yes Decimal64(4),
    inventory_no Decimal64(4),
    inventory_state LowCardinality(String),

    -- Decision
    action LowCardinality(String),
    reason_code LowCardinality(String),
    reason_detail String,
    size Decimal64(4),

    -- Execution (nullable, filled later)
    executed_price Nullable(Decimal64(6)),
    slippage Nullable(Decimal64(6)),
    latency_us Nullable(UInt32),

) ENGINE = MergeTree()
PARTITION BY toYYYYMMDD(timestamp)
ORDER BY (event_id, timestamp);

-- Counterfactuals (L3)
CREATE TABLE counterfactuals (
    timestamp DateTime64(3),
    decision_id UUID,
    event_id String,

    actual_action LowCardinality(String),
    actual_pnl Decimal64(4),

    hypothetical_action LowCardinality(String),
    hypothetical_pnl Decimal64(4),

    regret Decimal64(4),  -- hypothetical - actual
    decision_quality LowCardinality(String),  -- Good, Bad, Neutral

) ENGINE = MergeTree()
PARTITION BY toYYYYMMDD(timestamp)
ORDER BY (event_id, timestamp);

-- Anomalies (L4)
CREATE TABLE anomalies (
    timestamp DateTime64(3),
    anomaly_type LowCardinality(String),
    severity LowCardinality(String),
    event_id Nullable(String),
    description String,
    context String,  -- JSON blob with full context

) ENGINE = MergeTree()
PARTITION BY toYYYYMMDD(timestamp)
ORDER BY (anomaly_type, timestamp);
```

---

## PHASE 8: Bot Modes and Main Binary

**Goal:** Wire everything together with mode switching

### 8.1 Live mode
- **What:** Real data, real execution
- **Where:** `crates/poly-bot/src/mode/live.rs`
- **Acceptance:** Trades real money

##### Subtasks:
- [ ] Wire LiveDataSource + LiveExecutor
- [ ] Full observability enabled
- [ ] Graceful shutdown saves state

### 8.2 Paper mode
- **What:** Real data, simulated execution
- **Where:** `crates/poly-bot/src/mode/paper.rs`
- **Acceptance:** Validates strategy with real prices

##### Subtasks:
- [ ] Wire LiveDataSource + PaperExecutor
- [ ] Simulated fills with configurable latency
- [ ] Full observability enabled

### 8.3 Shadow mode
- **What:** Real data, log only
- **Where:** `crates/poly-bot/src/mode/shadow.rs`
- **Acceptance:** Validates data feeds work

##### Subtasks:
- [ ] Wire LiveDataSource + NoOpExecutor
- [ ] Log all detected opportunities
- [ ] Validate feed connectivity

### 8.4 Backtest mode
- **What:** Replay historical data
- **Where:** `crates/poly-bot/src/mode/backtest.rs`
- **Acceptance:** Runs strategy against ClickHouse data

##### Subtasks:
- [ ] Load data from ClickHouse for date range
- [ ] Wire ReplayDataSource + BacktestExecutor
- [ ] Speed control (1x, 100x, max)
- [ ] Generate P&L report
- [ ] Support parameter sweeps

### 8.5 Main binary
- **What:** CLI entry point with mode selection
- **Where:** `crates/poly-bot/src/main.rs`
- **Acceptance:** All modes work from CLI

##### Subtasks:
- [ ] Parse CLI args (--mode, --config, --start, --end, etc.)
- [ ] Load config
- [ ] Initialize selected mode
- [ ] Run main loop
- [ ] Graceful shutdown

**CLI:**
```bash
# Live trading
poly-bot --mode live --config config/bot.toml

# Paper trading
poly-bot --mode paper

# Shadow mode
poly-bot --mode shadow

# Backtest
poly-bot --mode backtest --start 2026-01-01 --end 2026-01-07

# Backtest with speed multiplier
poly-bot --mode backtest --start 2026-01-01 --end 2026-01-07 --speed 100

# Parameter sweep
poly-bot --mode backtest --start 2026-01-01 --end 2026-01-07 \
    --sweep "min_margin=0.01:0.03:0.005"
```

---

## PHASE 9: Testing

**Goal:** Validate correctness

### 9.1 Unit tests
- **What:** Test individual functions
- **Where:** Throughout crates
- **Acceptance:** Core logic covered

##### Subtasks:
- [ ] Arb detection tests
- [ ] Inventory state tests
- [ ] Circuit breaker tests
- [ ] Toxic flow tests
- [ ] Config loading tests

### 9.2 Integration tests
- **What:** Test component interactions
- **Where:** `tests/`
- **Acceptance:** End-to-end flow works

##### Subtasks:
- [ ] Strategy + mock executor test
- [ ] Replay data source test
- [ ] Observability pipeline test

### 9.3 Backtest validation
- **What:** Run backtests, validate results make sense
- **Where:** Manual + CI
- **Acceptance:** P&L calculations are correct

##### Subtasks:
- [ ] Run backtest on collected data
- [ ] Verify P&L matches expected
- [ ] Check for obvious bugs (negative prices, etc.)

---

## Integration Points

- **Polymarket CLOB API:** Orders, fills via `polymarket-client-sdk`
- **Polymarket Gamma API:** Market discovery via `polymarket-client-sdk`
- **Polymarket CLOB WebSocket:** Order book streaming
- **Binance WebSocket:** Spot prices (custom client)
- **ClickHouse:** All data storage (raw data, decisions, counterfactuals)
- **Postgres:** Optional, for durable position snapshots

---

## Deployment Order

1. **Deploy ClickHouse** (if not already available in cluster)
2. **Deploy poly-collect** → Start collecting data immediately
3. **Develop poly-bot** (weeks 2-4)
4. **Run backtests** with collected data
5. **Deploy poly-bot --mode paper** → Validate with real prices
6. **Deploy poly-bot --mode live** → Start trading

---

## Phase 2 Features (Deferred)

- Multi-market scanner with scoring (spec section 19)
- NQ cross-asset signals (spec section 7.3)
- Advanced toxic flow detection
- Dynamic position sizing
- Grafana dashboards
- A/B experiment framework
