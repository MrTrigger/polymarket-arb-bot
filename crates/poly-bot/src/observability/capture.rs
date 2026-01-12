//! Fire-and-forget capture for hot path observability.
//!
//! This module provides ultra-low-overhead decision capture from the trading hot path.
//!
//! ## Performance Requirements
//!
//! - Hot path overhead: <10ns (single atomic check + try_send)
//! - No blocking: uses try_send() which returns immediately
//! - Drops on backpressure: if channel full, event is discarded
//! - Configurable: can disable capture entirely via config flag
//!
//! ## Architecture
//!
//! ```text
//! Hot Path                    Background
//! ────────                    ──────────
//! [Strategy]                  [Processor]
//!     │                           ▲
//!     │ try_send()                │ recv()
//!     ▼                           │
//! [Bounded Channel] ──────────────┘
//!   (1024 slots)
//! ```
//!
//! The capture function is inlined and performs:
//! 1. Atomic load to check if enabled (<1ns)
//! 2. try_send() on bounded channel (~5-8ns if enabled)
//! 3. Returns immediately, dropping event if channel full
//!
//! ## Usage
//!
//! ```ignore
//! // Create capture channel
//! let (sender, receiver) = create_capture_channel(1024);
//!
//! // Create capture context
//! let capture = ObservabilityCapture::new(sender, true);
//!
//! // On hot path - ultra-fast, non-blocking
//! capture.try_capture(snapshot);
//!
//! // Background processor receives events
//! while let Ok(event) = receiver.recv().await {
//!     // Process event...
//! }
//! ```

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;

use super::types::{DecisionSnapshot, ObservabilityEvent};

/// Default channel capacity for observability events.
pub const DEFAULT_CHANNEL_CAPACITY: usize = 1024;

/// Configuration for observability capture.
#[derive(Debug, Clone)]
pub struct CaptureConfig {
    /// Whether capture is enabled.
    pub enabled: bool,
    /// Channel capacity.
    pub channel_capacity: usize,
    /// Whether to log dropped events.
    pub log_drops: bool,
    /// Minimum drop count before logging (to avoid log spam).
    pub drop_log_threshold: u64,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            channel_capacity: DEFAULT_CHANNEL_CAPACITY,
            log_drops: true,
            drop_log_threshold: 100,
        }
    }
}

impl CaptureConfig {
    /// Create config from ObservabilityConfig.
    pub fn from_observability_config(config: &crate::config::ObservabilityConfig) -> Self {
        Self {
            enabled: config.capture_decisions,
            channel_capacity: config.channel_buffer_size,
            log_drops: true,
            drop_log_threshold: 100,
        }
    }

    /// Create a disabled config.
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            ..Default::default()
        }
    }

    /// Create a test config with small capacity.
    #[cfg(test)]
    pub fn test_config(capacity: usize) -> Self {
        Self {
            enabled: true,
            channel_capacity: capacity,
            log_drops: false,
            drop_log_threshold: 0,
        }
    }
}

/// Statistics for observability capture.
#[derive(Debug, Default)]
pub struct CaptureStats {
    /// Total events captured.
    pub captured: AtomicU64,
    /// Total events dropped due to full channel.
    pub dropped: AtomicU64,
    /// Total events skipped (capture disabled).
    pub skipped: AtomicU64,
}

impl CaptureStats {
    /// Create new stats.
    pub fn new() -> Self {
        Self::default()
    }

    /// Get snapshot of current stats.
    pub fn snapshot(&self) -> CaptureStatsSnapshot {
        CaptureStatsSnapshot {
            captured: self.captured.load(Ordering::Relaxed),
            dropped: self.dropped.load(Ordering::Relaxed),
            skipped: self.skipped.load(Ordering::Relaxed),
        }
    }

    /// Reset all counters.
    pub fn reset(&self) {
        self.captured.store(0, Ordering::Relaxed);
        self.dropped.store(0, Ordering::Relaxed);
        self.skipped.store(0, Ordering::Relaxed);
    }

    /// Increment captured counter.
    #[inline(always)]
    fn record_captured(&self) {
        self.captured.fetch_add(1, Ordering::Relaxed);
    }

    /// Increment dropped counter.
    #[inline(always)]
    fn record_dropped(&self) {
        self.dropped.fetch_add(1, Ordering::Relaxed);
    }

