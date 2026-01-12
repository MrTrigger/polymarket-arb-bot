//! Integration tests for replay data source.
//!
//! These tests verify:
//! - ReplayDataSource configuration
//! - Event ordering and priority queue behavior
//! - DataSource trait implementation
//!
//! Note: Tests that require ClickHouse are skipped unless
//! CLICKHOUSE_URL is set in the environment.

use std::collections::BinaryHeap;
use std::cmp::Ordering;

use chrono::{Duration, Utc};
use rust_decimal_macros::dec;

use poly_bot::{
    BookDeltaEvent, BookSnapshotEvent, DataSource, MarketEvent, SpotPriceEvent,
    WindowCloseEvent, WindowOpenEvent,
};
use poly_bot::data_source::replay::ReplayConfig;
use poly_bot::types::PriceLevel;
use poly_common::types::{CryptoAsset, Side};

/// Wrapper for testing event ordering in priority queue.
struct TimestampedTestEvent {
    timestamp: chrono::DateTime<Utc>,
    #[allow(dead_code)]
    event: MarketEvent,
}

impl Eq for TimestampedTestEvent {}

impl PartialEq for TimestampedTestEvent {
    fn eq(&self, other: &Self) -> bool {
        self.timestamp == other.timestamp
    }
}

impl Ord for TimestampedTestEvent {
    fn cmp(&self, other: &Self) -> Ordering {
        // Reverse for min-heap (earliest first)
        other.timestamp.cmp(&self.timestamp)
    }
}

