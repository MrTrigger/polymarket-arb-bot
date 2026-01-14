//! Rewards API client for Polymarket CLOB API.
//!
//! Fetches reward configurations and user earnings from the CLOB API.
//! Maker rebates are paid based on order volume and market activity.
//!
//! ## Endpoints
//!
//! - `GET /rewards/markets/current` - Current reward configuration per market
//! - `GET /rewards/user/percentages` - User's reward percentage by market
//! - `GET /rewards/user/total?date=YYYY-MM-DD` - User's total earnings for a date
//!
//! ## Usage
//!
//! ```ignore
//! let client = RewardsClient::new(Some("https://clob.polymarket.com".to_string()));
//! let rewards = client.fetch_current_rewards().await?;
//! let earnings = client.fetch_user_earnings("2024-01-15").await?;
//! ```

use std::collections::HashMap;
use std::sync::RwLock;
use std::time::Duration;

use reqwest::Client;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::{debug, warn};

/// Default CLOB API base URL.
const DEFAULT_CLOB_URL: &str = "https://clob.polymarket.com";

/// Request timeout for rewards API calls.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Errors that can occur when fetching rewards.
#[derive(Debug, Error)]
pub enum RewardsError {
    /// HTTP request failed.
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),

    /// API returned an error status.
    #[error("API error: status {status}, body: {body}")]
    ApiError { status: u16, body: String },

    /// JSON parsing failed.
    #[error("JSON parsing failed: {0}")]
    Json(String),

    /// Invalid date format.
    #[error("Invalid date format: {0}")]
    InvalidDate(String),
}

/// Current reward configuration for a market.
///
/// This represents the maker rebate program configuration for a specific market.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct CurrentRewardResponse {
    /// Market condition ID.
    pub condition_id: String,

    /// Daily rewards pool in USDC.
    #[serde(default)]
    pub rewards_daily_rate: Decimal,

    /// Minimum order size to qualify for rewards (in shares).
    #[serde(default)]
    pub rewards_min_size: Decimal,

    /// Maximum spread (in bps) to qualify for rewards.
    #[serde(default)]
    pub rewards_max_spread: u32,

    /// Whether rewards are currently active for this market.
    #[serde(default)]
    pub active: bool,

    /// Start timestamp for rewards period (Unix seconds).
    #[serde(default)]
    pub start_date: Option<i64>,

    /// End timestamp for rewards period (Unix seconds).
    #[serde(default)]
    pub end_date: Option<i64>,
}

impl CurrentRewardResponse {
    /// Returns the minimum order size as a Decimal.
    pub fn min_size(&self) -> Decimal {
        self.rewards_min_size
    }

    /// Returns the maximum spread in basis points.
    pub fn max_spread_bps(&self) -> u32 {
        self.rewards_max_spread
    }

    /// Returns true if rewards are active and properly configured.
    pub fn is_active(&self) -> bool {
        self.active && self.rewards_daily_rate > Decimal::ZERO
    }
}

/// Simplified reward configuration for strategy use.
///
/// Extracted from `CurrentRewardResponse` with only the fields needed for trading.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RewardsConfig {
    /// Minimum order size to qualify for rewards (in shares).
    pub min_size: Decimal,

    /// Maximum spread (in bps) to qualify for rewards.
    pub max_spread_bps: u32,

    /// Daily rewards pool in USDC.
    pub daily_rate: Decimal,

    /// Whether rewards are currently active.
    pub active: bool,
}

impl RewardsConfig {
    /// Create a new rewards config.
    pub fn new(min_size: Decimal, max_spread_bps: u32, daily_rate: Decimal, active: bool) -> Self {
        Self {
            min_size,
            max_spread_bps,
            daily_rate,
            active,
        }
    }

    /// Create from API response.
    pub fn from_response(response: &CurrentRewardResponse) -> Self {
        Self {
            min_size: response.rewards_min_size,
            max_spread_bps: response.rewards_max_spread,
            daily_rate: response.rewards_daily_rate,
            active: response.is_active(),
        }
    }

    /// Returns true if an order qualifies for rewards.
    ///
    /// # Arguments
    ///
    /// * `size` - Order size in shares
    /// * `spread_bps` - Current spread in basis points
    pub fn qualifies(&self, size: Decimal, spread_bps: u32) -> bool {
        self.active && size >= self.min_size && spread_bps <= self.max_spread_bps
    }
}

