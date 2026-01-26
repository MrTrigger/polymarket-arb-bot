//! CSV-based replay data source for backtesting.
//!
//! Loads historical data from CSV files and replays events in chronological
//! order for backtesting trading strategies.

use std::collections::{BinaryHeap, HashMap};
use std::cmp::Ordering;
use std::path::Path;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::Deserialize;
use tracing::{debug, info, warn};

use poly_common::types::CryptoAsset;

use super::{
    BookSnapshotEvent, BookSnapshotPairEvent, DataSource, DataSourceError, MarketEvent,
    SpotPriceEvent, WindowCloseEvent, WindowOpenEvent,
};
use crate::types::PriceLevel;

/// Configuration for the CSV replay data source.
#[derive(Debug, Clone)]
pub struct CsvReplayConfig {
    /// Directory containing the CSV files.
    pub data_dir: String,
    /// Start of replay period (filter).
    pub start_time: Option<DateTime<Utc>>,
    /// End of replay period (filter).
    pub end_time: Option<DateTime<Utc>>,
    /// Assets to filter (empty = all).
    pub assets: Vec<CryptoAsset>,
    /// Replay speed multiplier (1.0 = real-time, 0.0 = max speed).
    pub speed: f64,
}

impl Default for CsvReplayConfig {
    fn default() -> Self {
        Self {
            data_dir: "data".to_string(),
            start_time: None,
            end_time: None,
            assets: vec![
                CryptoAsset::Btc,
                CryptoAsset::Eth,
                CryptoAsset::Sol,
            ],
            speed: 0.0, // Max speed by default for backtesting
        }
    }
}

/// A timestamped event for the priority queue.
struct TimestampedEvent {
    timestamp: DateTime<Utc>,
    event: MarketEvent,
}

impl Eq for TimestampedEvent {}

impl PartialEq for TimestampedEvent {
    fn eq(&self, other: &Self) -> bool {
        self.timestamp == other.timestamp
    }
}

impl Ord for TimestampedEvent {
    fn cmp(&self, other: &Self) -> Ordering {
        // Reverse ordering for min-heap behavior (earliest first)
        other.timestamp.cmp(&self.timestamp)
    }
}

impl PartialOrd for TimestampedEvent {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// CSV row for spot prices.
#[derive(Debug, Clone, Deserialize)]
struct SpotPriceRow {
    asset: String,
    #[serde(with = "rust_decimal::serde::str")]
    price: Decimal,
    timestamp: DateTime<Utc>,
    #[serde(with = "rust_decimal::serde::str")]
    quantity: Decimal,
}

/// CSV row for market windows.
#[derive(Debug, Clone, Deserialize)]
struct MarketWindowRow {
    event_id: String,
    condition_id: String,
    asset: String,
    yes_token_id: String,
    no_token_id: String,
    #[serde(with = "rust_decimal::serde::str")]
    strike_price: Decimal,
    window_start: DateTime<Utc>,
    window_end: DateTime<Utc>,
    #[allow(dead_code)]
    discovered_at: DateTime<Utc>,
}

/// CSV row for price history (legacy format).
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
struct PriceHistoryRow {
    token_id: String,
    timestamp: DateTime<Utc>,
    #[serde(with = "rust_decimal::serde::str")]
    price: Decimal,
}

/// CSV row for aligned price pairs (new format with arb sanity check).
#[derive(Debug, Clone, Deserialize)]
struct AlignedPriceRow {
    event_id: String,
    yes_token_id: String,
    no_token_id: String,
    timestamp: DateTime<Utc>,
    #[serde(with = "rust_decimal::serde::str")]
    yes_price: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    no_price: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    #[allow(dead_code)]
    arb: Decimal,
}

/// CSV row for orderbook snapshots (live collected format).
#[derive(Debug, Clone, Deserialize)]
struct OrderBookSnapshotRow {
    token_id: String,
    event_id: String,
    timestamp: DateTime<Utc>,
    #[serde(with = "rust_decimal::serde::str")]
    best_bid: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    best_bid_size: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    best_ask: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    best_ask_size: Decimal,
    #[allow(dead_code)]
    spread_bps: i32,
    #[serde(with = "rust_decimal::serde::str")]
    bid_depth: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    ask_depth: Decimal,
}

/// Mapping of event_id to (yes_token_id, no_token_id)
type EventTokenMap = HashMap<String, (String, String)>;

/// CSV-based replay data source.
pub struct CsvReplayDataSource {
    config: CsvReplayConfig,
    /// Priority queue for merging events by timestamp.
    event_queue: BinaryHeap<TimestampedEvent>,
    /// Current replay time.
    current_time: Option<DateTime<Utc>>,
    /// Whether initial data has been loaded.
    data_loaded: bool,
    /// Whether replay is complete.
    is_exhausted: bool,
}

impl CsvReplayDataSource {
    /// Creates a new CSV replay data source.
    pub fn new(config: CsvReplayConfig) -> Self {
        Self {
            config,
            event_queue: BinaryHeap::new(),
            current_time: None,
            data_loaded: false,
            is_exhausted: false,
        }
    }

