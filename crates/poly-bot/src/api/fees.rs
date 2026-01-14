//! Fee rate client for Polymarket CLOB API.
//!
//! Fetches fee rates per token from the CLOB API and caches them
//! for the duration of a market session. Fee rates are expressed
//! in basis points (1 bps = 0.01%).
//!
//! ## Fee-Enabled Markets
//!
//! Currently only 15-minute crypto markets have fees enabled.
//! Fee-enabled markets return `fee_rate_bps > 0`, typically 1000 bps (10%).
//!
//! ## Usage
//!
//! ```ignore
//! let client = FeeRateClient::new(Some("https://clob.polymarket.com".to_string()));
//! let fee_bps = client.get_fee_rate("token_123").await?;
//! // fee_bps is cached - subsequent calls return cached value
//! ```

use std::collections::HashMap;
use std::sync::RwLock;
use std::time::Duration;

use reqwest::Client;
use serde::Deserialize;
use thiserror::Error;
use tracing::{debug, warn};

/// Default CLOB API base URL.
const DEFAULT_CLOB_URL: &str = "https://clob.polymarket.com";

/// Request timeout for fee rate API calls.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Errors that can occur when fetching fee rates.
#[derive(Debug, Error)]
pub enum FeeRateError {
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
}

/// Response from the fee rate API endpoint.
///
/// The API returns `base_fee` as the fee rate in basis points.
#[derive(Debug, Clone, Deserialize)]
pub struct FeeRateResponse {
    /// Fee rate in basis points (1 bps = 0.01%).
    /// Typically 0 for fee-free markets, 1000 for 15-minute crypto markets.
    #[serde(alias = "fee_rate_bps")]
    pub base_fee: u32,
}

impl FeeRateResponse {
    /// Create a new fee rate response.
    pub fn new(base_fee: u32) -> Self {
        Self { base_fee }
    }

    /// Returns the fee rate as a decimal multiplier (e.g., 0.10 for 1000 bps).
    pub fn as_decimal(&self) -> rust_decimal::Decimal {
        rust_decimal::Decimal::new(self.base_fee as i64, 4)
    }

    /// Returns true if this market has fees enabled.
    pub fn has_fees(&self) -> bool {
        self.base_fee > 0
    }
}

/// Client for fetching and caching fee rates from the Polymarket CLOB API.
///
/// Fee rates are cached per token ID for the lifetime of the client,
/// which should typically match a market session.
pub struct FeeRateClient {
    /// HTTP client for API requests.
    http: Client,
    /// Base URL for the CLOB API.
    base_url: String,
    /// Cached fee rates by token ID.
    cache: RwLock<HashMap<String, u32>>,
}

impl FeeRateClient {
    /// Create a new fee rate client.
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

    /// Create a client for testing with a custom base URL.
    pub fn with_url(base_url: &str) -> Self {
        Self::new(Some(base_url.to_string()))
    }

    /// Get the fee rate in basis points for a token.
    ///
    /// Returns cached value if available, otherwise fetches from API.
    /// Fee rates are cached for the lifetime of the client.
    ///
    /// # Arguments
    ///
    /// * `token_id` - The token ID to get the fee rate for.
    ///
    /// # Returns
    ///
    /// Fee rate in basis points (1 bps = 0.01%).
    /// Returns 0 for fee-free markets.
    pub async fn get_fee_rate(&self, token_id: &str) -> Result<u32, FeeRateError> {
        // Validate token ID
        if token_id.is_empty() {
            return Err(FeeRateError::InvalidTokenId(
                "Token ID cannot be empty".to_string(),
            ));
        }

        // Check cache first (read lock)
        {
            let cache = self.cache.read().unwrap();
            if let Some(&rate) = cache.get(token_id) {
                debug!(token_id = %token_id, rate = rate, "Fee rate cache hit");
                return Ok(rate);
            }
        }

        // Fetch from API
        let rate = self.fetch_fee_rate(token_id).await?;

        // Update cache (write lock)
        {
            let mut cache = self.cache.write().unwrap();
            cache.insert(token_id.to_string(), rate);
        }

        debug!(token_id = %token_id, rate = rate, "Fee rate fetched and cached");
        Ok(rate)
    }

    /// Fetch fee rate from the API (bypasses cache).
    ///
    /// Prefer using `get_fee_rate()` which caches results.
    pub async fn fetch_fee_rate(&self, token_id: &str) -> Result<u32, FeeRateError> {
        let url = format!("{}/fee-rate?token_id={}", self.base_url, token_id);
        debug!(url = %url, "Fetching fee rate");

        let response = self.http.get(&url).send().await?;
        let status = response.status();

        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            warn!(
                token_id = %token_id,
                status = status.as_u16(),
                body = %body,
                "Fee rate API error"
            );
            return Err(FeeRateError::ApiError {
                status: status.as_u16(),
                body,
            });
        }

        let response: FeeRateResponse = response.json().await.map_err(|e| {
            FeeRateError::Json(format!("Failed to parse fee rate response: {}", e))
        })?;

