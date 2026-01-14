//! Orderbook state management.
//!
//! Maintains in-memory orderbook state from WebSocket updates.

use std::collections::HashMap;

use chrono::{DateTime, TimeZone, Utc};
use poly_common::OrderBookSnapshot;
use rust_decimal::Decimal;

use crate::types::{BookMessage, PriceChange};

/// In-memory orderbook state for a single token.
#[derive(Debug, Clone, Default)]
pub struct OrderBookState {
    /// Token ID (YES or NO token).
    pub token_id: String,
    /// Event/condition ID.
    pub event_id: String,
    /// Bid levels (price -> size).
    pub bids: HashMap<Decimal, Decimal>,
    /// Ask levels (price -> size).
    pub asks: HashMap<Decimal, Decimal>,
    /// Last update timestamp.
    pub last_update: Option<DateTime<Utc>>,
}

impl OrderBookState {
    /// Create a new orderbook state.
    pub fn new(token_id: String, event_id: String) -> Self {
        Self {
            token_id,
            event_id,
            bids: HashMap::new(),
            asks: HashMap::new(),
            last_update: None,
        }
    }

    /// Apply a full book snapshot, replacing all levels.
    pub fn apply_book(&mut self, book: &BookMessage) {
        self.bids.clear();
        self.asks.clear();

        for level in &book.bids {
            if let (Ok(price), Ok(size)) = (level.price.parse(), level.size.parse()) {
                self.bids.insert(price, size);
            }
        }
        for level in &book.asks {
            if let (Ok(price), Ok(size)) = (level.price.parse(), level.size.parse()) {
                self.asks.insert(price, size);
            }
        }

        self.last_update = parse_timestamp(&book.timestamp);
    }

    /// Apply a price change (delta update).
    pub fn apply_price_change(&mut self, change: &PriceChange) {
        let price: Decimal = match change.price.parse() {
            Ok(p) => p,
            Err(_) => return,
        };
        let size: Decimal = match change.size.parse() {
            Ok(s) => s,
            Err(_) => return,
        };

        let book = match change.side.to_lowercase().as_str() {
            "buy" | "bid" => &mut self.bids,
            "sell" | "ask" => &mut self.asks,
            _ => return,
        };

        if size.is_zero() {
            book.remove(&price);
        } else {
            book.insert(price, size);
        }
    }

    /// Get best bid price and size.
    pub fn best_bid(&self) -> (Decimal, Decimal) {
        self.bids
            .iter()
            .max_by_key(|(p, _)| *p)
            .map(|(p, s)| (*p, *s))
            .unwrap_or((Decimal::ZERO, Decimal::ZERO))
    }

    /// Get best ask price and size.
    pub fn best_ask(&self) -> (Decimal, Decimal) {
        self.asks
            .iter()
            .min_by_key(|(p, _)| *p)
            .map(|(p, s)| (*p, *s))
            .unwrap_or((Decimal::ONE, Decimal::ZERO))
    }

    /// Calculate spread in basis points.
    pub fn spread_bps(&self) -> u32 {
        let (best_bid, _) = self.best_bid();
        let (best_ask, _) = self.best_ask();
        if best_bid.is_zero() || best_ask.is_zero() {
            return 0;
        }
        let spread = best_ask - best_bid;
        let mid = (best_ask + best_bid) / Decimal::TWO;
        if mid.is_zero() {
            return 0;
        }
        // bps = (spread / mid) * 10000
        ((spread / mid) * Decimal::from(10000))
            .to_string()
            .parse::<f64>()
            .unwrap_or(0.0) as u32
    }

    /// Calculate mid price.
    pub fn mid_price(&self) -> Decimal {
        let (best_bid, _) = self.best_bid();
        let (best_ask, _) = self.best_ask();
        (best_bid + best_ask) / Decimal::TWO
    }

    /// Calculate total bid depth (sum of sizes).
    pub fn bid_depth(&self) -> Decimal {
        self.bids.values().copied().sum()
    }

    /// Calculate total ask depth (sum of sizes).
    pub fn ask_depth(&self) -> Decimal {
        self.asks.values().copied().sum()
    }

    /// Check if the orderbook has any data.
    pub fn is_empty(&self) -> bool {
        self.bids.is_empty() && self.asks.is_empty()
    }

    /// Check if orderbook has been updated recently.
    pub fn is_stale(&self, max_age: chrono::Duration) -> bool {
        match self.last_update {
            Some(ts) => Utc::now() - ts > max_age,
            None => true,
        }
    }

    /// Generate a snapshot record for storage (e.g., ClickHouse).
    pub fn to_snapshot(&self, timestamp: DateTime<Utc>) -> OrderBookSnapshot {
        let (best_bid, best_bid_size) = self.best_bid();
        let (best_ask, best_ask_size) = self.best_ask();

        OrderBookSnapshot {
            token_id: self.token_id.clone(),
            event_id: self.event_id.clone(),
            timestamp,
            best_bid,
            best_bid_size,
            best_ask,
            best_ask_size,
            spread_bps: self.spread_bps(),
            bid_depth: self.bid_depth(),
            ask_depth: self.ask_depth(),
        }
    }

    /// Get bid depth at a specific number of price levels.
    pub fn bid_depth_levels(&self, levels: usize) -> Decimal {
        let mut sorted: Vec<_> = self.bids.iter().collect();
        sorted.sort_by(|(a, _), (b, _)| b.cmp(a)); // Descending by price
        sorted.iter().take(levels).map(|(_, s)| **s).sum()
    }

