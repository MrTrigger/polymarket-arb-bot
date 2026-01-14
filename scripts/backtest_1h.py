#!/usr/bin/env python3
"""
Backtest analysis for 1-hour crypto up/down markets on Polymarket.

Downloads historical price data and analyzes arbitrage opportunities.
"""

import requests
import json
from datetime import datetime, timedelta
from dataclasses import dataclass
from typing import List, Dict, Optional, Tuple
import time

# Polymarket API endpoints
GAMMA_API = "https://gamma-api.polymarket.com"
CLOB_API = "https://clob.polymarket.com"


@dataclass
class MarketWindow:
    """A single 1-hour market window."""
    title: str
    slug: str
    event_id: str
    condition_id: str
    yes_token_id: str
    no_token_id: str
    start_time: datetime
    end_time: datetime
    volume: float
    outcome: Optional[str] = None  # "Up" or "Down" after settlement


@dataclass
class PricePoint:
    """A single price observation."""
    timestamp: datetime
    yes_price: float
    no_price: float
    combined_cost: float
    arb_margin: float  # How much below $1.00


def fetch_closed_markets(asset: str = "bitcoin", days_back: int = 5) -> List[MarketWindow]:
    """Fetch closed 1-hour markets for the given asset."""
    markets = []

    today = datetime.now()
    hours = [9, 10, 11, 12, 13, 14]  # Trading hours (ET)

    for days_ago in range(1, days_back + 1):
        date = today - timedelta(days=days_ago)
        month = date.strftime("%B").lower()
        day = date.day

        print(f"  Checking {month} {day}...")

        for hour in hours:
            period = "am" if hour < 12 else "pm"
            display_hour = hour if hour <= 12 else hour - 12
            if hour == 12:
                period = "pm"
                display_hour = 12

            slug = f"{asset}-up-or-down-{month}-{day}-{display_hour}{period}-et"

            try:
                resp = requests.get(f"{GAMMA_API}/events?slug={slug}", timeout=10)
                if resp.ok and resp.json():
                    event = resp.json()[0]
                    if event.get("closed"):
                        # Extract condition ID from market
                        for m in event.get("markets", []):
                            condition_id = m.get("conditionId", "")

                            # Get tokens from CLOB API
                            clob_resp = requests.get(f"{CLOB_API}/markets/{condition_id}", timeout=10)
                            if not clob_resp.ok:
                                continue

                            clob_data = clob_resp.json()
                            yes_token = None
                            no_token = None

                            for t in clob_data.get("tokens", []):
                                if t.get("outcome") == "Up":
                                    yes_token = t.get("token_id")
                                elif t.get("outcome") == "Down":
                                    no_token = t.get("token_id")

                            if yes_token and no_token:
                                # Parse end time to get start time (1 hour before)
                                end_str = event.get("endDate", "")
                                end_time = datetime.fromisoformat(end_str.replace("Z", "+00:00"))
                                start_time = end_time - timedelta(hours=1)

                                markets.append(MarketWindow(
                                    title=event["title"],
                                    slug=event["slug"],
                                    event_id=event["id"],
                                    condition_id=condition_id,
                                    yes_token_id=yes_token,
                                    no_token_id=no_token,
                                    start_time=start_time.replace(tzinfo=None),
                                    end_time=end_time.replace(tzinfo=None),
                                    volume=event.get("volume", 0)
                                ))
                                print(f"    Found: {event['title']}")
            except Exception as e:
                print(f"  Error fetching {slug}: {e}")

            time.sleep(0.1)  # Rate limiting

    return markets


def fetch_price_history(token_id: str, start_time: datetime, end_time: datetime) -> List[Tuple[datetime, float]]:
    """Fetch historical prices for a token."""
    prices = []

    # Convert to timestamps
    start_ts = int(start_time.timestamp())
    end_ts = int(end_time.timestamp())

    # Use CLOB prices endpoint
    try:
        url = f"{CLOB_API}/prices-history"
        params = {
            "market": token_id,
            "startTs": start_ts,
            "endTs": end_ts,
            "fidelity": 60  # 1-minute intervals
        }
        resp = requests.get(url, params=params, timeout=30)

        if resp.ok:
            data = resp.json()
            history = data.get("history", [])
            for point in history:
                ts = datetime.fromtimestamp(point.get("t", 0))
                price = float(point.get("p", 0))
                prices.append((ts, price))
    except Exception as e:
        print(f"  Error fetching prices for {token_id}: {e}")

    return prices


