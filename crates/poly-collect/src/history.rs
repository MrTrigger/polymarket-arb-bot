//! Historical data collection from Binance and Polymarket APIs.
//!
//! Fetches historical price data for a specified date range.
//! Uses parallel fetching for maximum performance.
//!
//! - Binance: Spot prices via klines API
//! - Polymarket: Token prices via CLOB prices-history API

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use futures::future::join_all;
use poly_common::{CryptoAsset, MarketWindow, PriceHistory, SpotPrice, WindowDuration};
use reqwest::Client;
use rust_decimal::Decimal;
use serde::Deserialize;
use tokio::sync::Semaphore;
use tracing::{debug, info, warn};

use crate::data_writer::DataWriter;

// ============================================================================
// Constants
// ============================================================================

/// Binance klines API URL.
const BINANCE_KLINES_URL: &str = "https://api.binance.com/api/v3/klines";

/// Polymarket Gamma API URL (for market discovery).
const GAMMA_API_URL: &str = "https://gamma-api.polymarket.com";

/// Polymarket CLOB API URL (for price history).
const CLOB_API_URL: &str = "https://clob.polymarket.com";

/// Maximum klines per request (Binance limit is 1000).
const MAX_KLINES_PER_REQUEST: usize = 1000;

/// Rate limit delay between requests (per worker).
const RATE_LIMIT_DELAY: Duration = Duration::from_millis(50);

/// Maximum concurrent requests.
const MAX_CONCURRENT_REQUESTS: usize = 10;

/// Number of days per chunk for parallel fetching.
const DAYS_PER_CHUNK: i64 = 1;

/// Keywords to identify crypto up/down markets (from question field).
/// Format: "{Asset} Up or Down" e.g., "Bitcoin Up or Down", "BTC Up or Down"
const CRYPTO_UP_DOWN_PATTERNS: &[&str] = &[
    "Bitcoin Up or Down",
    "BTC Up or Down",
    "Ethereum Up or Down",
    "ETH Up or Down",
    "Solana Up or Down",
    "SOL Up or Down",
    "XRP Up or Down",
];

// ============================================================================
// Historical Collector
// ============================================================================

/// Historical data collector with parallel fetching.
pub struct HistoricalCollector {
    client: Client,
    writer: Arc<DataWriter>,
    semaphore: Arc<Semaphore>,
    timeframes: Vec<WindowDuration>,
}

impl HistoricalCollector {
    /// Creates a new historical data collector.
    pub fn new(writer: Arc<DataWriter>, timeframes: Vec<WindowDuration>) -> Result<Self> {
        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .context("Failed to create HTTP client")?;

        Ok(Self {
            client,
            writer,
            semaphore: Arc::new(Semaphore::new(MAX_CONCURRENT_REQUESTS)),
            timeframes,
        })
    }

    /// Collects historical data for the specified assets and date range.
    /// Fetches both Binance spot prices and Polymarket token prices in parallel.
    pub async fn collect(
        &self,
        assets: &[CryptoAsset],
        start_date: NaiveDate,
        end_date: NaiveDate,
    ) -> Result<CollectionStats> {
        info!(
            "Starting parallel historical collection: {} assets, {} to {}",
            assets.len(),
            start_date,
            end_date
        );

        let stats = Arc::new(CollectionStatsAtomic::default());

        // Run Binance and Polymarket fetches in parallel
        let binance_future = self.collect_binance(assets, start_date, end_date, Arc::clone(&stats));
        let polymarket_future = self.collect_polymarket(assets, start_date, end_date, Arc::clone(&stats));

        // Wait for both to complete
        let (binance_result, polymarket_result) = tokio::join!(binance_future, polymarket_future);

        if let Err(e) = binance_result {
            warn!("Binance collection had errors: {}", e);
        }
        if let Err(e) = polymarket_result {
            warn!("Polymarket collection had errors: {}", e);
        }

        let result = stats.to_stats();
        info!("Historical collection complete: {:?}", result);
        Ok(result)
    }