    /// Load all historical data into the event queue.
    fn load_data(&mut self) -> Result<(), DataSourceError> {
        let data_dir_str = self.config.data_dir.clone();
        let data_dir = Path::new(&data_dir_str);

        if !data_dir.exists() {
            return Err(DataSourceError::Connection(format!(
                "Data directory not found: {}",
                data_dir_str
            )));
        }

        info!("Loading CSV replay data from {}", data_dir_str);

        // Load market windows first (creates window open/close events)
        let event_tokens = self.load_market_windows(data_dir)?;

        // Load spot prices
        self.load_spot_prices(data_dir)?;

        // Try to load orderbook snapshots first (live collected, full book data)
        // then fall back to aligned prices (historical, bid-only prices)
        let snapshots_loaded = self.load_orderbook_snapshots(data_dir, &event_tokens)?;
        if !snapshots_loaded {
            // Fall back to aligned prices (historical format)
            self.load_aligned_prices(data_dir, &event_tokens)?;
        }

        info!("Loaded {} events for replay", self.event_queue.len());
        self.data_loaded = true;

        Ok(())
    }

    /// Load market windows and return event -> token mapping.
    fn load_market_windows(&mut self, data_dir: &Path) -> Result<EventTokenMap, DataSourceError> {
        let path = data_dir.join("polymarket_market_windows.csv");
        if !path.exists() {
            warn!("Market windows file not found: {:?}", path);
            return Ok(HashMap::new());
        }

        let mut reader = csv::Reader::from_path(&path)
            .map_err(|e| DataSourceError::Parse(format!("Failed to open {:?}: {}", path, e)))?;

        let mut event_tokens: EventTokenMap = HashMap::new();
        let mut window_count = 0;

        for result in reader.deserialize() {
            let row: MarketWindowRow = result
                .map_err(|e| DataSourceError::Parse(format!("CSV parse error: {}", e)))?;

            // Filter by asset if specified
            let asset = parse_asset(&row.asset);
            if let Some(a) = asset
                && !self.config.assets.is_empty()
                && !self.config.assets.contains(&a)
            {
                continue;
            }

            // Filter by time if specified
            if let Some(start) = self.config.start_time
                && row.window_end < start
            {
                continue;
            }
            if let Some(end) = self.config.end_time
                && row.window_start > end
            {
                continue;
            }

            // Skip markets that were already expired when discovered (no live data for them)
            if row.discovered_at > row.window_end {
                continue;
            }

            // Build event -> token mapping for validating aligned prices
            event_tokens.insert(row.event_id.clone(), (row.yes_token_id.clone(), row.no_token_id.clone()));

            // Create window open event
            // Use window_start (not discovered_at) so window opens at the correct time
            if let Some(asset) = asset {
                let open_event = MarketEvent::WindowOpen(WindowOpenEvent {
                    event_id: row.event_id.clone(),
                    condition_id: row.condition_id.clone(),
                    asset,
                    yes_token_id: row.yes_token_id,
                    no_token_id: row.no_token_id,
                    strike_price: row.strike_price,
                    window_start: row.window_start,
                    window_end: row.window_end,
                    timestamp: row.window_start, // Use window_start as event timestamp
                });

                self.event_queue.push(TimestampedEvent {
                    timestamp: row.window_start, // Queue at window_start time
                    event: open_event,
                });

                // Create window close event
                let close_event = MarketEvent::WindowClose(WindowCloseEvent {
                    event_id: row.event_id,
                    outcome: None,
                    final_price: None,
                    timestamp: row.window_end,
                });

                self.event_queue.push(TimestampedEvent {
                    timestamp: row.window_end,
                    event: close_event,
                });

                window_count += 1;
            }
        }

        debug!("Loaded {} market windows", window_count);
        Ok(event_tokens)
    }

