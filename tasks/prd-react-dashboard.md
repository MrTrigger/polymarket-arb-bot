# PRD: React Trading Dashboard

## Introduction

The Polymarket arbitrage bot needs a real-time visualization frontend to monitor operations during paper trading and live execution. Currently, operators must rely on logs and ClickHouse queries to understand bot behavior. A dedicated dashboard will provide instant visibility into performance, market conditions, and trading decisions.

This dashboard must follow the bot's core principle: **never impact the critical trading path**. The frontend communication layer must use fire-and-forget patterns identical to the existing observability system.

## Goals

1. **Real-time visibility**: Display live bot state, metrics, and market data with <100ms latency
2. **Multi-market monitoring**: Show aggregate performance plus drill-down into individual markets
3. **Zero hot-path impact**: WebSocket broadcast must not slow the trading engine (<10ns overhead)
4. **Historical context**: Display PnL graphs, equity curves, and trade history for the current session
5. **Actionable insights**: Surface arb opportunities, toxic flow events, and circuit breaker status

## User Stories

### US-001: WebSocket Server Infrastructure

**Description:** As a developer, I want a WebSocket server in poly-bot that broadcasts state updates without affecting trading performance.

**Acceptance Criteria:**
- [ ] `cargo clippy -p poly-bot -- -D warnings` passes
- [ ] `cargo test -p poly-bot` passes
- [ ] WebSocket server starts on configurable port (default 3001)
- [ ] State broadcasts use bounded channel with `try_send()` (fire-and-forget)
- [ ] Channel backpressure drops messages rather than blocking
- [ ] Benchmark confirms <10ns overhead on hot path for state capture

**Technical Notes:**
- Follow the pattern in `observability/capture.rs`
- Use `tokio-tungstenite` for WebSocket server
- Broadcast JSON-serialized `DashboardState` snapshots at configurable interval (default 100ms)

---

### US-002: Frontend Project Setup

**Description:** As a developer, I want a properly configured Bun + React + TypeScript project with shadcn/ui.

**Acceptance Criteria:**
- [ ] `bun install` succeeds in `frontend/` directory
- [ ] `bun run typecheck` passes (0 errors)
- [ ] `bun run build` produces static files in `frontend/dist/`
- [ ] `bun run dev` starts development server
- [ ] shadcn/ui components are available
- [ ] TailwindCSS is configured
- [ ] Lightweight Charts package is installed

**Technical Notes:**
- Create `frontend/` directory at workspace root
- Use Vite as bundler (works with Bun)
- Configure path aliases (@/components, etc.)

---

### US-003: WebSocket Connection Management

**Description:** As a frontend, I want to maintain a persistent WebSocket connection to the bot with automatic reconnection.

