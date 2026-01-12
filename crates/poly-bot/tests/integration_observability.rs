//! Integration tests for the observability pipeline.
//!
//! These tests verify:
//! - Fire-and-forget capture with minimal overhead
//! - Channel behavior under load
//! - Stats tracking and reporting
//! - Full pipeline from capture to processing

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

use chrono::Utc;
use rust_decimal_macros::dec;

use poly_bot::observability::{
    CaptureConfig, CaptureStats, CaptureStatsSnapshot, DecisionSnapshot, ObservabilityCapture,
    ObservabilityEvent, SnapshotBuilder, create_capture_channel, create_shared_capture,
    DEFAULT_CHANNEL_CAPACITY,
};

#[test]
fn test_capture_config_default() {
    let config = CaptureConfig::default();

    assert!(config.enabled);
    assert_eq!(config.channel_capacity, DEFAULT_CHANNEL_CAPACITY);
    assert!(config.log_drops);
    assert_eq!(config.drop_log_threshold, 100);
}

#[test]
fn test_capture_config_disabled() {
    let config = CaptureConfig::disabled();

    assert!(!config.enabled);
}

#[test]
fn test_capture_stats_new() {
    let stats = CaptureStats::new();

    assert_eq!(stats.captured.load(Ordering::Relaxed), 0);
    assert_eq!(stats.dropped.load(Ordering::Relaxed), 0);
    assert_eq!(stats.skipped.load(Ordering::Relaxed), 0);
}

#[test]
fn test_capture_stats_snapshot() {
    let stats = CaptureStats::new();
    stats.captured.store(100, Ordering::Relaxed);
    stats.dropped.store(10, Ordering::Relaxed);
    stats.skipped.store(5, Ordering::Relaxed);

    let snapshot = stats.snapshot();
    assert_eq!(snapshot.captured, 100);
    assert_eq!(snapshot.dropped, 10);
    assert_eq!(snapshot.skipped, 5);
}

#[test]
fn test_capture_stats_snapshot_drop_rate() {
    // 10% drop rate
    let snapshot = CaptureStatsSnapshot {
        captured: 90,
        dropped: 10,
        skipped: 0,
    };
    assert!((snapshot.drop_rate() - 10.0).abs() < 0.001);

    // 0% drop rate
    let snapshot_no_drops = CaptureStatsSnapshot {
        captured: 100,
        dropped: 0,
        skipped: 0,
    };
    assert_eq!(snapshot_no_drops.drop_rate(), 0.0);

    // Handle zero total
    let snapshot_empty = CaptureStatsSnapshot {
        captured: 0,
        dropped: 0,
        skipped: 0,
    };
    assert_eq!(snapshot_empty.drop_rate(), 0.0);
}

#[test]
fn test_capture_stats_total_attempts() {
    let snapshot = CaptureStatsSnapshot {
        captured: 80,
        dropped: 20,
        skipped: 5, // skipped doesn't count as attempt
    };
    assert_eq!(snapshot.total_attempts(), 100);
}

#[test]
fn test_capture_stats_reset() {
    let stats = CaptureStats::new();
    stats.captured.store(100, Ordering::Relaxed);
    stats.dropped.store(50, Ordering::Relaxed);
    stats.skipped.store(25, Ordering::Relaxed);

    stats.reset();

    assert_eq!(stats.captured.load(Ordering::Relaxed), 0);
    assert_eq!(stats.dropped.load(Ordering::Relaxed), 0);
    assert_eq!(stats.skipped.load(Ordering::Relaxed), 0);
}

#[test]
fn test_create_capture_channel() {
    let (sender, mut receiver) = create_capture_channel(10);

    // Send an event
    sender
        .try_send(ObservabilityEvent::Decision(DecisionSnapshot::default()))
        .unwrap();

    // Receive it
    let event = receiver.try_recv().unwrap();
    match event {
        ObservabilityEvent::Decision(_) => {}
        _ => panic!("Wrong event type"),
    }
}

