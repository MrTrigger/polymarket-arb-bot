#!/usr/bin/env python3
"""
Full bot backtest for 1-hour crypto up/down markets on Polymarket.

These markets resolve to:
- "Up" if close >= open for the 1-hour candle
- "Down" if close < open

Simulates both engines:
1. Arbitrage: Buy YES+NO when combined < $1.00 (guaranteed profit)
2. Directional: Signal-based allocation based on current spot vs opening price
"""

import requests
import json
from datetime import datetime, timedelta
from dataclasses import dataclass, field
from typing import List, Dict, Optional, Tuple
import time
import re

GAMMA_API = "https://gamma-api.polymarket.com"
CLOB_API = "https://clob.polymarket.com"
BINANCE_API = "https://api.binance.com/api/v3"


@dataclass
class MarketWindow:
    title: str
    slug: str
    event_id: str
    condition_id: str
    yes_token_id: str
    no_token_id: str
    start_time: datetime
    end_time: datetime
    volume: float
    asset: str = "BTC"
    open_price: float = 0.0  # Opening price from Binance
    settlement: Optional[str] = None


@dataclass
class Trade:
    timestamp: datetime
    market: str
    asset: str
    engine: str
    yes_size: float
    no_size: float
    yes_price: float
    no_price: float
    total_cost: float
    signal: str = ""
    spot_price: float = 0.0
    open_price: float = 0.0
    settlement: str = ""
    pnl: float = 0.0


@dataclass
class EngineStats:
    name: str
    trades: int = 0
    capital: float = 0.0
    pnl: float = 0.0
    winners: int = 0
    losers: int = 0
    total_wins: float = 0.0
    total_losses: float = 0.0


@dataclass
class Config:
    arb_enabled: bool = True
    arb_min_margin_bps: int = 50

    dir_enabled: bool = True
    dir_max_combined_cost: float = 1.03  # Allow 3% premium
    dir_strong_threshold_pct: float = 0.15  # 0.15% for strong signal
    dir_lean_threshold_pct: float = 0.05   # 0.05% for lean signal
    dir_strong_ratio: float = 0.75
    dir_lean_ratio: float = 0.60

    order_size: float = 50.0
    max_position_per_market: float = 200.0


def detect_asset(title: str) -> str:
    title_lower = title.lower()
    if "btc" in title_lower or "bitcoin" in title_lower:
        return "BTC"
    if "eth" in title_lower or "ethereum" in title_lower:
        return "ETH"
    if "sol" in title_lower or "solana" in title_lower:
        return "SOL"
    return "UNKNOWN"


def fetch_candle_open(asset: str, timestamp: datetime) -> Optional[float]:
    """Fetch the opening price of the 1-hour candle from Binance."""
    symbol = f"{asset.upper()}USDT"
    # Round down to hour start
    hour_start = timestamp.replace(minute=0, second=0, microsecond=0)
    start_ms = int(hour_start.timestamp() * 1000)
    end_ms = start_ms + 3600000  # 1 hour

    try:
        url = f"{BINANCE_API}/klines"
        params = {"symbol": symbol, "interval": "1h", "startTime": start_ms, "endTime": end_ms, "limit": 1}
        resp = requests.get(url, params=params, timeout=10)
        if resp.ok and resp.json():
            return float(resp.json()[0][1])  # Open is index 1
    except:
        pass
    return None


def fetch_spot_price(asset: str, timestamp: datetime) -> Optional[float]:
    """Fetch spot price at a specific minute."""
    symbol = f"{asset.upper()}USDT"
    start_ms = int(timestamp.timestamp() * 1000)
    end_ms = start_ms + 60000

    try:
        url = f"{BINANCE_API}/klines"
        params = {"symbol": symbol, "interval": "1m", "startTime": start_ms, "endTime": end_ms, "limit": 1}
        resp = requests.get(url, params=params, timeout=10)
        if resp.ok and resp.json():
            return float(resp.json()[0][4])  # Close of minute candle
    except:
        pass
    return None


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


