#!/usr/bin/env python3
"""
HISTORICAL DATA FETCHER FOR BACKTESTING (Parallelized)
=======================================================

Fetches historical data for backtesting with parallel workers:
1. BTC price history from Binance (parallelized)
2. Resolved BTC Up/Down markets from Polymarket
3. Combines into backtest-ready dataset

Usage:
    python fetch_historical_data.py --days 7
    python fetch_historical_data.py --days 30 --threads 30
"""

import requests
import pandas as pd
import numpy as np
import time
import os
import sys
import re
import json
from datetime import datetime, timedelta, timezone
from concurrent.futures import ThreadPoolExecutor, as_completed
from typing import Optional, Dict, List, Tuple
import argparse

# =============================================================================
# CONFIGURATION
# =============================================================================

CONFIG = {
    "output_dir": "historical_data",
    "threads": 20,
    "binance_api": "https://api.binance.com/api/v3",
    "gamma_api": "https://gamma-api.polymarket.com",
}

session = requests.Session()

# =============================================================================
# POLYMARKET: FETCH RESOLVED MARKETS
# =============================================================================

def fetch_resolved_btc_markets(limit: int = 500) -> pd.DataFrame:
    """Fetch resolved BTC Up/Down markets from Polymarket."""
    all_markets = []
    offset = 0
    
    print(f"Fetching resolved BTC markets from Polymarket...")
    
    while len(all_markets) < limit:
        url = f"{CONFIG['gamma_api']}/markets"
        params = {
            "closed": "true",
            "limit": 100,
            "offset": offset,
            "order": "closedTime",
            "ascending": "false",
        }
        
        try:
            resp = session.get(url, params=params, timeout=10)
            if resp.status_code != 200:
                print(f"API error: {resp.status_code}")
                break
            
            markets = resp.json()
            if not markets:
                break
            
            # Filter for BTC Up/Down markets
            btc_markets = [
                m for m in markets
                if "Bitcoin Up or Down" in m.get("question", "")
                or "BTC Up or Down" in m.get("question", "")
            ]
            
            all_markets.extend(btc_markets)
            sys.stdout.write(f"\rMarkets found: {len(all_markets)}")
            sys.stdout.flush()
            
            offset += 100
            
            if len(markets) < 100:
                break
                
        except Exception as e:
            print(f"\nError fetching markets: {e}")
            break
        
        time.sleep(0.1)
    
    print(f"\nTotal BTC markets found: {len(all_markets)}")
    
    # Parse markets
    parsed = []
    for m in all_markets:
        try:
            data = parse_market(m)
            if data:
                parsed.append(data)
        except Exception as e:
            pass
    
    print(f"Successfully parsed: {len(parsed)} markets")
    return pd.DataFrame(parsed)


def parse_market(m: dict) -> Optional[dict]:
    """Parse a market JSON into structured data."""
    question = m.get("question", "")

    # Get timing from events or market fields
    end_date_str = m.get("endDate")
    start_date_str = None

    # Try to get start time from events
    events = m.get("events", [])
    if events and len(events) > 0:
        start_date_str = events[0].get("startTime")

    if not end_date_str:
        return None

    try:
        end_time = pd.to_datetime(end_date_str, utc=True)
        if start_date_str:
            start_time = pd.to_datetime(start_date_str, utc=True)
        else:
            # Guess duration from question (5m, 15m, etc.)
            if "5:" in question or "-5:" in question or "5m" in m.get("slug", ""):
                start_time = end_time - timedelta(minutes=5)
            else:
                start_time = end_time - timedelta(minutes=15)
    except:
        return None

    # Determine winner from outcomePrices
    winner = None
    outcomes_str = m.get("outcomes", "[]")
    outcome_prices_str = m.get("outcomePrices", "[]")

    try:
        outcomes = json.loads(outcomes_str) if isinstance(outcomes_str, str) else outcomes_str
        outcome_prices = json.loads(outcome_prices_str) if isinstance(outcome_prices_str, str) else outcome_prices_str

        if outcome_prices and len(outcome_prices) >= 2:
            prices = [float(p) for p in outcome_prices]
            if prices[0] > 0.9:
                winner = outcomes[0] if outcomes else "Up"
            elif prices[1] > 0.9:
                winner = outcomes[1] if outcomes else "Down"
    except:
        pass

    if not winner:
        return None

    return {
        "market_id": m.get("id") or m.get("conditionId"),
        "question": question,
        "start_time": start_time,
        "end_time": end_time,
        "winner": winner,
    }

