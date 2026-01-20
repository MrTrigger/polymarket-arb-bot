//! Market discovery via Polymarket Gamma API.
//!
//! Discovers active 15-minute up/down markets for crypto assets.

use std::collections::HashSet;
use std::time::Duration;

use chrono::{DateTime, Utc};
use poly_common::{CryptoAsset, WindowDuration};
use reqwest::Client;
use rust_decimal::Decimal;
use serde::Deserialize;
use thiserror::Error;
use tracing::{debug, error, info, warn};

use crate::types::{GammaEvent, GammaMarket, TokenIds};

/// Gamma API base URL.
const GAMMA_API_URL: &str = "https://gamma-api.polymarket.com";

/// Keywords to identify up/down markets in titles.
const UP_DOWN_KEYWORDS: &[&str] = &["up or down", "higher or lower", "above or below"];

/// Binance REST API base URL.
const BINANCE_API_URL: &str = "https://api.binance.com";

/// Ethereum mainnet RPC URL (free public endpoint).
/// Used for querying Chainlink price feeds.
const ETH_RPC_URL: &str = "https://rpc.ankr.com/eth";

/// Chainlink price feed contract addresses on Ethereum mainnet.
/// These are the same feeds Polymarket uses for market resolution.
mod chainlink_feeds {
    /// BTC/USD price feed
    pub const BTC_USD: &str = "0xF4030086522a5bEEa4988F8cA5B36dbC97BeE88c";
    /// ETH/USD price feed
    pub const ETH_USD: &str = "0x5f4eC3Df9cbd43714FE2740f5E3616155c5b8419";
    /// SOL/USD price feed
    pub const SOL_USD: &str = "0x4ffC43a60e009B551865A93d232E33Fce9f01507";
    /// XRP/USD price feed (Note: may not be available on mainnet)
    pub const XRP_USD: &str = "0xCed2660c6Dd1Ffd856A5A82C67f3482d88C50b12";
}

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

/// Binance kline (candlestick) response.
/// Each kline is an array: [open_time, open, high, low, close, volume, ...]
#[derive(Debug, Deserialize)]
struct BinanceKline(Vec<serde_json::Value>);

impl BinanceKline {
    /// Get the open price from the kline.
    fn open_price(&self) -> Option<Decimal> {
        // Index 1 is the open price (as string)
        self.0.get(1)?.as_str()?.parse().ok()
    }
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
    /// Window duration (15min or 1h).
    pub window_duration: WindowDuration,
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
    /// Window duration to look for (15min or 1h).
    pub window_duration: WindowDuration,
    /// HTTP request timeout.
    pub request_timeout: Duration,
    /// Discovery polling interval.
    pub poll_interval: Duration,
}

