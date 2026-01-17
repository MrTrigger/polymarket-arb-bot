//! Market data client for Polymarket CLOB API.
//!
//! Fetches market-specific data including minimum order size, tick size,
//! and orderbook state from the CLOB API.
//!
//! ## Usage
//!
//! ```ignore
//! let client = MarketClient::new(None);
//! let orderbook = client.get_orderbook("token_123").await?;
//! let min_order_size = orderbook.min_order_size;
//! ```

use std::collections::HashMap;
use std::sync::RwLock;
use std::time::Duration;

use reqwest::Client;
use rust_decimal::Decimal;
use serde::Deserialize;
use thiserror::Error;
use tracing::{debug, warn};

/// Default CLOB API base URL.
const DEFAULT_CLOB_URL: &str = "https://clob.polymarket.com";

/// Request timeout for market API calls.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Default minimum order size if API doesn't return one.
const DEFAULT_MIN_ORDER_SIZE: &str = "1";

/// Errors that can occur when fetching market data.
#[derive(Debug, Error)]
pub enum MarketError {
    /// HTTP request failed.
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),

    /// API returned an error status.
    #[error("API error: status {status}, body: {body}")]
    ApiError { status: u16, body: String },

    /// JSON parsing failed.
    #[error("JSON parsing failed: {0}")]
    Json(String),

    /// Invalid token ID.
    #[error("Invalid token ID: {0}")]
    InvalidTokenId(String),

    /// Market not found.
    #[error("Market not found: {0}")]
    NotFound(String),
}

/// Orderbook summary from the CLOB API.
///
/// Contains market parameters like minimum order size and tick size
/// in addition to the order book levels.
#[derive(Debug, Clone, Deserialize)]
pub struct OrderBookResponse {
    /// Market/condition ID.
    #[serde(default)]
    pub market: Option<String>,

    /// Asset/token ID.
    #[serde(default)]
    pub asset_id: Option<String>,

    /// Minimum order size in USDC (as string from API).
    #[serde(default = "default_min_order_size")]
    pub min_order_size: String,

    /// Tick size for price increments (as string from API).
    #[serde(default)]
    pub tick_size: Option<String>,

    /// Whether this is a neg-risk market.
    #[serde(default)]
    pub neg_risk: Option<bool>,

    /// Bid levels.
    #[serde(default)]
    pub bids: Vec<OrderLevel>,

    /// Ask levels.
    #[serde(default)]
    pub asks: Vec<OrderLevel>,

    /// Last trade price.
    #[serde(default)]
    pub last_trade_price: Option<String>,
}

fn default_min_order_size() -> String {
    DEFAULT_MIN_ORDER_SIZE.to_string()
}

impl OrderBookResponse {
    /// Get minimum order size as Decimal.
    pub fn min_order_size_decimal(&self) -> Decimal {
        self.min_order_size
            .parse()
            .unwrap_or(Decimal::ONE)
    }

    /// Get tick size as Decimal.
    pub fn tick_size_decimal(&self) -> Option<Decimal> {
        self.tick_size.as_ref().and_then(|s| s.parse().ok())
    }
}

/// Order level in the book.
#[derive(Debug, Clone, Deserialize)]
pub struct OrderLevel {
    /// Price as string.
    pub price: String,
    /// Size as string.
    pub size: String,
}

/// Market info from the markets endpoint.
#[derive(Debug, Clone, Deserialize)]
pub struct MarketResponse {
    /// Condition ID.
    pub condition_id: Option<String>,

    /// Minimum order size.
    #[serde(default = "default_min_order_size")]
    pub minimum_order_size: String,

    /// Minimum tick size.
    #[serde(default)]
    pub minimum_tick_size: Option<String>,

    /// Whether the market is active.
    #[serde(default)]
    pub active: bool,

    /// Whether the market is closed.
    #[serde(default)]
    pub closed: bool,
}

impl MarketResponse {
    /// Get minimum order size as Decimal.
    pub fn min_order_size_decimal(&self) -> Decimal {
        self.minimum_order_size
            .parse()
            .unwrap_or(Decimal::ONE)
    }
}

/// Client for fetching market data from the CLOB API.
///
/// Caches orderbook/market data to reduce API calls during a session.
pub struct MarketClient {
    http: Client,
    base_url: String,
    /// Cache of min_order_size by token_id.
    cache: RwLock<HashMap<String, Decimal>>,
}

impl MarketClient {
    /// Create a new market client.
    ///
    /// # Arguments
    ///
    /// * `base_url` - Optional custom CLOB API base URL. Defaults to production.
    pub fn new(base_url: Option<String>) -> Self {
        let http = Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .expect("Failed to create HTTP client");

        Self {
            http,
            base_url: base_url.unwrap_or_else(|| DEFAULT_CLOB_URL.to_string()),
            cache: RwLock::new(HashMap::new()),
        }
    }

