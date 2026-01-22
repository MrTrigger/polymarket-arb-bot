//! Data source abstraction for live and replay market data.
//!
//! This module provides the `DataSource` trait that abstracts the source of
//! market events. The same strategy code can work with:
//! - Live WebSocket feeds for real trading
//! - Replay from ClickHouse for backtesting
//!
//! ## Events
//!
//! The `MarketEvent` enum represents all events that can affect trading decisions:
//! - Price updates from Binance spot markets
//! - Order book snapshots and deltas from Polymarket CLOB
//! - Fill notifications for position tracking
//! - Market window lifecycle events

pub mod csv_replay;
pub mod live;
pub mod replay;
pub mod vec_replay;

use std::fmt;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use thiserror::Error;

use poly_common::types::{CryptoAsset, Outcome, Side};

use crate::types::PriceLevel;

/// Errors that can occur during data source operations.
#[derive(Debug, Error)]
pub enum DataSourceError {
    #[error("Connection failed: {0}")]
    Connection(String),

    #[error("WebSocket error: {0}")]
    WebSocket(String),

    #[error("Parse error: {0}")]
    Parse(String),

    #[error("ClickHouse error: {0}")]
    ClickHouse(String),

    #[error("Stream ended")]
    StreamEnded,

    #[error("Shutdown requested")]
    Shutdown,

    #[error("Timeout")]
    Timeout,
}

/// A market event that can affect trading decisions.
///
/// Events are timestamped and processed in order by the strategy.
#[derive(Debug, Clone)]
pub enum MarketEvent {
    /// Spot price update from Binance.
    SpotPrice(SpotPriceEvent),

    /// Full order book snapshot from Polymarket.
    BookSnapshot(BookSnapshotEvent),

    /// Incremental order book delta from Polymarket.
    BookDelta(BookDeltaEvent),

    /// Order fill notification.
    Fill(FillEvent),

    /// New market window discovered.
    WindowOpen(WindowOpenEvent),

    /// Market window expired.
    WindowClose(WindowCloseEvent),

    /// Heartbeat for connection liveness.
    Heartbeat(DateTime<Utc>),
}

impl MarketEvent {
    /// Returns the timestamp of this event.
    pub fn timestamp(&self) -> DateTime<Utc> {
        match self {
            MarketEvent::SpotPrice(e) => e.timestamp,
            MarketEvent::BookSnapshot(e) => e.timestamp,
            MarketEvent::BookDelta(e) => e.timestamp,
            MarketEvent::Fill(e) => e.timestamp,
            MarketEvent::WindowOpen(e) => e.timestamp,
            MarketEvent::WindowClose(e) => e.timestamp,
            MarketEvent::Heartbeat(ts) => *ts,
        }
    }

    /// Returns true if this event is a heartbeat.
    pub fn is_heartbeat(&self) -> bool {
        matches!(self, MarketEvent::Heartbeat(_))
    }
}

impl fmt::Display for MarketEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MarketEvent::SpotPrice(e) => write!(f, "SpotPrice({} @ {})", e.asset, e.price),
            MarketEvent::BookSnapshot(e) => write!(f, "BookSnapshot({})", e.token_id),
            MarketEvent::BookDelta(e) => {
                write!(f, "BookDelta({} {} @ {})", e.token_id, e.side, e.price)
            }
            MarketEvent::Fill(e) => write!(
                f,
                "Fill({} {} {} @ {})",
                e.event_id, e.outcome, e.size, e.price
            ),
            MarketEvent::WindowOpen(e) => {
                write!(f, "WindowOpen({} {} strike={})", e.event_id, e.asset, e.strike_price)
            }
            MarketEvent::WindowClose(e) => write!(f, "WindowClose({})", e.event_id),
            MarketEvent::Heartbeat(ts) => write!(f, "Heartbeat({})", ts),
        }
    }
}