#[test]
fn test_observability_capture_new() {
    let (sender, _receiver) = create_capture_channel(10);
    let capture = ObservabilityCapture::new(sender, true);

    assert!(capture.is_enabled());
}

#[test]
fn test_observability_capture_disabled() {
    let capture = ObservabilityCapture::disabled();

    assert!(!capture.is_enabled());
    assert!(capture.sender().is_none());
}

#[test]
fn test_observability_capture_enable_disable() {
    let (sender, _receiver) = create_capture_channel(10);
    let capture = ObservabilityCapture::new(sender, true);

    assert!(capture.is_enabled());

    capture.disable();
    assert!(!capture.is_enabled());

    capture.enable();
    assert!(capture.is_enabled());
}

#[test]
fn test_try_capture_enabled() {
    let (sender, mut receiver) = create_capture_channel(10);
    let capture = ObservabilityCapture::new(sender, true);

    let snapshot = DecisionSnapshot {
        decision_id: 42,
        ..Default::default()
    };

    capture.try_capture(snapshot);

    let stats = capture.stats_snapshot();
    assert_eq!(stats.captured, 1);
    assert_eq!(stats.dropped, 0);
    assert_eq!(stats.skipped, 0);

    let event = receiver.try_recv().unwrap();
    match event {
        ObservabilityEvent::Decision(s) => assert_eq!(s.decision_id, 42),
        _ => panic!("Wrong event type"),
    }
}

#[test]
fn test_try_capture_disabled() {
    let capture = ObservabilityCapture::disabled();

    capture.try_capture(DecisionSnapshot::default());

    let stats = capture.stats_snapshot();
    assert_eq!(stats.captured, 0);
    assert_eq!(stats.dropped, 0);
    assert_eq!(stats.skipped, 1);
}

#[test]
fn test_try_capture_channel_full() {
    let (sender, _receiver) = create_capture_channel(2); // Small capacity
    let capture = ObservabilityCapture::new(sender, true);

    // Fill the channel
    capture.try_capture(DecisionSnapshot::default());
    capture.try_capture(DecisionSnapshot::default());

    // This should drop
    capture.try_capture(DecisionSnapshot::default());

    let stats = capture.stats_snapshot();
    assert_eq!(stats.captured, 2);
    assert_eq!(stats.dropped, 1);
}

#[test]
fn test_try_capture_event() {
    let (sender, mut receiver) = create_capture_channel(10);
    let capture = ObservabilityCapture::new(sender, true);

    let event = ObservabilityEvent::Anomaly {
        anomaly_type: "test_anomaly".to_string(),
        severity: 75,
        event_id_hash: 12345,
        timestamp_ms: 1000,
        data: 0,
    };

    capture.try_capture_event(event);

    let stats = capture.stats_snapshot();
    assert_eq!(stats.captured, 1);

    let received = receiver.try_recv().unwrap();
    match received {
        ObservabilityEvent::Anomaly { anomaly_type, severity, .. } => {
            assert_eq!(anomaly_type, "test_anomaly");
            assert_eq!(severity, 75);
        }
        _ => panic!("Wrong event type"),
    }
}

#[test]
fn test_capture_clone_shares_stats() {
    let (sender, _receiver) = create_capture_channel(100);
    let capture = ObservabilityCapture::new(sender, true);

    let cloned = capture.clone();

    // Capture via original
    capture.try_capture(DecisionSnapshot::default());
    capture.try_capture(DecisionSnapshot::default());

    // Both should see same stats
    assert_eq!(capture.stats_snapshot().captured, 2);
    assert_eq!(cloned.stats_snapshot().captured, 2);
}

#[test]
fn test_from_config_enabled() {
    let config = CaptureConfig {
        enabled: true,
        channel_capacity: 100,
        log_drops: false,
        drop_log_threshold: 0,
    };

    let (capture, receiver) = ObservabilityCapture::from_config(config);

    assert!(capture.is_enabled());
    assert!(receiver.is_some());
}

