//! Market discovery via Polymarket Gamma API.
//!
//! Discovers active 15-minute up/down markets for crypto assets (BTC, ETH, SOL, XRP),
//! extracts YES/NO token IDs, and stores market metadata to ClickHouse.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use poly_common::{ClickHouseClient, CryptoAsset, MarketWindow, WindowDuration};
use reqwest::Client;
use rust_decimal::Decimal;
use thiserror::Error;
use tracing::{debug, error, info, warn};

// Re-export types from poly-market for convenience
pub use poly_market::{GammaEvent, GammaMarket, GammaTag, TokenIds};

// Use poly-market's discovery for Binance price fetching (same method as live trading)
use poly_market::{DiscoveryConfig as PolyMarketDiscoveryConfig, MarketDiscovery as PolyMarketDiscovery};

/// Gamma API base URL.
const GAMMA_API_URL: &str = "https://gamma-api.polymarket.com";

/// Tag slugs for crypto 15-minute markets.
/// These may need adjustment based on actual Polymarket tagging.
#[allow(dead_code)]
const CRYPTO_TAG_SLUGS: &[&str] = &["crypto", "bitcoin", "ethereum", "solana", "xrp"];

/// Keywords to identify 15-minute up/down markets in titles.
const FIFTEEN_MIN_KEYWORDS: &[&str] = &["15 min", "15-min", "15min", "15 minute"];
const UP_DOWN_KEYWORDS: &[&str] = &["up or down", "higher or lower", "above or below"];