impl Default for DiscoveryConfig {
    fn default() -> Self {
        Self {
            assets: vec![CryptoAsset::Btc, CryptoAsset::Eth, CryptoAsset::Sol],
            window_duration: WindowDuration::OneHour, // Default to 1h since 15min not available
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

    /// Fetch historical price from Chainlink oracle at a specific time.
    ///
    /// This queries the same Chainlink price feeds that Polymarket uses for
    /// market resolution, ensuring our strike price matches theirs exactly.
    ///
    /// Searches backwards through Chainlink rounds to find the price that was
    /// active at the target timestamp.
    pub async fn fetch_chainlink_price(
        &self,
        asset: CryptoAsset,
        at_time: DateTime<Utc>,
    ) -> Result<Option<Decimal>, DiscoveryError> {
        // Get the Chainlink feed address for this asset
        let feed_address = match asset {
            CryptoAsset::Btc => chainlink_feeds::BTC_USD,
            CryptoAsset::Eth => chainlink_feeds::ETH_USD,
            CryptoAsset::Sol => chainlink_feeds::SOL_USD,
            CryptoAsset::Xrp => chainlink_feeds::XRP_USD,
        };

        let target_timestamp = at_time.timestamp() as u64;
        info!(
            "Fetching Chainlink historical price for {} at {} (unix={})",
            asset, at_time, target_timestamp
        );

        // Function selectors (first 4 bytes of keccak256 hash of function signature)
        // latestRoundData() = 0xfeaf968c
        // getRoundData(uint80) = 0x9a6fc8f5
        // decimals() = 0x313ce567
        const LATEST_ROUND_DATA: &str = "0xfeaf968c";
        const GET_ROUND_DATA: &str = "0x9a6fc8f5";
        const DECIMALS: &str = "0x313ce567";

        // Helper to make eth_call and parse response
        let make_call = |data: String| {
            let http = self.http.clone();
            let feed = feed_address.to_string();
            async move {
                let payload = serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": "eth_call",
                    "params": [{"to": feed, "data": data}, "latest"],
                    "id": 1
                });
                let resp = http.post(ETH_RPC_URL).json(&payload).send().await.ok()?;
                let json: serde_json::Value = resp.json().await.ok()?;
                json["result"].as_str().map(|s| s.trim_start_matches("0x").to_string())
            }
        };

        // Get decimals
        let decimals_hex = make_call(DECIMALS.to_string()).await.unwrap_or_else(|| "8".to_string());
        let decimals = u8::from_str_radix(&decimals_hex, 16).unwrap_or(8);

        // Get latest round to find current roundId
        let latest_result = match make_call(LATEST_ROUND_DATA.to_string()).await {
            Some(r) if r.len() >= 320 => r,
            _ => {
                warn!("Failed to get latest Chainlink round");
                return Ok(None);
            }
        };

        // Parse roundId from latest (first 32 bytes = 64 hex chars)
        let round_id_hex = &latest_result[0..64];
        let current_round_id = u128::from_str_radix(round_id_hex, 16).unwrap_or(0);

        // Parse latest round's updatedAt to check if we need to search backwards
        let latest_updated_at_hex = &latest_result[192..256];
        let latest_updated_at = u64::from_str_radix(latest_updated_at_hex, 16).unwrap_or(0);

        // If latest round is before or at target time, use it directly
        if latest_updated_at <= target_timestamp {
            return self.parse_chainlink_round(&latest_result, decimals, target_timestamp, asset);
        }

        // Search backwards through rounds to find the one active at target_timestamp
        // Chainlink roundId format: phaseId (16 bits) << 64 | aggregatorRoundId (64 bits)
        let phase_id = current_round_id >> 64;
        let mut aggregator_round = current_round_id & ((1u128 << 64) - 1);

        const MAX_SEARCH_ROUNDS: u32 = 100; // Limit search to avoid excessive RPC calls

        for i in 0..MAX_SEARCH_ROUNDS {
            if aggregator_round == 0 {
                warn!("Reached beginning of Chainlink phase, cannot find historical price");
                break;
            }

            aggregator_round -= 1;
            let prev_round_id = (phase_id << 64) | aggregator_round;

            // Encode getRoundData(uint80 _roundId) call
            // Function selector + roundId padded to 32 bytes
            let call_data = format!("{}{:064x}", GET_ROUND_DATA, prev_round_id);

            let round_result = match make_call(call_data).await {
                Some(r) if r.len() >= 320 => r,
                _ => {
                    debug!("Round {} doesn't exist or failed", prev_round_id);
                    continue;
                }
            };

            // Parse updatedAt from this round
            let updated_at_hex = &round_result[192..256];
            let updated_at = u64::from_str_radix(updated_at_hex, 16).unwrap_or(0);

            if updated_at == 0 {
                continue; // Invalid round
            }

            // Found a round that was updated before or at our target time
            if updated_at <= target_timestamp {
                info!(
                    "Found Chainlink round {} after searching {} rounds (updated_at={}, target={})",
                    prev_round_id, i + 1, updated_at, target_timestamp
                );
                return self.parse_chainlink_round(&round_result, decimals, target_timestamp, asset);
            }
        }

        warn!(
            "Could not find Chainlink round for timestamp {} after {} rounds",
            target_timestamp, MAX_SEARCH_ROUNDS
        );
        Ok(None)
    }