/// Spot price update from Binance.
#[derive(Debug, Clone)]
pub struct SpotPriceEvent {
    /// The cryptocurrency asset.
    pub asset: CryptoAsset,
    /// Current price in USDT.
    pub price: Decimal,
    /// Trade quantity (for volume tracking).
    pub quantity: Decimal,
    /// Event timestamp.
    pub timestamp: DateTime<Utc>,
}

/// Full order book snapshot.
#[derive(Debug, Clone)]
pub struct BookSnapshotEvent {
    /// Token ID (YES or NO token).
    pub token_id: String,
    /// Event ID for correlation.
    pub event_id: String,
    /// Bid levels (sorted by price descending).
    pub bids: Vec<PriceLevel>,
    /// Ask levels (sorted by price ascending).
    pub asks: Vec<PriceLevel>,
    /// Event timestamp.
    pub timestamp: DateTime<Utc>,
}

/// Incremental order book update.
#[derive(Debug, Clone)]
pub struct BookDeltaEvent {
    /// Token ID.
    pub token_id: String,
    /// Event ID for correlation.
    pub event_id: String,
    /// Side being updated.
    pub side: Side,
    /// Price level.
    pub price: Decimal,
    /// New size at this price (0 = level removed).
    pub size: Decimal,
    /// Event timestamp.
    pub timestamp: DateTime<Utc>,
}

/// Order fill notification.
#[derive(Debug, Clone)]
pub struct FillEvent {
    /// Event ID.
    pub event_id: String,
    /// Token ID that was filled.
    pub token_id: String,
    /// Outcome (YES or NO).
    pub outcome: Outcome,
    /// Order ID.
    pub order_id: String,
    /// Fill side (Buy or Sell).
    pub side: Side,
    /// Filled size.
    pub size: Decimal,
    /// Fill price.
    pub price: Decimal,
    /// Fee paid.
    pub fee: Decimal,
    /// Event timestamp.
    pub timestamp: DateTime<Utc>,
}

/// New market window opened.
#[derive(Debug, Clone)]
pub struct WindowOpenEvent {
    /// Event ID.
    pub event_id: String,
    /// Condition ID (for CTF contract interactions like redeeming).
    pub condition_id: String,
    /// Asset being tracked.
    pub asset: CryptoAsset,
    /// YES token ID.
    pub yes_token_id: String,
    /// NO token ID.
    pub no_token_id: String,
    /// Strike price for the market.
    pub strike_price: Decimal,
    /// Window start time.
    pub window_start: DateTime<Utc>,
    /// Window end time.
    pub window_end: DateTime<Utc>,
    /// Event timestamp.
    pub timestamp: DateTime<Utc>,
}

/// Market window closed.
#[derive(Debug, Clone)]
pub struct WindowCloseEvent {
    /// Event ID.
    pub event_id: String,
    /// Whether the outcome was YES (price above strike).
    pub outcome: Option<Outcome>,
    /// Final spot price at settlement.
    pub final_price: Option<Decimal>,
    /// Event timestamp.
    pub timestamp: DateTime<Utc>,
}

/// Data source trait for receiving market events.
///
/// Implementations provide market events from different sources:
/// - `LiveDataSource`: Real-time WebSocket feeds
/// - `ReplayDataSource`: Historical data from ClickHouse
#[async_trait]
pub trait DataSource: Send + Sync {
    /// Receive the next market event.
    ///
    /// Returns `None` when the source is exhausted (replay completed)
    /// or shutdown was requested.
    ///
    /// # Errors
    ///
    /// Returns an error if the connection fails or data cannot be parsed.
    async fn next_event(&mut self) -> Result<Option<MarketEvent>, DataSourceError>;

    /// Returns true if the data source has more events available.
    fn has_more(&self) -> bool;

    /// Get the current replay timestamp (for replay mode).
    ///
    /// Returns None for live data sources.
    fn current_time(&self) -> Option<DateTime<Utc>>;