    /// Collect Binance spot prices for all assets in parallel.
    async fn collect_binance(
        &self,
        assets: &[CryptoAsset],
        start_date: NaiveDate,
        end_date: NaiveDate,
        stats: Arc<CollectionStatsAtomic>,
    ) -> Result<()> {
        info!("Starting Binance historical collection");

        let futures: Vec<_> = assets
            .iter()
            .map(|asset| {
                let client = self.client.clone();
                let writer = Arc::clone(&self.writer);
                let semaphore = Arc::clone(&self.semaphore);
                let stats = Arc::clone(&stats);
                let asset = *asset;

                async move {
                    info!("Starting Binance fetch for {}", asset);
                    match fetch_asset_parallel(
                        &client,
                        &writer,
                        &semaphore,
                        &asset,
                        start_date,
                        end_date,
                    )
                    .await
                    {
                        Ok(count) => {
                            stats.binance_records.fetch_add(count, Ordering::Relaxed);
                            stats.assets_completed.fetch_add(1, Ordering::Relaxed);
                            info!("Completed Binance {} with {} records", asset, count);
                        }
                        Err(e) => {
                            warn!("Failed to fetch Binance {} data: {}", asset, e);
                            stats.assets_failed.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
            })
            .collect();

        join_all(futures).await;
        Ok(())
    }

    /// Collect Polymarket price history for discovered markets.
    async fn collect_polymarket(
        &self,
        assets: &[CryptoAsset],
        start_date: NaiveDate,
        end_date: NaiveDate,
        stats: Arc<CollectionStatsAtomic>,
    ) -> Result<()> {
        info!("Starting Polymarket historical collection");

        let start_dt = date_to_utc_start(start_date);
        let end_dt = date_to_utc_end(end_date);

        // Discover closed markets in the date range
        let markets = match self.discover_historical_markets(assets, start_dt, end_dt).await {
            Ok(m) => m,
            Err(e) => {
                warn!("Failed to discover Polymarket markets: {}", e);
                return Ok(());
            }
        };

        if markets.is_empty() {
            info!("No Polymarket markets found for the date range");
            return Ok(());
        }

        info!("Found {} Polymarket markets to fetch", markets.len());

        // Write market windows
        if let Err(e) = self.writer.write_market_windows(&markets).await {
            warn!("Failed to write market windows: {}", e);
        }

        // Fetch price history for each market's YES and NO tokens together
        // IMPORTANT: Align YES and NO prices to the same timestamps so they're always paired.
        // This prevents orderbook staleness issues during backtest where one side updates before the other.
        let futures: Vec<_> = markets
            .iter()
            .map(|market| {
                let client = self.client.clone();
                let writer = Arc::clone(&self.writer);
                let semaphore = Arc::clone(&self.semaphore);
                let stats = Arc::clone(&stats);
                let yes_token_id = market.yes_token_id.clone();
                let no_token_id = market.no_token_id.clone();
                let event_id = market.event_id.clone();
                let window_start = market.window_start;
                let window_end = market.window_end;

                async move {
                    let _permit = semaphore.acquire().await.unwrap();

                    // Fetch both YES and NO prices within the market's trading window
                    let (yes_result, no_result) = tokio::join!(
                        fetch_polymarket_prices(&client, &yes_token_id, window_start, window_end),
                        fetch_polymarket_prices(&client, &no_token_id, window_start, window_end)
                    );

                    let yes_prices = yes_result.unwrap_or_default();
                    let no_prices = no_result.unwrap_or_default();

                    // Align prices by timestamp (truncate to second)
                    // Only save pairs where both YES and NO have data at the same timestamp
                    let aligned = align_yes_no_prices(&event_id, &yes_token_id, &no_token_id, yes_prices, no_prices);

                    if !aligned.is_empty() {
                        let count = aligned.len();
                        if let Err(e) = writer.write_aligned_prices(&aligned).await {
                            warn!("Failed to write aligned price history for event {}: {}", event_id, e);
                        } else {
                            stats.polymarket_records.fetch_add(count, Ordering::Relaxed);
                            debug!("Wrote {} aligned price pairs for event {}", count, event_id);
                        }
                    }

                    // Rate limiting
                    tokio::time::sleep(RATE_LIMIT_DELAY).await;
                }
            })
            .collect();

        join_all(futures).await;
        Ok(())
    }

    /// Discover closed markets from Gamma API for the given date range.
    /// Uses /markets endpoint and filters by "Up or Down" in question.
    async fn discover_historical_markets(
        &self,
        assets: &[CryptoAsset],
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> Result<Vec<MarketWindow>> {
        let mut all_markets = Vec::new();
        let mut offset = 0;
        let limit = 100;
        let mut consecutive_out_of_range = 0;

        info!("Fetching resolved Up/Down markets from {} to {}",
              start.format("%Y-%m-%d"), end.format("%Y-%m-%d"));

        // Paginate through closed markets (sorted by closedTime descending)
        loop {
            let url = format!(
                "{}/markets?closed=true&limit={}&offset={}&order=closedTime&ascending=false",
                GAMMA_API_URL, limit, offset
            );

            debug!("Fetching markets: {}", url);

            let response = self
                .client
                .get(&url)
                .send()
                .await
                .context("Failed to fetch Gamma markets")?;

            if !response.status().is_success() {
                anyhow::bail!("Gamma API error: {}", response.status());
            }

            let markets: Vec<GammaMarketResponse> = response.json().await
                .context("Failed to parse Gamma markets response")?;

            if markets.is_empty() {
                break;
            }

            let mut found_in_range = 0;
            let mut found_before_range = 0;

            // Filter for crypto up/down markets within date range
            for market in &markets {
                if let Some(parsed) = self.parse_market_response(market, assets) {
                    // Check if market is within our date range
                    if parsed.window_end >= start && parsed.window_end <= end {
                        all_markets.push(parsed);
                        found_in_range += 1;
                    } else if parsed.window_end < start {
                        // Market is before our range - we've gone too far back
                        found_before_range += 1;
                    }
                    // Markets after 'end' are skipped (too recent)
                }
            }

            info!("Fetched {} markets, {} in range, {} total matching",
                  offset + markets.len(), found_in_range, all_markets.len());

            // If all matching markets in this batch are before our range, stop
            if found_before_range > 0 && found_in_range == 0 {
                consecutive_out_of_range += 1;
                if consecutive_out_of_range >= 3 {
                    info!("Reached markets before date range, stopping pagination");
                    break;
                }
            } else {
                consecutive_out_of_range = 0;
            }

            if markets.len() < limit {
                break;
            }

            offset += limit;

            // Rate limiting
            tokio::time::sleep(Duration::from_millis(100)).await;

            // Safety limit - don't paginate forever (50k should cover ~2 months of markets)
            if offset >= 50000 {
                warn!("Hit pagination limit of 50000 markets");
                break;
            }
        }

        // Deduplicate markets by (asset, window_start, window_end)
        // The Gamma API can return multiple markets for the same time window
        // (e.g., from different queries or pagination). Keep only the first occurrence.
        let original_count = all_markets.len();
        let mut seen = std::collections::HashSet::new();
        all_markets.retain(|m| {
            let key = (m.asset.clone(), m.window_start, m.window_end);
            seen.insert(key)
        });
        if all_markets.len() < original_count {
            info!(
                "Deduplicated markets: {} -> {} (removed {} duplicates)",
                original_count,
                all_markets.len(),
                original_count - all_markets.len()
            );
        }

        info!("Found {} crypto Up/Down markets in date range", all_markets.len());

        // Fetch strike prices from Binance for markets that don't have them
        let markets_needing_strike: Vec<_> = all_markets
            .iter()
            .enumerate()
            .filter(|(_, m)| m.strike_price.is_zero())
            .map(|(i, m)| (i, m.asset.clone(), m.window_start))
            .collect();

        if !markets_needing_strike.is_empty() {
            info!(
                "Fetching strike prices from Binance for {} markets",
                markets_needing_strike.len()
            );

            let futures: Vec<_> = markets_needing_strike
                .into_iter()
                .map(|(idx, asset_str, window_start)| {
                    let client = self.client.clone();
                    let semaphore = Arc::clone(&self.semaphore);

                    async move {
                        let _permit = semaphore.acquire().await.unwrap();

                        let asset = match asset_str.as_str() {
                            "BTC" => CryptoAsset::Btc,
                            "ETH" => CryptoAsset::Eth,
                            "SOL" => CryptoAsset::Sol,
                            "XRP" => CryptoAsset::Xrp,
                            _ => return (idx, None),
                        };

                        let price = fetch_binance_spot_at_time(&client, asset, window_start).await;
                        tokio::time::sleep(RATE_LIMIT_DELAY).await;
                        (idx, price)
                    }
                })
                .collect();

            let results = join_all(futures).await;

            for (idx, price) in results {
                if let Some(p) = price {
                    debug!(
                        "Set strike for market {} ({}) to {}",
                        all_markets[idx].event_id, all_markets[idx].asset, p
                    );
                    all_markets[idx].strike_price = p;
                }
            }

            let filled = all_markets.iter().filter(|m| !m.strike_price.is_zero()).count();
            info!("Strike prices populated: {}/{} markets", filled, all_markets.len());
        }

        Ok(all_markets)
    }

    /// Parse a market response into a MarketWindow if it matches our criteria.
    fn parse_market_response(&self, market: &GammaMarketResponse, assets: &[CryptoAsset]) -> Option<MarketWindow> {
        let question = market.question.as_ref()?;

        // Check if it's a crypto up/down market
        let is_crypto_updown = CRYPTO_UP_DOWN_PATTERNS.iter()
            .any(|pattern| question.contains(pattern));
        if !is_crypto_updown {
            return None;
        }

        // Detect asset from question
        let asset = detect_asset_from_title(&question.to_lowercase())?;

        // Check if we're tracking this asset
        if !assets.contains(&asset) {
            return None;
        }

        // Get condition ID
        let condition_id = market.condition_id.as_ref()?.clone();

        // Parse token IDs from clobTokenIds (it's a JSON string)
        let clob_token_ids_str = market.clob_token_ids.as_ref()?;
        let clob_token_ids: Vec<String> = serde_json::from_str(clob_token_ids_str).ok()?;
        if clob_token_ids.len() < 2 {
            return None;
        }

        // Detect timeframe from slug pattern
        // The Gamma API doesn't provide explicit window duration, so we infer from patterns
        let slug = market.slug.as_deref().unwrap_or("");
        let question_lower = question.to_lowercase();

        // Skip markets with explicit longer durations (2h, 4h, etc.)
        if slug.contains("4h") || slug.contains("4-hour") || slug.contains("2h") || slug.contains("2-hour")
            || question_lower.contains("4:00pm-8:00pm") || question_lower.contains("4:00am-8:00am") {
            debug!("Skipping multi-hour market: {} (slug: {})", question, slug);
            return None;
        }

        let detected_duration = if slug.contains("5m") || slug.contains("-5-") || slug.contains("5-min") {
            WindowDuration::FiveMin
        } else if slug.contains("1h") || slug.contains("1-hour") || slug.contains("60-min") {
            WindowDuration::OneHour
        } else {
            // Default to configured timeframe for Up/Down markets
            // Most "X Up or Down - Date, Time ET" markets are 15-minute windows
            *self.timeframes.first().unwrap_or(&WindowDuration::FifteenMin)
        };

        // Check if this timeframe is in our configured list
        if !self.timeframes.contains(&detected_duration) {
            return None;
        }

        // Parse timestamps
        let end_date = market.end_date.as_ref()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.with_timezone(&Utc))?;

        // Calculate start based on detected duration
        let duration_mins = detected_duration.minutes() as i64;
        let start_date = end_date - chrono::Duration::minutes(duration_mins);

        // Parse strike price from question
        let strike_price = parse_strike_price(question).unwrap_or(Decimal::ZERO);

        Some(MarketWindow {
            event_id: market.id.clone().unwrap_or_default(),
            condition_id,
            asset: asset.as_str().to_string(),
            yes_token_id: clob_token_ids[0].clone(),
            no_token_id: clob_token_ids[1].clone(),
            strike_price,
            window_start: start_date,
            window_end: end_date,
            discovered_at: Utc::now(),
        })
    }
}

// ============================================================================
// Binance Functions
// ============================================================================

/// Fetch the spot price from Binance at a specific timestamp.
/// Returns the open price of the 1-minute candle containing the timestamp.
/// This is used to determine strike prices for "Up or Down" markets.
async fn fetch_binance_spot_at_time(
    client: &Client,
    asset: CryptoAsset,
    at_time: DateTime<Utc>,
) -> Option<Decimal> {
    let symbol = asset.binance_symbol().to_uppercase();
    let start_time = at_time.timestamp_millis();
    let end_time = start_time + 60_000; // +1 minute

    let url = format!(
        "{}?symbol={}&interval=1m&startTime={}&endTime={}&limit=1",
        BINANCE_KLINES_URL, symbol, start_time, end_time
    );

    let response = match client.get(&url).send().await {
        Ok(r) => r,
        Err(e) => {
            warn!("Failed to fetch Binance price for {} at {}: {}", asset, at_time, e);
            return None;
        }
    };

    if !response.status().is_success() {
        warn!(
            "Binance API error for {} at {}: HTTP {}",
            asset,
            at_time,
            response.status()
        );
        return None;
    }

    let data: Vec<Vec<serde_json::Value>> = match response.json().await {
        Ok(d) => d,
        Err(e) => {
            warn!("Failed to parse Binance response for {} at {}: {}", asset, at_time, e);
            return None;
        }
    };

    // Extract open price from first kline (index 1 is open price)
    data.first()
        .and_then(|arr| arr.get(1))
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<Decimal>().ok())
}

/// Fetch historical data for a single asset using parallel chunk fetching.
async fn fetch_asset_parallel(
    client: &Client,
    writer: &Arc<DataWriter>,
    semaphore: &Arc<Semaphore>,
    asset: &CryptoAsset,
    start_date: NaiveDate,
    end_date: NaiveDate,
) -> Result<usize> {
    let symbol = asset.binance_symbol().to_uppercase();
    let total_records = Arc::new(AtomicUsize::new(0));

    // Break date range into chunks for parallel fetching
    let mut chunks = Vec::new();
    let mut current = start_date;
    while current < end_date {
        let chunk_end = (current + chrono::Duration::days(DAYS_PER_CHUNK)).min(end_date);
        chunks.push((current, chunk_end));
        current = chunk_end;
    }

    debug!("Fetching {} in {} parallel chunks", asset, chunks.len());

    let futures: Vec<_> = chunks
        .into_iter()
        .map(|(chunk_start, chunk_end)| {
            let client = client.clone();
            let writer = Arc::clone(writer);
            let semaphore = Arc::clone(semaphore);
            let symbol = symbol.clone();
            let total_records = Arc::clone(&total_records);
            let asset = *asset;

            async move {
                let _permit = semaphore.acquire().await.unwrap();

                let start_dt = date_to_utc_start(chunk_start);
                let end_dt = date_to_utc_end(chunk_end);

                match fetch_binance_klines_chunk(&client, &writer, &asset, &symbol, start_dt, end_dt).await {
                    Ok(count) => {
                        total_records.fetch_add(count, Ordering::Relaxed);
                    }
                    Err(e) => {
                        warn!("Failed chunk {}-{} for {}: {}", chunk_start, chunk_end, asset, e);
                    }
                }
            }
        })
        .collect();

    join_all(futures).await;
    Ok(total_records.load(Ordering::Relaxed))
}

/// Fetch a single chunk of klines from Binance.
async fn fetch_binance_klines_chunk(
    client: &Client,
    writer: &Arc<DataWriter>,
    asset: &CryptoAsset,
    symbol: &str,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
) -> Result<usize> {
    let mut total_records = 0;
    let mut current_start = start.timestamp_millis();
    let end_ms = end.timestamp_millis();

    while current_start < end_ms {
        let klines = fetch_klines_batch(client, symbol, current_start, end_ms).await?;

        if klines.is_empty() {
            break;
        }

        let prices: Vec<SpotPrice> = klines
            .iter()
            .filter_map(|k| kline_to_spot_price(asset, k))
            .collect();

        let batch_count = prices.len();
        writer.write_spot_prices(&prices).await?;
        total_records += batch_count;

        if let Some(last) = klines.last() {
            current_start = last.close_time + 1;
        } else {
            break;
        }

        tokio::time::sleep(RATE_LIMIT_DELAY).await;
    }

    Ok(total_records)
}

/// Fetches a single batch of klines from Binance API.
async fn fetch_klines_batch(
    client: &Client,
    symbol: &str,
    start_time: i64,
    end_time: i64,
) -> Result<Vec<BinanceKline>> {
    let url = format!(
        "{}?symbol={}&interval=1m&startTime={}&endTime={}&limit={}",
        BINANCE_KLINES_URL, symbol, start_time, end_time, MAX_KLINES_PER_REQUEST
    );

    let response = client.get(&url).send().await.context("Failed to fetch klines")?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("Binance API error {}: {}", status, body);
    }

    let data: Vec<Vec<serde_json::Value>> = response.json().await.context("Failed to parse klines")?;

    let klines: Vec<BinanceKline> = data
        .into_iter()
        .filter_map(|arr| parse_kline_array(&arr))
        .collect();

    Ok(klines)
}

// ============================================================================
// Polymarket Functions
// ============================================================================

/// Align YES and NO prices by timestamp.
/// Only returns price pairs where both tokens have data at the same second.
/// This ensures the backtest always has synchronized YES+NO prices that sum to ~1.0.
fn align_yes_no_prices(
    event_id: &str,
    yes_token_id: &str,
    no_token_id: &str,
    yes_prices: Vec<PriceHistory>,
    no_prices: Vec<PriceHistory>,
) -> Vec<poly_common::AlignedPricePair> {
    use std::collections::HashMap;

    // Index NO prices by timestamp (truncated to second)
    let no_by_timestamp: HashMap<i64, Decimal> = no_prices
        .iter()
        .map(|p| (p.timestamp.timestamp(), p.price))
        .collect();

    // For each YES price, find matching NO price at the same second
    let mut aligned = Vec::new();
    for yes in yes_prices {
        let ts_sec = yes.timestamp.timestamp();
        if let Some(&no_price) = no_by_timestamp.get(&ts_sec) {
            let arb = yes.price + no_price;
            aligned.push(poly_common::AlignedPricePair {
                event_id: event_id.to_string(),
                yes_token_id: yes_token_id.to_string(),
                no_token_id: no_token_id.to_string(),
                timestamp: yes.timestamp,
                yes_price: yes.price,
                no_price,
                arb,
            });
        }
    }

    aligned
}

/// Maximum hours per batch for Polymarket price history API.
const POLYMARKET_HOURS_PER_BATCH: i64 = 24;

/// Fetch price history for a Polymarket token in batches.
/// The API rejects requests with intervals that are too long, so we batch by day.
async fn fetch_polymarket_prices(
    client: &Client,
    token_id: &str,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
) -> Result<Vec<PriceHistory>> {
    let mut all_prices = Vec::new();
    let mut batch_start = start;

    while batch_start < end {
        let batch_end = (batch_start + chrono::Duration::hours(POLYMARKET_HOURS_PER_BATCH)).min(end);

        match fetch_polymarket_prices_batch(client, token_id, batch_start, batch_end).await {
            Ok(prices) => {
                all_prices.extend(prices);
            }
            Err(e) => {
                // Log but continue - some batches may fail for old/inactive tokens
                debug!("Batch {}-{} failed for {}: {}", batch_start, batch_end, token_id, e);
            }
        }

        batch_start = batch_end;

        // Small delay between batches to avoid rate limiting
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    Ok(all_prices)
}

/// Fetch a single batch of price history from Polymarket.
async fn fetch_polymarket_prices_batch(
    client: &Client,
    token_id: &str,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
) -> Result<Vec<PriceHistory>> {
    let url = format!(
        "{}/prices-history?market={}&startTs={}&endTs={}&fidelity=1",
        CLOB_API_URL,
        token_id,
        start.timestamp(),
        end.timestamp()
    );

    debug!("Fetching Polymarket prices: {}", url);

    let response = client.get(&url).send().await.context("Failed to fetch price history")?;

    if !response.status().is_success() {
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(Vec::new()); // Token not found, return empty
        }
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("CLOB API error {}: {}", status, body);
    }

    let data: PricesHistoryResponse = response.json().await.context("Failed to parse price history")?;

    let prices: Vec<PriceHistory> = data
        .history
        .into_iter()
        .filter_map(|point| {
            let timestamp = DateTime::from_timestamp(point.t, 0)?;
            let price = Decimal::try_from(point.p).ok()?;
            Some(PriceHistory {
                token_id: token_id.to_string(),
                timestamp,
                price,
            })
        })
        .collect();

    Ok(prices)
}

// ============================================================================
// Helper Types and Functions
// ============================================================================

/// Gamma API /markets endpoint response.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GammaMarketResponse {
    id: Option<String>,
    question: Option<String>,
    slug: Option<String>,
    condition_id: Option<String>,
    start_date: Option<String>,
    end_date: Option<String>,
    /// This is a JSON string containing an array, e.g. "[\"token1\", \"token2\"]"
    clob_token_ids: Option<String>,
}

