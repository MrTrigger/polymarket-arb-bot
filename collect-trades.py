import requests
import pandas as pd
from datetime import datetime
import time

# Configuration
WALLET_ADDRESS = "0x7f69983eb28245bba0d5083502a78744a8f66162"
TARGET_MARKETS = 20
OUTPUT_FILE = "duster_20_markets_with_btc.csv"


def get_btc_price(timestamp_ms):
    """Fetches the BTC price at a specific time from Binance."""
    url = "https://api.binance.com/api/v3/klines"
    params = {
        "symbol": "BTCUSDT",
        "interval": "1m",
        "startTime": timestamp_ms,
        "limit": 1,
    }
    try:
        res = requests.get(url, params=params).json()
        return res[0][4]  # Return the Close price
    except:
        return None


def main():
    all_trades = []
    seen_market_ids = set()
    next_cursor = ""  # For pagination if needed

    print(f"Targeting {TARGET_MARKETS} markets for {WALLET_ADDRESS}...")

    while len(seen_market_ids) < TARGET_MARKETS:
        url = f"https://data-api.polymarket.com/activity?user={WALLET_ADDRESS}&type=TRADE&limit=100{next_cursor}"
        data = requests.get(url).json()

        if not data:
            break

        for trade in data:
            m_id = trade.get("conditionId")
            seen_market_ids.add(m_id)

            # Stop if we've collected enough unique markets
            if len(seen_market_ids) > TARGET_MARKETS:
                break

            ts_ms = int(trade["timestamp"]) * 1000
            readable_ts = datetime.fromtimestamp(trade["timestamp"]).strftime(
                "%Y-%m-%d %H:%M:%S"
            )

            print(
                f"[{len(seen_market_ids)}/{TARGET_MARKETS}] Fetching BTC price for: {readable_ts}",
                end="\r",
            )

            btc_price = get_btc_price(ts_ms)

            all_trades.append(
                {
                    "Timestamp": readable_ts,
                    "Market_Title": trade.get("title"),
                    "Outcome": trade.get("outcome"),
                    "Side": trade.get("side"),
                    "Price": trade.get("price"),
                    "Size_USDC": trade.get("usdcSize"),
                    "BTC_Price_At_Trade": btc_price,
                    "Market_ID": m_id,
                }
            )
            time.sleep(0.05)  # Prevent rate limits

    df = pd.DataFrame(all_trades)
    df.to_csv(OUTPUT_FILE, index=False)
    print(
        f"\nDone! Saved {len(all_trades)} trades from {TARGET_MARKETS} markets to {OUTPUT_FILE}"
    )


if __name__ == "__main__":
    main()