#[test]
fn test_from_config_disabled() {
    let config = CaptureConfig::disabled();
    let (capture, receiver) = ObservabilityCapture::from_config(config);

    assert!(!capture.is_enabled());
    assert!(receiver.is_none());
}

#[test]
fn test_create_shared_capture() {
    let config = CaptureConfig {
        enabled: true,
        channel_capacity: 50,
        log_drops: false,
        drop_log_threshold: 0,
    };

    let (capture, receiver) = create_shared_capture(config);

    assert!(capture.is_enabled());
    assert!(receiver.is_some());

    // Can be cloned without issue
    let _capture2 = Arc::clone(&capture);
}

#[test]
fn test_sender_method() {
    let (sender, _receiver) = create_capture_channel(10);
    let capture = ObservabilityCapture::new(sender, true);

    let sender_clone = capture.sender();
    assert!(sender_clone.is_some());

    // Can send via cloned sender
    sender_clone
        .unwrap()
        .try_send(ObservabilityEvent::Decision(DecisionSnapshot::default()))
        .unwrap();
}

#[test]
fn test_stats_handle() {
    let (sender, _receiver) = create_capture_channel(10);
    let capture = ObservabilityCapture::new(sender, true);

    let stats_handle = capture.stats_handle();

    // Capture some events
    capture.try_capture(DecisionSnapshot::default());
    capture.try_capture(DecisionSnapshot::default());

    // Stats handle should show same counts
    assert_eq!(stats_handle.snapshot().captured, 2);
}

#[test]
fn test_decision_snapshot_default() {
    let snapshot = DecisionSnapshot::default();

    assert_eq!(snapshot.decision_id, 0);
    assert_eq!(snapshot.event_id_hash, 0);
    assert_eq!(snapshot.timestamp_ms, 0);
}

#[test]
fn test_decision_snapshot_builder() {
    let now = Utc::now();

    // Use the actual SnapshotBuilder API
    let snapshot = SnapshotBuilder::new()
        .decision_id(123)
        .event_id("test-event-456") // Uses string, hashed internally
        .timestamp(now)
        .margin(dec!(0.03))
        .size(dec!(50))
        .build();

    assert_eq!(snapshot.decision_id, 123);
    assert!(snapshot.event_id_hash != 0); // Was hashed from string
    assert_eq!(snapshot.timestamp_ms, now.timestamp_millis());
    assert_eq!(snapshot.margin_bps, 300); // 0.03 * 10000
    assert_eq!(snapshot.size_cents, 5000); // 50.00 * 100
}

#[test]
fn test_capture_overhead_disabled() {
    let capture = ObservabilityCapture::disabled();
    let snapshot = DecisionSnapshot::default();

    // Warm up
    for _ in 0..1000 {
        capture.try_capture(snapshot);
    }

    // Measure
    let iterations = 100_000;
    let start = Instant::now();
    for _ in 0..iterations {
        capture.try_capture(snapshot);
    }
    let elapsed = start.elapsed();

    let ns_per_op = elapsed.as_nanos() as f64 / iterations as f64;
    println!("Disabled capture overhead: {:.2}ns per operation", ns_per_op);

    // Should be very fast when disabled
    assert!(ns_per_op < 100.0, "Disabled capture too slow: {:.2}ns", ns_per_op);
}

#[test]
fn test_capture_overhead_enabled() {
    let (sender, _receiver) = create_capture_channel(100_000);
    let capture = ObservabilityCapture::new(sender, true);
    let snapshot = DecisionSnapshot::default();

    // Warm up
    for _ in 0..1000 {
        capture.try_capture(snapshot);
    }

    // Measure
    let iterations = 10_000;
    let start = Instant::now();
    for _ in 0..iterations {
        capture.try_capture(snapshot);
    }
    let elapsed = start.elapsed();

    let ns_per_op = elapsed.as_nanos() as f64 / iterations as f64;
    println!("Enabled capture overhead: {:.2}ns per operation", ns_per_op);

    // Should still be reasonably fast
    assert!(ns_per_op < 1000.0, "Enabled capture too slow: {:.2}ns", ns_per_op);
}