    /// Get orderbook for a token, including min_order_size.
    ///
    /// # Arguments
    ///
    /// * `token_id` - The CLOB token ID (asset ID).
    ///
    /// # Returns
    ///
    /// The orderbook response with market parameters.
    pub async fn get_orderbook(&self, token_id: &str) -> Result<OrderBookResponse, MarketError> {
        if token_id.is_empty() {
            return Err(MarketError::InvalidTokenId("empty token ID".to_string()));
        }

        let url = format!("{}/book?token_id={}", self.base_url, token_id);
        debug!("Fetching orderbook from: {}", url);

        let response = self.http.get(&url).send().await?;
        let status = response.status();

        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(MarketError::NotFound(token_id.to_string()));
        }

        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(MarketError::ApiError {
                status: status.as_u16(),
                body,
            });
        }

        let body = response.text().await?;
        let orderbook: OrderBookResponse =
            serde_json::from_str(&body).map_err(|e| MarketError::Json(e.to_string()))?;

        // Cache the min_order_size
        if let Ok(mut cache) = self.cache.write() {
            cache.insert(token_id.to_string(), orderbook.min_order_size_decimal());
        }

        Ok(orderbook)
    }

    /// Get min_order_size for a token, using cache if available.
    ///
    /// Returns the cached value if available, otherwise fetches from API.
    /// Falls back to $1 (Polymarket default) if fetch fails.
    pub async fn get_min_order_size(&self, token_id: &str) -> Decimal {
        // Check cache first
        if let Ok(cache) = self.cache.read() {
            if let Some(&size) = cache.get(token_id) {
                return size;
            }
        }

        // Fetch from API
        match self.get_orderbook(token_id).await {
            Ok(ob) => ob.min_order_size_decimal(),
            Err(e) => {
                warn!("Failed to fetch min_order_size for {}: {}, using default $1", token_id, e);
                Decimal::ONE
            }
        }
    }

    /// Get market info by condition ID.
    ///
    /// # Arguments
    ///
    /// * `condition_id` - The market condition ID.
    ///
    /// # Returns
    ///
    /// The market info response.
    pub async fn get_market(&self, condition_id: &str) -> Result<MarketResponse, MarketError> {
        if condition_id.is_empty() {
            return Err(MarketError::InvalidTokenId("empty condition ID".to_string()));
        }

        let url = format!("{}/markets/{}", self.base_url, condition_id);
        debug!("Fetching market from: {}", url);

        let response = self.http.get(&url).send().await?;
        let status = response.status();

        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(MarketError::NotFound(condition_id.to_string()));
        }

        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(MarketError::ApiError {
                status: status.as_u16(),
                body,
            });
        }

        let body = response.text().await?;
        let market: MarketResponse =
            serde_json::from_str(&body).map_err(|e| MarketError::Json(e.to_string()))?;

        Ok(market)
    }

    /// Clear the cache.
    pub fn clear_cache(&self) {
        if let Ok(mut cache) = self.cache.write() {
            cache.clear();
        }
    }
}

impl Default for MarketClient {
    fn default() -> Self {
        Self::new(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_orderbook_response_parsing() {
        let json = r#"{
            "market": "cond123",
            "asset_id": "token123",
            "min_order_size": "5",
            "tick_size": "0.01",
            "neg_risk": false,
            "bids": [{"price": "0.45", "size": "100"}],
            "asks": [{"price": "0.55", "size": "150"}],
            "last_trade_price": "0.50"
        }"#;

        let ob: OrderBookResponse = serde_json::from_str(json).unwrap();
        assert_eq!(ob.min_order_size, "5");
        assert_eq!(ob.min_order_size_decimal(), Decimal::new(5, 0));
        assert_eq!(ob.tick_size_decimal(), Some(Decimal::new(1, 2)));
        assert_eq!(ob.bids.len(), 1);
        assert_eq!(ob.asks.len(), 1);
    }

    #[test]
    fn test_orderbook_default_min_order_size() {
        let json = r#"{
            "bids": [],
            "asks": []
        }"#;

        let ob: OrderBookResponse = serde_json::from_str(json).unwrap();
        assert_eq!(ob.min_order_size, "1");
        assert_eq!(ob.min_order_size_decimal(), Decimal::ONE);
    }

    #[test]
    fn test_market_response_parsing() {
        let json = r#"{
            "condition_id": "cond123",
            "minimum_order_size": "1",
            "minimum_tick_size": "0.001",
            "active": true,
            "closed": false
        }"#;

        let market: MarketResponse = serde_json::from_str(json).unwrap();
        assert_eq!(market.min_order_size_decimal(), Decimal::ONE);
        assert!(market.active);
        assert!(!market.closed);
    }

    #[test]
    fn test_market_client_creation() {
        let client = MarketClient::new(None);
        assert_eq!(client.base_url, DEFAULT_CLOB_URL);

        let custom = MarketClient::new(Some("https://custom.api".to_string()));
        assert_eq!(custom.base_url, "https://custom.api");
    }
}