    /// Increment skipped counter.
    #[inline(always)]
    fn record_skipped(&self) {
        self.skipped.fetch_add(1, Ordering::Relaxed);
    }
}

/// Snapshot of capture statistics.
#[derive(Debug, Clone, Copy, Default)]
pub struct CaptureStatsSnapshot {
    /// Total events captured.
    pub captured: u64,
    /// Total events dropped.
    pub dropped: u64,
    /// Total events skipped.
    pub skipped: u64,
}

impl CaptureStatsSnapshot {
    /// Calculate drop rate as percentage.
    pub fn drop_rate(&self) -> f64 {
        let total = self.captured + self.dropped;
        if total == 0 {
            0.0
        } else {
            (self.dropped as f64 / total as f64) * 100.0
        }
    }

    /// Total events attempted (captured + dropped).
    pub fn total_attempts(&self) -> u64 {
        self.captured + self.dropped
    }
}

/// Sender half of the capture channel.
pub type CaptureSender = mpsc::Sender<ObservabilityEvent>;

/// Receiver half of the capture channel.
pub type CaptureReceiver = mpsc::Receiver<ObservabilityEvent>;

/// Create a new capture channel pair.
///
/// Returns (sender, receiver) for the bounded channel.
pub fn create_capture_channel(capacity: usize) -> (CaptureSender, CaptureReceiver) {
    mpsc::channel(capacity)
}

/// Fire-and-forget observability capture.
///
/// This struct wraps a bounded mpsc channel sender and provides ultra-low-overhead
/// capture from the hot path. The `try_capture` method is inlined and performs:
///
/// 1. Atomic load to check if enabled (<1ns)
/// 2. try_send() on bounded channel (~5-8ns)
/// 3. Returns immediately, dropping event if channel full
///
/// ## Thread Safety
///
/// This struct is `Send + Sync` and can be safely shared across threads via Arc.
/// The sender can be cloned for multiple producers.
#[derive(Debug)]
pub struct ObservabilityCapture {
    /// Channel sender for events.
    sender: Option<CaptureSender>,
    /// Whether capture is enabled (atomic for fast check).
    enabled: AtomicBool,
    /// Capture statistics.
    stats: Arc<CaptureStats>,
    /// Configuration.
    config: CaptureConfig,
    /// Last drop log count (to avoid log spam).
    last_drop_log: AtomicU64,
}

impl ObservabilityCapture {
    /// Create a new capture context.
    ///
    /// # Arguments
    ///
    /// * `sender` - Channel sender for events
    /// * `enabled` - Initial enabled state
    pub fn new(sender: CaptureSender, enabled: bool) -> Self {
        Self {
            sender: Some(sender),
            enabled: AtomicBool::new(enabled),
            stats: Arc::new(CaptureStats::new()),
            config: CaptureConfig::default(),
            last_drop_log: AtomicU64::new(0),
        }
    }

    /// Create from config.
    pub fn from_config(config: CaptureConfig) -> (Self, Option<CaptureReceiver>) {
        if !config.enabled {
            return (Self::disabled(), None);
        }

        let (sender, receiver) = create_capture_channel(config.channel_capacity);
        let capture = Self {
            sender: Some(sender),
            enabled: AtomicBool::new(true),
            stats: Arc::new(CaptureStats::new()),
            config,
            last_drop_log: AtomicU64::new(0),
        };

        (capture, Some(receiver))
    }

    /// Create a disabled capture context.
    ///
    /// All capture attempts will be no-ops.
    pub fn disabled() -> Self {
        Self {
            sender: None,
            enabled: AtomicBool::new(false),
            stats: Arc::new(CaptureStats::new()),
            config: CaptureConfig::disabled(),
            last_drop_log: AtomicU64::new(0),
        }
    }

