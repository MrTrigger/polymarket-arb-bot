#!/usr/bin/env python3
"""
Fast production backtest with local caching and parameter optimization.

1. Downloads all market data upfront and caches to JSON
2. Runs fast parameter sweeps without API calls
3. Finds optimal parameters for profitability
"""

import requests
import json
import os
from datetime import datetime, timedelta
from dataclasses import dataclass
from typing import List, Dict, Optional, Tuple
import time
from concurrent.futures import ThreadPoolExecutor, as_completed

GAMMA_API = "https://gamma-api.polymarket.com"
CLOB_API = "https://clob.polymarket.com"
BINANCE_API = "https://api.binance.com/api/v3"
CACHE_FILE = "backtest_cache.json"


@dataclass
class CachedMarket:
    title: str
    asset: str
    start_time: str
    end_time: str
    open_price: float
    close_price: float
    settlement: str  # "Up" or "Down"
    yes_prices: List[Tuple[str, float]]  # [(timestamp, price), ...]
    no_prices: List[Tuple[str, float]]
    volume: float
    minute_prices: List[Tuple[int, float]] = None  # [(minute, spot_price), ...]


def fetch_binance_candle(asset: str, start_time: datetime) -> Tuple[float, float]:
    """Fetch open and close price for the 1-hour candle."""
    symbol = f"{asset.upper()}USDT"
    hour_start = start_time.replace(minute=0, second=0, microsecond=0)
    start_ms = int(hour_start.timestamp() * 1000)
    end_ms = start_ms + 3600000

    try:
        url = f"{BINANCE_API}/klines"
        params = {"symbol": symbol, "interval": "1h", "startTime": start_ms, "endTime": end_ms, "limit": 1}
        resp = requests.get(url, params=params, timeout=10)
        if resp.ok and resp.json():
            candle = resp.json()[0]
            return float(candle[1]), float(candle[4])  # open, close
    except:
        pass
    return 0.0, 0.0


def fetch_binance_minute_candles(asset: str, start_time: datetime) -> List[Tuple[int, float]]:
    """Fetch 1-minute candles for an hour. Returns [(minute, close_price), ...]"""
    symbol = f"{asset.upper()}USDT"
    hour_start = start_time.replace(minute=0, second=0, microsecond=0)
    start_ms = int(hour_start.timestamp() * 1000)
    end_ms = start_ms + 3600000

    try:
        url = f"{BINANCE_API}/klines"
        params = {"symbol": symbol, "interval": "1m", "startTime": start_ms, "endTime": end_ms, "limit": 60}
        resp = requests.get(url, params=params, timeout=15)
        if resp.ok and resp.json():
            candles = []
            for i, c in enumerate(resp.json()):
                candles.append((i, float(c[4])))  # minute, close
            return candles
    except:
        pass
    return []


def fetch_settlement(event_id: str) -> Optional[str]:
    try:
        resp = requests.get(f"{GAMMA_API}/events/{event_id}", timeout=10)
        if resp.ok:
            event = resp.json()
            for market in event.get("markets", []):
                outcomes = market.get("outcomePrices", "")
                if outcomes:
                    prices = json.loads(outcomes)
                    if len(prices) >= 2:
                        if float(prices[0]) == 1:
                            return "Up"
                        elif float(prices[1]) == 1:
                            return "Down"
    except:
        pass
    return None


def fetch_price_history(token_id: str, start: datetime, end: datetime) -> List[Tuple[str, float]]:
    prices = []
    try:
        url = f"{CLOB_API}/prices-history"
        params = {"market": token_id, "startTs": int(start.timestamp()), "endTs": int(end.timestamp()), "fidelity": 60}
        resp = requests.get(url, params=params, timeout=30)
        if resp.ok:
            for p in resp.json().get("history", []):
                ts = datetime.fromtimestamp(p["t"]).isoformat()
                prices.append((ts, float(p["p"])))
    except:
        pass
    return prices


def detect_asset(title: str) -> str:
    title_lower = title.lower()
    if "btc" in title_lower or "bitcoin" in title_lower:
        return "BTC"
    if "eth" in title_lower or "ethereum" in title_lower:
        return "ETH"
    if "sol" in title_lower or "solana" in title_lower:
        return "SOL"
    return "UNKNOWN"


