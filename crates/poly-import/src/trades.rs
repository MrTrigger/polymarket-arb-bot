//! Trade history import from Polymarket CLOB API.
//!
//! Downloads historical trade data for tokens and stores to ClickHouse.
//! Requires L2 authentication for the /data/trades endpoint.

use std::time::Duration;

use chrono::{DateTime, TimeZone, Utc};
use poly_common::{ClickHouseClient, TradeHistory};
use reqwest::Client;
use rust_decimal::Decimal;
use serde::Deserialize;
use thiserror::Error;
use tracing::{debug, info, warn};

/// Base URL for Polymarket CLOB API.
const CLOB_API_URL: &str = "https://clob.polymarket.com";

/// Errors that can occur during trade history import.
#[derive(Debug, Error)]
pub enum TradeImportError {
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),

    #[error("ClickHouse error: {0}")]
    ClickHouse(#[from] poly_common::ClickHouseError),

    #[error("Invalid response: {0}")]
    InvalidResponse(String),

    #[error("Rate limited, retry after {0} seconds")]
    RateLimited(u64),

    #[error("Authentication required")]
    AuthRequired,

    #[error("Decimal parse error: {0}")]
    DecimalParse(#[from] rust_decimal::Error),
}

/// Response from the /data/trades endpoint.
#[derive(Debug, Deserialize)]
struct TradesResponse {
    #[serde(default)]
    trades: Vec<ApiTrade>,
    /// Cursor for pagination (if present).
    #[serde(default)]
    next_cursor: Option<String>,
}

/// A single trade from the API.
#[derive(Debug, Deserialize)]
struct ApiTrade {
    /// Trade identifier.
    id: String,
    /// Market/condition ID (not stored but part of API response).
    #[serde(default)]
    #[allow(dead_code)]
    market: String,
    /// Token/asset ID.
    asset_id: String,
    /// Trade side (BUY/SELL).
    side: String,
    /// Trade size as string.
    size: String,
    /// Trade price as string.
    price: String,
    /// Execution timestamp (Unix seconds or ISO string).
    match_time: String,
    /// Trade status (not stored but part of API response).
    #[serde(default)]
    #[allow(dead_code)]
    status: String,
}

/// Configuration for the trade history importer.
#[derive(Debug, Clone)]
pub struct TradeImportConfig {
    /// Maximum requests per second to avoid rate limiting.
    pub requests_per_second: f64,
    /// Batch size for ClickHouse inserts.
    pub batch_size: usize,
    /// Request timeout.
    pub timeout: Duration,
    /// Number of retries on failure.
    pub max_retries: u32,
    /// Initial backoff duration for retries.
    pub initial_backoff: Duration,
    /// Maximum trades per API request.
    pub page_size: usize,
}

impl Default for TradeImportConfig {
    fn default() -> Self {
        Self {
            requests_per_second: 5.0, // Conservative to avoid rate limits
            batch_size: 1000,
            timeout: Duration::from_secs(30),
            max_retries: 3,
            initial_backoff: Duration::from_secs(1),
            page_size: 500, // Reasonable page size for pagination
        }
    }
}

/// L2 authentication headers for Polymarket CLOB API.
#[derive(Debug, Clone)]
pub struct L2Auth {
    /// POLY_ADDRESS header value.
    pub address: String,
    /// POLY_SIGNATURE header value.
    pub signature: String,
    /// POLY_TIMESTAMP header value.
    pub timestamp: String,
    /// POLY_NONCE header value (optional).
    pub nonce: Option<String>,
}

/// Imports trade history from Polymarket API to ClickHouse.
pub struct TradeImporter {
    http_client: Client,
    ch_client: ClickHouseClient,
    config: TradeImportConfig,
    auth: Option<L2Auth>,
}

impl TradeImporter {
    /// Creates a new trade importer without authentication.
    /// Note: Most trade queries require authentication.
    pub fn new(ch_client: ClickHouseClient, config: TradeImportConfig) -> Result<Self, TradeImportError> {
        let http_client = Client::builder()
            .timeout(config.timeout)
            .build()?;

        Ok(Self {
            http_client,
            ch_client,
            config,
            auth: None,
        })
    }

    /// Creates a new trade importer with L2 authentication.
    pub fn with_auth(
        ch_client: ClickHouseClient,
        config: TradeImportConfig,
        auth: L2Auth,
    ) -> Result<Self, TradeImportError> {
        let http_client = Client::builder()
            .timeout(config.timeout)
            .build()?;

        Ok(Self {
            http_client,
            ch_client,
            config,
            auth: Some(auth),
        })
    }

    /// Sets L2 authentication credentials.
    pub fn set_auth(&mut self, auth: L2Auth) {
        self.auth = Some(auth);
    }

    /// Fetches trades for a market within the given time range.
    ///
    /// # Arguments
    /// * `market` - The market/condition ID to fetch trades for
    /// * `start` - Start of the time range (inclusive)
    /// * `end` - End of the time range (inclusive)
    /// * `maker_address` - Optional: filter by maker address
    /// * `taker_address` - Optional: filter by taker address
    pub async fn fetch_trades(
        &self,
        market: &str,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        maker_address: Option<&str>,
        taker_address: Option<&str>,
    ) -> Result<Vec<TradeHistory>, TradeImportError> {
        let mut all_trades = Vec::new();
        let mut cursor: Option<String> = None;

        loop {
            let trades_page = self
                .fetch_trades_page(market, start, end, maker_address, taker_address, cursor.as_deref())
                .await?;

            let page_count = trades_page.trades.len();
            debug!(
                "Fetched {} trades for market {} (cursor: {:?})",
                page_count, market, cursor
            );

            // Convert API trades to TradeHistory
            for api_trade in trades_page.trades {
                match self.convert_trade(&api_trade) {
                    Ok(trade) => all_trades.push(trade),
                    Err(e) => {
                        warn!("Failed to convert trade {}: {}", api_trade.id, e);
                    }
                }
            }

            // Check for more pages
            if let Some(next) = trades_page.next_cursor {
                if !next.is_empty() && page_count > 0 {
                    cursor = Some(next);
                    // Rate limit delay between pages
                    let delay = Duration::from_secs_f64(1.0 / self.config.requests_per_second);
                    tokio::time::sleep(delay).await;
                } else {
                    break;
                }
            } else {
                break;
            }
        }

        Ok(all_trades)
    }

    /// Fetches a single page of trades.
    async fn fetch_trades_page(
        &self,
        market: &str,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        maker_address: Option<&str>,
        taker_address: Option<&str>,
        cursor: Option<&str>,
    ) -> Result<TradesResponse, TradeImportError> {
        let mut url = format!(
            "{}/data/trades?market={}&after={}&before={}",
            CLOB_API_URL,
            market,
            start.timestamp(),
            end.timestamp()
        );

        if let Some(maker) = maker_address {
            url.push_str(&format!("&maker={}", maker));
        }
        if let Some(taker) = taker_address {
            url.push_str(&format!("&taker={}", taker));
        }
        if let Some(c) = cursor {
            url.push_str(&format!("&cursor={}", c));
        }

        debug!("Fetching trades: {}", url);

        self.request_with_retry(&url).await
    }

    /// Converts an API trade to a TradeHistory record.
    fn convert_trade(&self, api_trade: &ApiTrade) -> Result<TradeHistory, TradeImportError> {
        // Parse timestamp - can be Unix seconds or ISO string
        let timestamp = self.parse_timestamp(&api_trade.match_time)?;

        // Parse price and size as Decimal
        let price = api_trade
            .price
            .parse::<Decimal>()
            .map_err(|_| TradeImportError::InvalidResponse(format!(
                "Invalid price: {}",
                api_trade.price
            )))?;

        let size = api_trade
            .size
            .parse::<Decimal>()
            .map_err(|_| TradeImportError::InvalidResponse(format!(
                "Invalid size: {}",
                api_trade.size
            )))?;

        Ok(TradeHistory {
            token_id: api_trade.asset_id.clone(),
            timestamp,
            side: api_trade.side.clone(),
            price,
            size,
            trade_id: api_trade.id.clone(),
        })
    }

    /// Parses a timestamp that could be Unix seconds or ISO string.
    fn parse_timestamp(&self, ts: &str) -> Result<DateTime<Utc>, TradeImportError> {
        // Try Unix timestamp first
        if let Ok(secs) = ts.parse::<i64>() {
            return Utc
                .timestamp_opt(secs, 0)
                .single()
                .ok_or_else(|| TradeImportError::InvalidResponse(format!(
                    "Invalid Unix timestamp: {}",
                    ts
                )));
        }

        // Try ISO 8601 format
        DateTime::parse_from_rfc3339(ts)
            .map(|dt| dt.with_timezone(&Utc))
            .map_err(|_| TradeImportError::InvalidResponse(format!(
                "Invalid timestamp format: {}",
                ts
            )))
    }

    /// Imports trades for a market and stores to ClickHouse.
    pub async fn import_market(
        &self,
        market: &str,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> Result<usize, TradeImportError> {
        info!(
            "Importing trade history for market {} from {} to {}",
            market, start, end
        );

        let trades = self.fetch_trades(market, start, end, None, None).await?;
        let total_count = trades.len();

        if trades.is_empty() {
            info!("No trade history found for market {}", market);
            return Ok(0);
        }

        // Batch insert to ClickHouse
        for chunk in trades.chunks(self.config.batch_size) {
            self.ch_client.insert_trade_history(chunk).await?;
            debug!("Inserted {} trade records", chunk.len());
        }

        info!(
            "Imported {} trades for market {}",
            total_count, market
        );

        // Rate limit delay
        let delay = Duration::from_secs_f64(1.0 / self.config.requests_per_second);
        tokio::time::sleep(delay).await;

        Ok(total_count)
    }

    /// Imports trade history for multiple markets.
    pub async fn import_markets(
        &self,
        markets: &[String],
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> Result<TradeImportStats, TradeImportError> {
        let mut stats = TradeImportStats::default();

        for (idx, market) in markets.iter().enumerate() {
            info!(
                "Processing market {}/{}: {}",
                idx + 1,
                markets.len(),
                market
            );

            match self.import_market(market, start, end).await {
                Ok(count) => {
                    stats.markets_processed += 1;
                    stats.trades_imported += count;
                }
                Err(e) => {
                    warn!("Failed to import market {}: {}", market, e);
                    stats.markets_failed += 1;
                    stats.errors.push((market.clone(), e.to_string()));
                }
            }
        }

        Ok(stats)
    }

    /// Makes a request with retry and exponential backoff.
    async fn request_with_retry(&self, url: &str) -> Result<TradesResponse, TradeImportError> {
        let mut backoff = self.config.initial_backoff;

        for attempt in 0..=self.config.max_retries {
            let mut request = self.http_client.get(url);

            // Add L2 auth headers if available
            if let Some(ref auth) = self.auth {
                request = request
                    .header("POLY_ADDRESS", &auth.address)
                    .header("POLY_SIGNATURE", &auth.signature)
                    .header("POLY_TIMESTAMP", &auth.timestamp);

                if let Some(ref nonce) = auth.nonce {
                    request = request.header("POLY_NONCE", nonce);
                }
            }

            match request.send().await {
                Ok(response) => {
                    if response.status().is_success() {
                        // Handle empty response
                        let text = response.text().await.map_err(TradeImportError::Http)?;
                        if text.is_empty() || text == "[]" {
                            return Ok(TradesResponse {
                                trades: vec![],
                                next_cursor: None,
                            });
                        }

                        // Try parsing as array first (some endpoints return raw array)
                        if let Ok(trades) = serde_json::from_str::<Vec<ApiTrade>>(&text) {
                            return Ok(TradesResponse {
                                trades,
                                next_cursor: None,
                            });
                        }

                        // Try parsing as object with trades field
                        return serde_json::from_str(&text)
                            .map_err(|e| TradeImportError::InvalidResponse(format!(
                                "JSON parse error: {} (body: {})",
                                e,
                                &text[..text.len().min(200)]
                            )));
                    } else if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
                        let retry_after = response
                            .headers()
                            .get("retry-after")
                            .and_then(|v| v.to_str().ok())
                            .and_then(|v| v.parse().ok())
                            .unwrap_or(60);

                        if attempt < self.config.max_retries {
                            warn!(
                                "Rate limited, waiting {} seconds (attempt {}/{})",
                                retry_after,
                                attempt + 1,
                                self.config.max_retries
                            );
                            tokio::time::sleep(Duration::from_secs(retry_after)).await;
                            continue;
                        } else {
                            return Err(TradeImportError::RateLimited(retry_after));
                        }
                    } else if response.status() == reqwest::StatusCode::UNAUTHORIZED
                        || response.status() == reqwest::StatusCode::FORBIDDEN
                    {
                        return Err(TradeImportError::AuthRequired);
                    } else if response.status() == reqwest::StatusCode::NOT_FOUND {
                        return Err(TradeImportError::InvalidResponse(
                            "Market not found".to_string(),
                        ));
                    } else {
                        let status = response.status();
                        let body = response.text().await.unwrap_or_default();
                        return Err(TradeImportError::InvalidResponse(format!(
                            "HTTP {}: {}",
                            status, body
                        )));
                    }
                }
                Err(e) if attempt < self.config.max_retries => {
                    warn!(
                        "Request failed: {} (attempt {}/{}), retrying in {:?}",
                        e,
                        attempt + 1,
                        self.config.max_retries,
                        backoff
                    );
                    tokio::time::sleep(backoff).await;
                    backoff *= 2; // Exponential backoff
                }
                Err(e) => return Err(TradeImportError::Http(e)),
            }
        }

        unreachable!()
    }
}

/// Statistics from a trade import run.
#[derive(Debug, Default)]
pub struct TradeImportStats {
    pub markets_processed: usize,
    pub markets_failed: usize,
    pub trades_imported: usize,
    pub errors: Vec<(String, String)>,
}

impl std::fmt::Display for TradeImportStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "Trade Import Statistics:")?;
        writeln!(f, "  Markets processed: {}", self.markets_processed)?;
        writeln!(f, "  Markets failed: {}", self.markets_failed)?;
        writeln!(f, "  Trades imported: {}", self.trades_imported)?;
        if !self.errors.is_empty() {
            writeln!(f, "  Errors:")?;
            for (market, err) in &self.errors {
                writeln!(f, "    {}: {}", market, err)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = TradeImportConfig::default();
        assert_eq!(config.requests_per_second, 5.0);
        assert_eq!(config.batch_size, 1000);
        assert_eq!(config.max_retries, 3);
        assert_eq!(config.page_size, 500);
    }

    #[test]
    fn test_trade_response_parsing() {
        let json = r#"{
            "trades": [
                {
                    "id": "trade123",
                    "market": "0xabc123",
                    "asset_id": "token456",
                    "side": "BUY",
                    "size": "100.5",
                    "price": "0.45",
                    "match_time": "1697875200",
                    "status": "CONFIRMED"
                }
            ]
        }"#;
        let response: TradesResponse = serde_json::from_str(json).unwrap();

        assert_eq!(response.trades.len(), 1);
        assert_eq!(response.trades[0].id, "trade123");
        assert_eq!(response.trades[0].side, "BUY");
        assert_eq!(response.trades[0].price, "0.45");
    }

    #[test]
    fn test_empty_response_parsing() {
        let json = r#"{"trades": []}"#;
        let response: TradesResponse = serde_json::from_str(json).unwrap();
        assert!(response.trades.is_empty());
    }

    #[test]
    fn test_trade_array_parsing() {
        // Some endpoints return raw array
        let json = r#"[
            {
                "id": "trade123",
                "market": "0xabc123",
                "asset_id": "token456",
                "side": "SELL",
                "size": "50",
                "price": "0.55",
                "match_time": "1697875200"
            }
        ]"#;
        let trades: Vec<ApiTrade> = serde_json::from_str(json).unwrap();
        assert_eq!(trades.len(), 1);
        assert_eq!(trades[0].side, "SELL");
    }

    #[test]
    fn test_timestamp_parsing_unix() {
        let config = TradeImportConfig::default();
        let ch_client = poly_common::ClickHouseClient::with_defaults();
        let importer = TradeImporter::new(ch_client, config).unwrap();

        let ts = importer.parse_timestamp("1697875200").unwrap();
        assert_eq!(ts.timestamp(), 1697875200);
    }

    #[test]
    fn test_timestamp_parsing_iso() {
        let config = TradeImportConfig::default();
        let ch_client = poly_common::ClickHouseClient::with_defaults();
        let importer = TradeImporter::new(ch_client, config).unwrap();

        let ts = importer.parse_timestamp("2023-10-21T08:00:00Z").unwrap();
        assert_eq!(ts.timestamp(), 1697875200);
    }

    #[test]
    fn test_import_stats_display() {
        let stats = TradeImportStats {
            markets_processed: 10,
            markets_failed: 2,
            trades_imported: 5000,
            errors: vec![("market1".to_string(), "error1".to_string())],
        };

        let output = format!("{}", stats);
        assert!(output.contains("Markets processed: 10"));
        assert!(output.contains("Trades imported: 5000"));
    }

    #[test]
    fn test_l2_auth_creation() {
        let auth = L2Auth {
            address: "0x123".to_string(),
            signature: "sig".to_string(),
            timestamp: "12345".to_string(),
            nonce: Some("nonce1".to_string()),
        };

        assert_eq!(auth.address, "0x123");
        assert_eq!(auth.nonce, Some("nonce1".to_string()));
    }
}