/// User's reward percentage by market.
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct UserPercentagesResponse {
    /// Map of condition_id -> percentage (0.0 to 1.0).
    #[serde(flatten)]
    pub percentages: HashMap<String, Decimal>,
}

/// Single earning entry for a market.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct UserEarning {
    /// Market condition ID.
    pub condition_id: String,

    /// Amount earned in USDC.
    pub amount: Decimal,

    /// User's percentage of the pool.
    #[serde(default)]
    pub percentage: Decimal,
}

/// Total user earnings for a date.
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct TotalUserEarningResponse {
    /// Date in YYYY-MM-DD format.
    pub date: String,

    /// Total earnings across all markets in USDC.
    pub total: Decimal,

    /// Per-market earnings breakdown.
    #[serde(default)]
    pub earnings: Vec<UserEarning>,
}

/// Client for fetching rewards data from the Polymarket CLOB API.
///
/// Provides methods to fetch current reward configurations, user percentages,
/// and historical earnings. Includes caching for reward configs to reduce
/// API calls during a market session.
pub struct RewardsClient {
    /// HTTP client for API requests.
    http: Client,
    /// Base URL for the CLOB API.
    base_url: String,
    /// Cached reward configs by condition_id.
    config_cache: RwLock<HashMap<String, RewardsConfig>>,
}

impl RewardsClient {
    /// Create a new rewards client.
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
            config_cache: RwLock::new(HashMap::new()),
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

    /// Fetch current reward configurations for all markets.
    ///
    /// Returns a list of active reward programs with their configurations.
    pub async fn fetch_current_rewards(&self) -> Result<Vec<CurrentRewardResponse>, RewardsError> {
        let url = format!("{}/rewards/markets/current", self.base_url);
        debug!(url = %url, "Fetching current rewards");

        let response = self.http.get(&url).send().await?;
        let status = response.status();

        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            warn!(
                status = status.as_u16(),
                body = %body,
                "Rewards API error"
            );
            return Err(RewardsError::ApiError {
                status: status.as_u16(),
                body,
            });
        }

        let rewards: Vec<CurrentRewardResponse> = response.json().await.map_err(|e| {
            RewardsError::Json(format!("Failed to parse rewards response: {}", e))
        })?;

