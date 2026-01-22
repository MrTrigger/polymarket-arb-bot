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
    BookSnapshotEvent, DataSource, DataSourceError, MarketEvent, SpotPriceEvent,
    WindowCloseEvent, WindowOpenEvent,
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

/// CSV row for price history.
#[derive(Debug, Clone, Deserialize)]
struct PriceHistoryRow {
    token_id: String,
    timestamp: DateTime<Utc>,
    #[serde(with = "rust_decimal::serde::str")]
    price: Decimal,
}

/// Mapping of token_id to (event_id, is_yes_token)
type TokenMap = HashMap<String, (String, bool)>;

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

        // Load market windows first (they define the token -> event mapping)
        let token_map = self.load_market_windows(data_dir)?;

        // Load spot prices
        self.load_spot_prices(data_dir)?;

        // Load price history and convert to book snapshots
        self.load_price_history(data_dir, &token_map)?;

        info!("Loaded {} events for replay", self.event_queue.len());
        self.data_loaded = true;

        Ok(())
    }

    /// Load market windows and return token -> event mapping.
    fn load_market_windows(&mut self, data_dir: &Path) -> Result<TokenMap, DataSourceError> {
        let path = data_dir.join("polymarket_market_windows.csv");
        if !path.exists() {
            warn!("Market windows file not found: {:?}", path);
            return Ok(HashMap::new());
        }

        let mut reader = csv::Reader::from_path(&path)
            .map_err(|e| DataSourceError::Parse(format!("Failed to open {:?}: {}", path, e)))?;

        let mut token_map: TokenMap = HashMap::new();
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

            // Build token -> event mapping
            token_map.insert(row.yes_token_id.clone(), (row.event_id.clone(), true));
            token_map.insert(row.no_token_id.clone(), (row.event_id.clone(), false));

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
        Ok(token_map)
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

    /// Load price history and convert to book snapshots.
    fn load_price_history(
        &mut self,
        data_dir: &Path,
        token_map: &TokenMap,
    ) -> Result<(), DataSourceError> {
        let path = data_dir.join("polymarket_price_history.csv");
        if !path.exists() {
            warn!("Price history file not found: {:?}", path);
            return Ok(());
        }

        let mut reader = csv::Reader::from_path(&path)
            .map_err(|e| DataSourceError::Parse(format!("Failed to open {:?}: {}", path, e)))?;

        let mut count = 0;

        for result in reader.deserialize() {
            let row: PriceHistoryRow = result
                .map_err(|e| DataSourceError::Parse(format!("CSV parse error: {}", e)))?;

            // Look up event_id from token_map
            let (event_id, _is_yes) = match token_map.get(&row.token_id) {
                Some(mapping) => mapping.clone(),
                None => continue, // Token not in any tracked market window
            };

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

            // Convert price history to a simplified book snapshot
            // Use the price as mid-price with a small spread
            let spread = Decimal::new(1, 2); // 0.01 spread
            let half_spread = spread / Decimal::TWO;

            let bid_price = (row.price - half_spread).max(Decimal::ZERO);
            let ask_price = (row.price + half_spread).min(Decimal::ONE);

            // Default size for simulated liquidity
            let default_size = Decimal::new(1000, 0);

            let bids = if bid_price > Decimal::ZERO {
                vec![PriceLevel::new(bid_price, default_size)]
            } else {
                vec![]
            };

            let asks = if ask_price < Decimal::ONE {
                vec![PriceLevel::new(ask_price, default_size)]
            } else {
                vec![]
            };

            let event = MarketEvent::BookSnapshot(BookSnapshotEvent {
                token_id: row.token_id,
                event_id,
                bids,
                asks,
                timestamp: row.timestamp,
            });

            self.event_queue.push(TimestampedEvent {
                timestamp: row.timestamp,
                event,
            });

            count += 1;
        }

        debug!("Loaded {} price history records as book snapshots", count);
        Ok(())
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
