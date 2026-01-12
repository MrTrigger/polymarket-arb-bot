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
TTL timestamp + INTERVAL 90 DAY;

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
TTL timestamp + INTERVAL 90 DAY;

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
TTL timestamp + INTERVAL 90 DAY;

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
CREATE TABLE IF NOT EXISTS decisions (
    decision_id UInt64,
    event_id String,
    timestamp DateTime64(3, 'UTC'),
    decision_type LowCardinality(String),
    yes_ask Decimal(18, 8),
    no_ask Decimal(18, 8),
    combined_cost Decimal(18, 8),
    arb_margin Decimal(18, 8),
    spot_price Decimal(18, 8),
    time_remaining_secs UInt32,
    action LowCardinality(String),
    reason String,
    confidence Float32
) ENGINE = MergeTree()
ORDER BY (event_id, timestamp)
PARTITION BY toYYYYMMDD(timestamp)
TTL timestamp + INTERVAL 180 DAY;