    /// Get ask depth at a specific number of price levels.
    pub fn ask_depth_levels(&self, levels: usize) -> Decimal {
        let mut sorted: Vec<_> = self.asks.iter().collect();
        sorted.sort_by(|(a, _), (b, _)| a.cmp(b)); // Ascending by price
        sorted.iter().take(levels).map(|(_, s)| **s).sum()
    }
}

/// Parse timestamp from milliseconds string.
pub fn parse_timestamp(ts: &str) -> Option<DateTime<Utc>> {
    ts.parse::<i64>()
        .ok()
        .and_then(|ms| Utc.timestamp_millis_opt(ms).single())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::OrderSummary;
    use rust_decimal_macros::dec;

    #[test]
    fn test_orderbook_state_apply_book() {
        let mut state = OrderBookState::new("token1".to_string(), "event1".to_string());

        let book = BookMessage {
            event_type: "book".to_string(),
            asset_id: "token1".to_string(),
            market: "event1".to_string(),
            timestamp: "1704067200000".to_string(),
            hash: "abc123".to_string(),
            bids: vec![
                OrderSummary {
                    price: "0.45".to_string(),
                    size: "100".to_string(),
                },
                OrderSummary {
                    price: "0.44".to_string(),
                    size: "200".to_string(),
                },
            ],
            asks: vec![
                OrderSummary {
                    price: "0.55".to_string(),
                    size: "150".to_string(),
                },
                OrderSummary {
                    price: "0.56".to_string(),
                    size: "250".to_string(),
                },
            ],
        };

        state.apply_book(&book);

        assert_eq!(state.bids.len(), 2);
        assert_eq!(state.asks.len(), 2);

        let (best_bid, best_bid_size) = state.best_bid();
        assert_eq!(best_bid, dec!(0.45));
        assert_eq!(best_bid_size, dec!(100));

        let (best_ask, best_ask_size) = state.best_ask();
        assert_eq!(best_ask, dec!(0.55));
        assert_eq!(best_ask_size, dec!(150));
    }

    #[test]
    fn test_orderbook_state_apply_price_change() {
        let mut state = OrderBookState::new("token1".to_string(), "event1".to_string());
        state.bids.insert(dec!(0.45), dec!(100));

        // Update existing level
        let change = PriceChange {
            asset_id: "token1".to_string(),
            price: "0.45".to_string(),
            size: "150".to_string(),
            side: "buy".to_string(),
            hash: None,
            best_bid: None,
            best_ask: None,
        };
        state.apply_price_change(&change);
        assert_eq!(state.bids.get(&dec!(0.45)), Some(&dec!(150)));

        // Remove level (size = 0)
        let change = PriceChange {
            asset_id: "token1".to_string(),
            price: "0.45".to_string(),
            size: "0".to_string(),
            side: "buy".to_string(),
            hash: None,
            best_bid: None,
            best_ask: None,
        };
        state.apply_price_change(&change);
        assert!(state.bids.get(&dec!(0.45)).is_none());
    }

    #[test]
    fn test_orderbook_state_depth() {
        let mut state = OrderBookState::new("token1".to_string(), "event1".to_string());
        state.bids.insert(dec!(0.45), dec!(100));
        state.bids.insert(dec!(0.44), dec!(200));
        state.asks.insert(dec!(0.55), dec!(150));
        state.asks.insert(dec!(0.56), dec!(250));

        assert_eq!(state.bid_depth(), dec!(300));
        assert_eq!(state.ask_depth(), dec!(400));
    }

    #[test]
    fn test_orderbook_state_spread_bps() {
        let mut state = OrderBookState::new("token1".to_string(), "event1".to_string());
        state.bids.insert(dec!(0.48), dec!(100));
        state.asks.insert(dec!(0.52), dec!(100));

        // Spread = 0.04, mid = 0.50, spread_bps = 0.04/0.50 * 10000 = 800
        let spread = state.spread_bps();
        assert_eq!(spread, 800);
    }

    #[test]
    fn test_orderbook_state_mid_price() {
        let mut state = OrderBookState::new("token1".to_string(), "event1".to_string());
        state.bids.insert(dec!(0.48), dec!(100));
        state.asks.insert(dec!(0.52), dec!(100));

        assert_eq!(state.mid_price(), dec!(0.50));
    }

    #[test]
    fn test_orderbook_depth_levels() {
        let mut state = OrderBookState::new("token1".to_string(), "event1".to_string());
        state.bids.insert(dec!(0.45), dec!(100));
        state.bids.insert(dec!(0.44), dec!(200));
        state.bids.insert(dec!(0.43), dec!(300));

        // Top 2 levels: 0.45 (100) + 0.44 (200) = 300
        assert_eq!(state.bid_depth_levels(2), dec!(300));
    }

    #[test]
    fn test_parse_timestamp() {
        let ts = parse_timestamp("1704067200000");
        assert!(ts.is_some());
        let dt = ts.unwrap();
        assert_eq!(dt.timestamp_millis(), 1704067200000);
    }

    #[test]
    fn test_orderbook_is_empty() {
        let state = OrderBookState::new("token1".to_string(), "event1".to_string());
        assert!(state.is_empty());
    }
}