    /// Shutdown the data source gracefully.
    async fn shutdown(&mut self);
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn test_spot_price_event() {
        let event = SpotPriceEvent {
            asset: CryptoAsset::Btc,
            price: dec!(50000.50),
            quantity: dec!(0.1),
            timestamp: Utc::now(),
        };

        let market_event = MarketEvent::SpotPrice(event.clone());
        assert!(!market_event.is_heartbeat());
        assert!(format!("{}", market_event).contains("BTC"));
        assert!(format!("{}", market_event).contains("50000.50"));
    }

    #[test]
    fn test_book_snapshot_event() {
        let event = BookSnapshotEvent {
            token_id: "token123".to_string(),
            event_id: "event456".to_string(),
            bids: vec![
                PriceLevel::new(dec!(0.45), dec!(100)),
                PriceLevel::new(dec!(0.44), dec!(200)),
            ],
            asks: vec![
                PriceLevel::new(dec!(0.55), dec!(150)),
                PriceLevel::new(dec!(0.56), dec!(250)),
            ],
            timestamp: Utc::now(),
        };

        assert_eq!(event.bids.len(), 2);
        assert_eq!(event.asks.len(), 2);
        assert_eq!(event.bids[0].price, dec!(0.45));
        assert_eq!(event.asks[0].price, dec!(0.55));
    }

    #[test]
    fn test_book_delta_event() {
        let event = BookDeltaEvent {
            token_id: "token123".to_string(),
            event_id: "event456".to_string(),
            side: Side::Buy,
            price: dec!(0.46),
            size: dec!(50),
            timestamp: Utc::now(),
        };

        let market_event = MarketEvent::BookDelta(event);
        assert!(format!("{}", market_event).contains("0.46"));
    }

    #[test]
    fn test_fill_event() {
        let event = FillEvent {
            event_id: "event123".to_string(),
            token_id: "token456".to_string(),
            outcome: Outcome::Yes,
            order_id: "order789".to_string(),
            side: Side::Buy,
            size: dec!(100),
            price: dec!(0.45),
            fee: dec!(0.001),
            timestamp: Utc::now(),
        };

        let market_event = MarketEvent::Fill(event);
        assert!(format!("{}", market_event).contains("100"));
        assert!(format!("{}", market_event).contains("0.45"));
    }

    #[test]
    fn test_window_events() {
        let open_event = WindowOpenEvent {
            event_id: "event123".to_string(),
            condition_id: "cond123".to_string(),
            asset: CryptoAsset::Btc,
            yes_token_id: "yes_token".to_string(),
            no_token_id: "no_token".to_string(),
            strike_price: dec!(100000),
            window_start: Utc::now(),
            window_end: Utc::now() + chrono::Duration::minutes(15),
            timestamp: Utc::now(),
        };

        let market_event = MarketEvent::WindowOpen(open_event);
        assert!(format!("{}", market_event).contains("BTC"));
        assert!(format!("{}", market_event).contains("100000"));

        let close_event = WindowCloseEvent {
            event_id: "event123".to_string(),
            outcome: Some(Outcome::Yes),
            final_price: Some(dec!(100500)),
            timestamp: Utc::now(),
        };

        let market_event = MarketEvent::WindowClose(close_event);
        assert!(format!("{}", market_event).contains("event123"));
    }

    #[test]
    fn test_heartbeat() {
        let now = Utc::now();
        let event = MarketEvent::Heartbeat(now);

        assert!(event.is_heartbeat());
        assert_eq!(event.timestamp(), now);
    }

    #[test]
    fn test_event_timestamp() {
        let now = Utc::now();

        let events = vec![
            MarketEvent::SpotPrice(SpotPriceEvent {
                asset: CryptoAsset::Btc,
                price: dec!(50000),
                quantity: dec!(1),
                timestamp: now,
            }),
            MarketEvent::Heartbeat(now),
        ];

        for event in events {
            assert_eq!(event.timestamp(), now);
        }
    }
}
