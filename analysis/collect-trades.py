import requests
import pandas as pd
import numpy as np
from datetime import datetime
import time
import os

# Configuration
WALLET_ADDRESS = "0x7f69983eb28245bba0d5083502a78744a8f66162"
TARGET_MARKETS = 20
OUTPUT_FILE = "duster_pro_analysis.csv"


def get_binance_indicators(symbol, timestamp_s):
    """Fetches klines and calculates Technical Indicators with fixed logic."""
    start_ms = (timestamp_s - 6000) * 1000
    url = "https://api.binance.com/api/v3/klines"
    params = {"symbol": symbol, "interval": "1m", "startTime": start_ms, "limit": 100}

    try:
        res = requests.get(url, params=params, timeout=5).json()
        k = pd.DataFrame(
            res,
            columns=[
                "Time",
                "O",
                "H",
                "L",
                "C",
                "V",
                "CT",
                "QV",
                "N",
                "TBB",
                "TBQ",
                "I",
            ],
        )
        k[["O", "H", "L", "C", "V", "TBB"]] = k[["O", "H", "L", "C", "V", "TBB"]].apply(
            pd.to_numeric
        )

        # RSI Calculation (Wilder's Smoothing)
        delta = k["C"].diff()
        gain = delta.where(delta > 0, 0)
        loss = -delta.where(delta < 0, 0)
        avg_gain = gain.rolling(window=14).mean()
        avg_loss = loss.rolling(window=14).mean()
        rs = avg_gain / avg_loss
        k["RSI"] = 100 - (100 / (1 + rs))

        # 9-EMA and Taker Ratio
        k["EMA_9"] = k["C"].ewm(span=9, adjust=False).mean()
        k["Dist_EMA"] = (k["C"] - k["EMA_9"]) / k["EMA_9"]
        k["Taker_Ratio"] = k["TBB"] / k["V"]

        latest = k.iloc[-1]
        return latest["RSI"], latest["EMA_9"], latest["Dist_EMA"], latest["Taker_Ratio"]
    except:
        return None, None, None, None


def main():
    all_trades = []
    seen_markets = set()
    last_ts = int(time.time())

    print(f"Generating Pro-Level Dataset for {WALLET_ADDRESS}...")

    while len(seen_markets) < TARGET_MARKETS:
        url = f"https://data-api.polymarket.com/activity?user={WALLET_ADDRESS}&type=TRADE&limit=50&end={last_ts}"
        try:
            data = requests.get(url).json()
        except:
            break

        if not data:
            break

        for t in data:
            last_ts = int(t["timestamp"]) - 1
            m_id = t.get("conditionId")
            if m_id:
                seen_markets.add(m_id)
            if len(seen_markets) > TARGET_MARKETS:
                break

            rsi, ema, dist_ema, taker_v = get_binance_indicators(
                "BTCUSDT", t["timestamp"]
            )

            # Fixed the formatting here
            rsi_display = f"{rsi:.2f}" if rsi is not None else "0.00"

            all_trades.append(
                {
                    "Timestamp": datetime.fromtimestamp(t["timestamp"]).strftime(
                        "%Y-%m-%d %H:%M:%S"
                    ),
                    "Market": t.get("title"),
                    "Outcome": t.get("outcome"),
                    "Price": t.get("price"),
                    "BTC_Price": t.get("btcPrice"),
                    "RSI": rsi,
                    "EMA_9": ema,
                    "Dist_to_EMA": dist_ema,
                    "Taker_Buy_Ratio": taker_v,
                    "Market_ID": m_id,
                }
            )
            print(
                f"Markets: {len(seen_markets)}/{TARGET_MARKETS} | RSI: {rsi_display}",
                end="\r",
            )
            time.sleep(0.1)

    pd.DataFrame(all_trades).to_csv(OUTPUT_FILE, index=False)
    print(f"\nSuccess! File saved as {OUTPUT_FILE}")


if __name__ == "__main__":
    main()
