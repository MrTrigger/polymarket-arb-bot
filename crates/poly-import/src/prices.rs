//! Price history import from Polymarket CLOB API.
//!
//! Downloads historical price data for tokens and stores to ClickHouse.

use std::time::Duration;

use chrono::{DateTime, Utc};
use poly_common::{ClickHouseClient, PriceHistory};
use reqwest::Client;
use rust_decimal::Decimal;
use serde::Deserialize;
use thiserror::Error;
use tracing::{debug, info, warn};

/// Base URL for Polymarket CLOB API.
const CLOB_API_URL: &str = "https://clob.polymarket.com";

/// Errors that can occur during price history import.
#[derive(Debug, Error)]
pub enum PriceImportError {
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),

    #[error("ClickHouse error: {0}")]
    ClickHouse(#[from] poly_common::ClickHouseError),

    #[error("Invalid response: {0}")]
    InvalidResponse(String),

    #[error("Rate limited, retry after {0} seconds")]
    RateLimited(u64),
}

/// Response from the /prices-history endpoint.
#[derive(Debug, Deserialize)]
struct PricesHistoryResponse {
    history: Vec<PricePoint>,
}

/// A single price point from the API.
#[derive(Debug, Deserialize)]
struct PricePoint {
    /// Unix timestamp in seconds.
    t: i64,
    /// Price value (0.0 to 1.0).
    p: f64,
}

/// Configuration for the price history importer.
#[derive(Debug, Clone)]
pub struct PriceImportConfig {
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
}

impl Default for PriceImportConfig {
    fn default() -> Self {
        Self {
            requests_per_second: 5.0, // Conservative to avoid rate limits
            batch_size: 1000,
            timeout: Duration::from_secs(30),
            max_retries: 3,
            initial_backoff: Duration::from_secs(1),
        }
    }
}

/// Imports price history from Polymarket API to ClickHouse.
pub struct PriceImporter {
    http_client: Client,
    ch_client: ClickHouseClient,
    config: PriceImportConfig,
}

impl PriceImporter {
    /// Creates a new price importer.
    pub fn new(ch_client: ClickHouseClient, config: PriceImportConfig) -> Result<Self, PriceImportError> {
        let http_client = Client::builder()
            .timeout(config.timeout)
            .build()?;

        Ok(Self {
            http_client,
            ch_client,
            config,
        })
    }

    /// Fetches price history for a token within the given time range.
    ///
    /// # Arguments
    /// * `token_id` - The CLOB token ID to fetch history for
    /// * `start` - Start of the time range (inclusive)
    /// * `end` - End of the time range (inclusive)
    /// * `fidelity` - Resolution in minutes (optional, defaults to API default)
    pub async fn fetch_prices(
        &self,
        token_id: &str,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        fidelity: Option<u32>,
    ) -> Result<Vec<PriceHistory>, PriceImportError> {
        let start_ts = start.timestamp();
        let end_ts = end.timestamp();

        let mut url = format!(
            "{}/prices-history?market={}&startTs={}&endTs={}",
            CLOB_API_URL, token_id, start_ts, end_ts
        );

        if let Some(fid) = fidelity {
            url.push_str(&format!("&fidelity={}", fid));
        }

        debug!("Fetching price history: {}", url);

        let response = self.request_with_retry(&url).await?;

        let prices: Vec<PriceHistory> = response
            .history
            .into_iter()
            .map(|point| {
                let timestamp = DateTime::from_timestamp(point.t, 0)
                    .unwrap_or_else(Utc::now);

                // Convert f64 to Decimal (the API returns floats)
                let price = Decimal::try_from(point.p)
                    .unwrap_or(Decimal::ZERO);

                PriceHistory {
                    token_id: token_id.to_string(),
                    timestamp,
                    price,
                }
            })
            .collect();

        debug!("Fetched {} price points for token {}", prices.len(), token_id);

        Ok(prices)
    }

