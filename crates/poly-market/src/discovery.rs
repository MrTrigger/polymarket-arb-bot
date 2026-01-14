//! Market discovery via Polymarket Gamma API.
//!
//! Discovers active 15-minute up/down markets for crypto assets.

use std::collections::HashSet;
use std::time::Duration;

use chrono::{DateTime, Utc};
use poly_common::CryptoAsset;
use reqwest::Client;
use rust_decimal::Decimal;
use thiserror::Error;
use tracing::{debug, error, info, warn};

use crate::types::{GammaEvent, GammaMarket, TokenIds};

/// Gamma API base URL.
const GAMMA_API_URL: &str = "https://gamma-api.polymarket.com";

/// Keywords to identify 15-minute up/down markets in titles.
const FIFTEEN_MIN_KEYWORDS: &[&str] = &["15 min", "15-min", "15min", "15 minute"];
const UP_DOWN_KEYWORDS: &[&str] = &["up or down", "higher or lower", "above or below"];

/// Errors that can occur during market discovery.
#[derive(Debug, Error)]
pub enum DiscoveryError {
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),

    #[error("JSON parsing failed: {0}")]
    Json(#[from] serde_json::Error),

    #[error("Invalid market data: {0}")]
    InvalidData(String),
}

/// A discovered market ready for trading/tracking.
#[derive(Debug, Clone)]
pub struct DiscoveredMarket {
    /// Event ID from Polymarket.
    pub event_id: String,
    /// Condition ID for the market.
    pub condition_id: String,
    /// Crypto asset (BTC, ETH, etc.).
    pub asset: CryptoAsset,
    /// YES token ID for CLOB subscription.
    pub yes_token_id: String,
    /// NO token ID for CLOB subscription.
    pub no_token_id: String,
    /// Strike price for the up/down market.
    pub strike_price: Decimal,
    /// Window start time.
    pub window_start: DateTime<Utc>,
    /// Window end time (settlement).
    pub window_end: DateTime<Utc>,
    /// When this market was discovered.
    pub discovered_at: DateTime<Utc>,
}

impl DiscoveredMarket {
    /// Get time remaining until window end.
    pub fn time_remaining(&self) -> chrono::Duration {
        self.window_end - Utc::now()
    }

    /// Check if the window is still active.
    pub fn is_active(&self) -> bool {
        let now = Utc::now();
        now >= self.window_start && now < self.window_end
    }

    /// Check if the window has expired.
    pub fn is_expired(&self) -> bool {
        Utc::now() >= self.window_end
    }

    /// Get minutes remaining.
    pub fn minutes_remaining(&self) -> f64 {
        self.time_remaining().num_seconds() as f64 / 60.0
    }
}

/// Configuration for market discovery.
#[derive(Debug, Clone)]
pub struct DiscoveryConfig {
    /// Assets to track.
    pub assets: Vec<CryptoAsset>,
    /// HTTP request timeout.
    pub request_timeout: Duration,
    /// Discovery polling interval.
    pub poll_interval: Duration,
}

impl Default for DiscoveryConfig {
    fn default() -> Self {
        Self {
            assets: vec![CryptoAsset::Btc, CryptoAsset::Eth, CryptoAsset::Sol],
            request_timeout: Duration::from_secs(30),
            poll_interval: Duration::from_secs(30),
        }
    }
}

/// Market discovery client for finding 15-minute crypto markets.
pub struct MarketDiscovery {
    http: Client,
    config: DiscoveryConfig,
    /// Known market event IDs to avoid re-processing.
    known_markets: HashSet<String>,
}

impl MarketDiscovery {
    /// Create a new market discovery client.
    pub fn new(config: DiscoveryConfig) -> Self {
        let http = Client::builder()
            .timeout(config.request_timeout)
            .build()
            .expect("Failed to create HTTP client");

        Self {
            http,
            config,
            known_markets: HashSet::new(),
        }
    }

    /// Create with default config.
    pub fn with_assets(assets: Vec<CryptoAsset>) -> Self {
        Self::new(DiscoveryConfig {
            assets,
            ..Default::default()
        })
    }