impl PartialOrd for TimestampedTestEvent {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[test]
fn test_replay_config_default() {
    let config = ReplayConfig::default();

    // Should have all 4 assets
    assert_eq!(config.assets.len(), 4);
    assert!(config.assets.contains(&CryptoAsset::Btc));
    assert!(config.assets.contains(&CryptoAsset::Eth));
    assert!(config.assets.contains(&CryptoAsset::Sol));
    assert!(config.assets.contains(&CryptoAsset::Xrp));

    // Should have sensible defaults
    assert_eq!(config.batch_size, 10_000);
    assert_eq!(config.speed, 0.0); // Max speed for backtesting

    // Should have reasonable time range
    assert!(config.end_time > config.start_time);
}

#[test]
fn test_replay_config_custom() {
    let start = Utc::now() - Duration::hours(6);
    let end = Utc::now();

    let config = ReplayConfig {
        start_time: start,
        end_time: end,
        event_ids: vec!["event1".to_string(), "event2".to_string()],
        assets: vec![CryptoAsset::Btc],
        batch_size: 5000,
        speed: 100.0,
    };

    assert_eq!(config.event_ids.len(), 2);
    assert_eq!(config.assets.len(), 1);
    assert_eq!(config.batch_size, 5000);
    assert_eq!(config.speed, 100.0);
}

#[test]
fn test_event_priority_queue_ordering() {
    let now = Utc::now();
    let mut queue: BinaryHeap<TimestampedTestEvent> = BinaryHeap::new();

    // Add events in random order
    let times = vec![
        now + Duration::seconds(5),
        now - Duration::seconds(10),
        now,
        now + Duration::seconds(10),
        now - Duration::seconds(5),
    ];

    for (i, time) in times.iter().enumerate() {
        queue.push(TimestampedTestEvent {
            timestamp: *time,
            event: MarketEvent::Heartbeat(*time),
        });
        assert_eq!(queue.len(), i + 1);
    }

    // Pop should return events in chronological order (earliest first)
    let mut prev_time = None;
    while let Some(event) = queue.pop() {
        if let Some(prev) = prev_time {
            assert!(
                event.timestamp >= prev,
                "Events should be in chronological order"
            );
        }
        prev_time = Some(event.timestamp);
    }
}

#[test]
fn test_market_event_display() {
    let now = Utc::now();

    // Test SpotPrice display
    let spot = MarketEvent::SpotPrice(SpotPriceEvent {
        asset: CryptoAsset::Btc,
        price: dec!(100000),
        quantity: dec!(1.5),
        timestamp: now,
    });
    let display = format!("{}", spot);
    assert!(display.contains("SpotPrice"));
    assert!(display.contains("BTC"));

    // Test BookSnapshot display
    let snapshot = MarketEvent::BookSnapshot(BookSnapshotEvent {
        token_id: "token123".to_string(),
        event_id: "event456".to_string(),
        bids: vec![PriceLevel::new(dec!(0.45), dec!(100))],
        asks: vec![PriceLevel::new(dec!(0.55), dec!(100))],
        timestamp: now,
    });
    let display = format!("{}", snapshot);
    assert!(display.contains("BookSnapshot"));
    assert!(display.contains("token123"));

    // Test WindowOpen display
    let open = MarketEvent::WindowOpen(WindowOpenEvent {
        event_id: "event789".to_string(),
        asset: CryptoAsset::Eth,
        yes_token_id: "yes".to_string(),
        no_token_id: "no".to_string(),
        strike_price: dec!(4000),
        window_start: now,
        window_end: now + Duration::minutes(15),
        timestamp: now,
    });
    let display = format!("{}", open);
    assert!(display.contains("WindowOpen"));
    assert!(display.contains("ETH"));
    assert!(display.contains("4000"));
}

#[test]
fn test_market_event_timestamp() {
    let now = Utc::now();

    let events = vec![
        MarketEvent::SpotPrice(SpotPriceEvent {
            asset: CryptoAsset::Btc,
            price: dec!(100000),
            quantity: dec!(1),
            timestamp: now,
        }),
        MarketEvent::BookSnapshot(BookSnapshotEvent {
            token_id: "token".to_string(),
            event_id: "event".to_string(),
            bids: vec![],
            asks: vec![],
            timestamp: now + Duration::seconds(1),
        }),
        MarketEvent::BookDelta(BookDeltaEvent {
            token_id: "token".to_string(),
            event_id: "event".to_string(),
            side: Side::Buy,
            price: dec!(0.50),
            size: dec!(100),
            timestamp: now + Duration::seconds(2),
        }),
        MarketEvent::WindowClose(WindowCloseEvent {
            event_id: "event".to_string(),
            outcome: None,
            final_price: None,
            timestamp: now + Duration::seconds(3),
        }),
        MarketEvent::Heartbeat(now + Duration::seconds(4)),
    ];

    // Verify each event returns correct timestamp
    for (i, event) in events.iter().enumerate() {
        let expected = now + Duration::seconds(i as i64);
        assert_eq!(
            event.timestamp(),
            expected,
            "Event {} has wrong timestamp",
            i
        );
    }
}

#[test]
fn test_market_event_is_heartbeat() {
    let now = Utc::now();

    let heartbeat = MarketEvent::Heartbeat(now);
    assert!(heartbeat.is_heartbeat());

    let spot = MarketEvent::SpotPrice(SpotPriceEvent {
        asset: CryptoAsset::Btc,
        price: dec!(100000),
        quantity: dec!(1),
        timestamp: now,
    });
    assert!(!spot.is_heartbeat());

    let snapshot = MarketEvent::BookSnapshot(BookSnapshotEvent {
        token_id: "token".to_string(),
        event_id: "event".to_string(),
        bids: vec![],
        asks: vec![],
        timestamp: now,
    });
    assert!(!snapshot.is_heartbeat());
}

#[test]
fn test_book_snapshot_event_fields() {
    let now = Utc::now();
    let bids = vec![
        PriceLevel::new(dec!(0.45), dec!(100)),
        PriceLevel::new(dec!(0.44), dec!(200)),
    ];
    let asks = vec![
        PriceLevel::new(dec!(0.55), dec!(150)),
        PriceLevel::new(dec!(0.56), dec!(250)),
    ];

    let snapshot = BookSnapshotEvent {
        token_id: "token123".to_string(),
        event_id: "event456".to_string(),
        bids: bids.clone(),
        asks: asks.clone(),
        timestamp: now,
    };

    assert_eq!(snapshot.token_id, "token123");
    assert_eq!(snapshot.event_id, "event456");
    assert_eq!(snapshot.bids.len(), 2);
    assert_eq!(snapshot.asks.len(), 2);
    assert_eq!(snapshot.bids[0].price, dec!(0.45));
    assert_eq!(snapshot.asks[0].price, dec!(0.55));
}

#[test]
fn test_book_delta_event_fields() {
    let now = Utc::now();

    let delta = BookDeltaEvent {
        token_id: "token123".to_string(),
        event_id: "event456".to_string(),
        side: Side::Buy,
        price: dec!(0.46),
        size: dec!(50),
        timestamp: now,
    };

    assert_eq!(delta.token_id, "token123");
    assert_eq!(delta.event_id, "event456");
    assert_eq!(delta.side, Side::Buy);
    assert_eq!(delta.price, dec!(0.46));
    assert_eq!(delta.size, dec!(50));
}

#[test]
fn test_window_open_event_fields() {
    let now = Utc::now();
    let window_end = now + Duration::minutes(15);

    let event = WindowOpenEvent {
        event_id: "event123".to_string(),
        asset: CryptoAsset::Sol,
        yes_token_id: "yes_token".to_string(),
        no_token_id: "no_token".to_string(),
        strike_price: dec!(200),
        window_start: now,
        window_end,
        timestamp: now,
    };

    assert_eq!(event.event_id, "event123");
    assert_eq!(event.asset, CryptoAsset::Sol);
    assert_eq!(event.yes_token_id, "yes_token");
    assert_eq!(event.no_token_id, "no_token");
    assert_eq!(event.strike_price, dec!(200));
    assert!(event.window_end > event.window_start);
}

#[test]
fn test_window_close_event_fields() {
    let now = Utc::now();

    // Window close with known outcome
    let event_with_outcome = WindowCloseEvent {
        event_id: "event123".to_string(),
        outcome: Some(poly_common::types::Outcome::Yes),
        final_price: Some(dec!(100500)),
        timestamp: now,
    };

    assert_eq!(event_with_outcome.event_id, "event123");
    assert!(event_with_outcome.outcome.is_some());
    assert!(event_with_outcome.final_price.is_some());

    // Window close without known outcome (replay)
    let event_no_outcome = WindowCloseEvent {
        event_id: "event456".to_string(),
        outcome: None,
        final_price: None,
        timestamp: now,
    };

    assert!(event_no_outcome.outcome.is_none());
    assert!(event_no_outcome.final_price.is_none());
}

#[test]
fn test_spot_price_event_fields() {
    let now = Utc::now();

    let event = SpotPriceEvent {
        asset: CryptoAsset::Xrp,
        price: dec!(2.50),
        quantity: dec!(1000),
        timestamp: now,
    };

    assert_eq!(event.asset, CryptoAsset::Xrp);
    assert_eq!(event.price, dec!(2.50));
    assert_eq!(event.quantity, dec!(1000));
    assert_eq!(event.timestamp, now);
}

#[test]
fn test_fill_event_display() {
    use poly_common::types::Outcome;

    let now = Utc::now();

    let fill = poly_bot::FillEvent {
        event_id: "event123".to_string(),
        token_id: "token456".to_string(),
        outcome: Outcome::Yes,
        order_id: "order789".to_string(),
        side: Side::Buy,
        size: dec!(100),
        price: dec!(0.45),
        fee: dec!(0.001),
        timestamp: now,
    };

    let event = MarketEvent::Fill(fill);
    let display = format!("{}", event);
    assert!(display.contains("Fill"));
    assert!(display.contains("100"));
    assert!(display.contains("0.45"));
}

#[test]
fn test_replay_config_with_empty_filters() {
    let config = ReplayConfig {
        start_time: Utc::now() - Duration::hours(1),
        end_time: Utc::now(),
        event_ids: vec![],
        assets: vec![],
        batch_size: 1000,
        speed: 0.0,
    };

    // Empty filters mean "all"
    assert!(config.event_ids.is_empty());
    assert!(config.assets.is_empty());
}

#[test]
fn test_priority_queue_with_same_timestamps() {
    let now = Utc::now();
    let mut queue: BinaryHeap<TimestampedTestEvent> = BinaryHeap::new();

    // Add multiple events with same timestamp
    for _ in 0..5 {
        queue.push(TimestampedTestEvent {
            timestamp: now,
            event: MarketEvent::Heartbeat(now),
        });
    }

    assert_eq!(queue.len(), 5);

    // All should have same timestamp
    let mut count = 0;
    while let Some(event) = queue.pop() {
        assert_eq!(event.timestamp, now);
        count += 1;
    }
    assert_eq!(count, 5);
}

#[test]
fn test_market_event_clone() {
    let now = Utc::now();

    let original = MarketEvent::SpotPrice(SpotPriceEvent {
        asset: CryptoAsset::Btc,
        price: dec!(100000),
        quantity: dec!(1),
        timestamp: now,
    });

    let cloned = original.clone();

    // Both should have same timestamp
    assert_eq!(original.timestamp(), cloned.timestamp());

    // Both should display the same
    assert_eq!(format!("{}", original), format!("{}", cloned));
}

/// Mock data source for replay-like behavior testing.
struct MockReplaySource {
    events: std::collections::VecDeque<MarketEvent>,
    current_time: Option<chrono::DateTime<Utc>>,
    exhausted: bool,
}

impl MockReplaySource {
    fn new(events: Vec<MarketEvent>) -> Self {
        Self {
            events: events.into(),
            current_time: None,
            exhausted: false,
        }
    }
}

#[async_trait::async_trait]
impl DataSource for MockReplaySource {
    async fn next_event(&mut self) -> Result<Option<MarketEvent>, poly_bot::DataSourceError> {
        match self.events.pop_front() {
            Some(event) => {
                self.current_time = Some(event.timestamp());
                Ok(Some(event))
            }
            None => {
                self.exhausted = true;
                Ok(None)
            }
        }
    }