    /// Load spot prices.
    fn load_spot_prices(&mut self, data_dir: &Path) -> Result<(), DataSourceError> {
        let path = data_dir.join("binance_spot_prices.csv");
        if !path.exists() {
            warn!("Spot prices file not found: {:?}", path);
            return Ok(());
        }

        let mut reader = csv::Reader::from_path(&path)
            .map_err(|e| DataSourceError::Parse(format!("Failed to open {:?}: {}", path, e)))?;

        let mut count = 0;

        for result in reader.deserialize() {
            let row: SpotPriceRow = result
                .map_err(|e| DataSourceError::Parse(format!("CSV parse error: {}", e)))?;

            // Filter by asset
            let asset = match parse_asset(&row.asset) {
                Some(a) => a,
                None => continue,
            };

            if !self.config.assets.is_empty() && !self.config.assets.contains(&asset) {
                continue;
            }

            // Filter by time
            if let Some(start) = self.config.start_time
                && row.timestamp < start
            {
                continue;
            }
            if let Some(end) = self.config.end_time
                && row.timestamp > end
            {
                continue;
            }

            let event = MarketEvent::SpotPrice(SpotPriceEvent {
                asset,
                price: row.price,
                quantity: row.quantity,
                timestamp: row.timestamp,
            });

            self.event_queue.push(TimestampedEvent {
                timestamp: row.timestamp,
                event,
            });

            count += 1;
        }

        debug!("Loaded {} spot prices", count);
        Ok(())
    }

