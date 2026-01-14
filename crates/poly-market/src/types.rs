//! Shared types for Polymarket integration.
//!
//! Data structures for API responses and WebSocket messages.

use serde::{Deserialize, Serialize};

/// Market data from Gamma API response.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GammaMarket {
    pub id: Option<String>,
    pub question: Option<String>,
    pub condition_id: Option<String>,
    pub slug: Option<String>,
    /// Token IDs as JSON string array: `["123", "456"]`
    pub clob_token_ids: Option<String>,
    /// Outcomes as JSON string array: `["Yes", "No"]`
    pub outcomes: Option<String>,
    pub end_date: Option<String>,
    pub active: Option<bool>,
    pub closed: Option<bool>,
}

/// Event data from Gamma API response.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GammaEvent {
    pub id: Option<String>,
    pub title: Option<String>,
    pub slug: Option<String>,
    pub start_date: Option<String>,
    pub end_date: Option<String>,
    pub active: Option<bool>,
    pub closed: Option<bool>,
    pub markets: Option<Vec<GammaMarket>>,
    pub tags: Option<Vec<GammaTag>>,
}

/// Tag from Gamma API.
#[derive(Debug, Clone, Deserialize)]
pub struct GammaTag {
    pub id: Option<String>,
    pub label: Option<String>,
    pub slug: Option<String>,
}

/// Parsed token IDs for a market's outcomes.
#[derive(Debug, Clone)]
pub struct TokenIds {
    pub yes_token_id: String,
    pub no_token_id: String,
}

/// Subscription message to Polymarket CLOB WebSocket.
#[derive(Debug, Serialize)]
pub struct SubscribeMessage {
    pub assets_ids: Vec<String>,
    #[serde(rename = "type")]
    pub msg_type: &'static str,
}

/// Dynamic subscription operation.
#[derive(Debug, Serialize)]
pub struct SubscriptionOp {
    pub assets_ids: Vec<String>,
    pub operation: &'static str,
}

/// Orderbook level from CLOB WebSocket.
#[derive(Debug, Clone, Deserialize)]
pub struct OrderSummary {
    pub price: String,
    pub size: String,
}

/// Book message from CLOB WebSocket (full orderbook snapshot).
#[derive(Debug, Clone, Deserialize)]
pub struct BookMessage {
    pub event_type: String,
    pub asset_id: String,
    pub market: String,
    pub timestamp: String,
    pub hash: String,
    pub bids: Vec<OrderSummary>,
    pub asks: Vec<OrderSummary>,
}

/// Price change entry (orderbook delta).
#[derive(Debug, Clone, Deserialize)]
pub struct PriceChange {
    pub asset_id: String,
    pub price: String,
    pub size: String,
    pub side: String,
    #[serde(default)]
    pub hash: Option<String>,
    #[serde(default)]
    pub best_bid: Option<String>,
    #[serde(default)]
    pub best_ask: Option<String>,
}

/// Price change message from CLOB WebSocket.
#[derive(Debug, Clone, Deserialize)]
pub struct PriceChangeMessage {
    pub event_type: String,
    pub asset_id: String,
    pub market: String,
    pub timestamp: String,
    pub price_changes: Vec<PriceChange>,
}

/// Generic message for detecting event type.
#[derive(Debug, Deserialize)]
pub struct GenericMessage {
    pub event_type: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_book_message_parsing() {
        let json = r#"{
            "event_type": "book",
            "asset_id": "token123",
            "market": "cond456",
            "timestamp": "1704067200000",
            "hash": "abc123",
            "bids": [{"price": "0.45", "size": "100"}],
            "asks": [{"price": "0.55", "size": "150"}]
        }"#;

        let book: BookMessage = serde_json::from_str(json).unwrap();
        assert_eq!(book.event_type, "book");
        assert_eq!(book.asset_id, "token123");
        assert_eq!(book.bids.len(), 1);
        assert_eq!(book.asks.len(), 1);
    }

    #[test]
    fn test_price_change_message_parsing() {
        let json = r#"{
            "event_type": "price_change",
            "asset_id": "token123",
            "market": "cond456",
            "timestamp": "1704067200000",
            "price_changes": [
                {
                    "asset_id": "token123",
                    "price": "0.46",
                    "size": "50",
                    "side": "buy"
                }
            ]
        }"#;

        let msg: PriceChangeMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.event_type, "price_change");
        assert_eq!(msg.price_changes.len(), 1);
        assert_eq!(msg.price_changes[0].price, "0.46");
    }

    #[test]
    fn test_subscribe_message_serialization() {
        let msg = SubscribeMessage {
            assets_ids: vec!["token1".to_string(), "token2".to_string()],
            msg_type: "market",
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"assets_ids\""));
        assert!(json.contains("\"type\":\"market\""));
    }

    #[test]
    fn test_gamma_event_parsing() {
        let json = r#"{
            "id": "event123",
            "title": "BTC 15-min up or down",
            "slug": "btc-15min",
            "startDate": "2024-01-01T12:00:00Z",
            "endDate": "2024-01-01T12:15:00Z",
            "active": true,
            "closed": false,
            "markets": [{
                "id": "market123",
                "question": "Will BTC be above $95000?",
                "conditionId": "cond123",
                "clobTokenIds": "[\"token1\", \"token2\"]",
                "outcomes": "[\"Yes\", \"No\"]",
                "active": true,
                "closed": false
            }],
            "tags": [{"id": "tag1", "label": "Crypto", "slug": "crypto"}]
        }"#;

        let event: GammaEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.id, Some("event123".to_string()));
        assert!(event.title.as_ref().unwrap().contains("BTC"));
        assert!(event.markets.is_some());
        assert_eq!(event.markets.as_ref().unwrap().len(), 1);
    }
}
