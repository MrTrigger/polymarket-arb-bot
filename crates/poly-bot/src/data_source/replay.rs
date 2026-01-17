//! Replay data source for backtesting.
//!
//! Loads historical data from ClickHouse and replays events in chronological
//! order for backtesting trading strategies.

use std::collections::BinaryHeap;
use std::cmp::Ordering;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use clickhouse::Row;
use rust_decimal::Decimal;
use serde::Deserialize;
use tracing::{debug, info};

use poly_common::types::{CryptoAsset, Side};
use poly_common::ClickHouseClient;

use super::{
    BookDeltaEvent, BookSnapshotEvent, DataSource, DataSourceError, MarketEvent, SpotPriceEvent,
    WindowCloseEvent, WindowOpenEvent,
};
use crate::types::PriceLevel;

/// Configuration for the replay data source.
#[derive(Debug, Clone)]
pub struct ReplayConfig {
    /// Start of replay period.
    pub start_time: DateTime<Utc>,
    /// End of replay period.
    pub end_time: DateTime<Utc>,
    /// Event IDs to filter (empty = all).
    pub event_ids: Vec<String>,
    /// Assets to filter (empty = all).
    pub assets: Vec<CryptoAsset>,
    /// Batch size for loading from ClickHouse.
    pub batch_size: usize,
    /// Replay speed multiplier (1.0 = real-time, 0.0 = max speed).
    pub speed: f64,
}