def fetch_closed_markets(asset: str, days_back: int = 30) -> List[MarketWindow]:
    markets = []
    today = datetime.now()
    hours = [9, 10, 11, 12, 13, 14]

    print(f"  Scanning {asset} markets...")

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

            slug = f"{asset}-up-or-down-{month}-{day}-{display_hour}{period}-et"

            try:
                resp = requests.get(f"{GAMMA_API}/events?slug={slug}", timeout=10)
                if resp.ok and resp.json():
                    event = resp.json()[0]
                    if event.get("closed"):
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
                                end_str = event.get("endDate", "")
                                end_time = datetime.fromisoformat(end_str.replace("Z", "+00:00"))
                                start_time = end_time - timedelta(hours=1)

                                title = event["title"]
                                asset_name = detect_asset(title)

                                # Get opening price from Binance
                                open_price = fetch_candle_open(asset_name, start_time)

                                # Get settlement
                                settlement = fetch_settlement(event["id"])

                                markets.append(MarketWindow(
                                    title=title,
                                    slug=event["slug"],
                                    event_id=event["id"],
                                    condition_id=cond_id,
                                    yes_token_id=yes_token,
                                    no_token_id=no_token,
                                    start_time=start_time.replace(tzinfo=None),
                                    end_time=end_time.replace(tzinfo=None),
                                    volume=event.get("volume", 0),
                                    asset=asset_name,
                                    open_price=open_price or 0.0,
                                    settlement=settlement
                                ))
            except:
                pass
            time.sleep(0.02)

    return markets


def fetch_price_history(token_id: str, start: datetime, end: datetime) -> List[Tuple[datetime, float]]:
    prices = []
    try:
        url = f"{CLOB_API}/prices-history"
        params = {"market": token_id, "startTs": int(start.timestamp()), "endTs": int(end.timestamp()), "fidelity": 60}
        resp = requests.get(url, params=params, timeout=30)
        if resp.ok:
            for p in resp.json().get("history", []):
                prices.append((datetime.fromtimestamp(p["t"]), float(p["p"])))
    except:
        pass
    return prices


def calc_signal(spot: float, open_price: float, secs_left: int, cfg: Config) -> Tuple[str, float, float]:
    """Calculate signal based on spot vs opening price."""
    if open_price <= 0 or spot <= 0:
        return ("Neutral", 0.5, 0.5)

    # Distance from open price as percentage
    dist_pct = abs(spot - open_price) / open_price * 100
    is_above = spot > open_price

    # Tighter thresholds near end (more certainty required)
    if secs_left < 180:
        factor = 0.3
    elif secs_left < 600:
        factor = 0.5
    elif secs_left < 1800:
        factor = 0.7
    else:
        factor = 1.0

    strong = cfg.dir_strong_threshold_pct * factor
    lean = cfg.dir_lean_threshold_pct * factor

    if dist_pct >= strong:
        if is_above:
            return ("StrongUp", cfg.dir_strong_ratio, 1 - cfg.dir_strong_ratio)
        return ("StrongDown", 1 - cfg.dir_strong_ratio, cfg.dir_strong_ratio)
    elif dist_pct >= lean:
        if is_above:
            return ("LeanUp", cfg.dir_lean_ratio, 1 - cfg.dir_lean_ratio)
        return ("LeanDown", 1 - cfg.dir_lean_ratio, cfg.dir_lean_ratio)
    return ("Neutral", 0.5, 0.5)


def calc_pnl(trade: Trade, settlement: str) -> float:
    if not settlement:
        return 0.0
    if settlement == "Up":
        return trade.yes_size * 1.0 - trade.total_cost
    return trade.no_size * 1.0 - trade.total_cost


def simulate_market(market: MarketWindow, cfg: Config) -> List[Trade]:
    trades = []

    if not market.settlement or market.open_price <= 0:
        return trades

    yes_prices = fetch_price_history(market.yes_token_id, market.start_time - timedelta(minutes=15), market.end_time)
    no_prices = fetch_price_history(market.no_token_id, market.start_time - timedelta(minutes=15), market.end_time)

    if not yes_prices or not no_prices:
        return trades

    yes_dict = {p[0].replace(second=0, microsecond=0): p[1] for p in yes_prices}
    no_dict = {p[0].replace(second=0, microsecond=0): p[1] for p in no_prices}
    common = sorted(set(yes_dict.keys()) & set(no_dict.keys()))

    total_cost = 0.0

    for ts in common:
        if ts < market.start_time or ts >= market.end_time:
            continue

        secs_left = int((market.end_time - ts).total_seconds())
        if secs_left < 60:
            continue

        yes_p = yes_dict[ts]
        no_p = no_dict[ts]
        combined = yes_p + no_p

        if total_cost >= cfg.max_position_per_market:
            continue

        remaining = cfg.max_position_per_market - total_cost
        size = min(cfg.order_size, remaining)

        # Check arb
        if cfg.arb_enabled:
            margin_bps = (1.0 - combined) * 10000
            if margin_bps >= cfg.arb_min_margin_bps:
                yes_qty = size / 2 / yes_p if yes_p > 0 else 0
                no_qty = size / 2 / no_p if no_p > 0 else 0
                cost = yes_qty * yes_p + no_qty * no_p

                trade = Trade(
                    timestamp=ts, market=market.title, asset=market.asset,
                    engine="arbitrage", yes_size=yes_qty, no_size=no_qty,
                    yes_price=yes_p, no_price=no_p, total_cost=cost,
                    signal=f"margin={margin_bps:.0f}bps",
                    open_price=market.open_price, settlement=market.settlement
                )
                trade.pnl = calc_pnl(trade, market.settlement)
                trades.append(trade)
                total_cost += cost
                continue

        # Check directional
        if cfg.dir_enabled and combined <= cfg.dir_max_combined_cost:
            spot = fetch_spot_price(market.asset, ts)
            if spot is None:
                continue

            signal, yes_alloc, no_alloc = calc_signal(spot, market.open_price, secs_left, cfg)

            if signal == "Neutral":
                continue

            yes_qty = size * yes_alloc / yes_p if yes_p > 0 else 0
            no_qty = size * no_alloc / no_p if no_p > 0 else 0
            cost = yes_qty * yes_p + no_qty * no_p

            trade = Trade(
                timestamp=ts, market=market.title, asset=market.asset,
                engine="directional", yes_size=yes_qty, no_size=no_qty,
                yes_price=yes_p, no_price=no_p, total_cost=cost,
                signal=signal, spot_price=spot, open_price=market.open_price,
                settlement=market.settlement
            )
            trade.pnl = calc_pnl(trade, market.settlement)
            trades.append(trade)
            total_cost += cost

    return trades