        debug!(count = rewards.len(), "Fetched current rewards");
        Ok(rewards)
    }

    /// Get reward config for a specific market.
    ///
    /// Returns cached value if available, otherwise fetches all configs
    /// and caches them.
    ///
    /// # Arguments
    ///
    /// * `condition_id` - The market condition ID.
    pub async fn get_reward_config(
        &self,
        condition_id: &str,
    ) -> Result<Option<RewardsConfig>, RewardsError> {
        // Check cache first
        {
            let cache = self.config_cache.read().unwrap();
            if let Some(config) = cache.get(condition_id) {
                debug!(condition_id = %condition_id, "Reward config cache hit");
                return Ok(Some(config.clone()));
            }
        }

        // Fetch all configs and cache them
        let rewards = self.fetch_current_rewards().await?;

        let mut cache = self.config_cache.write().unwrap();
        let mut result = None;

        for reward in rewards {
            let config = RewardsConfig::from_response(&reward);
            if reward.condition_id == condition_id {
                result = Some(config.clone());
            }
            cache.insert(reward.condition_id, config);
        }

        Ok(result)
    }

    /// Fetch user's reward percentages by market.
    ///
    /// Returns a map of condition_id -> percentage (0.0 to 1.0).
    /// Percentages represent the user's share of the daily rewards pool.
    pub async fn fetch_reward_percentages(
        &self,
    ) -> Result<HashMap<String, Decimal>, RewardsError> {
        let url = format!("{}/rewards/user/percentages", self.base_url);
        debug!(url = %url, "Fetching user reward percentages");

        let response = self.http.get(&url).send().await?;
        let status = response.status();

        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            warn!(
                status = status.as_u16(),
                body = %body,
                "Reward percentages API error"
            );
            return Err(RewardsError::ApiError {
                status: status.as_u16(),
                body,
            });
        }

        // Try parsing as object first, then as wrapped response
        let text = response.text().await.map_err(|e| {
            RewardsError::Json(format!("Failed to read response body: {}", e))
        })?;

        // Try direct HashMap parsing
        if let Ok(percentages) = serde_json::from_str::<HashMap<String, Decimal>>(&text) {
            debug!(count = percentages.len(), "Fetched reward percentages");
            return Ok(percentages);
        }

        // Try wrapped response parsing
        let response: UserPercentagesResponse = serde_json::from_str(&text).map_err(|e| {
            RewardsError::Json(format!("Failed to parse percentages response: {}", e))
        })?;

        debug!(count = response.percentages.len(), "Fetched reward percentages");
        Ok(response.percentages)
    }

    /// Fetch user's total earnings for a specific date.
    ///
    /// # Arguments
    ///
    /// * `date` - Date in YYYY-MM-DD format.
    pub async fn fetch_user_earnings(
        &self,
        date: &str,
    ) -> Result<TotalUserEarningResponse, RewardsError> {
        // Validate date format (simple check)
        if date.len() != 10 || date.chars().filter(|c| *c == '-').count() != 2 {
            return Err(RewardsError::InvalidDate(date.to_string()));
        }

        let url = format!("{}/rewards/user/total?date={}", self.base_url, date);
        debug!(url = %url, "Fetching user earnings");

        let response = self.http.get(&url).send().await?;
        let status = response.status();

        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            warn!(
                date = %date,
                status = status.as_u16(),
                body = %body,
                "User earnings API error"
            );
            return Err(RewardsError::ApiError {
                status: status.as_u16(),
                body,
            });
        }

        let earnings: TotalUserEarningResponse = response.json().await.map_err(|e| {
            RewardsError::Json(format!("Failed to parse earnings response: {}", e))
        })?;

        debug!(
            date = %date,
            total = %earnings.total,
            markets = earnings.earnings.len(),
            "Fetched user earnings"
        );

        Ok(earnings)
    }

    /// Prefetch reward configs for multiple markets.
    ///
    /// Useful for warming the cache at the start of a trading session.
    pub async fn prefetch_configs(&self, condition_ids: &[&str]) {
        // Fetch all configs (this populates the cache)
        if let Err(e) = self.fetch_current_rewards().await {
            warn!(error = %e, "Failed to prefetch reward configs");
            return;
        }

        // Log which requested markets have configs
        let cache = self.config_cache.read().unwrap();
        for condition_id in condition_ids {
            if cache.contains_key(*condition_id) {
                debug!(condition_id = %condition_id, "Reward config prefetched");
            } else {
                debug!(condition_id = %condition_id, "No reward config found");
            }
        }
    }

    /// Clear the reward config cache.
    ///
    /// Call this when starting a new trading session or when configs might have changed.
    pub fn clear_cache(&self) {
        let mut cache = self.config_cache.write().unwrap();
        cache.clear();
        debug!("Reward config cache cleared");
    }

    /// Get the number of cached configs.
    pub fn cache_size(&self) -> usize {
        let cache = self.config_cache.read().unwrap();
        cache.len()
    }

    /// Check if a market's config is cached.
    pub fn is_cached(&self, condition_id: &str) -> bool {
        let cache = self.config_cache.read().unwrap();
        cache.contains_key(condition_id)
    }
}