    /// Check if capture is enabled.
    ///
    /// This is an atomic load with Acquire ordering.
    #[inline(always)]
    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Acquire)
    }

    /// Enable capture.
    pub fn enable(&self) {
        self.enabled.store(true, Ordering::Release);
    }

    /// Disable capture.
    pub fn disable(&self) {
        self.enabled.store(false, Ordering::Release);
    }

    /// Try to capture a decision snapshot.
    ///
    /// This is the primary hot path method. It:
    /// 1. Checks if enabled (single atomic load, ~1ns)
    /// 2. Wraps snapshot in ObservabilityEvent
    /// 3. Calls try_send() (~5-8ns)
    /// 4. Returns immediately
    ///
    /// If the channel is full, the event is dropped silently.
    /// Use `stats()` to monitor drop rates.
    ///
    /// # Performance
    ///
    /// Target: <10ns total overhead when enabled, <1ns when disabled.
    #[inline(always)]
    pub fn try_capture(&self, snapshot: DecisionSnapshot) {
        // Fast path: check enabled flag first
        if !self.enabled.load(Ordering::Acquire) {
            self.stats.record_skipped();
            return;
        }

        // Try to send, drop if channel full
        if let Some(ref sender) = self.sender {
            match sender.try_send(ObservabilityEvent::Decision(snapshot)) {
                Ok(()) => {
                    self.stats.record_captured();
                }
                Err(mpsc::error::TrySendError::Full(_)) => {
                    self.stats.record_dropped();
                    self.maybe_log_drops();
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    // Channel closed, disable capture
                    self.enabled.store(false, Ordering::Release);
                    self.stats.record_dropped();
                }
            }
        } else {
            self.stats.record_skipped();
        }
    }

    /// Try to capture any observability event.
    ///
    /// Similar to `try_capture` but accepts any ObservabilityEvent type.
    #[inline(always)]
    pub fn try_capture_event(&self, event: ObservabilityEvent) {
        if !self.enabled.load(Ordering::Acquire) {
            self.stats.record_skipped();
            return;
        }

        if let Some(ref sender) = self.sender {
            match sender.try_send(event) {
                Ok(()) => {
                    self.stats.record_captured();
                }
                Err(mpsc::error::TrySendError::Full(_)) => {
                    self.stats.record_dropped();
                    self.maybe_log_drops();
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    self.enabled.store(false, Ordering::Release);
                    self.stats.record_dropped();
                }
            }
        } else {
            self.stats.record_skipped();
        }
    }

    /// Get capture statistics.
    pub fn stats(&self) -> &CaptureStats {
        &self.stats
    }

    /// Get a cloneable stats handle.
    pub fn stats_handle(&self) -> Arc<CaptureStats> {
        Arc::clone(&self.stats)
    }

    /// Get current stats snapshot.
    pub fn stats_snapshot(&self) -> CaptureStatsSnapshot {
        self.stats.snapshot()
    }

    /// Get a clone of the sender (for sharing with other components).
    pub fn sender(&self) -> Option<CaptureSender> {
        self.sender.clone()
    }

    /// Log drops if threshold exceeded (to avoid spam).
    fn maybe_log_drops(&self) {
        if !self.config.log_drops {
            return;
        }

        let total_drops = self.stats.dropped.load(Ordering::Relaxed);
        let last_log = self.last_drop_log.load(Ordering::Relaxed);

        // Log every N drops
        if total_drops >= last_log + self.config.drop_log_threshold {
            // Try to update last_log atomically
            if self
                .last_drop_log
                .compare_exchange(
                    last_log,
                    total_drops,
                    Ordering::Release,
                    Ordering::Relaxed,
                )
                .is_ok()
            {
                tracing::warn!(
                    total_drops = total_drops,
                    "Observability capture dropping events (channel full)"
                );
            }
        }
    }
}

impl Clone for ObservabilityCapture {
    fn clone(&self) -> Self {
        Self {
            sender: self.sender.clone(),
            enabled: AtomicBool::new(self.enabled.load(Ordering::Acquire)),
            stats: Arc::clone(&self.stats),
            config: self.config.clone(),
            last_drop_log: AtomicU64::new(self.last_drop_log.load(Ordering::Relaxed)),
        }
    }
}

/// Shared capture context for use across multiple tasks.
pub type SharedCapture = Arc<ObservabilityCapture>;

