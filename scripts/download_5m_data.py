#!/usr/bin/env python3
"""
Download 5-minute market data for backtesting.
Fetches historical markets and Binance price data.
"""

import requests
import json
import csv
from datetime import datetime, timedelta
from typing import List, Dict, Optional
import time

GAMMA_API = "https://gamma-api.polymarket.com"
BINANCE_API = "https://api.binance.com/api/v3"
CACHE_FILE = "backtest_cache_5m.json"
CSV_FILE = "scripts/backtest_5m.csv"


def fetch_5m_events(limit: int = 200) -> List[Dict]:
    """Fetch 5-minute up/down events from Polymarket."""
    print(f"Fetching 5m events (limit={limit})...")

    # Get closed events with 5M tag (capital M)
    url = f"{GAMMA_API}/events?tag_slug=5M&closed=true&limit={limit}"
    resp = requests.get(url, timeout=30)
    resp.raise_for_status()

    events = resp.json()

    # Filter for 5m markets (should already be filtered but double-check)
    five_min_events = []
    for e in events:
        slug = e.get('slug', '')
        if '-5m-' in slug:
            five_min_events.append(e)

    print(f"Found {len(five_min_events)} 5-minute events")
    return five_min_events


def detect_asset(title: str) -> Optional[str]:
    """Detect crypto asset from title."""
    title_lower = title.lower()
    if 'bitcoin' in title_lower or 'btc' in title_lower:
        return 'BTC'
    elif 'ethereum' in title_lower or 'eth' in title_lower:
        return 'ETH'
    elif 'solana' in title_lower or 'sol' in title_lower:
        return 'SOL'
    elif 'xrp' in title_lower:
        return 'XRP'
    return None


def fetch_binance_candles(asset: str, start_time: datetime, duration_minutes: int = 5) -> List[tuple]:
    """Fetch 1-minute candles from Binance for the given window."""
    symbol = f"{asset.upper()}USDT"
    start_ms = int(start_time.timestamp() * 1000)
    end_ms = start_ms + (duration_minutes * 60 * 1000)

    try:
        url = f"{BINANCE_API}/klines"
        params = {
            "symbol": symbol,
            "interval": "1m",
            "startTime": start_ms,
            "endTime": end_ms,
            "limit": duration_minutes + 1
        }
        resp = requests.get(url, params=params, timeout=10)
        if resp.ok and resp.json():
            candles = []
            for i, c in enumerate(resp.json()):
                candles.append((i, float(c[4])))  # minute, close price
            return candles
    except Exception as e:
        print(f"  Error fetching Binance data for {asset}: {e}")
    return []


def get_settlement(event: Dict) -> Optional[str]:
    """Get settlement outcome from event."""
    markets = event.get('markets', [])
    if markets:
        market = markets[0]
        outcome_prices = market.get('outcomePrices', '')
        if outcome_prices:
            try:
                prices = json.loads(outcome_prices)
                if len(prices) >= 2:
                    # Up is first outcome, Down is second
                    if float(prices[0]) > 0.9:
                        return "Up"
                    elif float(prices[1]) > 0.9:
                        return "Down"
            except:
                pass
    return None


def parse_event(event: Dict) -> Optional[Dict]:
    """Parse a Polymarket event into market data."""
    title = event.get('title', '')
    asset = detect_asset(title)

    if not asset:
        return None

    # Parse end time
    end_date = event.get('endDate')
    if not end_date:
        return None

    try:
        end_time = datetime.fromisoformat(end_date.replace('Z', '+00:00'))
    except:
        return None

    # Start time is 5 minutes before end
    start_time = end_time - timedelta(minutes=5)

    # Get settlement
    settlement = get_settlement(event)
    if not settlement:
        return None  # Market not settled yet

    return {
        'event_id': event.get('id'),
        'title': title,
        'asset': asset,
        'start_time': start_time.isoformat(),
        'end_time': end_time.isoformat(),
        'settlement': settlement,
        'slug': event.get('slug', '')
    }


def main():
    print("=" * 60)
    print("5-MINUTE MARKET DATA DOWNLOAD")
    print("=" * 60)

    # Fetch events
    events = fetch_5m_events(limit=500)

    # Parse and enrich markets
    markets = []
    for i, event in enumerate(events):
        parsed = parse_event(event)
        if not parsed:
            continue

        # Fetch Binance candles
        start_time = datetime.fromisoformat(parsed['start_time'].replace('+00:00', ''))
        candles = fetch_binance_candles(parsed['asset'], start_time, duration_minutes=5)

        if not candles:
            continue

        parsed['minute_prices'] = candles
        parsed['open_price'] = candles[0][1] if candles else 0
        parsed['close_price'] = candles[-1][1] if candles else 0

        markets.append(parsed)

        if (i + 1) % 10 == 0:
            print(f"  Processed {i + 1}/{len(events)} events, {len(markets)} valid markets")

        # Rate limit
        time.sleep(0.1)

    print(f"\nTotal valid markets: {len(markets)}")

    if not markets:
        print("ERROR: No valid markets found!")
        return

    # Save cache
    print(f"\nSaving cache to {CACHE_FILE}...")
    with open(CACHE_FILE, 'w') as f:
        json.dump(markets, f, indent=2)

    # Create CSV for Rust backtest
    print(f"Creating CSV at {CSV_FILE}...")
    rows = []
    for m in markets:
        asset = m['asset']
        start_time = datetime.fromisoformat(m['start_time'].replace('+00:00', ''))
        end_time = datetime.fromisoformat(m['end_time'].replace('+00:00', ''))
        strike = m['open_price']
        settlement = m['settlement']
        winner = 'Up' if settlement == 'Up' else 'Down'

        for minute, price in m.get('minute_prices', []):
            ts = start_time + timedelta(minutes=minute)
            seconds_remaining = (end_time - ts).total_seconds()
            minutes_remaining = seconds_remaining / 60
            distance = price - strike

            rows.append({
                'timestamp': ts.isoformat(),
                'market_id': f"{asset}_{start_time.strftime('%Y%m%d_%H%M')}",
                'btc_price': price,
                'btc_high': price,
                'btc_low': price,
                'strike_price': strike,
                'distance_to_strike': distance,
                'seconds_remaining': seconds_remaining,
                'minutes_remaining': minutes_remaining,
                'winner': winner
            })

    with open(CSV_FILE, 'w', newline='') as f:
        writer = csv.DictWriter(f, fieldnames=[
            'timestamp', 'market_id', 'btc_price', 'btc_high', 'btc_low',
            'strike_price', 'distance_to_strike', 'seconds_remaining',
            'minutes_remaining', 'winner'
        ])
        writer.writeheader()
        writer.writerows(rows)

    print(f"\nGenerated {len(rows)} CSV rows from {len(markets)} markets")

    # Summary
    print("\n" + "=" * 60)
    print("SUMMARY")
    print("=" * 60)
    by_asset = {}
    by_outcome = {'Up': 0, 'Down': 0}
    for m in markets:
        by_asset[m['asset']] = by_asset.get(m['asset'], 0) + 1
        by_outcome[m['settlement']] = by_outcome.get(m['settlement'], 0) + 1

    print(f"Total markets: {len(markets)}")
    print(f"By asset: {by_asset}")
    print(f"By outcome: {by_outcome}")
    print(f"\nCache saved to: {CACHE_FILE}")
    print(f"CSV saved to: {CSV_FILE}")
    print("\nRun backtest with:")
    print(f"  cargo run --example csv_backtest --release -- --data {CSV_FILE}")


if __name__ == "__main__":
    main()