    fn has_more(&self) -> bool {
        !self.exhausted && !self.events.is_empty()
    }

    fn current_time(&self) -> Option<chrono::DateTime<Utc>> {
        self.current_time
    }

    async fn shutdown(&mut self) {
        self.exhausted = true;
        self.events.clear();
    }
}

#[tokio::test]
async fn test_mock_replay_source_basic() {
    let now = Utc::now();
    let events = vec![
        MarketEvent::Heartbeat(now),
        MarketEvent::Heartbeat(now + Duration::seconds(1)),
        MarketEvent::Heartbeat(now + Duration::seconds(2)),
    ];

    let mut source = MockReplaySource::new(events);

    assert!(source.has_more());
    assert!(source.current_time().is_none());

    // First event
    let event1 = source.next_event().await.unwrap().unwrap();
    assert_eq!(event1.timestamp(), now);
    assert_eq!(source.current_time(), Some(now));

    // Second event
    let event2 = source.next_event().await.unwrap().unwrap();
    assert_eq!(event2.timestamp(), now + Duration::seconds(1));

    // Third event
    let event3 = source.next_event().await.unwrap().unwrap();
    assert_eq!(event3.timestamp(), now + Duration::seconds(2));

    // No more events
    let event4 = source.next_event().await.unwrap();
    assert!(event4.is_none());
    assert!(!source.has_more());
}

#[tokio::test]
async fn test_mock_replay_source_shutdown() {
    let now = Utc::now();
    let events = vec![
        MarketEvent::Heartbeat(now),
        MarketEvent::Heartbeat(now + Duration::seconds(1)),
    ];

    let mut source = MockReplaySource::new(events);
    assert!(source.has_more());

    source.shutdown().await;

    assert!(!source.has_more());
}

#[tokio::test]
async fn test_mock_replay_source_empty() {
    let mut source = MockReplaySource::new(vec![]);

    // Should return None immediately
    let event = source.next_event().await.unwrap();
    assert!(event.is_none());
    assert!(!source.has_more());
}

#[tokio::test]
async fn test_mock_replay_source_mixed_events() {
    let now = Utc::now();
    let events = vec![
        MarketEvent::SpotPrice(SpotPriceEvent {
            asset: CryptoAsset::Btc,
            price: dec!(100000),
            quantity: dec!(1),
            timestamp: now,
        }),
        MarketEvent::BookSnapshot(BookSnapshotEvent {
            token_id: "token".to_string(),
            event_id: "event".to_string(),
            bids: vec![],
            asks: vec![],
            timestamp: now + Duration::milliseconds(100),
        }),
        MarketEvent::WindowOpen(WindowOpenEvent {
            event_id: "event".to_string(),
            asset: CryptoAsset::Btc,
            yes_token_id: "yes".to_string(),
            no_token_id: "no".to_string(),
            strike_price: dec!(100000),
            window_start: now,
            window_end: now + Duration::minutes(15),
            timestamp: now + Duration::milliseconds(200),
        }),
    ];

    let mut source = MockReplaySource::new(events);
    let mut count = 0;

    while let Ok(Some(_)) = source.next_event().await {
        count += 1;
    }

    assert_eq!(count, 3);
}