    /// Parse a Chainlink round result and convert to Decimal price.
    fn parse_chainlink_round(
        &self,
        result_hex: &str,
        decimals: u8,
        target_timestamp: u64,
        asset: CryptoAsset,
    ) -> Result<Option<Decimal>, DiscoveryError> {
        // Parse the ABI-encoded response (5 x 32 bytes = 160 bytes = 320 hex chars)
        // roundId (uint80), answer (int256), startedAt (uint256), updatedAt (uint256), answeredInRound (uint80)
        if result_hex.len() < 320 {
            warn!("Chainlink response too short: {} chars", result_hex.len());
            return Ok(None);
        }

        // Extract answer (bytes 32-64, which is chars 64-128)
        let answer_hex = &result_hex[64..128];
        let answer = i128::from_str_radix(answer_hex, 16).unwrap_or(0);

        if answer <= 0 {
            warn!("Chainlink returned invalid price: {}", answer);
            return Ok(None);
        }

        // Extract updatedAt (bytes 96-128, chars 192-256)
        let updated_at_hex = &result_hex[192..256];
        let updated_at = u64::from_str_radix(updated_at_hex, 16).unwrap_or(0);

        // Convert from Chainlink's decimal format (usually 8 decimals)
        let divisor = 10i128.pow(decimals as u32);
        let price = Decimal::from(answer) / Decimal::from(divisor);

        let time_diff = (updated_at as i64) - (target_timestamp as i64);

        info!(
            "Chainlink historical price for {} at target {}: ${} (round_time={}, diff={}s)",
            asset, target_timestamp, price, updated_at, time_diff
        );

        Ok(Some(price))
    }

    /// Fetch historical spot price from Binance at a specific time.
    ///
    /// This is used as a fallback when Chainlink query fails.
    ///
    /// Returns the open price of the 1-minute candle that contains the given timestamp.
    pub async fn fetch_historical_spot_price(
        &self,
        asset: CryptoAsset,
        at_time: DateTime<Utc>,
    ) -> Result<Option<Decimal>, DiscoveryError> {
        let symbol = asset.binance_symbol().to_uppercase();
        let start_time = at_time.timestamp_millis();
        // Get the 1-minute candle that contains this timestamp
        let end_time = start_time + 60_000; // +1 minute

        let url = format!(
            "{}/api/v3/klines?symbol={}&interval=1m&startTime={}&endTime={}&limit=1",
            BINANCE_API_URL, symbol, start_time, end_time
        );

        info!(
            "Fetching Binance historical price for {} at {} (timestamp_ms={})",
            asset, at_time, start_time
        );
        debug!("Binance klines URL: {}", url);

        let response = self.http.get(&url).send().await?;

        if !response.status().is_success() {
            warn!(
                "Failed to fetch historical price for {}: HTTP {}",
                asset,
                response.status()
            );
            return Ok(None);
        }

        let klines: Vec<BinanceKline> = response.json().await?;

        if let Some(kline) = klines.first() {
            // Use open price - this is the first trade price in the 1-minute candle
            // that started at the requested timestamp
            let price = kline.open_price();
            if let Some(p) = price {
                info!(
                    "Binance historical price for {} at {}: ${}",
                    asset, at_time, p
                );
            }
            Ok(price)
        } else {
            warn!(
                "No Binance kline data returned for {} at {} (start_time={})",
                asset, at_time, start_time
            );
            Ok(None)
        }
    }

    /// Fetch recent spot prices from Binance for ATR warmup.
    ///
    /// Returns the last `minutes` worth of 1-minute close prices for the given asset.
    /// Used to warm up ATR trackers before trading starts.
    ///
    /// # Arguments
    /// * `asset` - The crypto asset to fetch prices for
    /// * `minutes` - Number of minutes of history to fetch (default 10)
    ///
    /// # Returns
    /// Vec of prices in chronological order (oldest first)
    pub async fn fetch_recent_prices(
        &self,
        asset: CryptoAsset,
        minutes: usize,
    ) -> Result<Vec<Decimal>, DiscoveryError> {
        let symbol = asset.binance_symbol().to_uppercase();
        let now = Utc::now();
        let start_time = (now - chrono::Duration::minutes(minutes as i64)).timestamp_millis();
        let end_time = now.timestamp_millis();

        let url = format!(
            "{}/api/v3/klines?symbol={}&interval=1m&startTime={}&endTime={}&limit={}",
            BINANCE_API_URL, symbol, start_time, end_time, minutes
        );

        debug!(
            "Fetching recent prices for {} ({} minutes): {}",
            asset, minutes, url
        );

        let response = self.http.get(&url).send().await?;

        if !response.status().is_success() {
            warn!(
                "Failed to fetch recent prices for {}: HTTP {}",
                asset,
                response.status()
            );
            return Ok(Vec::new());
        }

        let klines: Vec<BinanceKline> = response.json().await?;

        // Extract close prices (index 4 in kline array)
        let prices: Vec<Decimal> = klines
            .iter()
            .filter_map(|k| k.0.get(4)?.as_str()?.parse().ok())
            .collect();

        info!(
            "Fetched {} recent prices for {} for ATR warmup",
            prices.len(),
            asset
        );

        Ok(prices)
    }