    /// Imports price history for a token and stores to ClickHouse.
    ///
    /// Handles pagination by fetching data in chunks if the range is large.
    pub async fn import_token(
        &self,
        token_id: &str,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        fidelity: Option<u32>,
    ) -> Result<usize, PriceImportError> {
        info!(
            "Importing price history for token {} from {} to {}",
            token_id, start, end
        );

        let prices = self.fetch_prices(token_id, start, end, fidelity).await?;
        let total_count = prices.len();

        if prices.is_empty() {
            info!("No price history found for token {}", token_id);
            return Ok(0);
        }

        // Batch insert to ClickHouse
        for chunk in prices.chunks(self.config.batch_size) {
            self.ch_client.insert_price_history(chunk).await?;
            debug!("Inserted {} price records", chunk.len());
        }

        info!(
            "Imported {} price points for token {}",
            total_count, token_id
        );

        // Rate limit delay
        let delay = Duration::from_secs_f64(1.0 / self.config.requests_per_second);
        tokio::time::sleep(delay).await;

        Ok(total_count)
    }

    /// Imports price history for multiple tokens.
    pub async fn import_tokens(
        &self,
        token_ids: &[String],
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        fidelity: Option<u32>,
    ) -> Result<ImportStats, PriceImportError> {
        let mut stats = ImportStats::default();

        for (idx, token_id) in token_ids.iter().enumerate() {
            info!(
                "Processing token {}/{}: {}",
                idx + 1,
                token_ids.len(),
                token_id
            );

            match self.import_token(token_id, start, end, fidelity).await {
                Ok(count) => {
                    stats.tokens_processed += 1;
                    stats.records_imported += count;
                }
                Err(e) => {
                    warn!("Failed to import token {}: {}", token_id, e);
                    stats.tokens_failed += 1;
                    stats.errors.push((token_id.clone(), e.to_string()));
                }
            }
        }

        Ok(stats)
    }

    /// Makes a request with retry and exponential backoff.
    async fn request_with_retry(&self, url: &str) -> Result<PricesHistoryResponse, PriceImportError> {
        let mut backoff = self.config.initial_backoff;

        for attempt in 0..=self.config.max_retries {
            match self.http_client.get(url).send().await {
                Ok(response) => {
                    if response.status().is_success() {
                        return response.json().await.map_err(PriceImportError::Http);
                    } else if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
                        // Rate limited
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
                            return Err(PriceImportError::RateLimited(retry_after));
                        }
                    } else if response.status() == reqwest::StatusCode::NOT_FOUND {
                        return Err(PriceImportError::InvalidResponse(
                            "Market not found".to_string(),
                        ));
                    } else {
                        let status = response.status();
                        let body = response.text().await.unwrap_or_default();
                        return Err(PriceImportError::InvalidResponse(format!(
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
                Err(e) => return Err(PriceImportError::Http(e)),
            }
        }

        unreachable!()
    }
}

/// Statistics from an import run.
#[derive(Debug, Default)]
pub struct ImportStats {
    pub tokens_processed: usize,
    pub tokens_failed: usize,
    pub records_imported: usize,
    pub errors: Vec<(String, String)>,
}

impl std::fmt::Display for ImportStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "Import Statistics:")?;
        writeln!(f, "  Tokens processed: {}", self.tokens_processed)?;
        writeln!(f, "  Tokens failed: {}", self.tokens_failed)?;
        writeln!(f, "  Records imported: {}", self.records_imported)?;
        if !self.errors.is_empty() {
            writeln!(f, "  Errors:")?;
            for (token, err) in &self.errors {
                writeln!(f, "    {}: {}", token, err)?;
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
        let config = PriceImportConfig::default();
        assert_eq!(config.requests_per_second, 5.0);
        assert_eq!(config.batch_size, 1000);
        assert_eq!(config.max_retries, 3);
    }

    #[test]
    fn test_price_point_parsing() {
        let json = r#"{"history": [{"t": 1697875200, "p": 0.45}]}"#;
        let response: PricesHistoryResponse = serde_json::from_str(json).unwrap();

        assert_eq!(response.history.len(), 1);
        assert_eq!(response.history[0].t, 1697875200);
        assert!((response.history[0].p - 0.45).abs() < f64::EPSILON);
    }

    #[test]
    fn test_empty_response_parsing() {
        let json = r#"{"history": []}"#;
        let response: PricesHistoryResponse = serde_json::from_str(json).unwrap();

        assert!(response.history.is_empty());
    }

    #[test]
    fn test_import_stats_display() {
        let stats = ImportStats {
            tokens_processed: 10,
            tokens_failed: 2,
            records_imported: 5000,
            errors: vec![("token1".to_string(), "error1".to_string())],
        };

        let output = format!("{}", stats);
        assert!(output.contains("Tokens processed: 10"));
        assert!(output.contains("Records imported: 5000"));
    }
}