impl Default for RewardsClient {
    fn default() -> Self {
        Self::production()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn test_current_reward_response_is_active() {
        // Active with non-zero daily rate
        let active = CurrentRewardResponse {
            condition_id: "test".to_string(),
            rewards_daily_rate: dec!(100),
            rewards_min_size: dec!(10),
            rewards_max_spread: 200,
            active: true,
            start_date: None,
            end_date: None,
        };
        assert!(active.is_active());

        // Not active flag
        let inactive_flag = CurrentRewardResponse {
            condition_id: "test".to_string(),
            rewards_daily_rate: dec!(100),
            rewards_min_size: dec!(10),
            rewards_max_spread: 200,
            active: false,
            start_date: None,
            end_date: None,
        };
        assert!(!inactive_flag.is_active());

        // Zero daily rate
        let zero_rate = CurrentRewardResponse {
            condition_id: "test".to_string(),
            rewards_daily_rate: Decimal::ZERO,
            rewards_min_size: dec!(10),
            rewards_max_spread: 200,
            active: true,
            start_date: None,
            end_date: None,
        };
        assert!(!zero_rate.is_active());
    }

    #[test]
    fn test_current_reward_response_helpers() {
        let response = CurrentRewardResponse {
            condition_id: "test".to_string(),
            rewards_daily_rate: dec!(100),
            rewards_min_size: dec!(10),
            rewards_max_spread: 200,
            active: true,
            start_date: None,
            end_date: None,
        };

        assert_eq!(response.min_size(), dec!(10));
        assert_eq!(response.max_spread_bps(), 200);
    }

    #[test]
    fn test_rewards_config_new() {
        let config = RewardsConfig::new(dec!(5), 150, dec!(50), true);

        assert_eq!(config.min_size, dec!(5));
        assert_eq!(config.max_spread_bps, 150);
        assert_eq!(config.daily_rate, dec!(50));
        assert!(config.active);
    }

    #[test]
    fn test_rewards_config_from_response() {
        let response = CurrentRewardResponse {
            condition_id: "test".to_string(),
            rewards_daily_rate: dec!(100),
            rewards_min_size: dec!(10),
            rewards_max_spread: 200,
            active: true,
            start_date: None,
            end_date: None,
        };

        let config = RewardsConfig::from_response(&response);

        assert_eq!(config.min_size, dec!(10));
        assert_eq!(config.max_spread_bps, 200);
        assert_eq!(config.daily_rate, dec!(100));
        assert!(config.active);
    }

    #[test]
    fn test_rewards_config_qualifies() {
        let config = RewardsConfig::new(dec!(10), 200, dec!(100), true);

        // Qualifies: size >= 10, spread <= 200
        assert!(config.qualifies(dec!(10), 200));
        assert!(config.qualifies(dec!(100), 100));
        assert!(config.qualifies(dec!(10), 0));

        // Doesn't qualify: size too small
        assert!(!config.qualifies(dec!(9), 100));
        assert!(!config.qualifies(dec!(1), 50));

        // Doesn't qualify: spread too wide
        assert!(!config.qualifies(dec!(100), 201));
        assert!(!config.qualifies(dec!(50), 500));

        // Inactive config
        let inactive = RewardsConfig::new(dec!(10), 200, dec!(100), false);
        assert!(!inactive.qualifies(dec!(100), 100));
    }

    #[test]
    fn test_rewards_config_default() {
        let config = RewardsConfig::default();

        assert_eq!(config.min_size, Decimal::ZERO);
        assert_eq!(config.max_spread_bps, 0);
        assert_eq!(config.daily_rate, Decimal::ZERO);
        assert!(!config.active);
    }

    #[test]
    fn test_current_reward_response_deserialize() {
        let json = r#"{
            "condition_id": "0x1234",
            "rewards_daily_rate": "100.50",
            "rewards_min_size": "10",
            "rewards_max_spread": 200,
            "active": true
        }"#;

        let response: CurrentRewardResponse = serde_json::from_str(json).unwrap();

        assert_eq!(response.condition_id, "0x1234");
        assert_eq!(response.rewards_daily_rate, dec!(100.50));
        assert_eq!(response.rewards_min_size, dec!(10));
        assert_eq!(response.rewards_max_spread, 200);
        assert!(response.active);
    }

    #[test]
    fn test_current_reward_response_deserialize_with_defaults() {
        let json = r#"{"condition_id": "0x1234"}"#;

        let response: CurrentRewardResponse = serde_json::from_str(json).unwrap();

        assert_eq!(response.condition_id, "0x1234");
        assert_eq!(response.rewards_daily_rate, Decimal::ZERO);
        assert_eq!(response.rewards_min_size, Decimal::ZERO);
        assert_eq!(response.rewards_max_spread, 0);
        assert!(!response.active);
    }

    #[test]
    fn test_user_earning_deserialize() {
        let json = r#"{
            "condition_id": "0x5678",
            "amount": "12.34",
            "percentage": "0.05"
        }"#;

        let earning: UserEarning = serde_json::from_str(json).unwrap();

