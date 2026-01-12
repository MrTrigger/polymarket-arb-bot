//! Shared types for the Polymarket arbitrage bot.
//!
//! CRITICAL: All prices and quantities use `rust_decimal::Decimal`.
//! NEVER use f64 for financial math.

use chrono::{DateTime, Utc};
use clickhouse::Row;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

/// Supported cryptocurrency assets for 15-minute markets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum CryptoAsset {
    Btc,
    Eth,
    Sol,
    Xrp,
}

impl CryptoAsset {
    /// Returns the Binance trading pair symbol (e.g., "btcusdt").
    pub fn binance_symbol(&self) -> &'static str {
        match self {
            CryptoAsset::Btc => "btcusdt",
            CryptoAsset::Eth => "ethusdt",
            CryptoAsset::Sol => "solusdt",
            CryptoAsset::Xrp => "xrpusdt",
        }
    }

    /// Returns the display name.
    pub fn as_str(&self) -> &'static str {
        match self {
            CryptoAsset::Btc => "BTC",
            CryptoAsset::Eth => "ETH",
            CryptoAsset::Sol => "SOL",
            CryptoAsset::Xrp => "XRP",
        }
    }
}

impl std::fmt::Display for CryptoAsset {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// Order side for trading.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Side {
    Buy,
    Sell,
}

impl Side {
    pub fn opposite(&self) -> Self {
        match self {
            Side::Buy => Side::Sell,
            Side::Sell => Side::Buy,
        }
    }
}

impl std::fmt::Display for Side {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Side::Buy => write!(f, "BUY"),
            Side::Sell => write!(f, "SELL"),
        }
    }
}

/// Outcome type for binary options.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Outcome {
    Yes,
    No,
}

impl Outcome {
    pub fn opposite(&self) -> Self {
        match self {
            Outcome::Yes => Outcome::No,
            Outcome::No => Outcome::Yes,
        }
    }
}

impl std::fmt::Display for Outcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Outcome::Yes => write!(f, "YES"),
            Outcome::No => write!(f, "NO"),
        }
    }
}

/// A single level in an order book (price + quantity).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrderBookLevel {
    /// Price in USDC (0.00 to 1.00 for Polymarket).
    pub price: Decimal,
    /// Quantity available at this price.
    pub size: Decimal,
}

impl OrderBookLevel {
    pub fn new(price: Decimal, size: Decimal) -> Self {
        Self { price, size }
    }
}

/// Metadata for a 15-minute market window.
#[derive(Debug, Clone, Serialize, Deserialize, Row)]
pub struct MarketWindow {
    /// Unique event ID from Polymarket.
    pub event_id: String,
    /// Condition ID for this market.
    pub condition_id: String,
    /// The asset this market tracks.
    pub asset: String,
    /// Token ID for YES outcome.
    pub yes_token_id: String,
    /// Token ID for NO outcome.
    pub no_token_id: String,
    /// Strike price for the up/down market.
    #[serde(with = "rust_decimal::serde::str")]
    pub strike_price: Decimal,
    /// When the window opens.
    pub window_start: DateTime<Utc>,
    /// When the window closes.
    pub window_end: DateTime<Utc>,
    /// When this record was discovered/created.
    pub discovered_at: DateTime<Utc>,
}

impl MarketWindow {
    /// Returns the duration of the window in seconds.
    pub fn duration_secs(&self) -> i64 {
        (self.window_end - self.window_start).num_seconds()
    }

    /// Returns true if the window is currently active.
    pub fn is_active(&self, now: DateTime<Utc>) -> bool {
        now >= self.window_start && now < self.window_end
    }

    /// Returns seconds remaining until window close.
    pub fn seconds_remaining(&self, now: DateTime<Utc>) -> i64 {
        (self.window_end - now).num_seconds().max(0)
    }
}