impl Default for ReplayConfig {
    fn default() -> Self {
        Self {
            start_time: Utc::now() - chrono::Duration::hours(24),
            end_time: Utc::now(),
            event_ids: Vec::new(),
            assets: vec![
                CryptoAsset::Btc,
                CryptoAsset::Eth,
                CryptoAsset::Sol,
                CryptoAsset::Xrp,
            ],
            batch_size: 10_000,
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

/// ClickHouse row for spot prices.
#[derive(Debug, Clone, Row, Deserialize)]
struct SpotPriceRow {
    asset: String,
    #[serde(with = "rust_decimal::serde::str")]
    price: Decimal,
    timestamp_ms: i64,
    #[serde(with = "rust_decimal::serde::str")]
    quantity: Decimal,
}

/// ClickHouse row for order book snapshots.
#[derive(Debug, Clone, Row, Deserialize)]
struct SnapshotRow {
    token_id: String,
    event_id: String,
    timestamp_ms: i64,
    #[serde(with = "rust_decimal::serde::str")]
    best_bid: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    best_bid_size: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    best_ask: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    best_ask_size: Decimal,
}

/// ClickHouse row for order book deltas.
#[derive(Debug, Clone, Row, Deserialize)]
struct DeltaRow {
    token_id: String,
    event_id: String,
    timestamp_ms: i64,
    side: String,
    #[serde(with = "rust_decimal::serde::str")]
    price: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    size: Decimal,
}

/// ClickHouse row for market windows.
#[derive(Debug, Clone, Row, Deserialize)]
struct WindowRow {
    event_id: String,
    asset: String,
    yes_token_id: String,
    no_token_id: String,
    #[serde(with = "rust_decimal::serde::str")]
    strike_price: Decimal,
    window_start_ms: i64,
    window_end_ms: i64,
    discovered_at_ms: i64,
}

/// Helper to convert epoch milliseconds to DateTime<Utc>
fn ms_to_datetime(ms: i64) -> DateTime<Utc> {
    DateTime::from_timestamp_millis(ms).unwrap_or_default()
}

/// Replay data source that loads historical data from ClickHouse.
pub struct ReplayDataSource {
    config: ReplayConfig,
    clickhouse: ClickHouseClient,
    /// Priority queue for merging events by timestamp.
    event_queue: BinaryHeap<TimestampedEvent>,
    /// Current replay time.
    current_time: Option<DateTime<Utc>>,
    /// Whether initial data has been loaded.
    data_loaded: bool,
    /// Whether replay is complete.
    is_exhausted: bool,
}

impl ReplayDataSource {
    /// Creates a new replay data source.
    pub fn new(config: ReplayConfig, clickhouse: ClickHouseClient) -> Self {
        Self {
            config,
            clickhouse,
            event_queue: BinaryHeap::new(),
            current_time: None,
            data_loaded: false,
            is_exhausted: false,
        }
    }

    /// Load all historical data into the event queue.
    async fn load_data(&mut self) -> Result<(), DataSourceError> {
        info!(
            "Loading replay data from {} to {}",
            self.config.start_time, self.config.end_time
        );

        // Load market windows first (they define the trading periods)
        self.load_market_windows().await?;

        // Load spot prices
        self.load_spot_prices().await?;

        // Load order book snapshots
        self.load_snapshots().await?;

        // Load order book deltas
        self.load_deltas().await?;

        info!("Loaded {} events for replay", self.event_queue.len());
        self.data_loaded = true;

        Ok(())
    }

    /// Load market windows.
    async fn load_market_windows(&mut self) -> Result<(), DataSourceError> {
        let asset_filter = if self.config.assets.is_empty() {
            String::new()
        } else {
            let assets: Vec<String> = self.config.assets.iter().map(|a| format!("'{}'", a.as_str())).collect();
            format!("AND asset IN ({})", assets.join(", "))
        };

        // Format dates without 'Z' suffix for ClickHouse DateTime64 compatibility
        let start_str = self.config.start_time.format("%Y-%m-%d %H:%M:%S%.3f").to_string();
        let end_str = self.config.end_time.format("%Y-%m-%d %H:%M:%S%.3f").to_string();

        let query = format!(
            r#"
            SELECT
                event_id,
                asset,
                yes_token_id,
                no_token_id,
                toString(strike_price) AS strike_price,
                toUnixTimestamp64Milli(window_start) AS window_start_ms,
                toUnixTimestamp64Milli(window_end) AS window_end_ms,
                toUnixTimestamp64Milli(discovered_at) AS discovered_at_ms
            FROM market_windows
            WHERE window_start >= '{}'
              AND window_end <= '{}'
              {}
            ORDER BY window_start ASC
            "#,
            start_str, end_str, asset_filter
        );

        let rows: Vec<WindowRow> = self
            .clickhouse
            .inner()
            .query(&query)
            .fetch_all()
            .await
            .map_err(|e| DataSourceError::ClickHouse(e.to_string()))?;

        debug!("Loaded {} market windows", rows.len());

        for row in rows {
            let asset = parse_asset(&row.asset).unwrap_or(CryptoAsset::Btc);
            let window_start = ms_to_datetime(row.window_start_ms);
            let window_end = ms_to_datetime(row.window_end_ms);
            let discovered_at = ms_to_datetime(row.discovered_at_ms);

            // Window open event
            let open_event = MarketEvent::WindowOpen(WindowOpenEvent {
                event_id: row.event_id.clone(),
                asset,
                yes_token_id: row.yes_token_id.clone(),
                no_token_id: row.no_token_id.clone(),
                strike_price: row.strike_price,
                window_start,
                window_end,
                timestamp: discovered_at,
                min_order_size: Decimal::ONE, // Default for replay (historical data)
            });

            self.event_queue.push(TimestampedEvent {
                timestamp: discovered_at,
                event: open_event,
            });

            // Window close event
            let close_event = MarketEvent::WindowClose(WindowCloseEvent {
                event_id: row.event_id,
                outcome: None, // Could be enriched from settlement data
                final_price: None,
                timestamp: window_end,
            });

            self.event_queue.push(TimestampedEvent {
                timestamp: window_end,
                event: close_event,
            });
        }

        Ok(())
    }

    /// Load spot prices.
    async fn load_spot_prices(&mut self) -> Result<(), DataSourceError> {
        let asset_filter = if self.config.assets.is_empty() {
            String::new()
        } else {
            let assets: Vec<String> = self.config.assets.iter().map(|a| format!("'{}'", a.as_str())).collect();
            format!("AND asset IN ({})", assets.join(", "))
        };

        // Format dates without 'Z' suffix for ClickHouse DateTime64 compatibility
        let start_str = self.config.start_time.format("%Y-%m-%d %H:%M:%S%.3f").to_string();
        let end_str = self.config.end_time.format("%Y-%m-%d %H:%M:%S%.3f").to_string();
        let limit = self.config.batch_size as u64 * 100;

        let query = format!(
            r#"
            SELECT asset, toString(price) AS price, toUnixTimestamp64Milli(timestamp) AS timestamp_ms, toString(quantity) AS quantity
            FROM spot_prices
            WHERE timestamp >= '{}'
              AND timestamp <= '{}'
              {}
            ORDER BY timestamp ASC
            LIMIT {}
            "#,
            start_str, end_str, asset_filter, limit
        );

        let rows: Vec<SpotPriceRow> = self
            .clickhouse
            .inner()
            .query(&query)
            .fetch_all()
            .await
            .map_err(|e| DataSourceError::ClickHouse(e.to_string()))?;

        debug!("Loaded {} spot prices", rows.len());

        for row in rows {
            if let Some(asset) = parse_asset(&row.asset) {
                let timestamp = ms_to_datetime(row.timestamp_ms);
                let event = MarketEvent::SpotPrice(SpotPriceEvent {
                    asset,
                    price: row.price,
                    quantity: row.quantity,
                    timestamp,
                });

                self.event_queue.push(TimestampedEvent {
                    timestamp,
                    event,
                });
            }
        }

        Ok(())
    }

    /// Load order book snapshots.
    async fn load_snapshots(&mut self) -> Result<(), DataSourceError> {
        let event_filter = if self.config.event_ids.is_empty() {
            String::new()
        } else {
            let ids: Vec<String> = self.config.event_ids.iter().map(|id| format!("'{}'", id)).collect();
            format!("AND event_id IN ({})", ids.join(", "))
        };

        // Format dates without 'Z' suffix for ClickHouse DateTime64 compatibility
        let start_str = self.config.start_time.format("%Y-%m-%d %H:%M:%S%.3f").to_string();
        let end_str = self.config.end_time.format("%Y-%m-%d %H:%M:%S%.3f").to_string();
        let limit = self.config.batch_size as u64 * 10;

        let query = format!(
            r#"
            SELECT
                token_id,
                event_id,
                toUnixTimestamp64Milli(timestamp) AS timestamp_ms,
                toString(best_bid) AS best_bid,
                toString(best_bid_size) AS best_bid_size,
                toString(best_ask) AS best_ask,
                toString(best_ask_size) AS best_ask_size
            FROM orderbook_snapshots
            WHERE timestamp >= '{}'
              AND timestamp <= '{}'
              {}
            ORDER BY timestamp ASC
            LIMIT {}
            "#,
            start_str, end_str, event_filter, limit
        );

        let rows: Vec<SnapshotRow> = self
            .clickhouse
            .inner()
            .query(&query)
            .fetch_all()
            .await
            .map_err(|e| DataSourceError::ClickHouse(e.to_string()))?;

        debug!("Loaded {} order book snapshots", rows.len());

        for row in rows {
            let timestamp = ms_to_datetime(row.timestamp_ms);

            // Convert BBO to snapshot with single levels
            let bids = if row.best_bid > Decimal::ZERO {
                vec![PriceLevel::new(row.best_bid, row.best_bid_size)]
            } else {
                vec![]
            };

            let asks = if row.best_ask > Decimal::ZERO {
                vec![PriceLevel::new(row.best_ask, row.best_ask_size)]
            } else {
                vec![]
            };

            let event = MarketEvent::BookSnapshot(BookSnapshotEvent {
                token_id: row.token_id,
                event_id: row.event_id,
                bids,
                asks,
                timestamp,
            });

            self.event_queue.push(TimestampedEvent {
                timestamp,
                event,
            });
        }

        Ok(())
    }

    /// Load order book deltas.
    async fn load_deltas(&mut self) -> Result<(), DataSourceError> {
        let event_filter = if self.config.event_ids.is_empty() {
            String::new()
        } else {
            let ids: Vec<String> = self.config.event_ids.iter().map(|id| format!("'{}'", id)).collect();
            format!("AND event_id IN ({})", ids.join(", "))
        };

        // Format dates without 'Z' suffix for ClickHouse DateTime64 compatibility
        let start_str = self.config.start_time.format("%Y-%m-%d %H:%M:%S%.3f").to_string();
        let end_str = self.config.end_time.format("%Y-%m-%d %H:%M:%S%.3f").to_string();
        let limit = self.config.batch_size as u64 * 10;

        let query = format!(
            r#"
            SELECT
                token_id,
                event_id,
                toUnixTimestamp64Milli(timestamp) AS timestamp_ms,
                side,
                toString(price) AS price,
                toString(size) AS size
            FROM orderbook_deltas
            WHERE timestamp >= '{}'
              AND timestamp <= '{}'
              {}
            ORDER BY timestamp ASC
            LIMIT {}
            "#,
            start_str, end_str, event_filter, limit
        );

        let rows: Vec<DeltaRow> = self
            .clickhouse
            .inner()
            .query(&query)
            .fetch_all()
            .await
            .map_err(|e| DataSourceError::ClickHouse(e.to_string()))?;

        debug!("Loaded {} order book deltas", rows.len());

        for row in rows {
            let timestamp = ms_to_datetime(row.timestamp_ms);

            let side = match row.side.to_lowercase().as_str() {
                "buy" | "bid" => Side::Buy,
                "sell" | "ask" => Side::Sell,
                _ => continue,
            };

            let event = MarketEvent::BookDelta(BookDeltaEvent {
                token_id: row.token_id,
                event_id: row.event_id,
                side,
                price: row.price,
                size: row.size,
                timestamp,
            });

            self.event_queue.push(TimestampedEvent {
                timestamp,
                event,
            });
        }

        Ok(())
    }
}

#[async_trait]
impl DataSource for ReplayDataSource {
    async fn next_event(&mut self) -> Result<Option<MarketEvent>, DataSourceError> {
        // Load data on first call
        if !self.data_loaded {
            self.load_data().await?;
        }

        // Get next event from queue
        match self.event_queue.pop() {
            Some(timestamped) => {
                self.current_time = Some(timestamped.timestamp);

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
        let config = ReplayConfig::default();
        assert_eq!(config.assets.len(), 4);
        assert_eq!(config.batch_size, 10_000);
        assert_eq!(config.speed, 0.0);
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

        // Min-heap: earlier should come first
        assert!(event1 > event2); // Reversed ordering for BinaryHeap
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
    fn test_replay_config_with_filters() {
        let config = ReplayConfig {
            start_time: Utc::now() - chrono::Duration::hours(1),
            end_time: Utc::now(),
            event_ids: vec!["event1".to_string(), "event2".to_string()],
            assets: vec![CryptoAsset::Btc],
            batch_size: 1000,
            speed: 100.0,
        };

        assert_eq!(config.event_ids.len(), 2);
        assert_eq!(config.assets.len(), 1);
        assert_eq!(config.speed, 100.0);
    }

    #[test]
    fn test_priority_queue_behavior() {
        let mut queue: BinaryHeap<TimestampedEvent> = BinaryHeap::new();
        let now = Utc::now();

        // Add events in random order
        queue.push(TimestampedEvent {
            timestamp: now + chrono::Duration::seconds(5),
            event: MarketEvent::Heartbeat(now + chrono::Duration::seconds(5)),
        });
        queue.push(TimestampedEvent {
            timestamp: now,
            event: MarketEvent::Heartbeat(now),
        });
        queue.push(TimestampedEvent {
            timestamp: now + chrono::Duration::seconds(10),
            event: MarketEvent::Heartbeat(now + chrono::Duration::seconds(10)),
        });

        // Should pop in chronological order (earliest first)
        let e1 = queue.pop().unwrap();
        let e2 = queue.pop().unwrap();
        let e3 = queue.pop().unwrap();

        assert!(e1.timestamp <= e2.timestamp);
        assert!(e2.timestamp <= e3.timestamp);
    }
}
