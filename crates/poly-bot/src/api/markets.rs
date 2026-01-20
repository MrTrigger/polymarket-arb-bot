//! Market info client for Polymarket CLOB API.
//!
//! Fetches market information including minimum order size from the CLOB API.
//! The min_order_size is expressed in shares (not USDC).
//!
//! ## Usage
//!
//! ```ignore
//! let client = MarketClient::new(None);
//! let market_info = client.get_market("condition_id_123").await?;
//! println!("Min order size: {} shares", market_info.min_order_size);
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

/// Default minimum order size in shares (used for backtest or when API unavailable).
pub const DEFAULT_MIN_ORDER_SIZE: Decimal = Decimal::ONE;

/// Errors that can occur when fetching market info.
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

    /// Invalid condition ID.
    #[error("Invalid condition ID: {0}")]
    InvalidConditionId(String),
}

/// Response from the market info API endpoint.
#[derive(Debug, Clone, Deserialize)]
pub struct MarketInfoResponse {
    /// Condition ID (market identifier).
    pub condition_id: String,
    /// Minimum order size in shares.
    #[serde(deserialize_with = "deserialize_decimal_string")]
    pub minimum_order_size: Decimal,
    /// Minimum tick size (price increment).
    #[serde(default, deserialize_with = "deserialize_optional_decimal_string")]
    pub minimum_tick_size: Option<Decimal>,
}

/// Helper to deserialize decimal from string.
fn deserialize_decimal_string<'de, D>(deserializer: D) -> Result<Decimal, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let s: String = serde::Deserialize::deserialize(deserializer)?;
    s.parse::<Decimal>().map_err(serde::de::Error::custom)
}

/// Helper to deserialize optional decimal from string.
fn deserialize_optional_decimal_string<'de, D>(deserializer: D) -> Result<Option<Decimal>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let opt: Option<String> = serde::Deserialize::deserialize(deserializer)?;
    match opt {
        Some(s) if !s.is_empty() => s
            .parse::<Decimal>()
            .map(Some)
            .map_err(serde::de::Error::custom),
        _ => Ok(None),
    }
}

/// Cached market info.
#[derive(Debug, Clone)]
pub struct CachedMarketInfo {
    /// Minimum order size in shares.
    pub min_order_size: Decimal,
    /// Minimum tick size (price increment).
    pub min_tick_size: Option<Decimal>,
}

impl From<MarketInfoResponse> for CachedMarketInfo {
    fn from(response: MarketInfoResponse) -> Self {
        Self {
            min_order_size: response.minimum_order_size,
            min_tick_size: response.minimum_tick_size,
        }
    }
}

/// Client for fetching and caching market info from the Polymarket CLOB API.
pub struct MarketClient {
    /// HTTP client for API requests.
    http: Client,
    /// Base URL for the CLOB API.
    base_url: String,
    /// Cached market info by condition ID.
    cache: RwLock<HashMap<String, CachedMarketInfo>>,
}

impl MarketClient {
    /// Create a new market client.
    ///
    /// # Arguments
    ///
    /// * `base_url` - Optional custom CLOB API URL. Defaults to production.
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

    /// Create a client with the default production URL.
    pub fn production() -> Self {
        Self::new(None)
    }

    /// Get minimum order size for a market.
    ///
    /// Returns cached value if available, otherwise fetches from API.
    ///
    /// # Arguments
    ///
    /// * `condition_id` - The condition ID (market identifier).
    ///
    /// # Returns
    ///
    /// Minimum order size in shares.
    pub async fn get_min_order_size(&self, condition_id: &str) -> Result<Decimal, MarketError> {
        let info = self.get_market_info(condition_id).await?;
        Ok(info.min_order_size)
    }

    /// Get market info, with caching.
    ///
    /// # Arguments
    ///
    /// * `condition_id` - The condition ID (market identifier).
    pub async fn get_market_info(&self, condition_id: &str) -> Result<CachedMarketInfo, MarketError> {
        // Validate condition ID
        if condition_id.is_empty() {
            return Err(MarketError::InvalidConditionId(
                "Condition ID cannot be empty".to_string(),
            ));
        }

        // Check cache first (read lock)
        {
            let cache = self.cache.read().unwrap();
            if let Some(info) = cache.get(condition_id) {
                debug!(condition_id = %condition_id, "Market info cache hit");
                return Ok(info.clone());
            }
        }

        // Fetch from API
        let response = self.fetch_market_info(condition_id).await?;
        let info = CachedMarketInfo::from(response);

        // Update cache (write lock)
        {
            let mut cache = self.cache.write().unwrap();
            cache.insert(condition_id.to_string(), info.clone());
        }

        debug!(
            condition_id = %condition_id,
            min_order_size = %info.min_order_size,
            "Market info fetched and cached"
        );
        Ok(info)
    }