    /// Load aligned price pairs and convert to paired book snapshot events.
    /// Each aligned row has YES and NO prices at the same timestamp,
    /// and we emit a single BookSnapshotPair event to ensure both orderbooks
    /// are updated atomically (no staleness issues from event ordering).
    fn load_aligned_prices(
        &mut self,
        data_dir: &Path,
        event_tokens: &EventTokenMap,
    ) -> Result<(), DataSourceError> {
        let path = data_dir.join("polymarket_aligned_prices.csv");
        if !path.exists() {
            warn!("Aligned prices file not found: {:?}", path);
            return Ok(());
        }

        let mut reader = csv::Reader::from_path(&path)
            .map_err(|e| DataSourceError::Parse(format!("Failed to open {:?}: {}", path, e)))?;

        let mut count = 0;

        for result in reader.deserialize() {
            let row: AlignedPriceRow = result
                .map_err(|e| DataSourceError::Parse(format!("CSV parse error: {}", e)))?;

            // Validate event_id exists in our tracked markets
            if !event_tokens.contains_key(&row.event_id) {
                continue;
            }

            // Filter by config time range
            if let Some(start) = self.config.start_time
                && row.timestamp < start
            {
                continue;
            }
            if let Some(end) = self.config.end_time
                && row.timestamp > end
            {
                continue;
            }

            // Convert prices to book snapshots with small spread
            let spread = Decimal::new(1, 2); // 0.01 spread
            let half_spread = spread / Decimal::TWO;
            let default_size = Decimal::new(1000, 0);

            // Create YES book snapshot
            let yes_bid = (row.yes_price - half_spread).max(Decimal::ZERO);
            let yes_ask = (row.yes_price + half_spread).min(Decimal::ONE);
            let yes_snapshot = BookSnapshotEvent {
                token_id: row.yes_token_id.clone(),
                event_id: row.event_id.clone(),
                bids: if yes_bid > Decimal::ZERO { vec![PriceLevel::new(yes_bid, default_size)] } else { vec![] },
                asks: if yes_ask < Decimal::ONE { vec![PriceLevel::new(yes_ask, default_size)] } else { vec![] },
                timestamp: row.timestamp,
            };

            // Create NO book snapshot
            let no_bid = (row.no_price - half_spread).max(Decimal::ZERO);
            let no_ask = (row.no_price + half_spread).min(Decimal::ONE);
            let no_snapshot = BookSnapshotEvent {
                token_id: row.no_token_id,
                event_id: row.event_id.clone(),
                bids: if no_bid > Decimal::ZERO { vec![PriceLevel::new(no_bid, default_size)] } else { vec![] },
                asks: if no_ask < Decimal::ONE { vec![PriceLevel::new(no_ask, default_size)] } else { vec![] },
                timestamp: row.timestamp,
            };

            // Create a single paired event to ensure atomic update of both books
            let pair_event = MarketEvent::BookSnapshotPair(BookSnapshotPairEvent {
                event_id: row.event_id,
                yes_snapshot,
                no_snapshot,
                timestamp: row.timestamp,
            });

            self.event_queue.push(TimestampedEvent {
                timestamp: row.timestamp,
                event: pair_event,
            });

            count += 1;
        }

        debug!("Loaded {} aligned price pairs as paired book snapshots", count);
        Ok(())
    }

