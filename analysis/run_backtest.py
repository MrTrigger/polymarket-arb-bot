#!/usr/bin/env python3
"""
Run backtest on existing historical data with position management.
Usage: python run_backtest.py historical_data/backtest_data.csv
"""

import pandas as pd
import sys
from typing import Tuple
from enum import Enum


class Signal(Enum):
    STRONG_UP = "strong_up"
    LEAN_UP = "lean_up"
    NEUTRAL = "neutral"
    LEAN_DOWN = "lean_down"
    STRONG_DOWN = "strong_down"


# Market phases: (name, start_min, end_min, budget_%, min_confidence)
PHASES = [
    ("early", 15.0, 10.0, 0.15, 0.80),
    ("build", 10.0, 5.0, 0.25, 0.60),
    ("core", 5.0, 2.0, 0.30, 0.50),
    ("final", 2.0, 0.0, 0.30, 0.40),
]


def get_phase(mins_remaining: float):
    for name, start, end, budget, min_conf in PHASES:
        if end < mins_remaining <= start:
            return name, budget, min_conf
    return "final", 0.30, 0.40


def get_signal(distance: float, mins_remaining: float) -> Signal:
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
    # Time factor
    if mins_remaining > 12:
        time_conf = 0.4
    elif mins_remaining > 9:
        time_conf = 0.5
    elif mins_remaining > 6:
        time_conf = 0.6
    elif mins_remaining > 3:
        time_conf = 0.8
    else:
        time_conf = 1.0
    
    # Distance factor
    abs_dist = abs(distance)
    if abs_dist > 100:
        dist_conf = 1.0
    elif abs_dist > 50:
        dist_conf = 0.85
    elif abs_dist > 30:
        dist_conf = 0.7
    elif abs_dist > 20:
        dist_conf = 0.55
    elif abs_dist > 10:
        dist_conf = 0.4
    else:
        dist_conf = 0.2
    
    combined = (time_conf * dist_conf) ** 0.5
    if time_conf > 0.7 and dist_conf > 0.7:
        combined = min(1.0, combined * 1.2)
    
    return combined


def run_backtest(df: pd.DataFrame, budget_per_market: float = 1000) -> pd.DataFrame:
    """Run backtest with proper position management."""
    
    print(f"\nRunning backtest with position management on {df['market_id'].nunique()} markets...")
    
    results = []
    
    for market_id in df['market_id'].unique():
        mdf = df[df['market_id'] == market_id].sort_values('timestamp')
        
        if len(mdf) < 5:
            continue
        
        winner = mdf['winner'].iloc[0]
        strike = mdf['strike_price'].iloc[0]
        
        # Position tracking
        up_shares = 0
        down_shares = 0
        total_cost = 0
        trades = 0
        
        # Phase budget tracking
        phase_budgets = {name: budget_per_market * pct for name, _, _, pct, _ in PHASES}
        phase_spent = {name: 0.0 for name, _, _, _, _ in PHASES}
        
        for _, row in mdf.iterrows():
            distance = row['distance_to_strike']
            mins_left = row['minutes_remaining']
            
            # Get current phase
            phase_name, phase_pct, min_confidence = get_phase(mins_left)
            
            # Check confidence threshold
            confidence = calculate_confidence(distance, mins_left)
            if confidence < min_confidence:
                continue
            
            # Check phase budget
            phase_remaining = phase_budgets[phase_name] - phase_spent[phase_name]
            if phase_remaining < 1.0:
                continue
            
            # Check total budget
            budget_remaining = budget_per_market - total_cost
            if budget_remaining < 1.0:
                break
            
            # Get signal (skip neutral if we have position)
            signal = get_signal(distance, mins_left)
            if signal == Signal.NEUTRAL and total_cost > 0:
                continue
            
            # Calculate trade size
            base_size = phase_remaining / 15  # ~15 trades per phase
            size_mult = 0.5 + (confidence * 1.5)
            trade_size = base_size * size_mult
            trade_size = min(trade_size, phase_remaining)
            trade_size = min(trade_size, budget_remaining * 0.15)
            trade_size = max(trade_size, 1.0)
            
            # Get allocation
            up_ratio, down_ratio = signal_to_ratio(signal)
            
            # Execute
            up_cost = trade_size * up_ratio
            down_cost = trade_size * down_ratio
            
            up_price = 0.50
            down_price = 0.50
            
            up_shares += up_cost / up_price
            down_shares += down_cost / down_price
            total_cost += trade_size
            phase_spent[phase_name] += trade_size
            trades += 1
        
        if total_cost == 0:
            continue
        
        # Calculate P&L
        if winner.lower() == 'up':
            payout = up_shares
        else:
            payout = down_shares
        
        pnl = payout - total_cost
        
        up_exposure = (up_shares * 0.5) / total_cost if total_cost > 0 else 0.5
        
        results.append({
            'market_id': market_id,
            'strike_price': strike,
            'winner': winner,
            'trades': trades,
            'up_shares': up_shares,
            'down_shares': down_shares,
            'up_exposure': up_exposure,
            'total_cost': total_cost,
            'payout': payout,
            'pnl': pnl,
            'roi': pnl / total_cost * 100 if total_cost > 0 else 0,
            'budget_used_pct': total_cost / budget_per_market * 100,
        })
    
    return pd.DataFrame(results)


def print_results(results_df: pd.DataFrame):
    """Print backtest results summary."""
    print("\n" + "=" * 70)
    print("BACKTEST RESULTS (with Position Management)")
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
    
    print(f"\nPosition Management Stats:")
    print(f"  Avg trades/market: {results_df['trades'].mean():.1f}")
    print(f"  Avg budget used: {results_df['budget_used_pct'].mean():.1f}%")
    
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


def main():
    if len(sys.argv) < 2:
        data_file = "historical_data/backtest_data.csv"
    else:
        data_file = sys.argv[1]
    
    print(f"Loading data from {data_file}...")
    df = pd.read_csv(data_file)
    df['timestamp'] = pd.to_datetime(df['timestamp'])
    
    print(f"Loaded {len(df)} data points across {df['market_id'].nunique()} markets")
    
    results = run_backtest(df)
    results.to_csv("backtest_results_with_pm.csv", index=False)
    
    print_results(results)


if __name__ == "__main__":
    main()