def print_report(trades: List[Trade], markets: List[MarketWindow], cfg: Config):
    print("\n" + "=" * 80)
    print("                         BACKTEST REPORT")
    print("=" * 80)

    settled = [m for m in markets if m.settlement]
    with_open = [m for m in settled if m.open_price > 0]

    print(f"\n{'=' * 32} OVERVIEW {'=' * 32}")
    print(f"  Markets Found:           {len(markets)}")
    print(f"  With Settlement:         {len(settled)}")
    print(f"  With Open Price:         {len(with_open)}")
    print(f"  Total Volume:            ${sum(m.volume for m in markets):,.2f}")

    # Settlement breakdown
    for asset in ["BTC", "ETH", "SOL"]:
        asset_m = [m for m in settled if m.asset == asset]
        up = sum(1 for m in asset_m if m.settlement == "Up")
        down = len(asset_m) - up
        if asset_m:
            print(f"  {asset}: {len(asset_m)} markets (Up: {up}, Down: {down}, Up%: {up/len(asset_m)*100:.1f}%)")

    print(f"\n{'=' * 30} TRADING RESULTS {'=' * 30}")
    total_cost = sum(t.total_cost for t in trades)
    total_pnl = sum(t.pnl for t in trades)
    winners = sum(1 for t in trades if t.pnl > 0)
    losers = sum(1 for t in trades if t.pnl < 0)

    print(f"  Total Trades:            {len(trades)}")
    print(f"  Capital Deployed:        ${total_cost:,.2f}")
    print(f"  Gross P&L:               ${total_pnl:,.2f}")
    print(f"  Return:                  {total_pnl/max(1,total_cost)*100:.2f}%")
    print(f"  Winners:                 {winners} ({winners/max(1,len(trades))*100:.1f}%)")
    print(f"  Losers:                  {losers} ({losers/max(1,len(trades))*100:.1f}%)")

    # Engine breakdown
    print(f"\n{'=' * 30} ENGINE BREAKDOWN {'=' * 30}")
    for engine in ["arbitrage", "directional"]:
        et = [t for t in trades if t.engine == engine]
        if et:
            e_cost = sum(t.total_cost for t in et)
            e_pnl = sum(t.pnl for t in et)
            e_wins = sum(1 for t in et if t.pnl > 0)
            e_total_wins = sum(t.pnl for t in et if t.pnl > 0)
            e_total_loss = sum(abs(t.pnl) for t in et if t.pnl < 0)
            pf = e_total_wins / e_total_loss if e_total_loss > 0 else float('inf')

            print(f"\n  {engine.upper()}")
            print(f"    Trades:        {len(et)}")
            print(f"    Capital:       ${e_cost:,.2f}")
            print(f"    P&L:           ${e_pnl:,.2f} ({e_pnl/max(1,e_cost)*100:.2f}%)")
            print(f"    Win Rate:      {e_wins/len(et)*100:.1f}%")
            print(f"    Profit Factor: {pf:.2f}" if pf != float('inf') else f"    Profit Factor: inf")
            print(f"    Best Trade:    ${max(t.pnl for t in et):.2f}")
            print(f"    Worst Trade:   ${min(t.pnl for t in et):.2f}")

    # Signal analysis
    dir_t = [t for t in trades if t.engine == "directional"]
    if dir_t:
        print(f"\n{'=' * 28} DIRECTIONAL SIGNALS {'=' * 28}")
        signals = {}
        for t in dir_t:
            s = t.signal
            if s not in signals:
                signals[s] = {"n": 0, "pnl": 0, "wins": 0, "cost": 0}
            signals[s]["n"] += 1
            signals[s]["pnl"] += t.pnl
            signals[s]["cost"] += t.total_cost
            if t.pnl > 0:
                signals[s]["wins"] += 1

        print(f"  {'Signal':<12} {'Trades':>8} {'P&L':>12} {'Win%':>8} {'Return%':>10}")
        print(f"  {'-'*12} {'-'*8} {'-'*12} {'-'*8} {'-'*10}")
        for s, d in sorted(signals.items(), key=lambda x: -x[1]["n"]):
            wr = d["wins"]/d["n"]*100 if d["n"] > 0 else 0
            ret = d["pnl"]/d["cost"]*100 if d["cost"] > 0 else 0
            print(f"  {s:<12} {d['n']:>8} ${d['pnl']:>10.2f} {wr:>7.1f}% {ret:>9.2f}%")

    # P&L by asset
    print(f"\n{'=' * 32} BY ASSET {'=' * 32}")
    for asset in ["BTC", "ETH", "SOL"]:
        at = [t for t in trades if t.asset == asset]
        if at:
            a_pnl = sum(t.pnl for t in at)
            a_cost = sum(t.total_cost for t in at)
            print(f"  {asset}: {len(at)} trades | ${a_cost:,.2f} -> ${a_pnl:,.2f} ({a_pnl/max(1,a_cost)*100:.2f}%)")

    # Sample trades
    if trades:
        print(f"\n{'=' * 30} SAMPLE TRADES {'=' * 30}")
        sorted_t = sorted(trades, key=lambda t: t.pnl, reverse=True)
        print("\n  BEST:")
        for t in sorted_t[:5]:
            print(f"    {t.timestamp.strftime('%m-%d %H:%M')} | {t.asset} | {t.engine:10} | {t.signal:10} | ${t.pnl:+.2f}")
        print("\n  WORST:")
        for t in sorted_t[-5:]:
            print(f"    {t.timestamp.strftime('%m-%d %H:%M')} | {t.asset} | {t.engine:10} | {t.signal:10} | ${t.pnl:+.2f}")

    # Config
    print(f"\n{'=' * 31} CONFIGURATION {'=' * 31}")
    print(f"  Arb Min Margin:          {cfg.arb_min_margin_bps} bps")
    print(f"  Dir Max Combined:        {cfg.dir_max_combined_cost}")
    print(f"  Dir Strong Threshold:    {cfg.dir_strong_threshold_pct}%")
    print(f"  Dir Lean Threshold:      {cfg.dir_lean_threshold_pct}%")
    print(f"  Strong Ratio:            {cfg.dir_strong_ratio}")
    print(f"  Lean Ratio:              {cfg.dir_lean_ratio}")
    print(f"  Order Size:              ${cfg.order_size}")

    print("\n" + "=" * 80)


