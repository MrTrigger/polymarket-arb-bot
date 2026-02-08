#!/usr/bin/env bash
# Import CSV data into ClickHouse and create downsampled orderbook CSV.
#
# Usage:
#   ./scripts/import_and_downsample.sh [data_dir] [interval_seconds]
#
# Examples:
#   ./scripts/import_and_downsample.sh data/live_test 1    # 1s interval (default)
#   ./scripts/import_and_downsample.sh data/live_test 5    # 5s interval

set -euo pipefail

DATA_DIR="${1:-data/live_test}"
INTERVAL="${2:-1}"
CONTAINER_NAME="polybot-clickhouse"
CH_PORT=9100  # Non-default to avoid conflicts

# Resolve to absolute path
DATA_DIR="$(cd "$DATA_DIR" && pwd)"
OUTPUT_DIR="${DATA_DIR}/downsampled_${INTERVAL}s"

echo "=== ClickHouse Import & Downsample ==="
echo "Data dir:  $DATA_DIR"
echo "Interval:  ${INTERVAL}s"
echo "Output:    $OUTPUT_DIR"
echo ""

# --- 1. Start ClickHouse ---
if docker ps --format '{{.Names}}' | grep -q "^${CONTAINER_NAME}$"; then
    echo "[1/5] ClickHouse already running"
else
    echo "[1/5] Starting ClickHouse..."
    docker rm -f "$CONTAINER_NAME" 2>/dev/null || true
    docker run -d \
        --name "$CONTAINER_NAME" \
        --network host \
        -v "$DATA_DIR:/data:ro" \
        clickhouse/clickhouse-server:24.8-alpine \
        > /dev/null
    # Wait for it to be ready
    for i in $(seq 1 30); do
        if docker exec "$CONTAINER_NAME" clickhouse-client --query "SELECT 1" &>/dev/null; then
            echo "  ClickHouse ready"
            break
        fi
        sleep 1
    done
fi

CH="docker exec $CONTAINER_NAME clickhouse-client --port 9000"

# --- 2. Create tables ---
echo "[2/5] Creating tables..."
$CH --query "
CREATE TABLE IF NOT EXISTS market_windows (
    event_id UInt64,
    condition_id String,
    asset LowCardinality(String),
    yes_token_id String,
    no_token_id String,
    strike_price Decimal(18, 8),
    window_start DateTime64(9, 'UTC'),
    window_end DateTime64(9, 'UTC'),
    discovered_at DateTime64(9, 'UTC')
) ENGINE = ReplacingMergeTree(discovered_at)
ORDER BY (event_id, window_start);
"

$CH --query "
CREATE TABLE IF NOT EXISTS spot_prices (
    asset LowCardinality(String),
    price Decimal(18, 8),
    timestamp DateTime64(9, 'UTC'),
    quantity Decimal(18, 8)
) ENGINE = MergeTree()
ORDER BY (asset, timestamp);
"

$CH --query "
CREATE TABLE IF NOT EXISTS orderbook_snapshots (
    token_id String,
    event_id UInt64,
    timestamp DateTime64(9, 'UTC'),
    best_bid Decimal(18, 8),
    best_bid_size Decimal(18, 8),
    best_ask Decimal(18, 8),
    best_ask_size Decimal(18, 8),
    spread_bps UInt32,
    bid_depth Decimal(18, 8),
    ask_depth Decimal(18, 8)
) ENGINE = MergeTree()
ORDER BY (event_id, token_id, timestamp);
"

# --- 3. Import CSVs ---
echo "[3/5] Importing CSVs..."

ROW_COUNT=$($CH --query "SELECT count() FROM market_windows")
if [ "$ROW_COUNT" -gt 0 ]; then
    echo "  market_windows: $ROW_COUNT rows (already imported)"
else
    echo -n "  market_windows: "
    $CH --query "INSERT INTO market_windows FORMAT CSVWithNames" < "$DATA_DIR/polymarket_market_windows.csv"
    $CH --query "SELECT count() FROM market_windows FORMAT TabSeparated"
    echo " rows"