/// CLOB prices-history response.
#[derive(Debug, Deserialize)]
struct PricesHistoryResponse {
    history: Vec<PricePoint>,
}

#[derive(Debug, Deserialize)]
struct PricePoint {
    t: i64,
    p: f64,
}

/// Binance kline data.
#[derive(Debug)]
struct BinanceKline {
    #[allow(dead_code)]
    open_time: i64,
    #[allow(dead_code)]
    open: Decimal,
    #[allow(dead_code)]
    high: Decimal,
    #[allow(dead_code)]
    low: Decimal,
    close: Decimal,
    volume: Decimal,
    close_time: i64,
}

fn parse_kline_array(arr: &[serde_json::Value]) -> Option<BinanceKline> {
    if arr.len() < 11 {
        return None;
    }
    Some(BinanceKline {
        open_time: arr[0].as_i64()?,
        open: arr[1].as_str()?.parse().ok()?,
        high: arr[2].as_str()?.parse().ok()?,
        low: arr[3].as_str()?.parse().ok()?,
        close: arr[4].as_str()?.parse().ok()?,
        volume: arr[5].as_str()?.parse().ok()?,
        close_time: arr[6].as_i64()?,
    })
}

fn kline_to_spot_price(asset: &CryptoAsset, kline: &BinanceKline) -> Option<SpotPrice> {
    let timestamp = Utc.timestamp_millis_opt(kline.close_time).single()?;
    Some(SpotPrice {
        asset: asset.as_str().to_string(),
        price: kline.close,
        timestamp,
        quantity: kline.volume,
    })
}