    /// Discover active 15-minute markets.
    /// Returns newly discovered markets (not seen before).
    ///
    /// For "Up or Down" markets that are already in progress, this will
    /// fetch the historical spot price from Binance to determine the correct
    /// strike price.
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
            if let Some(mut market) = self.parse_crypto_market(&event)? {
                // For "Up or Down" markets with no strike in title:
                // If the market is already in progress, fetch historical price.
                // Try Chainlink first (same oracle Polymarket uses), fall back to Binance.
                if market.strike_price.is_zero() && market.is_active() {
                    // Try Chainlink first - this is the same oracle Polymarket uses
                    let chainlink_price = self
                        .fetch_chainlink_price(market.asset, market.window_start)
                        .await
                        .ok()
                        .flatten();

                    if let Some(price) = chainlink_price {
                        info!(
                            "Setting strike for {} from Chainlink at {}: ${}",
                            market.event_id, market.window_start, price
                        );
                        market.strike_price = price;
                    } else {
                        // Fall back to Binance
                        warn!("Chainlink query failed, falling back to Binance for strike price");
                        if let Ok(Some(historical_price)) = self
                            .fetch_historical_spot_price(market.asset, market.window_start)
                            .await
                        {
                            info!(
                                "Setting strike for {} from Binance at {}: ${} (fallback)",
                                market.event_id, market.window_start, historical_price
                            );
                            market.strike_price = historical_price;
                        } else {
                            warn!(
                                "Could not fetch historical price for {}, strike will be set from first spot price",
                                market.event_id
                            );
                        }
                    }
                }

                info!(
                    "Discovered new market: {} {} strike={} (ends {})",
                    market.asset, market.event_id, market.strike_price, market.window_end
                );
                new_markets.push(market.clone());

                // Only mark as known if we have a valid strike price.
                // Markets with strike=0 will be re-processed on next poll
                // until they become active and we can fetch historical price.
                // Also mark as known if expired to avoid retrying forever.
                if !market.strike_price.is_zero() || market.is_expired() {
                    self.known_markets.insert(event_id);
                }
            }
        }

        Ok(new_markets)
    }

    /// Discover all active markets (including previously seen).
    ///
    /// For "Up or Down" markets that are already in progress, this will
    /// fetch the historical price from Chainlink (with Binance fallback).
    pub async fn discover_all(&mut self) -> Result<Vec<DiscoveredMarket>, DiscoveryError> {
        let events = self.fetch_active_events().await?;
        let mut markets = Vec::new();

        for event in events {
            if let Some(mut market) = self.parse_crypto_market(&event)? {
                // For "Up or Down" markets with no strike in title:
                // If the market is already in progress, fetch historical price
                if market.strike_price.is_zero() && market.is_active() {
                    // Try Chainlink first
                    let chainlink_price = self
                        .fetch_chainlink_price(market.asset, market.window_start)
                        .await
                        .ok()
                        .flatten();

                    if let Some(price) = chainlink_price {
                        debug!(
                            "Setting strike price for {} from Chainlink: ${}",
                            market.event_id, price
                        );
                        market.strike_price = price;
                    } else if let Ok(Some(historical_price)) = self
                        .fetch_historical_spot_price(market.asset, market.window_start)
                        .await
                    {
                        debug!(
                            "Setting strike price for {} from Binance (fallback): ${}",
                            market.event_id, historical_price
                        );
                        market.strike_price = historical_price;
                    }
                }

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
        // Use tag_slug for 5M/15M markets since they're hidden from general listings
        let url = if let Some(tag) = self.config.window_duration.tag_slug() {
            format!(
                "{}/events?tag_slug={}&closed=false&limit=100",
                GAMMA_API_URL, tag
            )
        } else {
            format!(
                "{}/events?active=true&closed=false&limit=100",
                GAMMA_API_URL
            )
        };

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

    /// Parse a Gamma event into a DiscoveredMarket if it matches our configured window duration.
    fn parse_crypto_market(
        &self,
        event: &GammaEvent,
    ) -> Result<Option<DiscoveredMarket>, DiscoveryError> {
        let title = match &event.title {
            Some(t) => t.to_lowercase(),
            None => return Ok(None),
        };

        // Check if this market matches our configured window duration
        // First check title keywords, then slug patterns as fallback
        let keywords = self.config.window_duration.keywords();
        let slug_patterns = self.config.window_duration.slug_patterns();

        let matches_title = keywords.iter().any(|kw| title.contains(kw));
        let matches_slug = event.slug.as_ref().is_some_and(|slug| {
            let slug_lower = slug.to_lowercase();
            slug_patterns.iter().any(|p| slug_lower.contains(p))
        });

        if !matches_title && !matches_slug {
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

        // Calculate window_start based on configured duration
        let window_start = window_end - self.config.window_duration.as_duration();

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
            window_duration: self.config.window_duration,
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
    ///
    /// For "Up or Down" markets (relative markets), the title doesn't contain
    /// a strike price - the strike is the spot price at window open.
    /// Returns 0 for such markets, which signals that strike should be
    /// set from spot price when the window opens.
    fn parse_strike_price(&self, title: &str) -> Decimal {
        // Look for dollar amounts - only patterns that clearly indicate prices
        let re_patterns = [
            // $100,000 or $3,500.50 (dollar sign required)
            r"\$([0-9,]+(?:\.[0-9]+)?)",
            // 3500 USD/USDT/USDC (currency suffix REQUIRED, not optional)
            // This prevents matching random numbers like "14" from "January 14"
            r"(?i)(\d+(?:,\d{3})*(?:\.\d+)?)\s*(?:usd|usdt|usdc)",
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

        // Default to zero if we can't parse.
        // For "Up or Down" markets, strike will be set from spot price at window open.
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

        // Fixed strike markets
        assert_eq!(
            discovery.parse_strike_price("Will BTC be above $100,000 at 12:15 UTC?"),
            Decimal::from(100000)
        );
        assert_eq!(
            discovery.parse_strike_price("ETH above $3,500.50"),
            Decimal::new(350050, 2)
        );

        // No price in title
        assert_eq!(discovery.parse_strike_price("no price here"), Decimal::ZERO);

        // "Up or Down" markets - should return 0, NOT the date number
        // This is the critical test - "14" from "January 14" should NOT be parsed as a price
        assert_eq!(
            discovery.parse_strike_price("Bitcoin Up or Down - January 14, 5:00PM-5:15PM ET"),
            Decimal::ZERO
        );

        // Numbers with explicit currency suffix should work
        assert_eq!(
            discovery.parse_strike_price("ETH at 3500 USD"),
            Decimal::from(3500)
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
    fn test_window_duration_keywords() {
        // Test 15-minute market detection
        let title = "btc 15 minute up or down";
        let fifteen_min_keywords = WindowDuration::FifteenMin.keywords();
        assert!(fifteen_min_keywords.iter().any(|kw| title.contains(kw)));
        assert!(UP_DOWN_KEYWORDS.iter().any(|kw| title.contains(kw)));

        // Test 1-hour market detection via slug patterns
        let one_hour_patterns = WindowDuration::OneHour.slug_patterns();
        let slug = "bitcoin-up-or-down-january-14-9am-et";
        assert!(one_hour_patterns.iter().any(|p| slug.contains(p)));

        // Non-matching title
        let title2 = "btc weekly prediction";
        assert!(!fifteen_min_keywords.iter().any(|kw| title2.contains(kw)));
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
            window_duration: WindowDuration::FifteenMin,
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