def fetch_single_market(slug: str) -> Optional[CachedMarket]:
    """Fetch all data for a single market."""
    try:
        resp = requests.get(f"{GAMMA_API}/events?slug={slug}", timeout=10)
        if not resp.ok or not resp.json():
            return None

        event = resp.json()[0]
        if not event.get("closed"):
            return None

        title = event["title"]
        asset = detect_asset(title)

        # Get end time and calculate start
        end_str = event.get("endDate", "")
        end_time = datetime.fromisoformat(end_str.replace("Z", "+00:00"))
        start_time = end_time - timedelta(hours=1)

        # Get Binance candle data
        open_price, close_price = fetch_binance_candle(asset, start_time)
        if open_price <= 0:
            return None

        # Get settlement
        settlement = fetch_settlement(event["id"])
        if not settlement:
            return None

        # Get token IDs
        for m in event.get("markets", []):
            cond_id = m.get("conditionId", "")
            clob = requests.get(f"{CLOB_API}/markets/{cond_id}", timeout=10)
            if not clob.ok:
                continue

            clob_data = clob.json()
            yes_token = no_token = None

            for t in clob_data.get("tokens", []):
                if t.get("outcome") == "Up":
                    yes_token = t.get("token_id")
                elif t.get("outcome") == "Down":
                    no_token = t.get("token_id")

            if yes_token and no_token:
                # Fetch price histories
                fetch_start = start_time - timedelta(minutes=15)
                yes_prices = fetch_price_history(yes_token, fetch_start, end_time)
                no_prices = fetch_price_history(no_token, fetch_start, end_time)

                # Fetch Binance minute candles for realistic simulation
                minute_prices = fetch_binance_minute_candles(asset, start_time)

                return CachedMarket(
                    title=title,
                    asset=asset,
                    start_time=start_time.replace(tzinfo=None).isoformat(),
                    end_time=end_time.replace(tzinfo=None).isoformat(),
                    open_price=open_price,
                    close_price=close_price,
                    settlement=settlement,
                    yes_prices=yes_prices,
                    no_prices=no_prices,
                    volume=event.get("volume", 0),
                    minute_prices=minute_prices
                )
    except Exception as e:
        print(f"  Error fetching {slug}: {e}")
    return None


def build_cache(days_back: int = 30) -> List[Dict]:
    """Build cache of all market data."""
    print("Building market data cache...")
    print(f"Scanning {days_back} days for BTC, ETH, SOL markets")

    slugs = []
    today = datetime.now()
    hours = [9, 10, 11, 12, 13, 14]

    for asset in ["bitcoin", "ethereum", "solana"]:
        for days_ago in range(1, days_back + 1):
            date = today - timedelta(days=days_ago)
            if date.weekday() >= 5:
                continue
            month = date.strftime("%B").lower()
            day = date.day
            for hour in hours:
                period = "am" if hour < 12 else "pm"
                display_hour = hour if hour <= 12 else hour - 12
                if hour == 12:
                    display_hour = 12
                slugs.append(f"{asset}-up-or-down-{month}-{day}-{display_hour}{period}-et")

    print(f"Found {len(slugs)} potential markets to fetch")

    markets = []
    failed = 0

    # Fetch markets with progress
    for i, slug in enumerate(slugs):
        if (i + 1) % 20 == 0:
            print(f"  Progress: {i+1}/{len(slugs)} ({len(markets)} cached)")

        market = fetch_single_market(slug)
        if market:
            markets.append({
                "title": market.title,
                "asset": market.asset,
                "start_time": market.start_time,
                "end_time": market.end_time,
                "open_price": market.open_price,
                "close_price": market.close_price,
                "settlement": market.settlement,
                "yes_prices": market.yes_prices,
                "no_prices": market.no_prices,
                "volume": market.volume,
                "minute_prices": market.minute_prices or []
            })
        else:
            failed += 1

        time.sleep(0.05)  # Rate limit

    print(f"\nCache complete: {len(markets)} markets, {failed} failed")
    return markets