    /// Load orderbook snapshots from live collection (with full bid/ask data).
    /// Returns true if data was loaded, false if file doesn't exist.
    fn load_orderbook_snapshots(
        &mut self,
        data_dir: &Path,
        event_tokens: &EventTokenMap,
    ) -> Result<bool, DataSourceError> {
        let path = data_dir.join("polymarket_orderbook_snapshots.csv");
        if !path.exists() {
            debug!("Orderbook snapshots file not found: {:?}", path);
            return Ok(false);
        }

        let mut reader = csv::Reader::from_path(&path)
            .map_err(|e| DataSourceError::Parse(format!("Failed to open {:?}: {}", path, e)))?;

        // Group snapshots by (event_id, timestamp) to pair YES/NO
        let mut pending: HashMap<(String, DateTime<Utc>), OrderBookSnapshotRow> = HashMap::new();
        let mut count = 0;

        for result in reader.deserialize() {
            let row: OrderBookSnapshotRow = result
                .map_err(|e| DataSourceError::Parse(format!("CSV parse error: {}", e)))?;

            // Filter by config time range
            if let Some(start) = self.config.start_time
                && row.timestamp < start
            {
                continue;
            }
            if let Some(end) = self.config.end_time
                && row.timestamp > end
            {
                continue;
            }

            // Get token mapping for this event
            let Some((yes_token, _no_token)) = event_tokens.get(&row.event_id) else {
                continue;
            };

            let key = (row.event_id.clone(), row.timestamp);

            // Check if we have a pending partner for this snapshot
            if let Some(partner) = pending.remove(&key) {
                // Determine which is YES and which is NO
                let (yes_row, no_row) = if row.token_id == *yes_token {
                    (row, partner)
                } else if partner.token_id == *yes_token {
                    (partner, row)
                } else {
                    // Neither matches YES token, skip
                    continue;
                };

                // Create book snapshots with real bid/ask data
                let yes_snapshot = BookSnapshotEvent {
                    token_id: yes_row.token_id,
                    event_id: yes_row.event_id.clone(),
                    bids: if yes_row.best_bid > Decimal::ZERO {
                        vec![PriceLevel::new(yes_row.best_bid, yes_row.best_bid_size)]
                    } else {
                        vec![]
                    },
                    asks: if yes_row.best_ask < Decimal::ONE {
                        vec![PriceLevel::new(yes_row.best_ask, yes_row.best_ask_size)]
                    } else {
                        vec![]
                    },
                    timestamp: yes_row.timestamp,
                };

                let no_snapshot = BookSnapshotEvent {
                    token_id: no_row.token_id,
                    event_id: no_row.event_id,
                    bids: if no_row.best_bid > Decimal::ZERO {
                        vec![PriceLevel::new(no_row.best_bid, no_row.best_bid_size)]
                    } else {
                        vec![]
                    },
                    asks: if no_row.best_ask < Decimal::ONE {
                        vec![PriceLevel::new(no_row.best_ask, no_row.best_ask_size)]
                    } else {
                        vec![]
                    },
                    timestamp: no_row.timestamp,
                };

                // Create paired event
                let pair_event = MarketEvent::BookSnapshotPair(BookSnapshotPairEvent {
                    event_id: yes_snapshot.event_id.clone(),
                    yes_snapshot,
                    no_snapshot,
                    timestamp: key.1,
                });

                self.event_queue.push(TimestampedEvent {
                    timestamp: key.1,
                    event: pair_event,
                });

                count += 1;
            } else {
                // No partner yet, store this one
                pending.insert(key, row);
            }
        }

        if pending.len() > 10 {
            warn!(
                "{} orderbook snapshots had no YES/NO partner",
                pending.len()
            );
        }

        info!(
            "Loaded {} orderbook snapshot pairs from live data",
            count
        );
        Ok(count > 0)
    }
}

#[async_trait]
impl DataSource for CsvReplayDataSource {
    async fn next_event(&mut self) -> Result<Option<MarketEvent>, DataSourceError> {
        // Load data on first call
        if !self.data_loaded {
            self.load_data()?;
        }

        // Get next event from queue
        match self.event_queue.pop() {
            Some(timestamped) => {
                // Apply speed control if configured
                if self.config.speed > 0.0
                    && let Some(prev_time) = self.current_time
                {
                    let delta = timestamped.timestamp - prev_time;
                    if delta.num_milliseconds() > 0 {
                        let sleep_ms = (delta.num_milliseconds() as f64 / self.config.speed) as u64;
                        if sleep_ms > 0 {
                            tokio::time::sleep(std::time::Duration::from_millis(sleep_ms)).await;
                        }
                    }
                }

                self.current_time = Some(timestamped.timestamp);
                Ok(Some(timestamped.event))
            }
            None => {
                self.is_exhausted = true;
                Ok(None)
            }
        }
    }

    fn has_more(&self) -> bool {
        !self.is_exhausted && (!self.data_loaded || !self.event_queue.is_empty())
    }

    fn current_time(&self) -> Option<DateTime<Utc>> {
        self.current_time
    }

    async fn shutdown(&mut self) {
        self.is_exhausted = true;
        self.event_queue.clear();
    }
}

impl CsvReplayDataSource {
    /// Load all events from CSV files and return them in chronological order.
    ///
    /// This is useful for caching events when running multiple backtests
    /// (e.g., parameter sweeps) to avoid reloading CSV files each time.
    pub fn load_all_events(config: CsvReplayConfig) -> Result<Vec<MarketEvent>, DataSourceError> {
        let mut source = CsvReplayDataSource::new(config);
        source.load_data()?;

        // Drain the heap - events come out in chronological order
        // (BinaryHeap uses reversed Ord so pop() gives earliest first)
        let mut events = Vec::with_capacity(source.event_queue.len());
        while let Some(timestamped) = source.event_queue.pop() {
            events.push(timestamped.event);
        }

        info!("Loaded {} events from CSV files", events.len());
        Ok(events)
    }