def run_backtest(days_back: int = 30, min_markets: int = 200):
    print("=" * 80)
    print("       FULL BOT BACKTEST - 1-HOUR UP/DOWN MARKETS")
    print("       (Resolves: Up if close >= open, Down if close < open)")
    print("=" * 80)

    cfg = Config()
    all_markets = []

    for asset in ["bitcoin", "ethereum", "solana"]:
        markets = fetch_closed_markets(asset, days_back)
        print(f"  {asset}: {len(markets)} markets")
        all_markets.extend(markets)

    settled = [m for m in all_markets if m.settlement and m.open_price > 0]
    print(f"\nTotal: {len(all_markets)} | Usable: {len(settled)}")

    print("\nSimulating...")
    all_trades = []
    for i, m in enumerate(settled):
        if (i + 1) % 30 == 0:
            print(f"  {i+1}/{len(settled)}")
        trades = simulate_market(m, cfg)
        all_trades.extend(trades)
        time.sleep(0.03)

    print_report(all_trades, all_markets, cfg)

    # Save
    with open("backtest_full_results.json", "w") as f:
        json.dump({
            "summary": {
                "markets": len(all_markets),
                "usable": len(settled),
                "trades": len(all_trades),
                "pnl": sum(t.pnl for t in all_trades),
                "cost": sum(t.total_cost for t in all_trades),
            },
            "trades": [{"ts": str(t.timestamp), "asset": t.asset, "engine": t.engine,
                       "signal": t.signal, "cost": t.total_cost, "pnl": t.pnl, "settlement": t.settlement}
                      for t in all_trades]
        }, f, indent=2)
    print("\nResults saved to backtest_full_results.json")


if __name__ == "__main__":
    run_backtest(days_back=30, min_markets=200)