/// Spot price update from Binance.
#[derive(Debug, Clone, Serialize, Deserialize, Row)]
pub struct SpotPrice {
    /// The asset.
    pub asset: String,
    /// The price in USDT.
    #[serde(with = "rust_decimal::serde::str")]
    pub price: Decimal,
    /// Timestamp of the trade.
    pub timestamp: DateTime<Utc>,
    /// Trade quantity (for volume tracking).
    #[serde(with = "rust_decimal::serde::str")]
    pub quantity: Decimal,
}

/// Order book snapshot for storage.
#[derive(Debug, Clone, Serialize, Deserialize, Row)]
pub struct OrderBookSnapshot {
    /// Token ID (YES or NO).
    pub token_id: String,
    /// Event ID for correlation.
    pub event_id: String,
    /// Capture timestamp.
    pub timestamp: DateTime<Utc>,
    /// Best bid price.
    #[serde(with = "rust_decimal::serde::str")]
    pub best_bid: Decimal,
    /// Best bid size.
    #[serde(with = "rust_decimal::serde::str")]
    pub best_bid_size: Decimal,
    /// Best ask price.
    #[serde(with = "rust_decimal::serde::str")]
    pub best_ask: Decimal,
    /// Best ask size.
    #[serde(with = "rust_decimal::serde::str")]
    pub best_ask_size: Decimal,
    /// Spread in basis points.
    pub spread_bps: u32,
    /// Total bid depth (top N levels).
    #[serde(with = "rust_decimal::serde::str")]
    pub bid_depth: Decimal,
    /// Total ask depth (top N levels).
    #[serde(with = "rust_decimal::serde::str")]
    pub ask_depth: Decimal,
}

/// Order book delta for incremental updates.
#[derive(Debug, Clone, Serialize, Deserialize, Row)]
pub struct OrderBookDelta {
    /// Token ID.
    pub token_id: String,
    /// Event ID.
    pub event_id: String,
    /// Delta timestamp.
    pub timestamp: DateTime<Utc>,
    /// Side of the book (bid/ask).
    pub side: String,
    /// Price level.
    #[serde(with = "rust_decimal::serde::str")]
    pub price: Decimal,
    /// New size (0 = level removed).
    #[serde(with = "rust_decimal::serde::str")]
    pub size: Decimal,
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn test_crypto_asset_binance_symbol() {
        assert_eq!(CryptoAsset::Btc.binance_symbol(), "btcusdt");
        assert_eq!(CryptoAsset::Eth.binance_symbol(), "ethusdt");
        assert_eq!(CryptoAsset::Sol.binance_symbol(), "solusdt");
        assert_eq!(CryptoAsset::Xrp.binance_symbol(), "xrpusdt");
    }

    #[test]
    fn test_side_opposite() {
        assert_eq!(Side::Buy.opposite(), Side::Sell);
        assert_eq!(Side::Sell.opposite(), Side::Buy);
    }

    #[test]
    fn test_outcome_opposite() {
        assert_eq!(Outcome::Yes.opposite(), Outcome::No);
        assert_eq!(Outcome::No.opposite(), Outcome::Yes);
    }

    #[test]
    fn test_order_book_level() {
        let level = OrderBookLevel::new(dec!(0.45), dec!(100));
        assert_eq!(level.price, dec!(0.45));
        assert_eq!(level.size, dec!(100));
    }

    #[test]
    fn test_market_window_duration() {
        let window = MarketWindow {
            event_id: "test".to_string(),
            condition_id: "cond".to_string(),
            asset: "BTC".to_string(),
            yes_token_id: "yes123".to_string(),
            no_token_id: "no123".to_string(),
            strike_price: dec!(100000),
            window_start: DateTime::parse_from_rfc3339("2025-01-01T12:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
            window_end: DateTime::parse_from_rfc3339("2025-01-01T12:15:00Z")
                .unwrap()
                .with_timezone(&Utc),
            discovered_at: Utc::now(),
        };
        assert_eq!(window.duration_secs(), 15 * 60);
    }
}