# =============================================================================
# BINANCE: PARALLEL PRICE FETCHING
# =============================================================================

def fetch_btc_klines_batch(args: Tuple[int, int]) -> dict:
    """Worker: Fetch a batch of BTC klines (up to 1000)."""
    start_ts, end_ts = args
    url = f"{CONFIG['binance_api']}/klines"
    params = {
        "symbol": "BTCUSDT",
        "interval": "1m",
        "startTime": start_ts * 1000,
        "endTime": end_ts * 1000,
        "limit": 1000,
    }
    
    try:
        resp = session.get(url, params=params, timeout=10)
        if resp.status_code == 200:
            data = resp.json()
            results = {}
            for k in data:
                minute_ts = int(k[0] // 1000)
                results[minute_ts] = {
                    "open": float(k[1]),
                    "high": float(k[2]),
                    "low": float(k[3]),
                    "close": float(k[4]),
                    "volume": float(k[5]),
                }
            return {"start_ts": start_ts, "data": results}
        elif resp.status_code == 429:
            time.sleep(1)
            return fetch_btc_klines_batch(args)  # Retry
    except Exception as e:
        pass
    
    return {"start_ts": start_ts, "data": {}}


def fetch_all_btc_prices_parallel(
    markets_df: pd.DataFrame, 
    threads: int = 20
) -> Dict[int, dict]:
    """Fetch all required BTC prices in parallel batches."""
    
    # Determine the full time range needed
    min_time = markets_df['start_time'].min()
    max_time = markets_df['end_time'].max()
    
    # Convert to timestamps
    start_ts = int(min_time.timestamp())
    end_ts = int(max_time.timestamp())
    
    print(f"\nFetching BTC prices from {min_time} to {max_time}")
    
    # Create batches (1000 minutes = ~16.7 hours per batch)
    batch_size = 1000 * 60  # 1000 minutes in seconds
    batches = []
    current = start_ts
    
    while current < end_ts:
        batch_end = min(current + batch_size, end_ts)
        batches.append((current, batch_end))
        current = batch_end
    
    print(f"Fetching {len(batches)} batches with {threads} threads...")
    
    # Parallel fetch
    all_prices = {}
    completed = 0
    
    with ThreadPoolExecutor(max_workers=threads) as executor:
        futures = {executor.submit(fetch_btc_klines_batch, batch): batch for batch in batches}
        
        for future in as_completed(futures):
            result = future.result()
            all_prices.update(result.get("data", {}))
            completed += 1
            sys.stdout.write(f"\rBatches completed: {completed}/{len(batches)} | Prices: {len(all_prices)}")
            sys.stdout.flush()
    
    print(f"\nTotal price points fetched: {len(all_prices)}")
    return all_prices

# =============================================================================
# COMBINE DATA FOR BACKTESTING
# =============================================================================

def create_backtest_dataset(
    markets_df: pd.DataFrame,
    btc_prices: Dict[int, dict],
    sample_interval_sec: int = 60
) -> pd.DataFrame:
    """Combine markets with BTC prices into backtest dataset."""

    print(f"\nCreating backtest dataset (sampling every {sample_interval_sec}s)...")

    all_data = []
    markets_processed = 0

    for _, market in markets_df.iterrows():
        start_ts = int(market['start_time'].timestamp())
        end_ts = int(market['end_time'].timestamp())
        winner = market['winner']
        market_id = market['market_id']

        # Get strike price = BTC price at market start
        start_minute_ts = (start_ts // 60) * 60
        strike_data = btc_prices.get(start_minute_ts)
        if not strike_data or strike_data.get('close') is None:
            continue
        strike_price = strike_data['close']

        market_data = []

        # Sample at regular intervals
        for ts in range(start_ts, end_ts, sample_interval_sec):
            minute_ts = (ts // 60) * 60  # Round to minute

            price_data = btc_prices.get(minute_ts)
            if not price_data or price_data.get('close') is None:
                continue

            btc_price = price_data['close']
            distance = btc_price - strike_price
            seconds_remaining = end_ts - ts
            minutes_remaining = seconds_remaining / 60

            market_data.append({
                "timestamp": datetime.fromtimestamp(ts, tz=timezone.utc),
                "market_id": market_id,
                "btc_price": btc_price,
                "btc_high": price_data.get('high', btc_price),
                "btc_low": price_data.get('low', btc_price),
                "strike_price": strike_price,
                "distance_to_strike": distance,
                "seconds_remaining": seconds_remaining,
                "minutes_remaining": minutes_remaining,
                "winner": winner,
            })

        if len(market_data) >= 3:  # Need at least 3 data points (for 5min markets)
            all_data.extend(market_data)
            markets_processed += 1

        sys.stdout.write(f"\rMarkets processed: {markets_processed}")
        sys.stdout.flush()

    print(f"\nTotal data points: {len(all_data)} across {markets_processed} markets")

    return pd.DataFrame(all_data)

# =============================================================================
# BACKTEST
# =============================================================================

def run_backtest(df: pd.DataFrame, budget_per_market: float = 1000) -> pd.DataFrame:
    """Run backtest on historical data."""
    
    from enum import Enum
    
    class Signal(Enum):
        STRONG_UP = "strong_up"
        LEAN_UP = "lean_up"
        NEUTRAL = "neutral"
        LEAN_DOWN = "lean_down"
        STRONG_DOWN = "strong_down"
    
    def get_signal(distance: float, mins_remaining: float) -> Signal:
        # Tightened thresholds from verification
        if mins_remaining > 12:
            lean, conviction = 30, 60
        elif mins_remaining > 9:
            lean, conviction = 25, 50
        elif mins_remaining > 6:
            lean, conviction = 15, 40
        elif mins_remaining > 3:
            lean, conviction = 12, 35
        else:
            lean, conviction = 8, 25
        
        if distance > conviction:
            return Signal.STRONG_UP
        elif distance > lean:
            return Signal.LEAN_UP
        elif distance < -conviction:
            return Signal.STRONG_DOWN
        elif distance < -lean:
            return Signal.LEAN_DOWN
        else:
            return Signal.NEUTRAL
    
    def signal_to_ratio(signal: Signal) -> Tuple[float, float]:
        ratios = {
            Signal.STRONG_UP: (0.78, 0.22),
            Signal.LEAN_UP: (0.61, 0.39),
            Signal.NEUTRAL: (0.50, 0.50),
            Signal.LEAN_DOWN: (0.39, 0.61),
            Signal.STRONG_DOWN: (0.22, 0.78),
        }
        return ratios[signal]
    
    def calculate_confidence(distance: float, mins_remaining: float) -> float:
        if mins_remaining > 12:
            time_conf = 0.6
        elif mins_remaining > 9:
            time_conf = 0.8
        elif mins_remaining > 6:
            time_conf = 1.0
        elif mins_remaining > 3:
            time_conf = 1.3
        else:
            time_conf = 1.6
        
        abs_dist = abs(distance)
        if abs_dist > 100:
            dist_conf = 1.5
        elif abs_dist > 50:
            dist_conf = 1.3
        elif abs_dist > 30:
            dist_conf = 1.1
        elif abs_dist > 20:
            dist_conf = 1.0
        elif abs_dist > 10:
            dist_conf = 0.8
        else:
            dist_conf = 0.6
        
        if abs_dist < 20:
            time_conf *= 0.5
        elif abs_dist < 50:
            time_conf *= 0.8
        
        return (time_conf * dist_conf) ** 0.5
    
    print(f"\nRunning backtest on {df['market_id'].nunique()} markets...")
    
    results = []
    
    for market_id in df['market_id'].unique():
        mdf = df[df['market_id'] == market_id].sort_values('timestamp')
        
        if len(mdf) < 5:
            continue
        
        winner = mdf['winner'].iloc[0]
        strike = mdf['strike_price'].iloc[0]
        
        # Simulate trading
        up_shares = 0
        down_shares = 0
        total_cost = 0
        base_size = budget_per_market / 200
        trades = 0
        
        for _, row in mdf.iterrows():
            if total_cost >= budget_per_market * 0.9:
                break
            
            distance = row['distance_to_strike']
            mins_left = row['minutes_remaining']
            
            signal = get_signal(distance, mins_left)
            confidence = calculate_confidence(distance, mins_left)
            
            size_mult = min(max(confidence, 0.5), 2.0)
            trade_size = base_size * size_mult
            trade_size = min(trade_size, budget_per_market - total_cost)
            
            up_ratio, down_ratio = signal_to_ratio(signal)
            
            # Assume 50/50 pricing (conservative)
            up_price = 0.50
            down_price = 0.50
            
            up_cost = trade_size * up_ratio
            down_cost = trade_size * down_ratio
            
            up_shares += up_cost / up_price
            down_shares += down_cost / down_price
            total_cost += up_cost + down_cost
            trades += 1
        
        # Calculate P&L
        if winner.lower() == 'up':
            payout = up_shares
        else:
            payout = down_shares
        
        pnl = payout - total_cost
        
        results.append({
            'market_id': market_id,
            'strike_price': strike,
            'winner': winner,
            'trades': trades,
            'up_shares': up_shares,
            'down_shares': down_shares,
            'total_cost': total_cost,
            'payout': payout,
            'pnl': pnl,
            'roi': pnl / total_cost * 100 if total_cost > 0 else 0,
        })
    
    return pd.DataFrame(results)


def print_backtest_results(results_df: pd.DataFrame):
    """Print backtest results summary."""
    print("\n" + "=" * 70)
    print("BACKTEST RESULTS")
    print("=" * 70)
    
    print(f"\nMarkets tested: {len(results_df)}")
    print(f"Markets profitable: {(results_df['pnl'] > 0).sum()} ({(results_df['pnl'] > 0).mean()*100:.1f}%)")
    
    total_cost = results_df['total_cost'].sum()
    total_pnl = results_df['pnl'].sum()
    
    print(f"\nTotal invested: ${total_cost:,.2f}")
    print(f"Total payout: ${results_df['payout'].sum():,.2f}")
    print(f"Total P&L: ${total_pnl:,.2f}")
    print(f"Overall ROI: {total_pnl / total_cost * 100:.1f}%")
    
    print(f"\nPer-market stats:")
    print(f"  Average P&L: ${results_df['pnl'].mean():.2f}")
    print(f"  Median P&L: ${results_df['pnl'].median():.2f}")
    print(f"  Best market: ${results_df['pnl'].max():.2f}")
    print(f"  Worst market: ${results_df['pnl'].min():.2f}")
    print(f"  Std dev: ${results_df['pnl'].std():.2f}")
    
    # Win/loss breakdown
    wins = results_df[results_df['pnl'] > 0]
    losses = results_df[results_df['pnl'] <= 0]
    
    print(f"\nWin/Loss Analysis:")
    print(f"  Wins: {len(wins)} (avg +${wins['pnl'].mean():.2f})" if len(wins) > 0 else "  Wins: 0")
    print(f"  Losses: {len(losses)} (avg ${losses['pnl'].mean():.2f})" if len(losses) > 0 else "  Losses: 0")
    
    if len(wins) > 0 and len(losses) > 0:
        profit_factor = wins['pnl'].sum() / abs(losses['pnl'].sum())
        print(f"  Profit Factor: {profit_factor:.2f}")
    
    # By winner
    print(f"\nBy Outcome:")
    for winner in ['Up', 'Down']:
        subset = results_df[results_df['winner'].str.lower() == winner.lower()]
        if len(subset) > 0:
            win_rate = (subset['pnl'] > 0).mean() * 100
            avg_pnl = subset['pnl'].mean()
            print(f"  {winner}: {len(subset)} markets, {win_rate:.0f}% profitable, avg ${avg_pnl:.2f}")

# =============================================================================
# MAIN
# =============================================================================

def main():
    parser = argparse.ArgumentParser(description="Fetch historical data for backtesting")
    parser.add_argument("--days", type=int, default=7, help="Days of history")
    parser.add_argument("--threads", type=int, default=20, help="Parallel threads")
    parser.add_argument("--markets", type=int, default=100, help="Max markets to fetch")
    parser.add_argument("--interval", type=int, default=60, help="Sample interval in seconds")
    
    args = parser.parse_args()
    
    CONFIG["threads"] = args.threads
    
    # Create output directory
    output_dir = CONFIG["output_dir"]
    if not os.path.exists(output_dir):
        os.makedirs(output_dir)
    
    print("=" * 70)
    print("POLYMARKET BTC HISTORICAL DATA FETCHER")
    print("=" * 70)
    print(f"Days: {args.days} | Threads: {args.threads} | Markets: {args.markets}")
    
    # Stage 1: Fetch markets
    print(f"\n--- STAGE 1: FETCH MARKETS ---")
    markets_df = fetch_resolved_btc_markets(limit=args.markets)
    
    if markets_df.empty:
        print("No markets found!")
        return
    
    # Filter by date
    cutoff = datetime.now(timezone.utc) - timedelta(days=args.days)
    markets_df['start_time'] = pd.to_datetime(markets_df['start_time'], utc=True)
    markets_df['end_time'] = pd.to_datetime(markets_df['end_time'], utc=True)
    markets_df = markets_df[markets_df['end_time'] >= cutoff]
    
    print(f"Markets in last {args.days} days: {len(markets_df)}")
    
    if markets_df.empty:
        print("No markets in date range!")
        return
    
    # Save markets
    markets_df.to_csv(f"{output_dir}/markets.csv", index=False)
    
    # Stage 2: Fetch BTC prices
    print(f"\n--- STAGE 2: FETCH BTC PRICES (PARALLEL) ---")
    btc_prices = fetch_all_btc_prices_parallel(markets_df, threads=args.threads)
    
    if not btc_prices:
        print("No BTC prices fetched!")
        return
    
    # Save prices
    prices_df = pd.DataFrame([
        {"minute_ts": ts, **data} for ts, data in btc_prices.items()
    ])
    prices_df.to_csv(f"{output_dir}/btc_prices.csv", index=False)
    
    # Stage 3: Create combined dataset
    print(f"\n--- STAGE 3: CREATE BACKTEST DATASET ---")
    combined_df = create_backtest_dataset(
        markets_df, btc_prices, sample_interval_sec=args.interval
    )
    
    if combined_df.empty:
        print("Failed to create dataset!")
        return
    
    combined_df.to_csv(f"{output_dir}/backtest_data.csv", index=False)
    
    # Stage 4: Run backtest
    print(f"\n--- STAGE 4: RUN BACKTEST ---")
    results = run_backtest(combined_df)
    
    if results.empty:
        print("Backtest produced no results!")
        return
    
    results.to_csv(f"{output_dir}/backtest_results.csv", index=False)
    print_backtest_results(results)
    
    print(f"\n--- COMPLETE ---")
    print(f"Output files in: {output_dir}/")
    print(f"  markets.csv - {len(markets_df)} markets")
    print(f"  btc_prices.csv - {len(btc_prices)} price points")
    print(f"  backtest_data.csv - {len(combined_df)} data points")
    print(f"  backtest_results.csv - {len(results)} market results")


if __name__ == "__main__":
    main()