**Acceptance Criteria:**
- [ ] `bun run typecheck` passes
- [ ] WebSocket connects to configurable URL (default ws://localhost:3001)
- [ ] Auto-reconnects on disconnect with exponential backoff
- [ ] Connection status displayed in UI header
- [ ] Handles JSON parsing errors gracefully

---

### US-004: Main Dashboard - Overview Metrics

**Description:** As a trader, I want to see aggregate bot performance metrics at a glance.

**Acceptance Criteria:**
- [ ] `bun run typecheck` passes
- [ ] Dashboard visible at `/` route
- [ ] Displays key metrics cards:
  - Session P&L (USDC)
  - Total Volume
  - Trades Executed / Failed / Skipped
  - Win Rate percentage
  - Current Exposure
- [ ] Metrics update in real-time (<500ms after bot state change)
- [ ] Positive P&L shown in green, negative in red

---

### US-005: Main Dashboard - Equity Curve Chart

**Description:** As a trader, I want to see my session equity curve over time.

**Acceptance Criteria:**
- [ ] `bun run typecheck` passes
- [ ] Lightweight Charts line chart displays cumulative P&L over time
- [ ] X-axis shows time, Y-axis shows USDC value
- [ ] Chart updates in real-time as new data arrives
- [ ] Crosshair shows exact value on hover
- [ ] Chart auto-scales to fit data

---

### US-006: Main Dashboard - Active Markets Grid

**Description:** As a trader, I want to see all markets the bot is currently trading.

**Acceptance Criteria:**
- [ ] `bun run typecheck` passes
- [ ] Grid/table shows all active market windows
- [ ] Each row displays:
  - Asset (BTC, ETH, SOL, XRP)
  - Strike price
  - Time remaining
  - Current spot price
  - YES/NO best bid/ask
  - Arb spread (if any)
  - Position size
- [ ] Rows are clickable to navigate to market detail view
- [ ] Rows highlight when arb opportunity exists

---

### US-007: Main Dashboard - Circuit Breaker Status

**Description:** As a trader, I want to see the circuit breaker and risk status.

**Acceptance Criteria:**
- [ ] `bun run typecheck` passes
- [ ] Status indicator shows: Trading Enabled / Disabled / Circuit Breaker Tripped
- [ ] Green for enabled, yellow for disabled, red for tripped
- [ ] Shows consecutive failure count
- [ ] Shows cooldown remaining if tripped

---

### US-008: Market Detail View - Price Chart

**Description:** As a trader, I want to see a detailed price chart for a selected market with bot actions overlaid.

**Acceptance Criteria:**
- [ ] `bun run typecheck` passes
- [ ] Market detail view accessible at `/market/:eventId`
- [ ] Lightweight Charts candlestick or line chart shows spot price history
- [ ] Strike price shown as horizontal line
- [ ] Trade markers overlay on chart:
  - Green triangles for buys
  - Red triangles for sells
  - Different markers for YES vs NO
- [ ] Window start/end boundaries visible

---

### US-009: Market Detail View - Order Book Display

**Description:** As a trader, I want to see the current order book state for YES and NO tokens.

**Acceptance Criteria:**
- [ ] `bun run typecheck` passes
- [ ] Displays YES token order book (bid/ask with sizes)
- [ ] Displays NO token order book (bid/ask with sizes)
- [ ] Shows spread for each side
- [ ] Shows combined arb calculation: YES_ask + NO_ask vs $1.00
- [ ] Highlights when arb exists

---

### US-010: Market Detail View - Position & P&L

**Description:** As a trader, I want to see my position and P&L for the selected market.

**Acceptance Criteria:**
- [ ] `bun run typecheck` passes
- [ ] Shows YES shares held and cost basis
- [ ] Shows NO shares held and cost basis
- [ ] Shows imbalance ratio and inventory state (Balanced/Skewed/Exposed/Crisis)
- [ ] Shows realized P&L for this market
- [ ] Shows unrealized P&L based on current prices

---

### US-011: Market Detail View - Trades List

**Description:** As a trader, I want to see all trades executed in the selected market.

**Acceptance Criteria:**
- [ ] `bun run typecheck` passes
- [ ] Table shows recent trades for this market
- [ ] Columns: Time, Side (YES/NO), Action (BUY/SELL), Price, Size, P&L
- [ ] Sorted by time descending (newest first)
- [ ] Color-coded by outcome (profit green, loss red)

---

### US-012: Log Window

**Description:** As a trader, I want to see real-time log output from the bot.

**Acceptance Criteria:**
- [ ] `bun run typecheck` passes
- [ ] Log panel visible on dashboard (collapsible)
- [ ] Displays last N log entries (configurable, default 100)
- [ ] Color-coded by log level (ERROR=red, WARN=yellow, INFO=white, DEBUG=gray)
- [ ] Auto-scrolls to newest entry
- [ ] Filter by log level
- [ ] Search/filter by text

---

### US-013: Session Metrics Panel

**Description:** As a trader, I want to see detailed session statistics.

**Acceptance Criteria:**
- [ ] `bun run typecheck` passes
- [ ] Panel shows:
  - Session start time and duration
  - Events processed count
  - Opportunities detected count
  - Trade execution rate (trades per minute)
  - Average trade size
  - Shadow orders fired/filled ratio
- [ ] Updates in real-time

---

### US-014: Anomaly Alerts

**Description:** As a trader, I want to be notified of detected anomalies.

**Acceptance Criteria:**
- [ ] `bun run typecheck` passes
- [ ] Alert banner appears when anomaly detected
- [ ] Shows anomaly type, severity, and message
- [ ] Dismissable alerts
- [ ] Alert history accessible
- [ ] Critical anomalies highlighted

---

### US-015: Dark Theme

**Description:** As a trader, I want a dark theme suitable for trading environments.

**Acceptance Criteria:**
- [ ] `bun run typecheck` passes
- [ ] Dark theme is default
- [ ] Colors optimized for readability
- [ ] Charts use dark theme configuration
- [ ] No bright flashes or jarring transitions

---

### US-016: Static Build Integration

**Description:** As a developer, I want the production build to be servable from poly-bot.

**Acceptance Criteria:**
- [ ] `bun run build` produces optimized static files
- [ ] Build output is in `frontend/dist/`
- [ ] Assets are hashed for cache busting
- [ ] Index.html properly references all assets
- [ ] poly-bot can optionally serve static files (feature flag)

---

### US-017: Backend Dashboard State Serialization

**Description:** As a developer, I want a `DashboardState` struct that aggregates all data needed by the frontend.

**Acceptance Criteria:**
- [ ] `cargo clippy -p poly-bot -- -D warnings` passes
- [ ] `cargo test -p poly-bot` passes
- [ ] `DashboardState` struct contains:
  - Metrics snapshot
  - Active markets list
  - Recent trades list
  - Order book summaries
  - Inventory positions
  - Control flags (trading enabled, circuit breaker)
  - Recent log entries
  - Anomaly alerts
- [ ] Serializes to JSON via serde
- [ ] Snapshot creation is lock-free (reads from DashMaps)

---

### US-018: Historical Data for Charts

**Description:** As a frontend, I want historical data for charts (not just real-time updates).

**Acceptance Criteria:**
- [ ] `cargo test -p poly-bot` passes
- [ ] WebSocket sends initial snapshot with historical data on connection
- [ ] Historical equity curve points (last N minutes)
- [ ] Historical price data for each market
- [ ] Trade history for session
- [ ] Configurable history depth

---

## Functional Requirements

### FR-001: WebSocket Protocol
- Server broadcasts JSON messages at configurable interval
- Message types: `snapshot`, `trade`, `log`, `anomaly`, `marketUpdate`
- Clients receive full state on connect, then incremental updates
- Heartbeat/ping to detect dead connections

### FR-002: Dashboard Layout
- Responsive grid layout (desktop-optimized)
- Collapsible panels for logs and secondary info
- Navigation between main dashboard and market detail views
- Persistent connection status indicator

### FR-003: Chart Configuration
- Lightweight Charts with dark theme
- Time axis in local timezone
- Configurable time ranges (5m, 15m, 1h, session)
- Tooltips with precise values

### FR-004: Performance
- React components use memoization to prevent unnecessary re-renders
- Virtualized lists for large trade histories
- Debounced chart updates to prevent flicker
- Lazy load market detail data

## Non-Goals

- **Mobile responsiveness**: Desktop-only for initial release
- **Authentication**: Dashboard is local-only, no auth needed
- **Historical backtests**: Only live/paper session data
- **Trade execution from UI**: View-only, no trading controls
- **Multiple sessions**: Single session per dashboard instance
- **Persistent storage**: No local storage of dashboard state (frontend-side)

---

## Backend Persistence Layer

The dashboard requires persistent storage of metrics and events for:
- Equity curve visualization (historical P&L)
- Trade history and analysis
- Session comparison and analytics
- Post-session review

### New ClickHouse Tables

#### US-019: Sessions Table

**Description:** As a trader, I want sessions tracked so I can review historical performance.

**Acceptance Criteria:**
- [ ] `cargo clippy -p poly-bot -- -D warnings` passes
- [ ] `cargo test -p poly-bot` passes
- [ ] Session record created on bot startup
- [ ] Session record updated on shutdown (or crash recovery)
- [ ] Query: `SELECT * FROM sessions WHERE mode = 'paper' ORDER BY start_time DESC LIMIT 10`

**Schema:**
```sql
CREATE TABLE IF NOT EXISTS sessions (
    session_id UUID,
    mode LowCardinality(String),           -- 'live', 'paper', 'shadow', 'backtest'
    start_time DateTime64(3, 'UTC'),
    end_time Nullable(DateTime64(3, 'UTC')),
    config_hash String,                     -- SHA256 of config for reproducibility
    -- Aggregates (updated periodically and on shutdown)
    total_pnl Decimal(18, 8),
    total_volume Decimal(18, 8),
    trades_executed UInt32,
    trades_failed UInt32,
    trades_skipped UInt32,
    opportunities_detected UInt64,
    events_processed UInt64,
    markets_traded Array(String),           -- List of event_ids
    exit_reason LowCardinality(String)      -- 'graceful', 'crash', 'circuit_breaker', 'manual'
) ENGINE = ReplacingMergeTree(start_time)
ORDER BY (session_id)
PARTITION BY toYYYYMM(start_time);
```

---

#### US-020: Bot Trades Table

**Description:** As a trader, I want all trades executed by the bot stored for analysis.

**Acceptance Criteria:**
- [ ] `cargo clippy -p poly-bot -- -D warnings` passes
- [ ] `cargo test -p poly-bot` passes
- [ ] Trade record inserted after each fill
- [ ] Includes both legs of arb trades (YES and NO)
- [ ] Links to decision that triggered the trade

**Schema:**
```sql
CREATE TABLE IF NOT EXISTS bot_trades (
    trade_id UUID,
    session_id UUID,
    decision_id UInt64,
    event_id String,
    token_id String,
    outcome LowCardinality(String),         -- 'YES', 'NO'
    side LowCardinality(String),            -- 'BUY', 'SELL'
    order_type LowCardinality(String),      -- 'MARKET', 'LIMIT', 'SHADOW'
    -- Order details
    requested_price Decimal(18, 8),
    requested_size Decimal(18, 8),
    -- Fill details
    fill_price Decimal(18, 8),
    fill_size Decimal(18, 8),
    slippage_bps Int32,
    -- Costs
    fees Decimal(18, 8),
    total_cost Decimal(18, 8),
    -- Timing
    order_time DateTime64(3, 'UTC'),
    fill_time DateTime64(3, 'UTC'),
    latency_ms UInt32,
    -- Context
    spot_price_at_fill Decimal(18, 8),
    arb_margin_at_fill Decimal(18, 8),
    -- Status
    status LowCardinality(String)           -- 'FILLED', 'PARTIAL', 'CANCELLED', 'FAILED'
) ENGINE = MergeTree()
ORDER BY (session_id, fill_time, trade_id)
PARTITION BY toYYYYMMDD(fill_time)
TTL toDate(fill_time) + INTERVAL 365 DAY;
```

---

#### US-021: PnL Snapshots Table

**Description:** As a trader, I want periodic P&L snapshots for equity curve visualization.

**Acceptance Criteria:**
- [ ] `cargo clippy -p poly-bot -- -D warnings` passes
- [ ] `cargo test -p poly-bot` passes
- [ ] Snapshot inserted every N seconds (configurable, default 10s)
- [ ] Snapshot inserted after each trade fill
- [ ] Frontend can query for equity curve data

**Schema:**
```sql
CREATE TABLE IF NOT EXISTS pnl_snapshots (
    session_id UUID,
    timestamp DateTime64(3, 'UTC'),
    trigger LowCardinality(String),         -- 'periodic', 'trade', 'settlement'
    -- P&L
    realized_pnl Decimal(18, 8),
    unrealized_pnl Decimal(18, 8),
    total_pnl Decimal(18, 8),
    -- Exposure
    total_exposure Decimal(18, 8),
    yes_exposure Decimal(18, 8),
    no_exposure Decimal(18, 8),
    -- Cumulative
    cumulative_volume Decimal(18, 8),
    cumulative_fees Decimal(18, 8),
    trade_count UInt32,
    -- Risk
    max_drawdown Decimal(18, 8),
    current_drawdown Decimal(18, 8)
) ENGINE = MergeTree()
ORDER BY (session_id, timestamp)
PARTITION BY toYYYYMMDD(timestamp)
TTL toDate(timestamp) + INTERVAL 180 DAY;
```

---

#### US-022: Structured Logs Table

**Description:** As a trader, I want searchable structured logs for debugging and analysis.

**Acceptance Criteria:**
- [ ] `cargo clippy -p poly-bot -- -D warnings` passes
- [ ] `cargo test -p poly-bot` passes
- [ ] Logs captured via fire-and-forget channel (like observability)
- [ ] Frontend can query logs by level, time range, event_id
- [ ] No impact on hot path (<10ns overhead)

**Schema:**
```sql
CREATE TABLE IF NOT EXISTS structured_logs (
    session_id UUID,
    timestamp DateTime64(3, 'UTC'),
    level LowCardinality(String),           -- 'ERROR', 'WARN', 'INFO', 'DEBUG', 'TRACE'
    target String,                          -- Module path (e.g., 'poly_bot::strategy::arb')
    message String,
    -- Optional context
    event_id Nullable(String),
    token_id Nullable(String),
    trade_id Nullable(UUID),
    -- Structured fields (JSON for flexibility)
    fields String                           -- JSON object with additional context
) ENGINE = MergeTree()
ORDER BY (session_id, timestamp)
PARTITION BY toYYYYMMDD(timestamp)
TTL toDate(timestamp) + INTERVAL 30 DAY;
```

---

#### US-023: Market Sessions Table

**Description:** As a trader, I want per-market metrics within a session.

**Acceptance Criteria:**
- [ ] `cargo clippy -p poly-bot -- -D warnings` passes
- [ ] `cargo test -p poly-bot` passes
- [ ] Record created when bot starts trading a market window
- [ ] Updated on each trade and at window close
- [ ] Enables market-level P&L drill-down

**Schema:**
```sql
CREATE TABLE IF NOT EXISTS market_sessions (
    session_id UUID,
    event_id String,
    asset LowCardinality(String),
    strike_price Decimal(18, 8),
    window_start DateTime64(3, 'UTC'),
    window_end DateTime64(3, 'UTC'),
    -- Entry
    first_trade_time Nullable(DateTime64(3, 'UTC')),
    -- Position
    yes_shares Decimal(18, 8),
    no_shares Decimal(18, 8),
    yes_cost_basis Decimal(18, 8),
    no_cost_basis Decimal(18, 8),
    -- P&L
    realized_pnl Decimal(18, 8),
    unrealized_pnl Decimal(18, 8),
    -- Stats
    trades_count UInt32,
    opportunities_count UInt32,
    skipped_count UInt32,
    volume Decimal(18, 8),
    fees Decimal(18, 8),
    -- Outcome
    settlement_outcome Nullable(LowCardinality(String)),  -- 'YES', 'NO', null if not settled
    settlement_pnl Nullable(Decimal(18, 8))
) ENGINE = ReplacingMergeTree(window_end)
ORDER BY (session_id, event_id)
PARTITION BY toYYYYMM(window_start);
```

---

### Backend Capture Infrastructure

#### US-024: Dashboard Capture Channel

**Description:** As a developer, I want a fire-and-forget capture system for dashboard data.

**Acceptance Criteria:**
- [ ] `cargo clippy -p poly-bot -- -D warnings` passes
- [ ] `cargo test -p poly-bot` passes
- [ ] `DashboardCapture` struct with bounded channel
- [ ] `try_send()` pattern (drops on backpressure)
- [ ] Background processor writes to ClickHouse
- [ ] Benchmark confirms <10ns overhead on hot path

**Events to capture:**
```rust
enum DashboardEvent {
    TradeExecuted(TradeRecord),
    PnlSnapshot(PnlSnapshot),
    LogEntry(LogEntry),
    MarketUpdate(MarketSessionUpdate),
    SessionStart(SessionRecord),
    SessionEnd(SessionEndRecord),
}
```

---

#### US-025: REST API for Historical Queries

**Description:** As a frontend, I want REST endpoints to query historical data.

**Acceptance Criteria:**
- [ ] `cargo clippy -p poly-bot -- -D warnings` passes
- [ ] `cargo test -p poly-bot` passes
- [ ] `GET /api/sessions` - list recent sessions
- [ ] `GET /api/sessions/:id/equity` - equity curve for session
- [ ] `GET /api/sessions/:id/trades` - trades for session
- [ ] `GET /api/sessions/:id/logs` - logs for session (paginated)
- [ ] `GET /api/sessions/:id/markets` - market sessions
- [ ] Responses are JSON

**Technical Notes:**
- Use `axum` for HTTP server (already common in Rust async)
- Run on same port as WebSocket or separate port
- Can coexist with WebSocket server

## Technical Considerations

### Bot-Side Changes

1. **Dashboard Module** (`poly-bot/src/dashboard/`)
   - `mod.rs`: Module exports
   - `server.rs`: Combined WebSocket + HTTP server (axum + tokio-tungstenite)
   - `state.rs`: DashboardState struct and snapshot creation
   - `capture.rs`: Fire-and-forget capture channel for dashboard events
   - `processor.rs`: Background processor writing to ClickHouse
   - `api.rs`: REST API handlers for historical queries

2. **Integration Points**
   - Tap into existing GlobalState for metrics
   - Tap into observability for trade decisions
   - Add dashboard capture channel (like observability)
   - Integrate with executor for trade fills
   - Integrate with tracing for structured logs

3. **Configuration**
   ```rust
   pub struct DashboardConfig {
       pub enabled: bool,
       pub ws_port: u16,                    // WebSocket for live data
       pub http_port: u16,                  // REST API for historical queries (can be same)
       pub broadcast_interval_ms: u64,      // How often to push state (default 100ms)
       pub pnl_snapshot_interval_secs: u64, // How often to snapshot P&L (default 10s)
       pub log_channel_capacity: usize,     // Bounded channel size for logs
       pub event_channel_capacity: usize,   // Bounded channel size for events
   }
   ```

4. **Schema Migrations**
   - Add new tables to `schema.sql`
   - Document migration steps for existing deployments

### Frontend Stack

- **Runtime**: Bun
- **Framework**: React 18+ with TypeScript
- **Build**: Vite
- **UI Components**: shadcn/ui (Radix primitives)
- **Styling**: TailwindCSS
- **Charts**: Lightweight Charts (TradingView)
- **State**: React Query for WebSocket data + Zustand for UI state
- **Routing**: React Router

### File Structure (Frontend)

```
frontend/
├── package.json
├── bun.lockb
├── vite.config.ts
├── tsconfig.json
├── tailwind.config.js
├── components.json          # shadcn config
├── index.html
├── src/
│   ├── main.tsx
│   ├── App.tsx
│   ├── components/
│   │   ├── ui/              # shadcn components
│   │   ├── dashboard/
│   │   │   ├── MetricsCards.tsx
│   │   │   ├── EquityCurve.tsx
│   │   │   ├── MarketsGrid.tsx
│   │   │   ├── CircuitBreakerStatus.tsx
│   │   │   └── LogWindow.tsx
│   │   └── market/
│   │       ├── PriceChart.tsx
│   │       ├── OrderBookDisplay.tsx
│   │       ├── PositionPanel.tsx
│   │       └── TradesTable.tsx
│   ├── hooks/
│   │   ├── useWebSocket.ts
│   │   └── useDashboardState.ts
│   ├── lib/
│   │   ├── types.ts          # TypeScript interfaces matching Rust structs
│   │   └── utils.ts
│   └── pages/
│       ├── Dashboard.tsx
│       └── MarketDetail.tsx
```

### Message Types (JSON)

```typescript
interface DashboardSnapshot {
  timestamp: number;
  metrics: MetricsSnapshot;
  control: ControlState;
  markets: ActiveMarket[];
  positions: Position[];
  recentTrades: Trade[];
  equityHistory: EquityPoint[];
  logs: LogEntry[];
  anomalies: Anomaly[];
}

interface MetricsSnapshot {
  eventsProcessed: number;
  opportunitiesDetected: number;
  tradesExecuted: number;
  tradesFailed: number;
  tradesSkipped: number;
  pnlUsdc: string;  // Decimal as string
  volumeUsdc: string;
  shadowOrdersFired: number;
  shadowOrdersFilled: number;
}
```

## Dependencies

### Rust (poly-bot)
- `tokio-tungstenite` - WebSocket server
- `axum` - HTTP server for REST API
- `tower-http` - CORS, static file serving
- `serde_json` - JSON serialization (already present)
- `uuid` - Session IDs
- `tracing-subscriber` - For structured log capture layer

### Frontend (new)
- `react`, `react-dom`
- `typescript`
- `vite`
- `tailwindcss`
- `@radix-ui/*` (via shadcn)
- `lightweight-charts`
- `@tanstack/react-query`
- `zustand`
- `react-router-dom`

## Success Metrics

1. Dashboard loads and connects within 2 seconds
2. State updates visible within 500ms of bot state change
3. No measurable impact on bot hot-path latency (<10ns overhead)
4. Zero crashes during 24-hour paper trading session
5. All charts render correctly with 1000+ data points
6. Historical data loads within 1 second for sessions with 10k+ trades
7. Log queries return within 500ms for 100k+ entries

---

## User Story Summary

| ID | Category | Description |
|----|----------|-------------|
| US-001 | Backend | WebSocket Server Infrastructure |
| US-002 | Frontend | Project Setup (Bun + React + shadcn) |
| US-003 | Frontend | WebSocket Connection Management |
| US-004 | Frontend | Main Dashboard - Overview Metrics |
| US-005 | Frontend | Main Dashboard - Equity Curve Chart |
| US-006 | Frontend | Main Dashboard - Active Markets Grid |
| US-007 | Frontend | Main Dashboard - Circuit Breaker Status |
| US-008 | Frontend | Market Detail - Price Chart |
| US-009 | Frontend | Market Detail - Order Book Display |
| US-010 | Frontend | Market Detail - Position & P&L |
| US-011 | Frontend | Market Detail - Trades List |
| US-012 | Frontend | Log Window |
| US-013 | Frontend | Session Metrics Panel |
| US-014 | Frontend | Anomaly Alerts |
| US-015 | Frontend | Dark Theme |
| US-016 | Frontend | Static Build Integration |
| US-017 | Backend | Dashboard State Serialization |
| US-018 | Backend | Historical Data for Charts |
| US-019 | Backend | Sessions Table (ClickHouse) |
| US-020 | Backend | Bot Trades Table (ClickHouse) |
| US-021 | Backend | PnL Snapshots Table (ClickHouse) |
| US-022 | Backend | Structured Logs Table (ClickHouse) |
| US-023 | Backend | Market Sessions Table (ClickHouse) |
| US-024 | Backend | Dashboard Capture Channel |
| US-025 | Backend | REST API for Historical Queries |