fn date_to_utc_start(date: NaiveDate) -> DateTime<Utc> {
    date.and_hms_opt(0, 0, 0)
        .map(|dt| Utc.from_utc_datetime(&dt))
        .unwrap_or_else(Utc::now)
}

fn date_to_utc_end(date: NaiveDate) -> DateTime<Utc> {
    date.and_hms_opt(23, 59, 59)
        .map(|dt| Utc.from_utc_datetime(&dt))
        .unwrap_or_else(Utc::now)
}

fn detect_asset_from_title(title: &str) -> Option<CryptoAsset> {
    let title = title.to_lowercase();
    if title.contains("bitcoin") || title.contains("btc") {
        Some(CryptoAsset::Btc)
    } else if title.contains("ethereum") || title.contains("eth") {
        Some(CryptoAsset::Eth)
    } else if title.contains("solana") || title.contains("sol") {
        Some(CryptoAsset::Sol)
    } else if title.contains("xrp") || title.contains("ripple") {
        Some(CryptoAsset::Xrp)
    } else {
        None
    }
}

fn parse_strike_price(text: &str) -> Option<Decimal> {
    // Look for patterns like "$100,000" or "100000"
    let re = regex::Regex::new(r"\$?([\d,]+(?:\.\d+)?)").ok()?;
    for cap in re.captures_iter(text) {
        if let Some(m) = cap.get(1) {
            let num_str = m.as_str().replace(',', "");
            if let Ok(price) = num_str.parse::<Decimal>() {
                // Only consider reasonable strike prices (> $100)
                if price > Decimal::from(100) {
                    return Some(price);
                }
            }
        }
    }
    None
}