    /// Discover active 15-minute markets.
    /// Returns newly discovered markets (not seen before).
    pub async fn discover(&mut self) -> Result<Vec<DiscoveredMarket>, DiscoveryError> {
        info!("Starting market discovery for {:?}", self.config.assets);

        let events = self.fetch_active_events().await?;
        debug!("Fetched {} active events", events.len());

        let mut new_markets = Vec::new();

        for event in events {
            // Skip if we've seen this event
            let event_id = match &event.id {
                Some(id) => id.clone(),
                None => continue,
            };

            if self.known_markets.contains(&event_id) {
                continue;
            }

            // Check if this is a 15-minute crypto market
            if let Some(market) = self.parse_crypto_market(&event)? {
                info!(
                    "Discovered new market: {} {} strike={} (ends {})",
                    market.asset, market.event_id, market.strike_price, market.window_end
                );
                new_markets.push(market);
                self.known_markets.insert(event_id);
            }
        }

        Ok(new_markets)
    }

    /// Discover all active markets (including previously seen).
    pub async fn discover_all(&mut self) -> Result<Vec<DiscoveredMarket>, DiscoveryError> {
        let events = self.fetch_active_events().await?;
        let mut markets = Vec::new();

        for event in events {
            if let Some(market) = self.parse_crypto_market(&event)? {
                // Track in known markets
                if let Some(id) = &event.id {
                    self.known_markets.insert(id.clone());
                }
                markets.push(market);
            }
        }

        Ok(markets)
    }

    /// Fetch active events from Gamma API.
    async fn fetch_active_events(&self) -> Result<Vec<GammaEvent>, DiscoveryError> {
        let url = format!(
            "{}/events?active=true&closed=false&limit=100",
            GAMMA_API_URL
        );

        let response = self.http.get(&url).send().await?;

        if !response.status().is_success() {
            return Err(DiscoveryError::InvalidData(format!(
                "Gamma API returned status {}",
                response.status()
            )));
        }

        let events: Vec<GammaEvent> = response.json().await?;
        Ok(events)
    }

    /// Parse a Gamma event into a DiscoveredMarket if it's a valid 15-minute crypto market.
    fn parse_crypto_market(
        &self,
        event: &GammaEvent,
    ) -> Result<Option<DiscoveredMarket>, DiscoveryError> {
        let title = match &event.title {
            Some(t) => t.to_lowercase(),
            None => return Ok(None),
        };

        // Check if this is a 15-minute market
        let is_15min = FIFTEEN_MIN_KEYWORDS.iter().any(|kw| title.contains(kw));

        if !is_15min {
            return Ok(None);
        }

        // Check if it's an up/down market
        let is_up_down = UP_DOWN_KEYWORDS.iter().any(|kw| title.contains(kw));

        if !is_up_down {
            debug!("Skipping non-up/down market: {}", title);
            return Ok(None);
        }

        // Determine which crypto asset this is for
        let asset = match self.detect_asset(&title) {
            Some(a) => a,
            None => {
                debug!("No matching asset for market: {}", title);
                return Ok(None);
            }
        };

        // Check if we're tracking this asset
        if !self.config.assets.contains(&asset) {
            return Ok(None);
        }

        // Get the market from the event
        let market = match &event.markets {
            Some(markets) if !markets.is_empty() => &markets[0],
            _ => {
                warn!(
                    "Event {} has no markets",
                    event.id.as_deref().unwrap_or("unknown")
                );
                return Ok(None);
            }
        };

        // Parse token IDs
        let token_ids = match self.parse_token_ids(market)? {
            Some(t) => t,
            None => {
                warn!(
                    "Could not parse token IDs for market {}",
                    market.id.as_deref().unwrap_or("unknown")
                );
                return Ok(None);
            }
        };

        // Parse timestamps
        let window_end = match self.parse_datetime(&event.end_date)? {
            Some(t) => t,
            None => {
                warn!("Could not parse end_date for event {:?}", event.id);
                return Ok(None);
            }
        };

        // For 15-minute markets, window_start is 15 minutes before end
        let window_start = window_end - chrono::Duration::minutes(15);

        // Parse strike price from title
        let strike_price = self.parse_strike_price(&title);

        let discovered_market = DiscoveredMarket {
            event_id: event.id.clone().unwrap_or_default(),
            condition_id: market.condition_id.clone().unwrap_or_default(),
            asset,
            yes_token_id: token_ids.yes_token_id,
            no_token_id: token_ids.no_token_id,
            strike_price,
            window_start,
            window_end,
            discovered_at: Utc::now(),
        };

        Ok(Some(discovered_market))
    }