#[tokio::test]
async fn test_async_receive() {
    let (sender, mut receiver) = create_capture_channel(10);
    let capture = ObservabilityCapture::new(sender, true);

    let snapshot = DecisionSnapshot {
        decision_id: 999,
        ..Default::default()
    };

    capture.try_capture(snapshot);

    let event = receiver.recv().await.unwrap();
    match event {
        ObservabilityEvent::Decision(s) => assert_eq!(s.decision_id, 999),
        _ => panic!("Wrong event type"),
    }
}

#[tokio::test]
async fn test_channel_closed_detection() {
    let (sender, receiver) = create_capture_channel(10);
    let capture = ObservabilityCapture::new(sender, true);

    // Drop receiver to close channel
    drop(receiver);

    // This should detect closed channel and disable capture
    capture.try_capture(DecisionSnapshot::default());

    assert!(!capture.is_enabled());
}

#[tokio::test]
async fn test_multiple_producers() {
    let (sender, mut receiver) = create_capture_channel(100);

    // Create multiple captures sharing same channel
    let capture1 = Arc::new(ObservabilityCapture::new(sender.clone(), true));
    let capture2 = Arc::new(ObservabilityCapture::new(sender.clone(), true));
    let capture3 = Arc::new(ObservabilityCapture::new(sender, true));

    // Each sends events
    for i in 0..10 {
        capture1.try_capture(DecisionSnapshot {
            decision_id: i,
            ..Default::default()
        });
        capture2.try_capture(DecisionSnapshot {
            decision_id: 100 + i,
            ..Default::default()
        });
        capture3.try_capture(DecisionSnapshot {
            decision_id: 200 + i,
            ..Default::default()
        });
    }

    // Should receive all 30 events
    let mut count = 0;
    while let Ok(event) = receiver.try_recv() {
        match event {
            ObservabilityEvent::Decision(_) => count += 1,
            _ => {}
        }
    }

    assert_eq!(count, 30);
}

#[tokio::test]
async fn test_backpressure_handling() {
    let (sender, receiver) = create_capture_channel(5); // Very small capacity
    let capture = ObservabilityCapture::new(sender, true);

    // Send more events than capacity
    for i in 0..20 {
        capture.try_capture(DecisionSnapshot {
            decision_id: i,
            ..Default::default()
        });
    }

    let stats = capture.stats_snapshot();

    // Some should have been captured, some dropped
    assert!(stats.captured > 0);
    assert!(stats.dropped > 0);
    assert_eq!(stats.captured + stats.dropped, 20);

    // Channel should be at capacity
    drop(receiver); // Allow cleanup
}

#[tokio::test]
async fn test_observability_event_variants() {
    let (sender, mut receiver) = create_capture_channel(100);
    let capture = ObservabilityCapture::new(sender, true);

    // Send Decision event
    capture.try_capture_event(ObservabilityEvent::Decision(DecisionSnapshot::default()));

    // Send Anomaly event
    capture.try_capture_event(ObservabilityEvent::Anomaly {
        anomaly_type: "flash_crash".to_string(),
        severity: 90,
        event_id_hash: 12345,
        timestamp_ms: 1000,
        data: 100,
    });

    // Receive and verify
    let mut decisions = 0;
    let mut anomalies = 0;

    while let Ok(event) = receiver.try_recv() {
        match event {
            ObservabilityEvent::Decision(_) => decisions += 1,
            ObservabilityEvent::Anomaly { .. } => anomalies += 1,
            _ => {}
        }
    }

    assert_eq!(decisions, 1);
    assert_eq!(anomalies, 1);
}