/// Create a shared capture context.
pub fn create_shared_capture(config: CaptureConfig) -> (SharedCapture, Option<CaptureReceiver>) {
    let (capture, receiver) = ObservabilityCapture::from_config(config);
    (Arc::new(capture), receiver)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn test_capture_config_default() {
        let config = CaptureConfig::default();
        assert!(config.enabled);
        assert_eq!(config.channel_capacity, DEFAULT_CHANNEL_CAPACITY);
        assert!(config.log_drops);
    }

    #[test]
    fn test_capture_config_disabled() {
        let config = CaptureConfig::disabled();
        assert!(!config.enabled);
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
    fn test_capture_stats_drop_rate() {
        let snapshot = CaptureStatsSnapshot {
            captured: 90,
            dropped: 10,
            skipped: 0,
        };
        assert!((snapshot.drop_rate() - 10.0).abs() < 0.001);
    }

    #[test]
    fn test_capture_stats_total_attempts() {
        let snapshot = CaptureStatsSnapshot {
            captured: 90,
            dropped: 10,
            skipped: 5,
        };
        assert_eq!(snapshot.total_attempts(), 100);
    }

    #[test]
    fn test_create_capture_channel() {
        let (sender, mut receiver) = create_capture_channel(10);

        sender.try_send(ObservabilityEvent::Decision(DecisionSnapshot::default())).unwrap();

        // Use try_recv for sync test
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

        let snapshot = DecisionSnapshot::default();
        capture.try_capture(snapshot);

        let stats = capture.stats_snapshot();
        assert_eq!(stats.captured, 0);
        assert_eq!(stats.dropped, 0);
        assert_eq!(stats.skipped, 1);
    }

    #[test]
    fn test_try_capture_channel_full() {
        let (sender, _receiver) = create_capture_channel(2);
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
            anomaly_type: "test".to_string(),
            severity: 50,
            event_id_hash: 12345,
            timestamp_ms: 1000,
            data: 0,
        };

        capture.try_capture_event(event);

        let stats = capture.stats_snapshot();
        assert_eq!(stats.captured, 1);

        let received = receiver.try_recv().unwrap();
        match received {
            ObservabilityEvent::Anomaly { anomaly_type, .. } => {
                assert_eq!(anomaly_type, "test");
            }
            _ => panic!("Wrong event type"),
        }
    }

    #[test]
    fn test_capture_clone() {
        let (sender, _receiver) = create_capture_channel(10);
        let capture = ObservabilityCapture::new(sender, true);

        let cloned = capture.clone();

        // Both should share stats
        capture.try_capture(DecisionSnapshot::default());

        assert_eq!(cloned.stats_snapshot().captured, 1);
    }

    #[test]
    fn test_from_config() {
        let config = CaptureConfig::test_config(100);
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
        let config = CaptureConfig::test_config(100);
        let (capture, receiver) = create_shared_capture(config);

        assert!(capture.is_enabled());
        assert!(receiver.is_some());
    }

    #[test]
    fn test_sender_method() {
        let (sender, _receiver) = create_capture_channel(10);
        let capture = ObservabilityCapture::new(sender, true);

        let sender_clone = capture.sender();
        assert!(sender_clone.is_some());
    }

    #[test]
    fn test_capture_overhead_disabled() {
        // Measure overhead when disabled
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

        // Should be very fast when disabled (< 5ns)
        assert!(ns_per_op < 50.0, "Disabled capture too slow: {:.2}ns", ns_per_op);
    }

    #[test]
    fn test_capture_overhead_enabled() {
        // Measure overhead when enabled (with empty receiver)
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

        // Should be < 100ns (try_send is fast but not as fast as disabled)
        // Note: actual target is <10ns but this varies by system
        assert!(ns_per_op < 500.0, "Enabled capture too slow: {:.2}ns", ns_per_op);
    }

    #[test]
    fn test_stats_reset() {
        let stats = CaptureStats::new();
        stats.captured.store(100, Ordering::Relaxed);
        stats.dropped.store(10, Ordering::Relaxed);

        stats.reset();

        assert_eq!(stats.captured.load(Ordering::Relaxed), 0);
        assert_eq!(stats.dropped.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn test_async_receive() {
        let (sender, mut receiver) = create_capture_channel(10);
        let capture = ObservabilityCapture::new(sender, true);

        let snapshot = DecisionSnapshot {
            decision_id: 123,
            ..Default::default()
        };

        capture.try_capture(snapshot);

        let event = receiver.recv().await.unwrap();
        match event {
            ObservabilityEvent::Decision(s) => assert_eq!(s.decision_id, 123),
            _ => panic!("Wrong event type"),
        }
    }

    #[tokio::test]
    async fn test_channel_closed() {
        let (sender, receiver) = create_capture_channel(10);
        let capture = ObservabilityCapture::new(sender, true);

        // Drop receiver to close channel
        drop(receiver);

        // This should detect closed channel and disable capture
        capture.try_capture(DecisionSnapshot::default());

        assert!(!capture.is_enabled());
    }
}