def analyze_market(market: MarketWindow) -> Dict:
    """Analyze a single market for arbitrage opportunities."""
    print(f"\nAnalyzing: {market.title}")

    # Extend time range to capture pre-window activity
    start = market.start_time - timedelta(minutes=30)
    end = market.end_time + timedelta(minutes=5)

    # Fetch price histories
    yes_prices = fetch_price_history(market.yes_token_id, start, end)
    no_prices = fetch_price_history(market.no_token_id, start, end)

    if not yes_prices or not no_prices:
        print(f"  No price data available")
        return {"market": market.title, "error": "No price data"}

    print(f"  YES prices: {len(yes_prices)} points")
    print(f"  NO prices: {len(no_prices)} points")

    # Build time-aligned price series
    yes_dict = {p[0].replace(second=0, microsecond=0): p[1] for p in yes_prices}
    no_dict = {p[0].replace(second=0, microsecond=0): p[1] for p in no_prices}

    # Find common timestamps
    common_times = sorted(set(yes_dict.keys()) & set(no_dict.keys()))

    if not common_times:
        print(f"  No overlapping timestamps")
        return {"market": market.title, "error": "No overlapping data"}

    # Calculate combined costs and arb margins
    opportunities = []
    for ts in common_times:
        yes_ask = yes_dict[ts]
        no_ask = no_dict[ts]
        combined = yes_ask + no_ask
        margin = 1.0 - combined

        if margin > 0:  # Arb opportunity
            opportunities.append({
                "timestamp": ts,
                "yes_ask": yes_ask,
                "no_ask": no_ask,
                "combined": combined,
                "margin": margin,
                "margin_bps": margin * 10000
            })

    # Calculate statistics
    all_margins = [1.0 - (yes_dict[t] + no_dict[t]) for t in common_times]

    result = {
        "market": market.title,
        "volume": market.volume,
        "data_points": len(common_times),
        "arb_opportunities": len(opportunities),
        "min_margin": min(all_margins) if all_margins else 0,
        "max_margin": max(all_margins) if all_margins else 0,
        "avg_margin": sum(all_margins) / len(all_margins) if all_margins else 0,
        "best_arbs": sorted(opportunities, key=lambda x: -x["margin"])[:5]
    }

    print(f"  Data points: {result['data_points']}")
    print(f"  Arb opportunities: {result['arb_opportunities']} ({100*result['arb_opportunities']/max(1,result['data_points']):.1f}%)")
    print(f"  Margin range: {result['min_margin']*10000:.0f} to {result['max_margin']*10000:.0f} bps")
    print(f"  Avg margin: {result['avg_margin']*10000:.1f} bps")

    if result['best_arbs']:
        print(f"  Best arb: {result['best_arbs'][0]['margin_bps']:.0f} bps at {result['best_arbs'][0]['timestamp']}")

    return result


def run_backtest():
    """Run full backtest on historical 1-hour markets."""
    print("=" * 60)
    print("1-HOUR MARKET BACKTEST ANALYSIS")
    print("=" * 60)

    # Fetch closed markets
    print("\nFetching closed BTC 1-hour markets...")
    btc_markets = fetch_closed_markets("bitcoin", days_back=5)
    print(f"Found {len(btc_markets)} closed BTC markets")

    print("\nFetching closed ETH 1-hour markets...")
    eth_markets = fetch_closed_markets("ethereum", days_back=5)
    print(f"Found {len(eth_markets)} closed ETH markets")

    print("\nFetching closed SOL 1-hour markets...")
    sol_markets = fetch_closed_markets("solana", days_back=5)
    print(f"Found {len(sol_markets)} closed SOL markets")

    all_markets = btc_markets + eth_markets + sol_markets
    print(f"\nTotal markets to analyze: {len(all_markets)}")

    # Analyze each market
    results = []
    for market in all_markets:
        result = analyze_market(market)
        results.append(result)
        time.sleep(0.5)  # Rate limiting

    # Summary statistics
    print("\n" + "=" * 60)
    print("SUMMARY")
    print("=" * 60)

    valid_results = [r for r in results if "error" not in r]

    if valid_results:
        total_arb_opps = sum(r["arb_opportunities"] for r in valid_results)
        total_data_points = sum(r["data_points"] for r in valid_results)
        avg_arb_rate = total_arb_opps / total_data_points if total_data_points > 0 else 0

        all_margins = [r["avg_margin"] for r in valid_results if r["avg_margin"] != 0]

        print(f"\nMarkets analyzed: {len(valid_results)}")
        print(f"Total data points: {total_data_points}")
        print(f"Total arb opportunities: {total_arb_opps}")
        print(f"Arb opportunity rate: {100*avg_arb_rate:.1f}%")

        if all_margins:
            print(f"\nAverage margin across markets: {10000*sum(all_margins)/len(all_margins):.1f} bps")

        # Best markets
        print("\nTop 5 markets by arb opportunities:")
        for r in sorted(valid_results, key=lambda x: -x["arb_opportunities"])[:5]:
            print(f"  {r['market']}: {r['arb_opportunities']} opps, avg margin {r['avg_margin']*10000:.1f} bps")

    # Save results
    output_file = "backtest_1h_results.json"
    with open(output_file, "w") as f:
        json.dump(results, f, indent=2, default=str)
    print(f"\nResults saved to {output_file}")

    return results


if __name__ == "__main__":
    run_backtest()