fi

ROW_COUNT=$($CH --query "SELECT count() FROM spot_prices")
if [ "$ROW_COUNT" -gt 0 ]; then
    echo "  spot_prices: $ROW_COUNT rows (already imported)"
else
    echo -n "  spot_prices: "
    $CH --query "INSERT INTO spot_prices FORMAT CSVWithNames" < "$DATA_DIR/binance_spot_prices.csv"
    $CH --query "SELECT count() FROM spot_prices FORMAT TabSeparated"
    echo " rows"
fi

ROW_COUNT=$($CH --query "SELECT count() FROM orderbook_snapshots")
if [ "$ROW_COUNT" -gt 0 ]; then
    echo "  orderbook_snapshots: $ROW_COUNT rows (already imported)"
else
    echo -n "  orderbook_snapshots: importing ~92M rows..."
    $CH --query "INSERT INTO orderbook_snapshots FORMAT CSVWithNames" < "$DATA_DIR/polymarket_orderbook_snapshots.csv"
    CNT=$($CH --query "SELECT count() FROM orderbook_snapshots FORMAT TabSeparated")
    echo " $CNT rows"
fi

# --- 4. Downsample orderbook ---
echo "[4/5] Downsampling orderbook to ${INTERVAL}s intervals..."

# For each (event_id, token_id) group, keep one row per INTERVAL seconds.
# Uses argMax to pick the last snapshot in each interval bucket.
mkdir -p "$OUTPUT_DIR"

$CH --query "
SELECT
    token_id,
    event_id,
    max(timestamp) as timestamp,
    argMax(best_bid, timestamp) as best_bid,
    argMax(best_bid_size, timestamp) as best_bid_size,
    argMax(best_ask, timestamp) as best_ask,
    argMax(best_ask_size, timestamp) as best_ask_size,
    argMax(spread_bps, timestamp) as spread_bps,
    argMax(bid_depth, timestamp) as bid_depth,
    argMax(ask_depth, timestamp) as ask_depth
FROM orderbook_snapshots
GROUP BY
    event_id,
    token_id,
    toStartOfInterval(timestamp, INTERVAL ${INTERVAL} SECOND)
ORDER BY timestamp, event_id, token_id
FORMAT CSVWithNames
" > "$OUTPUT_DIR/polymarket_orderbook_snapshots.csv"

OB_ORIG=$($CH --query "SELECT count() FROM orderbook_snapshots FORMAT TabSeparated")
OB_DOWN=$(wc -l < "$OUTPUT_DIR/polymarket_orderbook_snapshots.csv")
OB_DOWN=$((OB_DOWN - 1))  # subtract header

echo "  Original: $OB_ORIG rows"
echo "  Downsampled: $OB_DOWN rows ($(( OB_ORIG / OB_DOWN ))x reduction)"

# --- 5. Copy other files (unchanged) ---
echo "[5/5] Copying spot prices and market windows..."
cp "$DATA_DIR/binance_spot_prices.csv" "$OUTPUT_DIR/"
cp "$DATA_DIR/polymarket_market_windows.csv" "$OUTPUT_DIR/"

echo ""
echo "=== Done ==="
echo "Downsampled data in: $OUTPUT_DIR"
echo ""
echo "To use in backtest:"
echo "  [backtest]"
echo "  data_dir = \"${OUTPUT_DIR}\""
echo ""

# Print some stats
$CH --query "
SELECT
    'Time range' as metric,
    toString(min(timestamp)) || ' to ' || toString(max(timestamp)) as value
FROM orderbook_snapshots
UNION ALL
SELECT
    'Unique events',
    toString(uniqExact(event_id))
FROM orderbook_snapshots
UNION ALL
SELECT
    'Unique tokens',
    toString(uniqExact(token_id))
FROM orderbook_snapshots
FORMAT PrettyCompact
"