    /// Detect which crypto asset a market title refers to.
    fn detect_asset(&self, title: &str) -> Option<CryptoAsset> {
        let title_lower = title.to_lowercase();

        if title_lower.contains("btc") || title_lower.contains("bitcoin") {
            return Some(CryptoAsset::Btc);
        }
        if title_lower.contains("eth") || title_lower.contains("ethereum") {
            return Some(CryptoAsset::Eth);
        }
        if title_lower.contains("sol") || title_lower.contains("solana") {
            return Some(CryptoAsset::Sol);
        }
        if title_lower.contains("xrp") || title_lower.contains("ripple") {
            return Some(CryptoAsset::Xrp);
        }

        None
    }

    /// Parse token IDs from the market's clob_token_ids field.
    fn parse_token_ids(&self, market: &GammaMarket) -> Result<Option<TokenIds>, DiscoveryError> {
        let clob_tokens_str = match &market.clob_token_ids {
            Some(s) => s,
            None => return Ok(None),
        };

        // Parse the JSON string array
        let tokens: Vec<String> = match serde_json::from_str(clob_tokens_str) {
            Ok(t) => t,
            Err(e) => {
                debug!(
                    "Failed to parse clob_token_ids '{}': {}",
                    clob_tokens_str, e
                );
                return Ok(None);
            }
        };

        if tokens.len() != 2 {
            debug!(
                "Expected 2 token IDs, got {}: {:?}",
                tokens.len(),
                tokens
            );
            return Ok(None);
        }

        // Parse outcomes to determine which is YES and which is NO
        let outcomes = match &market.outcomes {
            Some(s) => {
                let parsed: Vec<String> = serde_json::from_str(s).unwrap_or_default();
                parsed
            }
            None => vec!["Yes".to_string(), "No".to_string()],
        };

        // Typically index 0 is Yes, index 1 is No
        let (yes_idx, no_idx) = if outcomes.len() >= 2 {
            let yes_pos = outcomes
                .iter()
                .position(|o| o.to_lowercase() == "yes")
                .unwrap_or(0);
            let no_pos = outcomes
                .iter()
                .position(|o| o.to_lowercase() == "no")
                .unwrap_or(1);
            (yes_pos, no_pos)
        } else {
            (0, 1)
        };

        Ok(Some(TokenIds {
            yes_token_id: tokens.get(yes_idx).cloned().unwrap_or_default(),
            no_token_id: tokens.get(no_idx).cloned().unwrap_or_default(),
        }))
    }

    /// Parse a datetime string from the API.
    fn parse_datetime(
        &self,
        dt_str: &Option<String>,
    ) -> Result<Option<DateTime<Utc>>, DiscoveryError> {
        let s = match dt_str {
            Some(s) => s,
            None => return Ok(None),
        };

        // Try ISO 8601 format
        if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
            return Ok(Some(dt.with_timezone(&Utc)));
        }