    /// Load warmup prices from CSV for ATR initialization.
    ///
    /// Returns spot prices from the CSV file that are BEFORE the start_time,
    /// grouped by asset. This allows warming up indicators before the backtest
    /// starts, ensuring consistent behavior with live trading.
    ///
    /// # Arguments
    /// * `data_dir` - Directory containing the CSV files
    /// * `start_time` - The backtest start time (warmup prices will be before this)
    /// * `warmup_duration` - How far back to look for warmup prices
    /// * `assets` - Assets to load prices for (empty = all)
    pub fn load_warmup_prices(
        data_dir: &str,
        start_time: DateTime<Utc>,
        warmup_duration: chrono::Duration,
        assets: &[CryptoAsset],
    ) -> HashMap<CryptoAsset, Vec<Decimal>> {
        let path = Path::new(data_dir).join("binance_spot_prices.csv");
        if !path.exists() {
            warn!("Spot prices file not found for warmup: {:?}", path);
            return HashMap::new();
        }

        let warmup_start = start_time - warmup_duration;
        let mut result: HashMap<CryptoAsset, Vec<Decimal>> = HashMap::new();

        let reader = match csv::Reader::from_path(&path) {
            Ok(r) => r,
            Err(e) => {
                warn!("Failed to open spot prices for warmup: {}", e);
                return HashMap::new();
            }
        };

        for row_result in reader.into_deserialize::<SpotPriceRow>() {
            let row = match row_result {
                Ok(r) => r,
                Err(_) => continue,
            };

            // Only include prices in warmup window (before start_time)
            if row.timestamp >= start_time || row.timestamp < warmup_start {
                continue;
            }

            let asset = match parse_asset(&row.asset) {
                Some(a) => a,
                None => continue,
            };

            // Filter by requested assets
            if !assets.is_empty() && !assets.contains(&asset) {
                continue;
            }

            result.entry(asset).or_default().push(row.price);
        }

        // Prices are already in chronological order from the CSV
        // Nothing to sort

        for (asset, prices) in &result {
            debug!("Loaded {} warmup prices for {}", prices.len(), asset);
        }

        result
    }
}

/// Parse asset string to CryptoAsset.
fn parse_asset(s: &str) -> Option<CryptoAsset> {
    match s.to_uppercase().as_str() {
        "BTC" => Some(CryptoAsset::Btc),
        "ETH" => Some(CryptoAsset::Eth),
        "SOL" => Some(CryptoAsset::Sol),
        "XRP" => Some(CryptoAsset::Xrp),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = CsvReplayConfig::default();
        assert_eq!(config.data_dir, "data");
        assert_eq!(config.assets.len(), 3);
        assert_eq!(config.speed, 0.0);
    }

    #[test]
    fn test_parse_asset() {
        assert_eq!(parse_asset("BTC"), Some(CryptoAsset::Btc));
        assert_eq!(parse_asset("btc"), Some(CryptoAsset::Btc));
        assert_eq!(parse_asset("ETH"), Some(CryptoAsset::Eth));
        assert_eq!(parse_asset("SOL"), Some(CryptoAsset::Sol));
        assert_eq!(parse_asset("XRP"), Some(CryptoAsset::Xrp));
        assert_eq!(parse_asset("UNKNOWN"), None);
    }

    #[test]
    fn test_timestamped_event_ordering() {
        let now = Utc::now();
        let earlier = now - chrono::Duration::seconds(10);
        let later = now + chrono::Duration::seconds(10);

        let event1 = TimestampedEvent {
            timestamp: earlier,
            event: MarketEvent::Heartbeat(earlier),
        };
        let event2 = TimestampedEvent {
            timestamp: later,
            event: MarketEvent::Heartbeat(later),
        };

        // Min-heap: earlier should come first (reversed ordering)
        assert!(event1 > event2);
    }
}
