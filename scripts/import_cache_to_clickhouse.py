#!/usr/bin/env python3
"""
Import cached backtest data into ClickHouse for Rust backtest.
Reads backtest_cache.json and populates spot_prices and market_windows tables.
"""

import json
import requests
from datetime import datetime
from typing import List, Dict

# ClickHouse connection
CH_URL = "http://clickhouse.wallintech.eu:30123"
CH_USER = "default"
CH_PASSWORD = "fsT595rSW6CoAb48h1TG"
CH_DATABASE = "polymarket"

def ch_query(query: str, data: str = None) -> str:
    """Execute ClickHouse query."""
    params = {"user": CH_USER, "password": CH_PASSWORD, "database": CH_DATABASE}
    if data:
        # Pass query in URL params, data in body
        resp = requests.post(f"{CH_URL}/?query={query}", params=params, data=data)
    else:
        resp = requests.get(CH_URL, params={**params, "query": query})
    resp.raise_for_status()
    return resp.text


def ch_insert_batched(query: str, rows: List[str], batch_size: int = 1000):
    """Insert rows in batches."""
    total = 0
    for i in range(0, len(rows), batch_size):
        batch = rows[i:i + batch_size]
        data = "\n".join(batch)
        ch_query(query, data)
        total += len(batch)
        if total % 5000 == 0:
            print(f"    {total}/{len(rows)} rows...")
    return total


def import_spot_prices(markets: List[Dict]):
    """Import spot prices from minute candles."""
    print("Importing spot prices...")

    rows = []
    for m in markets:
        asset = m["asset"]
        start_time = datetime.fromisoformat(m["start_time"])

        for minute, price in m.get("minute_prices", []):
            ts = start_time.replace(minute=minute, second=0, microsecond=0)
            # Schema: asset, price, timestamp, quantity
            rows.append(f"{asset}\t{price}\t{ts.strftime('%Y-%m-%d %H:%M:%S.000')}\t0")

    if rows:
        query = "INSERT INTO spot_prices (asset, price, timestamp, quantity) FORMAT TabSeparated"
        count = ch_insert_batched(query, rows)
        print(f"  Inserted {count} spot prices")
    else:
        print("  No spot price data to insert")


def import_market_windows(markets: List[Dict]):
    """Import market windows."""
    print("Importing market windows...")

    rows = []
    now = datetime.now().strftime('%Y-%m-%d %H:%M:%S.000')

    for i, m in enumerate(markets):
        event_id = f"cached_{i}"
        condition_id = f"cond_{i}"

        start_time = datetime.fromisoformat(m["start_time"])
        end_time = datetime.fromisoformat(m["end_time"])
        asset = m["asset"]

        # Schema: event_id, condition_id, asset, yes_token_id, no_token_id,
        #         strike_price, window_start, window_end, discovered_at
        rows.append(
            f"{event_id}\t{condition_id}\t{asset}\tyes_{i}\tno_{i}\t"
            f"{m['open_price']}\t{start_time.strftime('%Y-%m-%d %H:%M:%S.000')}\t"
            f"{end_time.strftime('%Y-%m-%d %H:%M:%S.000')}\t{now}"
        )

    if rows:
        query = "INSERT INTO market_windows (event_id, condition_id, asset, yes_token_id, no_token_id, strike_price, window_start, window_end, discovered_at) FORMAT TabSeparated"
        count = ch_insert_batched(query, rows)
        print(f"  Inserted {count} market windows")


def main():
    # Load cache
    print("Loading cache...")
    with open("backtest_cache.json") as f:
        markets = json.load(f)
    print(f"Loaded {len(markets)} markets")

    # Check for minute data
    has_minutes = sum(1 for m in markets if m.get("minute_prices"))
    print(f"Markets with minute data: {has_minutes}")

    if has_minutes == 0:
        print("ERROR: No minute price data in cache. Run fast_backtest.py --rebuild first.")
        return

    # Import data
    import_spot_prices(markets)
    import_market_windows(markets)

    # Verify
    print("\nVerifying...")
    spot_count = ch_query("SELECT count() FROM spot_prices")
    window_count = ch_query("SELECT count() FROM market_windows")
    print(f"  spot_prices: {spot_count.strip()} rows")
    print(f"  market_windows: {window_count.strip()} rows")

    print("\nDone! You can now run the Rust backtest.")


if __name__ == "__main__":
    main()