def load_or_build_cache(days_back: int = 30, force_rebuild: bool = False) -> List[Dict]:
    """Load cache from file or build it."""
    if os.path.exists(CACHE_FILE) and not force_rebuild:
        print(f"Loading cache from {CACHE_FILE}...")
        with open(CACHE_FILE, 'r') as f:
            data = json.load(f)
            print(f"Loaded {len(data)} markets from cache")
            return data

    markets = build_cache(days_back)

    print(f"Saving cache to {CACHE_FILE}...")
    with open(CACHE_FILE, 'w') as f:
        json.dump(markets, f)

    return markets


@dataclass
class Config:
    # Directional thresholds (percentage from open price)
    # Lower defaults for linear interpolation (0.02% = 20 bps)
    strong_threshold: float = 0.02
    lean_threshold: float = 0.01

    # Allocation ratios
    strong_ratio: float = 0.75
    lean_ratio: float = 0.60

    # Combined cost limit
    max_combined: float = 1.03

    # Position sizing
    order_size: float = 50.0
    max_per_market: float = 200.0

    # Time thresholds (seconds) for tightening
    time_tight_1: int = 1800  # 30 min
    time_tight_2: int = 600   # 10 min
    time_tight_3: int = 180   # 3 min


def simulate_fast(markets: List[Dict], cfg: Config) -> Dict:
    """
    Fast simulation using real Binance minute candle data.

    Uses actual minute-by-minute spot prices from Binance to calculate signals,
    which captures price reversals and random walk behavior.
    YES/NO prices are estimated from implied probability.
    """
    results = {
        "trades": 0,
        "capital": 0.0,
        "pnl": 0.0,
        "winners": 0,
        "losers": 0,
        "by_signal": {},
        "by_asset": {}
    }

    for m in markets:
        if not m["settlement"] or m["open_price"] <= 0:
            continue

        open_price = m["open_price"]
        settlement = m["settlement"]
        asset = m["asset"]

        # Build minute price dict from cached data
        minute_prices = {mp[0]: mp[1] for mp in m.get("minute_prices", [])}
        if not minute_prices:
            # Fallback to linear interpolation if no minute data
            for minute in range(0, 60):
                progress = minute / 60.0
                minute_prices[minute] = open_price + (m["close_price"] - open_price) * progress

        total_cost = 0.0

        # Simulate at multiple entry points (every 5 minutes from minute 5 to minute 55)
        for minute in range(5, 56, 5):
            secs_left = (60 - minute) * 60

            if total_cost >= cfg.max_per_market:
                break

            # Get real spot price at this minute
            spot = minute_prices.get(minute, open_price)

            # Calculate time-adjusted thresholds
            if secs_left < cfg.time_tight_3:
                factor = 0.3
            elif secs_left < cfg.time_tight_2:
                factor = 0.5
            elif secs_left < cfg.time_tight_1:
                factor = 0.7
            else:
                factor = 1.0

            strong = cfg.strong_threshold * factor
            lean = cfg.lean_threshold * factor

            # Calculate signal based on real spot price
            dist_pct = abs(spot - open_price) / open_price * 100
            is_above = spot > open_price

            if dist_pct >= strong:
                signal = "StrongUp" if is_above else "StrongDown"
                ratio = cfg.strong_ratio
            elif dist_pct >= lean:
                signal = "LeanUp" if is_above else "LeanDown"
                ratio = cfg.lean_ratio
            else:
                continue  # Neutral - no trade

            # Estimate YES/NO prices from implied probability
            progress = minute / 60.0
            base_prob = 0.50
            time_weight = progress ** 0.5
            if is_above:
                yes_prob = base_prob + (0.5 * time_weight * min(dist_pct, 1.0))
            else:
                yes_prob = base_prob - (0.5 * time_weight * min(dist_pct, 1.0))
            yes_prob = max(0.05, min(0.95, yes_prob))

            # Add typical spread (2%)
            yes_p = yes_prob + 0.01
            no_p = (1 - yes_prob) + 0.01
            combined = yes_p + no_p

            if combined > cfg.max_combined:
                continue

            # Calculate allocation
            if "Up" in signal:
                yes_alloc, no_alloc = ratio, 1 - ratio
            else:
                yes_alloc, no_alloc = 1 - ratio, ratio

            remaining = cfg.max_per_market - total_cost
            size = min(cfg.order_size, remaining)

            yes_qty = size * yes_alloc / yes_p if yes_p > 0 else 0
            no_qty = size * no_alloc / no_p if no_p > 0 else 0
            cost = yes_qty * yes_p + no_qty * no_p

            # Calculate P&L
            if settlement == "Up":
                pnl = yes_qty * 1.0 - cost
            else:
                pnl = no_qty * 1.0 - cost

            # Update results
            results["trades"] += 1
            results["capital"] += cost
            results["pnl"] += pnl
            if pnl > 0:
                results["winners"] += 1
            else:
                results["losers"] += 1

            # By signal
            if signal not in results["by_signal"]:
                results["by_signal"][signal] = {"n": 0, "pnl": 0, "cost": 0, "wins": 0}
            results["by_signal"][signal]["n"] += 1
            results["by_signal"][signal]["pnl"] += pnl
            results["by_signal"][signal]["cost"] += cost
            if pnl > 0:
                results["by_signal"][signal]["wins"] += 1

            # By asset
            if asset not in results["by_asset"]:
                results["by_asset"][asset] = {"n": 0, "pnl": 0, "cost": 0}
            results["by_asset"][asset]["n"] += 1
            results["by_asset"][asset]["pnl"] += pnl
            results["by_asset"][asset]["cost"] += cost

            total_cost += cost

    return results


