-- ClickHouse schema for Polymarket Arbitrage Bot
-- Run these queries to create the required tables

-- Market windows metadata
CREATE TABLE IF NOT EXISTS market_windows (
    event_id String,
    condition_id String,
    asset LowCardinality(String),
    yes_token_id String,
    no_token_id String,
    strike_price Decimal(18, 8),
    window_start DateTime64(3, 'UTC'),
    window_end DateTime64(3, 'UTC'),
    discovered_at DateTime64(3, 'UTC')
) ENGINE = ReplacingMergeTree(discovered_at)
ORDER BY (event_id, window_start)
PARTITION BY toYYYYMMDD(window_start);

-- Spot prices from Binance
CREATE TABLE IF NOT EXISTS spot_prices (
    asset LowCardinality(String),
    price Decimal(18, 8),
    timestamp DateTime64(3, 'UTC'),
    quantity Decimal(18, 8)
) ENGINE = MergeTree()
ORDER BY (asset, timestamp)
PARTITION BY toYYYYMMDD(timestamp)
TTL toDate(timestamp) + INTERVAL 90 DAY;

-- Order book snapshots (periodic full state)
CREATE TABLE IF NOT EXISTS orderbook_snapshots (
    token_id String,
    event_id String,
    timestamp DateTime64(3, 'UTC'),
    best_bid Decimal(18, 8),
    best_bid_size Decimal(18, 8),
    best_ask Decimal(18, 8),
    best_ask_size Decimal(18, 8),
    spread_bps UInt32,
    bid_depth Decimal(18, 8),
    ask_depth Decimal(18, 8)
) ENGINE = MergeTree()
ORDER BY (token_id, timestamp)
PARTITION BY toYYYYMMDD(timestamp)
TTL toDate(timestamp) + INTERVAL 90 DAY;

-- Order book deltas (incremental updates)
CREATE TABLE IF NOT EXISTS orderbook_deltas (
    token_id String,
    event_id String,
    timestamp DateTime64(3, 'UTC'),
    side LowCardinality(String),
    price Decimal(18, 8),
    size Decimal(18, 8)
) ENGINE = MergeTree()
ORDER BY (token_id, timestamp)
PARTITION BY toYYYYMMDD(timestamp)
TTL toDate(timestamp) + INTERVAL 90 DAY;

-- Price history from Polymarket API (historical import)
CREATE TABLE IF NOT EXISTS price_history (
    token_id String,
    timestamp DateTime64(3, 'UTC'),
    price Decimal(18, 8)
) ENGINE = MergeTree()
ORDER BY (token_id, timestamp)
PARTITION BY toYYYYMM(timestamp);

-- Trade history from Polymarket API (historical import)
CREATE TABLE IF NOT EXISTS trade_history (
    token_id String,
    timestamp DateTime64(3, 'UTC'),
    side LowCardinality(String),
    price Decimal(18, 8),
    size Decimal(18, 8),
    trade_id String
) ENGINE = ReplacingMergeTree()
ORDER BY (token_id, timestamp, trade_id)
PARTITION BY toYYYYMM(timestamp);

-- Trading decisions (observability)
-- Note: Uses Float64 for price fields because clickhouse-rs doesn't support rust_decimal
CREATE TABLE IF NOT EXISTS decisions (
    decision_id UInt64,
    event_id String,
    timestamp DateTime64(3, 'UTC'),
    decision_type LowCardinality(String),
    yes_ask Float64,
    no_ask Float64,
    combined_cost Float64,
    arb_margin Float64,
    spot_price Float64,
    time_remaining_secs UInt32,
    action LowCardinality(String),
    reason String,
    confidence Float32
) ENGINE = MergeTree()
ORDER BY (event_id, timestamp)
PARTITION BY toYYYYMMDD(timestamp)
TTL toDate(timestamp) + INTERVAL 180 DAY;

-- Counterfactual analysis (post-settlement what-if)
CREATE TABLE IF NOT EXISTS counterfactuals (
    decision_id UInt64,
    event_id String,
    settlement_time DateTime64(3, 'UTC'),
    original_action LowCardinality(String),
    settlement_outcome LowCardinality(String),
    original_size Decimal(18, 8),
    hypothetical_size Decimal(18, 8),
    hypothetical_cost Decimal(18, 8),
    hypothetical_pnl Decimal(18, 8),
    actual_pnl Decimal(18, 8),
    missed_pnl Decimal(18, 8),
    was_correct UInt8,
    assessment_reason String,
    decision_margin Decimal(18, 8),
    decision_confidence UInt8,
    decision_toxic_severity UInt8,
    decision_seconds_remaining UInt32
) ENGINE = MergeTree()
ORDER BY (event_id, settlement_time)
PARTITION BY toYYYYMMDD(settlement_time)
TTL toDate(settlement_time) + INTERVAL 180 DAY;

-- Anomaly detection (observability)
CREATE TABLE IF NOT EXISTS anomalies (
    anomaly_id UInt64,
    anomaly_type LowCardinality(String),
    severity LowCardinality(String),
    timestamp DateTime64(3, 'UTC'),
    event_id Nullable(String),
    token_id Nullable(String),
    description String,
    current_value Decimal(18, 8),
    expected_value Decimal(18, 8),
    deviation Decimal(18, 8),
    context String
) ENGINE = MergeTree()
ORDER BY (anomaly_type, timestamp)
PARTITION BY toYYYYMMDD(timestamp)
TTL toDate(timestamp) + INTERVAL 90 DAY;

-- ============================================================================
-- Dashboard Tables (for React dashboard frontend)
-- ============================================================================

-- Bot sessions (tracks each bot run)
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

-- Bot trades (all trades executed by the bot)
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

-- PnL snapshots (for equity curve visualization)
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

-- Structured logs (searchable logs for debugging)
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

-- Market sessions (per-market metrics within a session)
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