        // Try other common formats
        if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.fZ") {
            return Ok(Some(dt.and_utc()));
        }

        if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S") {
            return Ok(Some(dt.and_utc()));
        }

        debug!("Could not parse datetime: {}", s);
        Ok(None)
    }

    /// Parse strike price from market title.
    /// Example: "Will BTC be above $100,000 at 12:15 UTC?"
    fn parse_strike_price(&self, title: &str) -> Decimal {
        // Look for dollar amounts
        let re_patterns = [
            r"\$([0-9,]+(?:\.[0-9]+)?)",
            r"(\d+(?:,\d{3})*(?:\.\d+)?)\s*(?:usd|usdt|usdc)?",
        ];

        for pattern in &re_patterns {
            if let Ok(re) = regex::Regex::new(pattern)
                && let Some(captures) = re.captures(title)
                && let Some(price_str) = captures.get(1)
            {
                let cleaned = price_str.as_str().replace(',', "");
                if let Ok(price) = cleaned.parse::<Decimal>() {
                    return price;
                }
            }
        }

        // Default to zero if we can't parse
        Decimal::ZERO
    }

    /// Get count of known markets.
    pub fn known_market_count(&self) -> usize {
        self.known_markets.len()
    }

    /// Clear known markets (useful for testing or resetting state).
    pub fn clear_known_markets(&mut self) {
        self.known_markets.clear();
    }

    /// Run discovery loop with callback.
    pub async fn run_loop<F>(
        mut self,
        mut shutdown: tokio::sync::broadcast::Receiver<()>,
        mut on_discovery: F,
    ) -> Result<(), DiscoveryError>
    where
        F: FnMut(Vec<DiscoveredMarket>),
    {
        info!(
            "Starting discovery loop with {:?} interval",
            self.config.poll_interval
        );

        loop {
            match self.discover().await {
                Ok(markets) => {
                    if !markets.is_empty() {
                        info!("Discovery found {} new markets", markets.len());
                        on_discovery(markets);
                    } else {
                        debug!("Discovery found no new markets");
                    }
                }
                Err(e) => {
                    error!("Discovery error: {}", e);
                }
            }

            tokio::select! {
                _ = tokio::time::sleep(self.config.poll_interval) => {}
                _ = shutdown.recv() => {
                    info!("Discovery loop received shutdown signal");
                    break;
                }
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_discovery() -> MarketDiscovery {
        MarketDiscovery::new(DiscoveryConfig {
            assets: vec![
                CryptoAsset::Btc,
                CryptoAsset::Eth,
                CryptoAsset::Sol,
                CryptoAsset::Xrp,
            ],
            ..Default::default()
        })
    }

    #[test]
    fn test_detect_asset() {
        let discovery = test_discovery();

        assert_eq!(discovery.detect_asset("will btc go up"), Some(CryptoAsset::Btc));
        assert_eq!(
            discovery.detect_asset("bitcoin 15 minute"),
            Some(CryptoAsset::Btc)
        );
        assert_eq!(
            discovery.detect_asset("eth price prediction"),
            Some(CryptoAsset::Eth)
        );
        assert_eq!(
            discovery.detect_asset("ethereum 15min"),
            Some(CryptoAsset::Eth)
        );
        assert_eq!(discovery.detect_asset("sol up or down"), Some(CryptoAsset::Sol));
        assert_eq!(discovery.detect_asset("xrp price"), Some(CryptoAsset::Xrp));
        assert_eq!(discovery.detect_asset("random market"), None);
    }

    #[test]
    fn test_parse_strike_price() {
        let discovery = test_discovery();

        assert_eq!(
            discovery.parse_strike_price("Will BTC be above $100,000 at 12:15 UTC?"),
            Decimal::from(100000)
        );
        assert_eq!(
            discovery.parse_strike_price("ETH above $3,500.50"),
            Decimal::new(350050, 2)
        );
        assert_eq!(discovery.parse_strike_price("no price here"), Decimal::ZERO);
    }

    #[test]
    fn test_parse_token_ids() {
        let discovery = test_discovery();

        let market = GammaMarket {
            id: Some("test".to_string()),
            question: None,
            condition_id: None,
            slug: None,
            clob_token_ids: Some(r#"["token_yes", "token_no"]"#.to_string()),
            outcomes: Some(r#"["Yes", "No"]"#.to_string()),
            end_date: None,
            active: Some(true),
            closed: Some(false),
        };

        let tokens = discovery.parse_token_ids(&market).unwrap().unwrap();
        assert_eq!(tokens.yes_token_id, "token_yes");
        assert_eq!(tokens.no_token_id, "token_no");
    }

    #[test]
    fn test_is_15min_market() {
        let title = "btc 15 minute up or down";
        assert!(FIFTEEN_MIN_KEYWORDS.iter().any(|kw| title.contains(kw)));
        assert!(UP_DOWN_KEYWORDS.iter().any(|kw| title.contains(kw)));

        let title2 = "btc hourly prediction";
        assert!(!FIFTEEN_MIN_KEYWORDS.iter().any(|kw| title2.contains(kw)));
    }

    #[test]
    fn test_discovered_market_time_methods() {
        let market = DiscoveredMarket {
            event_id: "test".to_string(),
            condition_id: "cond".to_string(),
            asset: CryptoAsset::Btc,
            yes_token_id: "yes".to_string(),
            no_token_id: "no".to_string(),
            strike_price: Decimal::from(100000),
            window_start: Utc::now() - chrono::Duration::minutes(5),
            window_end: Utc::now() + chrono::Duration::minutes(10),
            discovered_at: Utc::now(),
        };

        assert!(market.is_active());
        assert!(!market.is_expired());
        assert!(market.minutes_remaining() > 9.0);
        assert!(market.minutes_remaining() < 11.0);
    }

    #[test]
    fn test_discovery_config_default() {
        let config = DiscoveryConfig::default();
        assert_eq!(config.assets.len(), 3);
        assert_eq!(config.request_timeout, Duration::from_secs(30));
        assert_eq!(config.poll_interval, Duration::from_secs(30));
    }
}