def run_parameter_sweep(markets: List[Dict]) -> List[Dict]:
    """Run parameter sweep to find optimal config."""
    print("\n" + "=" * 60)
    print("PARAMETER SWEEP")
    print("=" * 60)

    configs = []

    # Generate parameter combinations
    # Lower thresholds since linear interpolation produces smooth price progression
    for strong in [0.01, 0.02, 0.03, 0.05, 0.07, 0.10]:
        for lean in [0.005, 0.01, 0.015, 0.02, 0.03]:
            if lean >= strong:
                continue
            for strong_ratio in [0.70, 0.75, 0.80, 0.85, 0.90]:
                for lean_ratio in [0.55, 0.60, 0.65]:
                    for max_combined in [1.01, 1.02, 1.03, 1.05]:
                        configs.append(Config(
                            strong_threshold=strong,
                            lean_threshold=lean,
                            strong_ratio=strong_ratio,
                            lean_ratio=lean_ratio,
                            max_combined=max_combined
                        ))

    print(f"Testing {len(configs)} parameter combinations...")

    results = []
    for i, cfg in enumerate(configs):
        if (i + 1) % 100 == 0:
            print(f"  Progress: {i+1}/{len(configs)}")

        r = simulate_fast(markets, cfg)
        if r["trades"] > 0:
            ret = r["pnl"] / r["capital"] * 100 if r["capital"] > 0 else 0
            results.append({
                "config": {
                    "strong_threshold": cfg.strong_threshold,
                    "lean_threshold": cfg.lean_threshold,
                    "strong_ratio": cfg.strong_ratio,
                    "lean_ratio": cfg.lean_ratio,
                    "max_combined": cfg.max_combined,
                },
                "trades": r["trades"],
                "capital": r["capital"],
                "pnl": r["pnl"],
                "return_pct": ret,
                "win_rate": r["winners"] / r["trades"] * 100,
                "by_signal": r["by_signal"]
            })

    # Sort by return
    results.sort(key=lambda x: x["return_pct"], reverse=True)
    return results