#[derive(Debug, Error)]
pub enum DiscoveryError {
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),

    #[error("JSON parsing failed: {0}")]
    Json(#[from] serde_json::Error),

    #[error("ClickHouse error: {0}")]
    ClickHouse(#[from] poly_common::ClickHouseError),

    #[error("Invalid market data: {0}")]
    InvalidData(String),
}

/// Market discovery client for finding 15-minute crypto markets.
pub struct MarketDiscovery {
    http: Client,
    db: Arc<ClickHouseClient>,
    /// Assets to track.
    assets: Vec<CryptoAsset>,
    /// Known market event IDs to avoid re-processing.
    known_markets: HashSet<String>,
    /// Recently discovered market windows (for CLOB subscription).
    discovered_windows: Vec<MarketWindow>,
    /// Binance price fetcher (uses same logic as live trading).
    price_fetcher: PolyMarketDiscovery,
}

impl MarketDiscovery {
    /// Create a new market discovery client.
    pub fn new(db: Arc<ClickHouseClient>, assets: Vec<CryptoAsset>) -> Self {
        let http = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .expect("Failed to create HTTP client");

        // Create price fetcher using poly-market's discovery (same as live trading)
        let price_fetcher = PolyMarketDiscovery::new(PolyMarketDiscoveryConfig {
            assets: assets.clone(),
            window_duration: WindowDuration::FifteenMin,
            ..Default::default()
        });

        Self {
            http,
            db,
            assets,
            known_markets: HashSet::new(),
            discovered_windows: Vec::new(),
            price_fetcher,
        }
    }

    /// Discover active 15-minute markets and store new ones to ClickHouse.
    /// Returns the number of new markets discovered.
    pub async fn discover(&mut self) -> Result<usize, DiscoveryError> {
        info!("Starting market discovery for {:?}", self.assets);

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
            if let Some(mut market_window) = self.parse_crypto_market(&event)? {
                // For "Up or Down" markets with no strike in title, fetch from Binance
                // This is the same logic used in live trading (poly-market crate)
                let now = Utc::now();
                let is_active = now >= market_window.window_start && now < market_window.window_end;
                let is_expired = now >= market_window.window_end;

                if market_window.strike_price.is_zero() && (is_active || is_expired) {
                    // Fetch historical price at exact window_start time from Binance
                    // Parse asset from string (only 4 supported assets)
                    let asset = match market_window.asset.to_uppercase().as_str() {
                        "BTC" => CryptoAsset::Btc,
                        "ETH" => CryptoAsset::Eth,
                        "SOL" => CryptoAsset::Sol,
                        "XRP" => CryptoAsset::Xrp,
                        _ => {
                            warn!("Unknown asset: {}, defaulting to BTC", market_window.asset);
                            CryptoAsset::Btc
                        }
                    };

                    match self.price_fetcher.fetch_historical_spot_price(asset, market_window.window_start).await {
                        Ok(Some(price)) => {
                            info!(
                                "Setting strike for {} {} from Binance at {}: ${}",
                                market_window.asset, market_window.event_id, market_window.window_start, price
                            );
                            market_window.strike_price = price;
                        }
                        Ok(None) => {
                            warn!(
                                "Binance returned no price data for {} at {}, strike stays at 0",
                                market_window.event_id, market_window.window_start
                            );
                        }
                        Err(e) => {
                            warn!(
                                "Failed to fetch price for {}: {}, strike stays at 0",
                                market_window.event_id, e
                            );
                        }
                    }
                } else if market_window.strike_price.is_zero() && !is_active && !is_expired {
                    debug!(
                        "Market {} has no strike, waiting for window to start",
                        market_window.event_id
                    );
                }

                let duration_mins = (market_window.window_end - market_window.window_start).num_minutes();
                info!(
                    "Discovered new market: {} {} strike={} ({}min window, ends {})",
                    market_window.asset, market_window.event_id, market_window.strike_price,
                    duration_mins, market_window.window_end
                );
                new_markets.push(market_window);
                self.known_markets.insert(event_id);
            }
        }

        // Store new markets to ClickHouse and update discovered windows
        if !new_markets.is_empty() {
            // Try to store to ClickHouse, but don't fail if it errors
            // (we can still collect data to CSV)
            if let Err(e) = self.store_markets(&new_markets).await {
                warn!("Failed to store markets to ClickHouse (continuing): {}", e);
            }
            // Always extend discovered windows for CLOB subscription
            self.discovered_windows.extend(new_markets.clone());
        }

        Ok(new_markets.len())
    }

    /// Get all discovered market windows (for CLOB subscription).
    /// This is a non-async method that returns a clone of discovered windows.
    pub async fn get_discovered_windows(&self) -> Result<Vec<MarketWindow>, DiscoveryError> {
        Ok(self.discovered_windows.clone())
    }

    /// Fetch active events from Gamma API.
    async fn fetch_active_events(&self) -> Result<Vec<GammaEvent>, DiscoveryError> {
        // Fetch from all relevant tag_slugs since markets are hidden from general listings
        let tag_slugs = ["5M", "15M"]; // 5-minute and 15-minute markets
        let mut all_events = Vec::new();

        for tag in tag_slugs {
            let url = format!(
                "{}/events?tag_slug={}&closed=false&limit=100",
                GAMMA_API_URL, tag
            );

            let response = self.http.get(&url).send().await?;
            if response.status().is_success() {
                let events: Vec<GammaEvent> = response.json().await?;
                debug!("Fetched {} events from tag_slug={}", events.len(), tag);
                all_events.extend(events);
            }
        }

        // Also try 1-hour markets (no tag_slug, use active=true)
        let url = format!(
            "{}/events?active=true&closed=false&limit=100",
            GAMMA_API_URL
        );
        if let Ok(response) = self.http.get(&url).send().await {
            if response.status().is_success() {
                if let Ok(events) = response.json::<Vec<GammaEvent>>().await {
                    debug!("Fetched {} events from active=true", events.len());
                    all_events.extend(events);
                }
            }
        }

        info!("Total events fetched: {}", all_events.len());
        Ok(all_events)
    }

    /// Parse a Gamma event into a MarketWindow if it's a valid crypto up/down market.
    /// Since we fetch from tag_slug=5M and tag_slug=15M, we don't need to check for
    /// time duration keywords - just verify it's an up/down market.
    fn parse_crypto_market(
        &self,
        event: &GammaEvent,
    ) -> Result<Option<MarketWindow>, DiscoveryError> {
        let title = match &event.title {
            Some(t) => t.to_lowercase(),
            None => return Ok(None),
        };

        // Check if it's an up/down market
        let is_up_down = UP_DOWN_KEYWORDS
            .iter()
            .any(|kw| title.contains(kw));

        if !is_up_down {
            debug!("Skipping non-up/down market: {}", title);
            return Ok(None);
        }

        // Determine which crypto asset this is for
        let asset = self.detect_asset(&title)?;
        let asset = match asset {
            Some(a) => a,
            None => {
                debug!("No matching asset for market: {}", title);
                return Ok(None);
            }
        };

        // Check if we're tracking this asset
        if !self.assets.contains(&asset) {
            return Ok(None);
        }

        // Get the market from the event
        let market = match &event.markets {
            Some(markets) if !markets.is_empty() => &markets[0],
            _ => {
                warn!("Event {} has no markets", event.id.as_deref().unwrap_or("unknown"));
                return Ok(None);
            }
        };

        // Parse token IDs
        let token_ids = self.parse_token_ids(market)?;
        let token_ids = match token_ids {
            Some(t) => t,
            None => {
                warn!(
                    "Could not parse token IDs for market {}",
                    market.id.as_deref().unwrap_or("unknown")
                );
                return Ok(None);
            }
        };

        // Parse timestamps - try event.end_date first, fall back to market.end_date
        let window_end = self.parse_datetime(&event.end_date)?
            .or_else(|| self.parse_datetime(&market.end_date).ok().flatten());
        let window_end = match window_end {
            Some(t) => t,
            None => {
                warn!("Could not parse end_date for event {:?}", event.id);
                return Ok(None);
            }
        };

        // Detect window duration from tags (5M, 15M, 1H)
        let window_duration = self.detect_window_duration(event);
        let window_start = window_end - window_duration;

        // Strike price will be fetched from Binance at window_start time.
        // Don't parse from title — titles like "BTC up at 1/26 20:05" yield garbage values.
        let strike_price = Decimal::ZERO;

        let market_window = MarketWindow {
            event_id: event.id.clone().unwrap_or_default(),
            condition_id: market.condition_id.clone().unwrap_or_default(),
            asset: asset.as_str().to_string(),
            yes_token_id: token_ids.yes_token_id,
            no_token_id: token_ids.no_token_id,
            strike_price,
            window_start,
            window_end,
            discovered_at: Utc::now(),
        };

        Ok(Some(market_window))
    }

    /// Detect window duration from event tags (5M, 15M, 1H).
    /// Defaults to 15 minutes if no recognized tag is found.
    fn detect_window_duration(&self, event: &GammaEvent) -> chrono::Duration {
        if let Some(tags) = &event.tags {
            for tag in tags {
                if let Some(slug) = &tag.slug {
                    let slug_upper = slug.to_uppercase();
                    if slug_upper == "5M" || slug_upper.contains("5-MIN") || slug_upper.contains("5MIN") {
                        return chrono::Duration::minutes(5);
                    }
                    if slug_upper == "1H" || slug_upper.contains("1-HOUR") || slug_upper.contains("1HOUR") {
                        return chrono::Duration::minutes(60);
                    }
                    if slug_upper == "15M" || slug_upper.contains("15-MIN") || slug_upper.contains("15MIN") {
                        return chrono::Duration::minutes(15);
                    }
                }
            }
        }

        // Also check title for duration hints
        if let Some(title) = &event.title {
            let title_lower = title.to_lowercase();
            if title_lower.contains("5 min") || title_lower.contains("5-min") || title_lower.contains("5min") {
                return chrono::Duration::minutes(5);
            }
            if title_lower.contains("1 hour") || title_lower.contains("1-hour") || title_lower.contains("1hour") || title_lower.contains("hourly") {
                return chrono::Duration::minutes(60);
            }
        }

        // Default to 15 minutes
        chrono::Duration::minutes(15)
    }

    /// Detect which crypto asset a market title refers to.
    fn detect_asset(&self, title: &str) -> Result<Option<CryptoAsset>, DiscoveryError> {
        let title_lower = title.to_lowercase();

        if title_lower.contains("btc") || title_lower.contains("bitcoin") {
            return Ok(Some(CryptoAsset::Btc));
        }
        if title_lower.contains("eth") || title_lower.contains("ethereum") {
            return Ok(Some(CryptoAsset::Eth));
        }
        if title_lower.contains("sol") || title_lower.contains("solana") {
            return Ok(Some(CryptoAsset::Sol));
        }
        if title_lower.contains("xrp") || title_lower.contains("ripple") {
            return Ok(Some(CryptoAsset::Xrp));
        }

        Ok(None)
    }

    /// Parse token IDs from the market's clob_token_ids field.
    /// The API returns this as a JSON string: "[\"123\", \"456\"]"
    fn parse_token_ids(&self, market: &GammaMarket) -> Result<Option<TokenIds>, DiscoveryError> {
        let clob_tokens_str = match &market.clob_token_ids {
            Some(s) => s,
            None => return Ok(None),
        };

        // Parse the JSON string array
        let tokens: Vec<String> = match serde_json::from_str(clob_tokens_str) {
            Ok(t) => t,
            Err(e) => {
                debug!("Failed to parse clob_token_ids '{}': {}", clob_tokens_str, e);
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
    fn parse_datetime(&self, dt_str: &Option<String>) -> Result<Option<DateTime<Utc>>, DiscoveryError> {
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

    /// Store discovered markets to ClickHouse.
    async fn store_markets(&self, markets: &[MarketWindow]) -> Result<(), DiscoveryError> {
        self.db
            .insert_market_windows(markets)
            .await
            .map_err(DiscoveryError::ClickHouse)?;
        info!("Stored {} new markets to ClickHouse", markets.len());
        Ok(())
    }

    /// Get all currently known active markets.
    pub fn active_markets(&self) -> &HashSet<String> {
        &self.known_markets
    }

    /// Run discovery loop with periodic refresh.
    pub async fn run_loop(
        mut self,
        interval: Duration,
        mut shutdown: tokio::sync::broadcast::Receiver<()>,
    ) -> Result<(), DiscoveryError> {
        info!(
            "Starting discovery loop with {} second interval",
            interval.as_secs()
        );

        loop {
            match self.discover().await {
                Ok(count) => {
                    if count > 0 {
                        info!("Discovery found {} new markets", count);
                    } else {
                        debug!("Discovery found no new markets");
                    }
                }
                Err(e) => {
                    error!("Discovery error: {}", e);
                }
            }

            tokio::select! {
                _ = tokio::time::sleep(interval) => {}
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
        let assets = vec![
            CryptoAsset::Btc,
            CryptoAsset::Eth,
            CryptoAsset::Sol,
            CryptoAsset::Xrp,
        ];
        MarketDiscovery {
            http: Client::new(),
            db: Arc::new(ClickHouseClient::with_defaults()),
            assets: assets.clone(),
            known_markets: HashSet::new(),
            discovered_windows: Vec::new(),
            price_fetcher: PolyMarketDiscovery::new(PolyMarketDiscoveryConfig {
                assets,
                window_duration: WindowDuration::FifteenMin,
                ..Default::default()
            }),
        }
    }

    #[test]
    fn test_detect_asset() {
        let discovery = test_discovery();

        assert_eq!(
            discovery.detect_asset("will btc go up").unwrap(),
            Some(CryptoAsset::Btc)
        );
        assert_eq!(
            discovery.detect_asset("bitcoin 15 minute").unwrap(),
            Some(CryptoAsset::Btc)
        );
        assert_eq!(
            discovery.detect_asset("eth price prediction").unwrap(),
            Some(CryptoAsset::Eth)
        );
        assert_eq!(
            discovery.detect_asset("ethereum 15min").unwrap(),
            Some(CryptoAsset::Eth)
        );
        assert_eq!(
            discovery.detect_asset("sol up or down").unwrap(),
            Some(CryptoAsset::Sol)
        );
        assert_eq!(
            discovery.detect_asset("xrp price").unwrap(),
            Some(CryptoAsset::Xrp)
        );
        assert_eq!(
            discovery.detect_asset("random market").unwrap(),
            None
        );
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
        assert_eq!(
            discovery.parse_strike_price("no price here"),
            Decimal::ZERO
        );
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
}