        Ok(response.base_fee)
    }

    /// Get the full fee rate response for a token.
    ///
    /// Includes helper methods for working with the fee rate.
    pub async fn get_fee_rate_response(
        &self,
        token_id: &str,
    ) -> Result<FeeRateResponse, FeeRateError> {
        let rate = self.get_fee_rate(token_id).await?;
        Ok(FeeRateResponse::new(rate))
    }

    /// Prefetch fee rates for multiple tokens.
    ///
    /// Useful for warming the cache at the start of a market session.
    /// Errors are logged but don't stop other fetches.
    pub async fn prefetch_fee_rates(&self, token_ids: &[&str]) {
        for token_id in token_ids {
            if let Err(e) = self.get_fee_rate(token_id).await {
                warn!(token_id = %token_id, error = %e, "Failed to prefetch fee rate");
            }
        }
    }

    /// Clear the fee rate cache.
    ///
    /// Call this when starting a new market session.
    pub fn clear_cache(&self) {
        let mut cache = self.cache.write().unwrap();
        cache.clear();
        debug!("Fee rate cache cleared");
    }

    /// Get the number of cached entries.
    pub fn cache_size(&self) -> usize {
        let cache = self.cache.read().unwrap();
        cache.len()
    }

    /// Check if a token's fee rate is cached.
    pub fn is_cached(&self, token_id: &str) -> bool {
        let cache = self.cache.read().unwrap();
        cache.contains_key(token_id)
    }
}

impl Default for FeeRateClient {
    fn default() -> Self {
        Self::production()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fee_rate_response_new() {
        let response = FeeRateResponse::new(1000);
        assert_eq!(response.base_fee, 1000);
    }

    #[test]
    fn test_fee_rate_response_as_decimal() {
        // 1000 bps = 0.10 (10%)
        let response = FeeRateResponse::new(1000);
        let decimal = response.as_decimal();
        assert_eq!(decimal, rust_decimal::Decimal::new(1000, 4));
        assert_eq!(decimal.to_string(), "0.1000");

        // 0 bps = 0
        let response_zero = FeeRateResponse::new(0);
        assert_eq!(
            response_zero.as_decimal(),
            rust_decimal::Decimal::new(0, 4)
        );

        // 50 bps = 0.005 (0.5%)
        let response_50 = FeeRateResponse::new(50);
        assert_eq!(response_50.as_decimal(), rust_decimal::Decimal::new(50, 4));
    }

    #[test]
    fn test_fee_rate_response_has_fees() {
        assert!(!FeeRateResponse::new(0).has_fees());
        assert!(FeeRateResponse::new(1).has_fees());
        assert!(FeeRateResponse::new(1000).has_fees());
    }

    #[test]
    fn test_fee_rate_response_deserialize_base_fee() {
        let json = r#"{"base_fee": 1000}"#;
        let response: FeeRateResponse = serde_json::from_str(json).unwrap();
        assert_eq!(response.base_fee, 1000);
    }

    #[test]
    fn test_fee_rate_response_deserialize_fee_rate_bps() {
        // Support both field names via serde alias
        let json = r#"{"fee_rate_bps": 500}"#;
        let response: FeeRateResponse = serde_json::from_str(json).unwrap();
        assert_eq!(response.base_fee, 500);
    }

    #[test]
    fn test_client_new_default_url() {
        let client = FeeRateClient::new(None);
        assert_eq!(client.base_url, DEFAULT_CLOB_URL);
    }

    #[test]
    fn test_client_new_custom_url() {
        let client = FeeRateClient::new(Some("http://localhost:8080".to_string()));
        assert_eq!(client.base_url, "http://localhost:8080");
    }

    #[test]
    fn test_client_with_url() {
        let client = FeeRateClient::with_url("http://test.example.com");
        assert_eq!(client.base_url, "http://test.example.com");
    }

    #[test]
    fn test_client_production() {
        let client = FeeRateClient::production();
        assert_eq!(client.base_url, DEFAULT_CLOB_URL);
    }

    #[test]
    fn test_client_default() {
        let client = FeeRateClient::default();
        assert_eq!(client.base_url, DEFAULT_CLOB_URL);
    }

    #[test]
    fn test_cache_operations() {
        let client = FeeRateClient::production();

        // Initially empty
        assert_eq!(client.cache_size(), 0);
        assert!(!client.is_cached("token_1"));

        // Manually populate cache
        {
            let mut cache = client.cache.write().unwrap();
            cache.insert("token_1".to_string(), 1000);
            cache.insert("token_2".to_string(), 0);
        }

        assert_eq!(client.cache_size(), 2);
        assert!(client.is_cached("token_1"));
        assert!(client.is_cached("token_2"));
        assert!(!client.is_cached("token_3"));

        // Clear cache
        client.clear_cache();
        assert_eq!(client.cache_size(), 0);
        assert!(!client.is_cached("token_1"));
    }

    #[tokio::test]
    async fn test_get_fee_rate_empty_token_id() {
        let client = FeeRateClient::production();
        let result = client.get_fee_rate("").await;

        assert!(result.is_err());
        match result {
            Err(FeeRateError::InvalidTokenId(msg)) => {
                assert!(msg.contains("empty"));
            }
            _ => panic!("Expected InvalidTokenId error"),
        }
    }

    #[tokio::test]
    async fn test_get_fee_rate_cache_hit() {
        let client = FeeRateClient::production();

        // Pre-populate cache
        {
            let mut cache = client.cache.write().unwrap();
            cache.insert("cached_token".to_string(), 500);
        }

        // Should return cached value without HTTP request
        let result = client.get_fee_rate("cached_token").await;
        assert_eq!(result.unwrap(), 500);
    }

    #[test]
    fn test_error_display() {
        let http_err = FeeRateError::Json("parse failed".to_string());
        assert!(http_err.to_string().contains("parse failed"));

        let api_err = FeeRateError::ApiError {
            status: 404,
            body: "not found".to_string(),
        };
        assert!(api_err.to_string().contains("404"));
        assert!(api_err.to_string().contains("not found"));

        let invalid_err = FeeRateError::InvalidTokenId("bad".to_string());
        assert!(invalid_err.to_string().contains("bad"));
    }
}