/// Atomic statistics for thread-safe accumulation.
#[derive(Default)]
struct CollectionStatsAtomic {
    assets_completed: AtomicUsize,
    assets_failed: AtomicUsize,
    binance_records: AtomicUsize,
    polymarket_records: AtomicUsize,
}

impl CollectionStatsAtomic {
    fn to_stats(&self) -> CollectionStats {
        CollectionStats {
            assets_completed: self.assets_completed.load(Ordering::Relaxed),
            assets_failed: self.assets_failed.load(Ordering::Relaxed),
            binance_records: self.binance_records.load(Ordering::Relaxed),
            polymarket_records: self.polymarket_records.load(Ordering::Relaxed),
        }
    }
}

/// Statistics for historical collection.
#[derive(Debug, Default)]
pub struct CollectionStats {
    pub assets_completed: usize,
    pub assets_failed: usize,
    pub binance_records: usize,
    pub polymarket_records: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_date_to_utc() {
        let date = NaiveDate::from_ymd_opt(2025, 1, 15).unwrap();
        let start = date_to_utc_start(date);
        let end = date_to_utc_end(date);

        assert_eq!(start.format("%Y-%m-%d %H:%M:%S").to_string(), "2025-01-15 00:00:00");
        assert_eq!(end.format("%Y-%m-%d %H:%M:%S").to_string(), "2025-01-15 23:59:59");
    }

    #[test]
    fn test_detect_asset() {
        assert_eq!(detect_asset_from_title("Bitcoin 15min up or down"), Some(CryptoAsset::Btc));
        assert_eq!(detect_asset_from_title("ETH price prediction"), Some(CryptoAsset::Eth));
        assert_eq!(detect_asset_from_title("Solana market"), Some(CryptoAsset::Sol));
        assert_eq!(detect_asset_from_title("XRP ripple"), Some(CryptoAsset::Xrp));
        assert_eq!(detect_asset_from_title("Unknown market"), None);
    }

    #[test]
    fn test_parse_strike_price() {
        assert_eq!(parse_strike_price("above $100,000"), Some(Decimal::from(100000)));
        assert_eq!(parse_strike_price("at 95000.50"), Some(Decimal::try_from(95000.50).unwrap()));
        assert_eq!(parse_strike_price("no price here"), None);
    }
}
