import requests
import pandas as pd
from datetime import datetime
import time

# Configuration
WALLET_ADDRESS = "0x7f69983eb28245bba0d5083502a78744a8f66162"
TARGET_MARKETS = 20
OUTPUT_FILE = "duster_trades_historical.csv"


def get_btc_price(timestamp_s):
    """Fetches BTC price at a specific second from Binance."""
    ts_ms = int(timestamp_s) * 1000
    url = "https://api.binance.com/api/v3/klines"
    params = {"symbol": "BTCUSDT", "interval": "1m", "startTime": ts_ms, "limit": 1}
    try:
        res = requests.get(url, params=params, timeout=5).json()
        return res[0][4]  # Close price of that minute
    except:
        return None


def main():
    all_trades = []
    seen_market_ids = set()
    offset = 0

    print(f"Starting collection for {WALLET_ADDRESS}...")

    while len(seen_market_ids) < TARGET_MARKETS:
        # 1. Fetch Polymarket Trades with Offset
        poly_url = f"https://data-api.polymarket.com/activity?user={WALLET_ADDRESS}&type=TRADE&limit=50&offset={offset}"
        try:
            response = requests.get(poly_url, timeout=10)
            trades = response.json()
        except Exception as e:
            print(f"\nError fetching Polymarket data: {e}")
            break

        if not trades or len(trades) == 0:
            print("\nNo more trades found.")
            break

        for trade in trades:
            m_id = trade.get("conditionId")
            seen_market_ids.add(m_id)

            # Stop if we hit our limit of unique markets
            if len(seen_market_ids) > TARGET_MARKETS:
                break

            readable_ts = datetime.fromtimestamp(trade["timestamp"]).strftime(
                "%Y-%m-%d %H:%M:%S"
            )

            # 2. Get BTC Price (Rate limited)
            btc_price = get_btc_price(trade["timestamp"])

            all_trades.append(
                {
                    "Timestamp": readable_ts,
                    "Market": trade.get("title"),
                    "Outcome": trade.get("outcome"),
                    "Side": trade.get("side"),
                    "Price": trade.get("price"),
                    "Size_USDC": trade.get("usdcSize"),
                    "BTC_Price": btc_price,
                    "Market_ID": m_id,
                }
            )

            # Real-time progress update
            print(
                f"Markets: {len(seen_market_ids)}/{TARGET_MARKETS} | Offset: {offset} | Trade: {readable_ts}",
                end="\r",
            )
            time.sleep(0.1)  # Respect Binance rate limits

        offset += 50  # Move to the next page of results

    # 3. Save to CSV
    if all_trades:
        df = pd.DataFrame(all_trades)
        df.to_csv(OUTPUT_FILE, index=False)
        print(
            f"\n\nSuccess! Data for {len(seen_market_ids)} markets saved to {OUTPUT_FILE}"
        )
    else:
        print("\nNo trades were collected.")


if __name__ == "__main__":
    main()