def print_results(markets: List[Dict], cfg: Config):
    """Print detailed results for a config."""
    r = simulate_fast(markets, cfg)

    print("\n" + "=" * 70)
    print("                         BACKTEST RESULTS")
    print("=" * 70)

    # Market stats
    up_count = sum(1 for m in markets if m["settlement"] == "Up")
    down_count = sum(1 for m in markets if m["settlement"] == "Down")

    print(f"\nMARKET DATA")
    print(f"  Total Markets:    {len(markets)}")
    print(f"  Up Settlements:   {up_count} ({up_count/len(markets)*100:.1f}%)")
    print(f"  Down Settlements: {down_count} ({down_count/len(markets)*100:.1f}%)")

    print(f"\nTRADING RESULTS")
    print(f"  Trades:           {r['trades']}")
    print(f"  Capital Deployed: ${r['capital']:,.2f}")
    print(f"  Gross P&L:        ${r['pnl']:,.2f}")
    ret = r['pnl'] / r['capital'] * 100 if r['capital'] > 0 else 0
    print(f"  Return:           {ret:.2f}%")
    wr = r['winners'] / r['trades'] * 100 if r['trades'] > 0 else 0
    print(f"  Win Rate:         {wr:.1f}%")

    print(f"\nBY SIGNAL")
    print(f"  {'Signal':<12} {'Trades':>8} {'P&L':>12} {'Win%':>8} {'Return%':>10}")
    print(f"  {'-'*12} {'-'*8} {'-'*12} {'-'*8} {'-'*10}")
    for sig, data in sorted(r["by_signal"].items(), key=lambda x: -x[1]["n"]):
        wr = data["wins"]/data["n"]*100 if data["n"] > 0 else 0
        ret = data["pnl"]/data["cost"]*100 if data["cost"] > 0 else 0
        print(f"  {sig:<12} {data['n']:>8} ${data['pnl']:>10.2f} {wr:>7.1f}% {ret:>9.2f}%")

    print(f"\nBY ASSET")
    for asset, data in r["by_asset"].items():
        ret = data['pnl']/data['cost']*100 if data['cost'] > 0 else 0
        print(f"  {asset}: {data['n']} trades | ${data['cost']:,.2f} -> ${data['pnl']:,.2f} ({ret:.2f}%)")

    print(f"\nCONFIGURATION")
    print(f"  Strong Threshold: {cfg.strong_threshold}%")
    print(f"  Lean Threshold:   {cfg.lean_threshold}%")
    print(f"  Strong Ratio:     {cfg.strong_ratio}")
    print(f"  Lean Ratio:       {cfg.lean_ratio}")
    print(f"  Max Combined:     {cfg.max_combined}")

    print("\n" + "=" * 70)


def main():
    import argparse
    parser = argparse.ArgumentParser(description="Fast backtest with parameter optimization")
    parser.add_argument("--rebuild", action="store_true", help="Force rebuild cache")
    parser.add_argument("--days", type=int, default=30, help="Days of history")
    parser.add_argument("--sweep", action="store_true", help="Run parameter sweep")
    args = parser.parse_args()

    # Load or build cache
    markets = load_or_build_cache(args.days, args.rebuild)

    if not markets:
        print("No markets found!")
        return

    if args.sweep:
        # Run parameter sweep
        results = run_parameter_sweep(markets)

        print("\n" + "=" * 70)
        print("TOP 10 CONFIGURATIONS")
        print("=" * 70)
        for i, r in enumerate(results[:10]):
            cfg = r["config"]
            print(f"\n#{i+1}: Return {r['return_pct']:.2f}% | Win Rate {r['win_rate']:.1f}%")
            print(f"    Trades: {r['trades']} | Capital: ${r['capital']:,.2f} | P&L: ${r['pnl']:,.2f}")
            print(f"    Strong: {cfg['strong_threshold']}% | Lean: {cfg['lean_threshold']}%")
            print(f"    Ratios: {cfg['strong_ratio']}/{cfg['lean_ratio']} | MaxCombined: {cfg['max_combined']}")

        # Save sweep results
        with open("sweep_results.json", "w") as f:
            json.dump(results[:50], f, indent=2)
        print(f"\nTop 50 results saved to sweep_results.json")

        # Run detailed results with best config
        if results:
            best = results[0]["config"]
            best_cfg = Config(
                strong_threshold=best["strong_threshold"],
                lean_threshold=best["lean_threshold"],
                strong_ratio=best["strong_ratio"],
                lean_ratio=best["lean_ratio"],
                max_combined=best["max_combined"]
            )
            print_results(markets, best_cfg)
    else:
        # Run with default config
        cfg = Config()
        print_results(markets, cfg)


if __name__ == "__main__":
    main()
