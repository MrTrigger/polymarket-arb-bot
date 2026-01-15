#!/usr/bin/env python3
"""
Parameter sweep for position management backtesting.
Tests different configurations and finds optimal parameters for each window duration.
"""

import pandas as pd
import sys
import json
from typing import Tuple, List, Dict
from enum import Enum
from dataclasses import dataclass
from itertools import product
import argparse


class Signal(Enum):
    STRONG_UP = "strong_up"
    LEAN_UP = "lean_up"
    NEUTRAL = "neutral"
    LEAN_DOWN = "lean_down"
    STRONG_DOWN = "strong_down"


# Minimum ATR values per asset (fallback when dynamic ATR not available)
# These prevent over-trading in extremely low volatility periods
MIN_ATR_BY_ASSET = {
    "BTC": 20.0,    # Min $20 ATR for BTC
    "ETH": 1.0,     # Min $1 ATR for ETH
    "SOL": 0.20,    # Min $0.20 ATR for SOL
    "XRP": 0.002,   # Min $0.002 ATR for XRP
}


def get_min_atr(asset: str) -> float:
    """Get minimum ATR for an asset, defaulting to BTC if unknown."""
    return MIN_ATR_BY_ASSET.get(asset.upper(), 20.0)


def calculate_dynamic_atr(prices: list, window_size: int = 5) -> float:
    """Calculate dynamic ATR from a list of prices.

    Uses rolling window approach:
    - Divide prices into windows of ~window_size points
    - Calculate range (high - low) for each window
    - Return average of ranges

    This mirrors the Rust implementation in atr.rs
    """
    if len(prices) < 2:
        return 0.0

    # Calculate ranges for rolling windows
    ranges = []
    window_len = max(1, len(prices) // window_size)

    for i in range(0, len(prices) - window_len + 1, window_len):
        window = prices[i:i + window_len]
        if len(window) >= 2:
            high = max(window)
            low = min(window)
            ranges.append(high - low)

    if not ranges:
        # Fallback: calculate overall range
        return max(prices) - min(prices)

    return sum(ranges) / len(ranges)


@dataclass
class PMConfig:
    """Position Management configuration."""
    # Phase budget allocations (must sum to 1.0)
    early_budget: float = 0.15
    build_budget: float = 0.25
    core_budget: float = 0.30
    final_budget: float = 0.30

    # Confidence thresholds per phase
    early_conf: float = 0.80
    build_conf: float = 0.60
    core_conf: float = 0.50
    final_conf: float = 0.40

    # Signal allocation ratios
    strong_ratio: float = 0.78
    lean_ratio: float = 0.61

    # Signal thresholds (base values in dollars for BTC-scale)
    # These get scaled by time remaining
    base_lean: float = 30.0
    base_conviction: float = 60.0

    # Trade sizing
    trades_per_phase: int = 15
    size_mult_min: float = 0.5
    size_mult_max: float = 2.0


def get_phase_for_window(mins_remaining: float, window_mins: float) -> Tuple[str, float, float]:
    """Get phase based on minutes remaining, scaled for window duration."""
    # Scale phase boundaries based on window duration
    # For 15min: early=15->10, build=10->5, core=5->2, final=2->0
    # Scale proportionally for other windows
    scale = window_mins / 15.0

    early_start = 15.0 * scale
    build_start = 10.0 * scale
    core_start = 5.0 * scale
    final_start = 2.0 * scale

    if mins_remaining > build_start:
        return "early", 0.15, 0.80
    elif mins_remaining > core_start:
        return "build", 0.25, 0.60
    elif mins_remaining > final_start:
        return "core", 0.30, 0.50
    else:
        return "final", 0.30, 0.40


def get_phase(mins_remaining: float, cfg: PMConfig, window_mins: float = 15.0) -> Tuple[str, float, float]:
    """Get phase with custom config."""
    scale = window_mins / 15.0

    early_start = 15.0 * scale
    build_start = 10.0 * scale
    core_start = 5.0 * scale
    final_start = 2.0 * scale

    if mins_remaining > build_start:
        return "early", cfg.early_budget, cfg.early_conf
    elif mins_remaining > core_start:
        return "build", cfg.build_budget, cfg.build_conf
    elif mins_remaining > final_start:
        return "core", cfg.core_budget, cfg.core_conf
    else:
        return "final", cfg.final_budget, cfg.final_conf


def get_signal(distance: float, mins_remaining: float, cfg: PMConfig, window_mins: float = 15.0) -> Signal:
    """Get signal based on distance and time, scaled for window."""
    # Scale thresholds based on window duration
    scale = window_mins / 15.0

    # Time-based threshold adjustment
    if mins_remaining > 12 * scale:
        lean = cfg.base_lean
        conviction = cfg.base_conviction
    elif mins_remaining > 9 * scale:
        lean = cfg.base_lean * 0.83
        conviction = cfg.base_conviction * 0.83
    elif mins_remaining > 6 * scale:
        lean = cfg.base_lean * 0.50
        conviction = cfg.base_conviction * 0.67
    elif mins_remaining > 3 * scale:
        lean = cfg.base_lean * 0.40
        conviction = cfg.base_conviction * 0.58
    else:
        lean = cfg.base_lean * 0.27
        conviction = cfg.base_conviction * 0.42

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


def signal_to_ratio(signal: Signal, cfg: PMConfig) -> Tuple[float, float]:
    """Get UP/DOWN allocation ratio for signal."""
    if signal == Signal.STRONG_UP:
        return (cfg.strong_ratio, 1 - cfg.strong_ratio)
    elif signal == Signal.LEAN_UP:
        return (cfg.lean_ratio, 1 - cfg.lean_ratio)
    elif signal == Signal.NEUTRAL:
        return (0.50, 0.50)
    elif signal == Signal.LEAN_DOWN:
        return (1 - cfg.lean_ratio, cfg.lean_ratio)
    else:  # STRONG_DOWN
        return (1 - cfg.strong_ratio, cfg.strong_ratio)


def calculate_confidence(distance: float, mins_remaining: float, window_mins: float = 15.0, asset: str = "BTC", dynamic_atr: float = 0.0) -> float:
    """Calculate confidence score based on time and ATR-normalized distance.

    Distance confidence is now based on ATR multiples instead of fixed dollar amounts.
    This ensures consistent behavior across assets with different volatilities.

    Args:
        distance: Distance from spot to strike in dollars
        mins_remaining: Minutes remaining in window
        window_mins: Total window duration
        asset: Asset type (BTC, ETH, SOL, XRP)
        dynamic_atr: Dynamically calculated ATR from price data. If 0, uses minimum.
    """
    scale = window_mins / 15.0

    # Time factor
    if mins_remaining > 12 * scale:
        time_conf = 0.4
    elif mins_remaining > 9 * scale:
        time_conf = 0.5
    elif mins_remaining > 6 * scale:
        time_conf = 0.6
    elif mins_remaining > 3 * scale:
        time_conf = 0.8
    else:
        time_conf = 1.0

    # Use dynamic ATR if provided, otherwise use minimum
    min_atr = get_min_atr(asset)
    atr = max(dynamic_atr, min_atr) if dynamic_atr > 0 else min_atr
    atr_multiple = abs(distance) / atr if atr > 0 else 0

    # Thresholds based on ATR multiples:
    # - 0.25 ATR: small move (might be noise)
    # - 0.50 ATR: moderate move
    # - 0.75 ATR: significant move
    # - 1.00 ATR: large move
    # - 1.50 ATR: very large move
    if atr_multiple > 1.5:
        dist_conf = 1.0
    elif atr_multiple > 1.0:
        dist_conf = 0.85
    elif atr_multiple > 0.75:
        dist_conf = 0.7
    elif atr_multiple > 0.50:
        dist_conf = 0.55
    elif atr_multiple > 0.25:
        dist_conf = 0.4
    else:
        dist_conf = 0.2

    combined = (time_conf * dist_conf) ** 0.5
    if time_conf > 0.7 and dist_conf > 0.7:
        combined = min(1.0, combined * 1.2)

    return combined


def run_backtest_with_config(df: pd.DataFrame, cfg: PMConfig, budget_per_market: float = 1000, window_mins: float = 15.0, warmup_mins: int = 5) -> Dict:
    """Run backtest with specific config.

    Args:
        df: DataFrame with market data
        cfg: Position management config
        budget_per_market: Budget per market in dollars
        window_mins: Window duration in minutes
        warmup_mins: Number of minutes to use for ATR warm-up (simulates historical data)
    """
    results = {
        "trades": 0,
        "capital": 0.0,
        "pnl": 0.0,
        "winners": 0,
        "losers": 0,
        "markets": 0,
        "markets_profitable": 0,
        "by_signal": {},
        "by_phase": {},
    }

    for market_id in df['market_id'].unique():
        mdf = df[df['market_id'] == market_id].sort_values('timestamp')

        if len(mdf) < 3:
            continue

        winner = mdf['winner'].iloc[0]
        # Get asset for ATR-based confidence (default to BTC if not available)
        asset = mdf['asset'].iloc[0] if 'asset' in mdf.columns else "BTC"

        # Pre-calculate rolling ATR for each minute using only past data
        # This simulates having ATR warmed up from historical prices
        if 'spot_price' in mdf.columns:
            all_prices = mdf['spot_price'].tolist()
        else:
            all_prices = mdf['distance_to_strike'].tolist()

        # Calculate ATR at each point using only prices seen so far (plus warmup)
        # We assume the first `warmup_mins` of data is used for ATR calculation
        # and trading starts after that (realistic scenario)
        atr_by_idx = {}
        for i in range(len(all_prices)):
            # Use prices from start to current point for ATR
            # This simulates rolling ATR with proper warm-up
            prices_seen = all_prices[:i + 1]
            if len(prices_seen) >= warmup_mins:
                atr_by_idx[i] = calculate_dynamic_atr(prices_seen)
            elif len(prices_seen) >= 2:
                # Not fully warmed up, but have some data
                atr_by_idx[i] = calculate_dynamic_atr(prices_seen)
            else:
                atr_by_idx[i] = 0.0

        up_shares = 0
        down_shares = 0
        total_cost = 0
        market_trades = 0

        phase_budgets = {
            "early": budget_per_market * cfg.early_budget,
            "build": budget_per_market * cfg.build_budget,
            "core": budget_per_market * cfg.core_budget,
            "final": budget_per_market * cfg.final_budget,
        }
        phase_spent = {"early": 0, "build": 0, "core": 0, "final": 0}

        for idx, (_, row) in enumerate(mdf.iterrows()):
            distance = row['distance_to_strike']
            mins_left = row['minutes_remaining']

            # Get rolling ATR at this point (only past data, no look-ahead)
            dynamic_atr = atr_by_idx.get(idx, 0.0)

            phase_name, phase_pct, min_confidence = get_phase(mins_left, cfg, window_mins)
            confidence = calculate_confidence(distance, mins_left, window_mins, asset, dynamic_atr)

            if confidence < min_confidence:
                continue

            phase_remaining = phase_budgets[phase_name] - phase_spent[phase_name]
            if phase_remaining < 1.0:
                continue

            budget_remaining = budget_per_market - total_cost
            if budget_remaining < 1.0:
                break

            signal = get_signal(distance, mins_left, cfg, window_mins)
            if signal == Signal.NEUTRAL and total_cost > 0:
                continue

            base_size = phase_remaining / cfg.trades_per_phase
            size_mult = cfg.size_mult_min + (confidence * (cfg.size_mult_max - cfg.size_mult_min))
            trade_size = base_size * size_mult
            trade_size = min(trade_size, phase_remaining)
            trade_size = min(trade_size, budget_remaining * 0.15)
            trade_size = max(trade_size, 1.0)

            up_ratio, down_ratio = signal_to_ratio(signal, cfg)

            up_cost = trade_size * up_ratio
            down_cost = trade_size * down_ratio

            up_price = 0.50
            down_price = 0.50

            up_shares += up_cost / up_price
            down_shares += down_cost / down_price
            total_cost += trade_size
            phase_spent[phase_name] += trade_size
            market_trades += 1

            # Track by signal
            sig_name = signal.value
            if sig_name not in results["by_signal"]:
                results["by_signal"][sig_name] = {"n": 0, "cost": 0, "pnl": 0}
            results["by_signal"][sig_name]["n"] += 1
            results["by_signal"][sig_name]["cost"] += trade_size

            # Track by phase
            if phase_name not in results["by_phase"]:
                results["by_phase"][phase_name] = {"n": 0, "cost": 0, "pnl": 0}
            results["by_phase"][phase_name]["n"] += 1
            results["by_phase"][phase_name]["cost"] += trade_size

        if total_cost == 0:
            continue

        if winner.lower() == 'up':
            payout = up_shares
        else:
            payout = down_shares

        pnl = payout - total_cost

        results["markets"] += 1
        results["trades"] += market_trades
        results["capital"] += total_cost
        results["pnl"] += pnl

        if pnl > 0:
            results["winners"] += 1
            results["markets_profitable"] += 1
        else:
            results["losers"] += 1

        # Distribute P&L to signals (approximate)
        for sig_name in results["by_signal"]:
            if results["by_signal"][sig_name]["cost"] > 0:
                sig_share = results["by_signal"][sig_name]["cost"] / total_cost
                results["by_signal"][sig_name]["pnl"] += pnl * sig_share

    return results


def run_baseline(df: pd.DataFrame, budget_per_market: float = 1000, window_mins: float = 15.0) -> Dict:
    """Run baseline without position management (simple threshold-based)."""
    results = {
        "trades": 0,
        "capital": 0.0,
        "pnl": 0.0,
        "winners": 0,
        "losers": 0,
        "markets": 0,
        "markets_profitable": 0,
    }

    for market_id in df['market_id'].unique():
        mdf = df[df['market_id'] == market_id].sort_values('timestamp')

        if len(mdf) < 3:
            continue

        winner = mdf['winner'].iloc[0]
        # Get asset for ATR-based thresholds
        asset = mdf['asset'].iloc[0] if 'asset' in mdf.columns else "BTC"

        # Calculate dynamic ATR for baseline too
        if 'spot_price' in mdf.columns:
            all_prices = mdf['spot_price'].tolist()
        else:
            all_prices = mdf['distance_to_strike'].tolist()
        dynamic_atr = calculate_dynamic_atr(all_prices) if len(all_prices) >= 2 else 0.0
        atr = max(dynamic_atr, get_min_atr(asset))

        up_shares = 0
        down_shares = 0
        total_cost = 0
        market_trades = 0

        # Simple: trade every minute with fixed allocation
        for _, row in mdf.iterrows():
            distance = row['distance_to_strike']
            mins_left = row['minutes_remaining']

            if total_cost >= budget_per_market:
                break

            # Simple ATR-based threshold (no phases, no confidence)
            atr_multiple = abs(distance) / atr if atr > 0 else 0
            if atr_multiple < 0.25:  # Skip if less than 0.25 ATR from strike
                continue

            trade_size = min(50, budget_per_market - total_cost)
            if trade_size < 1:
                break

            # Simple ratio based on direction (ATR-normalized)
            if atr_multiple > 0.75 and distance > 0:
                up_ratio, down_ratio = 0.75, 0.25
            elif atr_multiple > 0.25 and distance > 0:
                up_ratio, down_ratio = 0.60, 0.40
            elif atr_multiple > 0.75 and distance < 0:
                up_ratio, down_ratio = 0.25, 0.75
            elif atr_multiple > 0.25 and distance < 0:
                up_ratio, down_ratio = 0.40, 0.60
            else:
                continue

            up_shares += (trade_size * up_ratio) / 0.50
            down_shares += (trade_size * down_ratio) / 0.50
            total_cost += trade_size
            market_trades += 1

        if total_cost == 0:
            continue

        if winner.lower() == 'up':
            payout = up_shares
        else:
            payout = down_shares

        pnl = payout - total_cost

        results["markets"] += 1
        results["trades"] += market_trades
        results["capital"] += total_cost
        results["pnl"] += pnl

        if pnl > 0:
            results["winners"] += 1
            results["markets_profitable"] += 1
        else:
            results["losers"] += 1

    return results


def run_parameter_sweep(df: pd.DataFrame, window_mins: float = 15.0) -> List[Dict]:
    """Run parameter sweep to find optimal config."""
    configs = []

    # Generate parameter combinations (reduced for speed)
    for strong_ratio in [0.75, 0.78, 0.82]:
        for lean_ratio in [0.58, 0.61, 0.65]:
            if lean_ratio >= strong_ratio:
                continue
            for base_lean in [20, 30, 40]:
                for base_conv in [40, 60, 80]:
                    if base_conv <= base_lean:
                        continue
                    for early_conf in [0.75, 0.80, 0.85]:
                        for final_conf in [0.35, 0.40, 0.45]:
                            configs.append(PMConfig(
                                strong_ratio=strong_ratio,
                                lean_ratio=lean_ratio,
                                base_lean=base_lean,
                                base_conviction=base_conv,
                                early_conf=early_conf,
                                build_conf=(early_conf + final_conf) / 2 + 0.1,
                                core_conf=(early_conf + final_conf) / 2,
                                final_conf=final_conf,
                            ))

    print(f"Testing {len(configs)} parameter combinations...")

    results = []
    for i, cfg in enumerate(configs):
        if (i + 1) % 500 == 0:
            print(f"  Progress: {i+1}/{len(configs)}")

        r = run_backtest_with_config(df, cfg, window_mins=window_mins)
        if r["trades"] > 0 and r["capital"] > 0:
            roi = r["pnl"] / r["capital"] * 100
            win_rate = r["markets_profitable"] / r["markets"] * 100 if r["markets"] > 0 else 0
            results.append({
                "config": {
                    "strong_ratio": cfg.strong_ratio,
                    "lean_ratio": cfg.lean_ratio,
                    "base_lean": cfg.base_lean,
                    "base_conviction": cfg.base_conviction,
                    "early_conf": cfg.early_conf,
                    "final_conf": cfg.final_conf,
                },
                "trades": r["trades"],
                "markets": r["markets"],
                "capital": r["capital"],
                "pnl": r["pnl"],
                "roi": roi,
                "win_rate": win_rate,
            })

    results.sort(key=lambda x: x["roi"], reverse=True)
    return results


def print_comparison(baseline: Dict, pm_default: Dict, pm_best: Dict, best_config: Dict):
    """Print comparison of results."""
    print("\n" + "=" * 80)
    print("COMPARISON: Baseline vs Position Management")
    print("=" * 80)

    def calc_metrics(r):
        roi = r["pnl"] / r["capital"] * 100 if r["capital"] > 0 else 0
        win_rate = r["markets_profitable"] / r["markets"] * 100 if r["markets"] > 0 else 0
        avg_trades = r["trades"] / r["markets"] if r["markets"] > 0 else 0
        return roi, win_rate, avg_trades

    b_roi, b_wr, b_trades = calc_metrics(baseline)
    d_roi, d_wr, d_trades = calc_metrics(pm_default)
    o_roi, o_wr, o_trades = calc_metrics(pm_best)

    print(f"\n{'Metric':<25} {'Baseline':>15} {'PM Default':>15} {'PM Optimized':>15}")
    print("-" * 80)
    print(f"{'Markets':<25} {baseline['markets']:>15} {pm_default['markets']:>15} {pm_best['markets']:>15}")
    print(f"{'Trades':<25} {baseline['trades']:>15} {pm_default['trades']:>15} {pm_best['trades']:>15}")
    print(f"{'Capital Deployed':<25} ${baseline['capital']:>14,.0f} ${pm_default['capital']:>14,.0f} ${pm_best['capital']:>14,.0f}")
    print(f"{'P&L':<25} ${baseline['pnl']:>14,.2f} ${pm_default['pnl']:>14,.2f} ${pm_best['pnl']:>14,.2f}")
    print(f"{'ROI':<25} {b_roi:>14.1f}% {d_roi:>14.1f}% {o_roi:>14.1f}%")
    print(f"{'Win Rate':<25} {b_wr:>14.1f}% {d_wr:>14.1f}% {o_wr:>14.1f}%")
    print(f"{'Avg Trades/Market':<25} {b_trades:>15.1f} {d_trades:>15.1f} {o_trades:>15.1f}")

    print(f"\nImprovement over Baseline:")
    print(f"  PM Default: ROI {d_roi - b_roi:+.1f}%, Win Rate {d_wr - b_wr:+.1f}%")
    print(f"  PM Optimized: ROI {o_roi - b_roi:+.1f}%, Win Rate {o_wr - b_wr:+.1f}%")

    print(f"\nOptimal Configuration:")
    for k, v in best_config.items():
        print(f"  {k}: {v}")


def load_json_data(filepath: str, window_mins: float) -> pd.DataFrame:
    """Load backtest data from JSON format and convert to DataFrame.

    The JSON format has:
    - event_id, asset, settlement (winner)
    - minute_prices: [[minute_offset, spot_price], ...]
    - open_price (strike), close_price

    We convert this to columns:
    - market_id, asset, winner, timestamp
    - spot_price (for dynamic ATR calculation)
    - distance_to_strike (spot - strike in dollars)
    - minutes_remaining
    """
    with open(filepath, 'r') as f:
        data = json.load(f)

    rows = []
    for idx, market in enumerate(data):
        event_id = market.get('event_id', market.get('title', f'market_{idx}'))
        asset = market.get('asset', 'BTC')
        settlement = market.get('settlement', '').lower()

        # Determine winner from settlement
        if 'up' in settlement:
            winner = 'up'
        elif 'down' in settlement:
            winner = 'down'
        else:
            continue  # Skip markets without clear settlement

        strike = market.get('open_price', 0)
        if not strike or strike == 0:
            continue

        minute_prices = market.get('minute_prices', [])
        for mp in minute_prices:
            if len(mp) >= 2:
                minute_offset = mp[0]
                spot_price = mp[1]

                # Calculate distance to strike in dollars
                distance = spot_price - strike

                # Minutes remaining (window_mins - minute_offset)
                mins_remaining = window_mins - minute_offset

                if mins_remaining < 0:
                    continue

                rows.append({
                    'market_id': event_id,
                    'asset': asset,
                    'winner': winner,
                    'timestamp': minute_offset,  # Use minute offset as timestamp
                    'spot_price': spot_price,    # Include for dynamic ATR
                    'distance_to_strike': distance,
                    'minutes_remaining': mins_remaining,
                })

    return pd.DataFrame(rows)


def main():
    parser = argparse.ArgumentParser(description="Parameter sweep for position management")
    parser.add_argument("data_file", help="JSON or CSV file with backtest data")
    parser.add_argument("--window", type=float, default=15.0, help="Window duration in minutes (5, 15, or 60)")
    parser.add_argument("--sweep", action="store_true", help="Run full parameter sweep")
    parser.add_argument("--top", type=int, default=10, help="Number of top results to show")
    parser.add_argument("--sample", type=int, default=0, help="Sample N markets for faster sweep (0=all)")
    args = parser.parse_args()

    print(f"Loading data from {args.data_file}...")
    if args.data_file.endswith('.json'):
        df = load_json_data(args.data_file, args.window)
    else:
        df = pd.read_csv(args.data_file)
        df['timestamp'] = pd.to_datetime(df['timestamp'])

    # Sample markets if requested
    if args.sample > 0:
        market_ids = df['market_id'].unique()
        if len(market_ids) > args.sample:
            import random
            random.seed(42)
            sampled_ids = random.sample(list(market_ids), args.sample)
            df = df[df['market_id'].isin(sampled_ids)]
            print(f"Sampled {args.sample} markets from {len(market_ids)} total")

    n_markets = df['market_id'].nunique()
    print(f"Loaded {len(df)} data points across {n_markets} markets")
    print(f"Window duration: {args.window} minutes")

    # Run baseline (without PM)
    print("\nRunning baseline (no position management)...")
    baseline = run_baseline(df, window_mins=args.window)

    # Run with default PM config
    print("Running with default position management...")
    pm_default = run_backtest_with_config(df, PMConfig(), window_mins=args.window)

    if args.sweep:
        print(f"\nRunning parameter sweep...")
        sweep_results = run_parameter_sweep(df, window_mins=args.window)

        print(f"\n{'=' * 80}")
        print(f"TOP {args.top} CONFIGURATIONS")
        print("=" * 80)

        for i, r in enumerate(sweep_results[:args.top]):
            print(f"\n#{i+1}: ROI {r['roi']:.1f}% | Win Rate {r['win_rate']:.1f}%")
            print(f"    Trades: {r['trades']} | Capital: ${r['capital']:,.0f} | P&L: ${r['pnl']:,.2f}")
            cfg = r['config']
            print(f"    Ratios: {cfg['strong_ratio']}/{cfg['lean_ratio']} | Thresholds: ${cfg['base_lean']}/${cfg['base_conviction']}")
            print(f"    Confidence: early={cfg['early_conf']}, final={cfg['final_conf']}")

        # Get best config results
        if sweep_results:
            best_cfg = PMConfig(
                strong_ratio=sweep_results[0]['config']['strong_ratio'],
                lean_ratio=sweep_results[0]['config']['lean_ratio'],
                base_lean=sweep_results[0]['config']['base_lean'],
                base_conviction=sweep_results[0]['config']['base_conviction'],
                early_conf=sweep_results[0]['config']['early_conf'],
                final_conf=sweep_results[0]['config']['final_conf'],
                build_conf=(sweep_results[0]['config']['early_conf'] + sweep_results[0]['config']['final_conf']) / 2 + 0.1,
                core_conf=(sweep_results[0]['config']['early_conf'] + sweep_results[0]['config']['final_conf']) / 2,
            )
            pm_best = run_backtest_with_config(df, best_cfg, window_mins=args.window)

            print_comparison(baseline, pm_default, pm_best, sweep_results[0]['config'])

            # Save results
            output_file = f"sweep_results_{int(args.window)}m.json"
            with open(output_file, "w") as f:
                json.dump({
                    "window_mins": args.window,
                    "baseline": {
                        "markets": baseline["markets"],
                        "trades": baseline["trades"],
                        "capital": baseline["capital"],
                        "pnl": baseline["pnl"],
                        "roi": baseline["pnl"] / baseline["capital"] * 100 if baseline["capital"] > 0 else 0,
                    },
                    "pm_default": {
                        "markets": pm_default["markets"],
                        "trades": pm_default["trades"],
                        "capital": pm_default["capital"],
                        "pnl": pm_default["pnl"],
                        "roi": pm_default["pnl"] / pm_default["capital"] * 100 if pm_default["capital"] > 0 else 0,
                    },
                    "top_configs": sweep_results[:50],
                }, f, indent=2)
            print(f"\nResults saved to {output_file}")
    else:
        # Just compare baseline vs default PM
        print_comparison(baseline, pm_default, pm_default, {
            "strong_ratio": 0.78,
            "lean_ratio": 0.61,
            "base_lean": 30,
            "base_conviction": 60,
            "early_conf": 0.80,
            "final_conf": 0.40,
        })


if __name__ == "__main__":
    main()
