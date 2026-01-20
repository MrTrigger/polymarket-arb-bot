//! Vec-based replay data source for backtesting.
//!
//! Takes pre-loaded events and replays them in order. Used by sweep mode
//! to avoid reloading CSV files for each parameter combination.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use super::{DataSource, DataSourceError, MarketEvent};

/// Vec-based replay data source.
///
/// Uses Arc to share events across multiple backtests without cloning.
/// Much faster than CsvReplayDataSource when running multiple backtests
/// on the same data.
pub struct VecReplayDataSource {
    /// Events to replay (shared reference, in chronological order).
    events: Arc<Vec<MarketEvent>>,
    /// Current position in the event list.
    position: usize,
    /// Current replay time.
    current_time: Option<DateTime<Utc>>,
    /// Replay speed (0.0 = max speed).
    speed: f64,
}

impl VecReplayDataSource {
    /// Creates a new Vec replay data source from pre-loaded events.
    ///
    /// Events should already be sorted in chronological order.
    pub fn new(events: Arc<Vec<MarketEvent>>, speed: f64) -> Self {
        Self {
            events,
            position: 0,
            current_time: None,
            speed,
        }
    }

    /// Returns the number of events.
    pub fn event_count(&self) -> usize {
        self.events.len()
    }
}

#[async_trait]
impl DataSource for VecReplayDataSource {
    async fn next_event(&mut self) -> Result<Option<MarketEvent>, DataSourceError> {
        if self.position >= self.events.len() {
            return Ok(None);
        }

        let event = self.events[self.position].clone();
        self.position += 1;

        // Apply speed control if configured
        if self.speed > 0.0
            && let Some(prev_time) = self.current_time
        {
            let delta = event.timestamp() - prev_time;
            if delta.num_milliseconds() > 0 {
                let sleep_ms = (delta.num_milliseconds() as f64 / self.speed) as u64;
                if sleep_ms > 0 {
                    tokio::time::sleep(std::time::Duration::from_millis(sleep_ms)).await;
                }
            }
        }

        self.current_time = Some(event.timestamp());
        Ok(Some(event))
    }

    fn has_more(&self) -> bool {
        self.position < self.events.len()
    }

    fn current_time(&self) -> Option<DateTime<Utc>> {
        self.current_time
    }

    async fn shutdown(&mut self) {
        // Nothing to clean up
        self.position = self.events.len();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    use crate::data_source::SpotPriceEvent;
    use poly_common::types::CryptoAsset;

    #[tokio::test]
    async fn test_vec_replay_basic() {
        let now = Utc::now();
        let events = vec![
            MarketEvent::SpotPrice(SpotPriceEvent {
                asset: CryptoAsset::Btc,
                price: dec!(50000),
                quantity: dec!(1),
                timestamp: now,
            }),
            MarketEvent::SpotPrice(SpotPriceEvent {
                asset: CryptoAsset::Btc,
                price: dec!(50001),
                quantity: dec!(1),
                timestamp: now + chrono::Duration::seconds(1),
            }),
        ];

        let mut source = VecReplayDataSource::new(Arc::new(events), 0.0);
        assert_eq!(source.event_count(), 2);
        assert!(source.has_more());

        let e1 = source.next_event().await.unwrap().unwrap();
        assert!(matches!(e1, MarketEvent::SpotPrice(_)));

        let e2 = source.next_event().await.unwrap().unwrap();
        assert!(matches!(e2, MarketEvent::SpotPrice(_)));

        let e3 = source.next_event().await.unwrap();
        assert!(e3.is_none());
        assert!(!source.has_more());
    }

    #[tokio::test]
    async fn test_vec_replay_empty() {
        let mut source = VecReplayDataSource::new(Arc::new(vec![]), 0.0);
        assert_eq!(source.event_count(), 0);
        assert!(!source.has_more());

        let event = source.next_event().await.unwrap();
        assert!(event.is_none());
    }
}