    /// Fetch market info from the API (bypasses cache).
    async fn fetch_market_info(&self, condition_id: &str) -> Result<MarketInfoResponse, MarketError> {
        let url = format!("{}/markets/{}", self.base_url, condition_id);
        debug!(url = %url, "Fetching market info");

        let response = self.http.get(&url).send().await?;
        let status = response.status();

        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            warn!(
                condition_id = %condition_id,
                status = status.as_u16(),
                body = %body,
                "Market info API error"
            );
            return Err(MarketError::ApiError {
                status: status.as_u16(),
                body,
            });
        }

        let response: MarketInfoResponse = response.json().await.map_err(|e| {
            MarketError::Json(format!("Failed to parse market info response: {}", e))
        })?;

        Ok(response)
    }

    /// Clear the cache.
    pub fn clear_cache(&self) {
        let mut cache = self.cache.write().unwrap();
        cache.clear();
        debug!("Market info cache cleared");
    }

    /// Get the number of cached entries.
    pub fn cache_size(&self) -> usize {
        let cache = self.cache.read().unwrap();
        cache.len()
    }
}

impl Default for MarketClient {
    fn default() -> Self {
        Self::production()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn test_default_min_order_size() {
        assert_eq!(DEFAULT_MIN_ORDER_SIZE, dec!(1));
    }

    #[test]
    fn test_client_new_default_url() {
        let client = MarketClient::new(None);
        assert_eq!(client.base_url, DEFAULT_CLOB_URL);
    }

    #[test]
    fn test_client_new_custom_url() {
        let client = MarketClient::new(Some("http://localhost:8080".to_string()));
        assert_eq!(client.base_url, "http://localhost:8080");
    }

    #[test]
    fn test_cache_operations() {
        let client = MarketClient::production();

        // Initially empty
        assert_eq!(client.cache_size(), 0);

        // Manually populate cache
        {
            let mut cache = client.cache.write().unwrap();
            cache.insert(
                "cond_1".to_string(),
                CachedMarketInfo {
                    min_order_size: dec!(5),
                    min_tick_size: Some(dec!(0.01)),
                },
            );
        }

        assert_eq!(client.cache_size(), 1);

        // Clear cache
        client.clear_cache();
        assert_eq!(client.cache_size(), 0);
    }

    #[tokio::test]
    async fn test_get_market_info_empty_condition_id() {
        let client = MarketClient::production();
        let result = client.get_market_info("").await;

        assert!(result.is_err());
        match result {
            Err(MarketError::InvalidConditionId(msg)) => {
                assert!(msg.contains("empty"));
            }
            _ => panic!("Expected InvalidConditionId error"),
        }
    }

    #[tokio::test]
    async fn test_get_market_info_cache_hit() {
        let client = MarketClient::production();

        // Pre-populate cache
        {
            let mut cache = client.cache.write().unwrap();
            cache.insert(
                "cached_cond".to_string(),
                CachedMarketInfo {
                    min_order_size: dec!(10),
                    min_tick_size: None,
                },
            );
        }

        // Should return cached value without HTTP request
        let result = client.get_market_info("cached_cond").await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap().min_order_size, dec!(10));
    }

    #[test]
    fn test_deserialize_market_info_response() {
        let json = r#"{
            "condition_id": "0x123",
            "minimum_order_size": "5",
            "minimum_tick_size": "0.01"
        }"#;
        let response: MarketInfoResponse = serde_json::from_str(json).unwrap();
        assert_eq!(response.condition_id, "0x123");
        assert_eq!(response.minimum_order_size, dec!(5));
        assert_eq!(response.minimum_tick_size, Some(dec!(0.01)));
    }

    #[test]
    fn test_deserialize_market_info_response_minimal() {
        let json = r#"{
            "condition_id": "0x456",
            "minimum_order_size": "1"
        }"#;
        let response: MarketInfoResponse = serde_json::from_str(json).unwrap();
        assert_eq!(response.condition_id, "0x456");
        assert_eq!(response.minimum_order_size, dec!(1));
        assert_eq!(response.minimum_tick_size, None);
    }

    #[test]
    fn test_error_display() {
        let json_err = MarketError::Json("parse failed".to_string());
        assert!(json_err.to_string().contains("parse failed"));

        let api_err = MarketError::ApiError {
            status: 404,
            body: "not found".to_string(),
        };
        assert!(api_err.to_string().contains("404"));

        let invalid_err = MarketError::InvalidConditionId("bad".to_string());
        assert!(invalid_err.to_string().contains("bad"));
    }
}
