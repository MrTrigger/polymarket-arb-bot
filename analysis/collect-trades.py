import requests
import pandas as pd
import numpy as np
import time
import os
import sys
from datetime import datetime, timedelta
from concurrent.futures import ThreadPoolExecutor, as_completed

# CONFIGURATION
WALLET = "0x7f69983eb28245bba0d5083502a78744a8f66162"
FOLDER = "disguised_duster_analysis"
TARGET_MARKETS = 50
THREADS = 20  # Number of parallel Binance workers
OUTPUT_FILE = os.path.join(FOLDER, "duster_btc_deep_analysis.csv")

if not os.path.exists(FOLDER): 
    os.makedirs(FOLDER)

session = requests.Session()

def fetch_binance_minute(item):
    """Worker function to fetch a single minute of indicators."""
    symbol, ts_s = item
    minute_ts = (ts_s // 60) * 60
    start_ms = (minute_ts - 6000) * 1000 
    url = "https://api.binance.com/api/v3/klines"
    params = {"symbol": symbol, "interval": "1m", "startTime": start_ms, "limit": 100}
    
    try:
        res = session.get(url, params=params, timeout=5).json()
        k = pd.DataFrame(res, columns=['Time','O','H','L','C','V','CT','QV','N','TBB','TBQ','I'])
        k = k.astype(float)
        
        # Calculate Technicals
        delta = k['C'].diff()
        gain = delta.where(delta > 0, 0).rolling(14).mean()
        loss = -delta.where(delta < 0, 0).rolling(14).mean()
        rsi = 100 - (100 / (1 + (gain / loss))).iloc[-1]
        tr = np.maximum(k['H'] - k['L'], np.maximum(abs(k['H'] - k['C'].shift(1)), abs(k['L'] - k['C'].shift(1))))
        atr = tr.rolling(14).mean().iloc[-1]
        ema = k['C'].ewm(span=9, adjust=False).mean().iloc[-1]
        
        return {
            "key": (symbol, minute_ts),
            "data": {
                f"{symbol}_Price": k.iloc[-1]['C'],
                f"{symbol}_RSI": rsi,
                f"{symbol}_EMA": ema,
                f"{symbol}_ATR": atr,
                f"{symbol}_Taker_USDT": k.iloc[-1]['TBQ']
            }
        }
    except:
        return {"key": (symbol, minute_ts), "data": {}}

def fetch_strike_price(item):
    """Worker function to fetch strike prices."""
    symbol, strike_ts = item
    url = "https://api.binance.com/api/v3/klines"
    params = {"symbol": symbol, "interval": "1m", "startTime": strike_ts * 1000, "limit": 1}
    try:
        res = session.get(url, params=params, timeout=5).json()
        return {"key": (symbol, strike_ts), "price": float(res[0][1])}
    except:
        return {"key": (symbol, strike_ts), "price": None}

def main():
    raw_trades = []
    seen_ids = set()
    last_ts = int(time.time())

    print(f"--- STAGE 1: DISCOVERY ---")
    print(f"Fetching BTC trades for {WALLET}...")

    try:
        while len(seen_ids) < TARGET_MARKETS:
            url = f"https://data-api.polymarket.com/activity?user={WALLET}&type=TRADE&limit=50&end={last_ts}"
            trades = session.get(url).json()
            if not trades: break
            
            for t in trades:
                last_ts = int(t['timestamp']) - 1
                if "Bitcoin" not in t.get('title', ''): continue
                
                m_id = t.get('conditionId')
                if m_id: seen_ids.add(m_id)
                if len(seen_ids) > TARGET_MARKETS: break
                raw_trades.append(t)
                
            sys.stdout.write(f"\rMarkets Found: {len(seen_ids)}/{TARGET_MARKETS} | Raw Trades: {len(raw_trades)}")
            sys.stdout.flush()

        print(f"\n\n--- STAGE 2: PARALLEL ENRICHMENT ---")
        # Identify unique work items to avoid redundant calls
        unique_minutes = set()
        unique_strikes = set()
        for t in raw_trades:
            ts = int(t['timestamp'])
            unique_minutes.add(("BTCUSDT", (ts // 60) * 60))
            unique_minutes.add(("ETHUSDT", (ts // 60) * 60))
            
            dt = datetime.fromtimestamp(ts)
            strike_dt = dt - timedelta(minutes=dt.minute % 15, seconds=dt.second, microseconds=dt.microsecond)
            unique_strikes.add(("BTCUSDT", int(strike_dt.timestamp())))

        # Parallel Fetching
        metrics_map = {}
        strike_map = {}
        
        with ThreadPoolExecutor(max_workers=THREADS) as executor:
            # Task 1: Binance Indicators
            print(f"Enriching {len(unique_minutes)} unique minute-intervals...")
            futures = [executor.submit(fetch_binance_minute, item) for item in unique_minutes]
            for f in as_completed(futures):
                res = f.result()
                metrics_map[res['key']] = res['data']

            # Task 2: Strike Prices
            print(f"Fetching {len(unique_strikes)} unique strike prices...")
            futures = [executor.submit(fetch_strike_price, item) for item in unique_strikes]
            for f in as_completed(futures):
                res = f.result()
                strike_map[res['key']] = res['price']

        # Final Assembly
        print(f"Assembling final dataset...")
        final_data = []
        for t in raw_trades:
            ts = int(t['timestamp'])
            min_ts = (ts // 60) * 60
            
            dt = datetime.fromtimestamp(ts)
            strike_ts = int((dt - timedelta(minutes=dt.minute % 15, seconds=dt.second, microseconds=dt.microsecond)).timestamp())
            
            btc = metrics_map.get(("BTCUSDT", min_ts), {})
            eth = metrics_map.get(("ETHUSDT", min_ts), {})
            strike = strike_map.get(("BTCUSDT", strike_ts))
            
            row = {
                "Timestamp": datetime.fromtimestamp(ts).strftime('%Y-%m-%d %H:%M:%S'),
                "Market": t['title'],
                "Outcome": t['outcome'],
                "Price": t['price'],
                "Size_USDC": t['usdcSize'],
                "Market_ID": t['conditionId'],
                "Strike_Price": strike,
                "Dist_to_Strike": (btc.get('BTCUSDT_Price', 0) - strike) if strike else None
            }
            row.update(btc)
            row.update(eth)
            final_data.append(row)

        pd.DataFrame(final_data).to_csv(OUTPUT_FILE, index=False)
        print(f"Success! Saved {len(final_data)} enriched trades to {OUTPUT_FILE}.")

    except KeyboardInterrupt:
        print(f"\n\nInterrupt received. Stopping...")

if __name__ == "__main__":
    main()