#[tokio::test]
async fn test_high_throughput() {
    let (sender, mut receiver) = create_capture_channel(10_000);
    let capture = Arc::new(ObservabilityCapture::new(sender, true));

    // Spawn producer task
    let capture_clone = Arc::clone(&capture);
    let producer = tokio::spawn(async move {
        for i in 0..1000 {
            capture_clone.try_capture(DecisionSnapshot {
                decision_id: i,
                ..Default::default()
            });
        }
    });

    // Spawn consumer task
    let consumer = tokio::spawn(async move {
        let mut count = 0;
        while let Ok(Some(_)) = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            async { receiver.recv().await }
        ).await {
            count += 1;
            if count >= 1000 {
                break;
            }
        }
        count
    });

    producer.await.unwrap();
    let received = consumer.await.unwrap();

    // Should receive most events
    assert!(received >= 900, "Expected ~1000 events, got {}", received);
}

#[test]
fn test_snapshot_builder_all_fields() {
    let now = Utc::now();

    let snapshot = SnapshotBuilder::new()
        .decision_id(1)
        .event_id("test-event")
        .yes_token_id("yes-token")
        .no_token_id("no-token")
        .timestamp(now)
        .latency_us(12)
        .seconds_remaining(300)
        .yes_ask(dec!(0.45))
        .no_ask(dec!(0.52))
        .combined_cost(dec!(0.97))
        .margin(dec!(0.03))
        .threshold(dec!(0.025))
        .size(dec!(50))
        .expected_cost(dec!(48.50))
        .expected_profit(dec!(1.50))
        .inventory(dec!(100), dec!(80), dec!(170))
        .imbalance(dec!(0.11))
        .confidence(75)
        .toxic_severity(2)
        .build();

    assert_eq!(snapshot.decision_id, 1);
    assert!(snapshot.event_id_hash != 0);
    assert!(snapshot.yes_token_hash != 0);
    assert!(snapshot.no_token_hash != 0);
    assert_eq!(snapshot.latency_us, 12);
    assert_eq!(snapshot.seconds_remaining, 300);
    assert_eq!(snapshot.yes_ask_bps, 4500);
    assert_eq!(snapshot.no_ask_bps, 5200);
    assert_eq!(snapshot.combined_cost_bps, 9700);
    assert_eq!(snapshot.margin_bps, 300);
    assert_eq!(snapshot.threshold_bps, 250);
    assert_eq!(snapshot.size_cents, 5000);
    assert_eq!(snapshot.expected_cost_cents, 4850);
    assert_eq!(snapshot.expected_profit_cents, 150);
    assert_eq!(snapshot.yes_shares_cents, 10000);
    assert_eq!(snapshot.no_shares_cents, 8000);
    assert_eq!(snapshot.exposure_cents, 17000);
    assert_eq!(snapshot.imbalance_pct, 11);
    assert_eq!(snapshot.confidence, 75);
    assert_eq!(snapshot.toxic_severity, 2);
}

#[test]
fn test_snapshot_is_copy() {
    let snapshot = DecisionSnapshot::default();
    let _copy = snapshot; // Should work because DecisionSnapshot is Copy

    // Original still usable
    assert_eq!(snapshot.decision_id, 0);
}

#[test]
fn test_decision_snapshot_accessors() {
    let snapshot = SnapshotBuilder::new()
        .yes_ask(dec!(0.4567))
        .no_ask(dec!(0.5123))
        .margin(dec!(0.0310))
        .size(dec!(75.50))
        .build();

    assert_eq!(snapshot.yes_ask(), dec!(0.4567));
    assert_eq!(snapshot.no_ask(), dec!(0.5123));
    assert_eq!(snapshot.margin(), dec!(0.031));
    assert_eq!(snapshot.size(), dec!(75.5));
}

#[test]
fn test_snapshot_was_executed() {
    use poly_bot::observability::ActionType;

    let executed = SnapshotBuilder::new()
        .action(ActionType::Execute)
        .build();
    assert!(executed.was_executed());

    let skipped = SnapshotBuilder::new()
        .action(ActionType::SkipToxic)
        .build();
    assert!(!skipped.was_executed());
}