        assert_eq!(earning.condition_id, "0x5678");
        assert_eq!(earning.amount, dec!(12.34));
        assert_eq!(earning.percentage, dec!(0.05));
    }

    #[test]
    fn test_total_user_earning_response_deserialize() {
        let json = r#"{
            "date": "2024-01-15",
            "total": "56.78",
            "earnings": [
                {"condition_id": "0x1", "amount": "30.00", "percentage": "0.03"},
                {"condition_id": "0x2", "amount": "26.78", "percentage": "0.02"}
            ]
        }"#;

        let response: TotalUserEarningResponse = serde_json::from_str(json).unwrap();

        assert_eq!(response.date, "2024-01-15");
        assert_eq!(response.total, dec!(56.78));
        assert_eq!(response.earnings.len(), 2);
        assert_eq!(response.earnings[0].condition_id, "0x1");
        assert_eq!(response.earnings[0].amount, dec!(30));
    }

    #[test]
    fn test_client_new_default_url() {
        let client = RewardsClient::new(None);
        assert_eq!(client.base_url, DEFAULT_CLOB_URL);
    }

    #[test]
    fn test_client_new_custom_url() {
        let client = RewardsClient::new(Some("http://localhost:8080".to_string()));
        assert_eq!(client.base_url, "http://localhost:8080");
    }

    #[test]
    fn test_client_with_url() {
        let client = RewardsClient::with_url("http://test.example.com");
        assert_eq!(client.base_url, "http://test.example.com");
    }

    #[test]
    fn test_client_production() {
        let client = RewardsClient::production();
        assert_eq!(client.base_url, DEFAULT_CLOB_URL);
    }

    #[test]
    fn test_client_default() {
        let client = RewardsClient::default();
        assert_eq!(client.base_url, DEFAULT_CLOB_URL);
    }

    #[test]
    fn test_cache_operations() {
        let client = RewardsClient::production();

        // Initially empty
        assert_eq!(client.cache_size(), 0);
        assert!(!client.is_cached("condition_1"));

        // Manually populate cache
        {
            let mut cache = client.config_cache.write().unwrap();
            cache.insert(
                "condition_1".to_string(),
                RewardsConfig::new(dec!(10), 200, dec!(100), true),
            );
            cache.insert(
                "condition_2".to_string(),
                RewardsConfig::new(dec!(5), 150, dec!(50), false),
            );
        }

        assert_eq!(client.cache_size(), 2);
        assert!(client.is_cached("condition_1"));
        assert!(client.is_cached("condition_2"));
        assert!(!client.is_cached("condition_3"));

        // Clear cache
        client.clear_cache();
        assert_eq!(client.cache_size(), 0);
        assert!(!client.is_cached("condition_1"));
    }

    #[tokio::test]
    async fn test_fetch_user_earnings_invalid_date() {
        let client = RewardsClient::production();

        // Invalid format
        let result = client.fetch_user_earnings("2024-1-15").await;
        assert!(matches!(result, Err(RewardsError::InvalidDate(_))));

        let result = client.fetch_user_earnings("20240115").await;
        assert!(matches!(result, Err(RewardsError::InvalidDate(_))));

        let result = client.fetch_user_earnings("").await;
        assert!(matches!(result, Err(RewardsError::InvalidDate(_))));
    }

    #[test]
    fn test_error_display() {
        let json_err = RewardsError::Json("parse failed".to_string());
        assert!(json_err.to_string().contains("parse failed"));

        let api_err = RewardsError::ApiError {
            status: 404,
            body: "not found".to_string(),
        };
        assert!(api_err.to_string().contains("404"));
        assert!(api_err.to_string().contains("not found"));

        let date_err = RewardsError::InvalidDate("bad-date".to_string());
        assert!(date_err.to_string().contains("bad-date"));
    }

    #[test]
    fn test_user_percentages_response_deserialize_direct() {
        let json = r#"{"0x1234": "0.15", "0x5678": "0.25"}"#;

        let percentages: HashMap<String, Decimal> = serde_json::from_str(json).unwrap();

        assert_eq!(percentages.len(), 2);
        assert_eq!(percentages.get("0x1234"), Some(&dec!(0.15)));
        assert_eq!(percentages.get("0x5678"), Some(&dec!(0.25)));
    }
}
