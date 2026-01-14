//! Trade interval enforcement for API rate limiting.
//!
//! The `TradeInterval` struct enforces a minimum time between trades
//! to avoid API rate limits and ensure orderly execution.
//!
//! ## Usage
//!
//! ```ignore
//! let mut interval = TradeInterval::new(Duration::from_millis(500));
//!
//! // Check if we can trade
//! if interval.can_trade() {
//!     // Execute trade...
//!     interval.record_trade();
//! } else {
//!     // Wait for time_until_next()
//!     let wait = interval.time_until_next();
//!     tokio::time::sleep(wait).await;
//! }
//! ```

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// Default minimum interval between trades (500ms).
pub const DEFAULT_MIN_INTERVAL_MS: u64 = 500;

/// Configuration for trade interval enforcement.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TradeIntervalConfig {
    /// Minimum interval between trades in milliseconds.
    pub min_interval_ms: u64,
    /// Whether interval enforcement is enabled.
    pub enabled: bool,
}

impl Default for TradeIntervalConfig {
    fn default() -> Self {
        Self {
            min_interval_ms: DEFAULT_MIN_INTERVAL_MS,
            enabled: true,
        }
    }
}

impl TradeIntervalConfig {
    /// Create a new config with specified interval.
    pub fn new(min_interval_ms: u64) -> Self {
        Self {
            min_interval_ms,
            enabled: true,
        }
    }

    /// Create a disabled config (no interval enforcement).
    pub fn disabled() -> Self {
        Self {
            min_interval_ms: 0,
            enabled: false,
        }
    }

    /// Get the minimum interval as a Duration.
    pub fn min_interval(&self) -> Duration {
        Duration::from_millis(self.min_interval_ms)
    }
}

/// Enforces minimum time between trades.
///
/// Uses atomic operations for thread-safe tracking of the last trade time.
/// This ensures consistent behavior across async tasks without locks.
#[derive(Debug)]
pub struct TradeInterval {
    /// Minimum interval between trades.
    min_interval: Duration,
    /// Last trade timestamp as microseconds since creation + 1 to distinguish from "never traded".
    /// Value of 0 means never traded, any other value is (elapsed_us + 1).
    /// Stored as u64 for atomic operations.
    last_trade_us: AtomicU64,
    /// Reference instant for calculating relative times.
    epoch: Instant,
    /// Whether interval enforcement is enabled.
    enabled: bool,
    /// Trade count for statistics.
    trade_count: AtomicU64,
}

impl TradeInterval {
    /// Create a new trade interval with default 500ms minimum.
    pub fn new(min_interval: Duration) -> Self {
        Self {
            min_interval,
            last_trade_us: AtomicU64::new(0),
            epoch: Instant::now(),
            enabled: true,
            trade_count: AtomicU64::new(0),
        }
    }

    /// Create from configuration.
    pub fn from_config(config: &TradeIntervalConfig) -> Self {
        Self {
            min_interval: config.min_interval(),
            last_trade_us: AtomicU64::new(0),
            epoch: Instant::now(),
            enabled: config.enabled,
            trade_count: AtomicU64::new(0),
        }
    }

    /// Create with interval enforcement disabled.
    pub fn disabled() -> Self {
        Self {
            min_interval: Duration::ZERO,
            last_trade_us: AtomicU64::new(0),
            epoch: Instant::now(),
            enabled: false,
            trade_count: AtomicU64::new(0),
        }
    }

    /// Check if enough time has passed since the last trade.
    ///
    /// This is a fast, lock-free check using atomic operations.
    /// Returns `true` if trading is allowed, `false` if waiting is required.
    #[inline]
    pub fn can_trade(&self) -> bool {
        if !self.enabled {
            return true;
        }

        let now_us = self.now_us();
        let last_us_stored = self.last_trade_us.load(Ordering::Acquire);
        let min_interval_us = self.min_interval.as_micros() as u64;

        // First trade is always allowed (last_us_stored == 0 means never traded)
        if last_us_stored == 0 {
            return true;
        }

        // Subtract 1 to get the actual timestamp (we store now_us + 1)
        let last_us = last_us_stored.saturating_sub(1);
        now_us.saturating_sub(last_us) >= min_interval_us
    }

    /// Record that a trade was executed.
    ///
    /// Updates the last trade timestamp to now and increments the trade count.
    /// Should be called immediately after a successful trade.
    #[inline]
    pub fn record_trade(&self) {
        // Store now_us + 1 to distinguish from "never traded" (which is 0)
        let now_us = self.now_us();
        self.last_trade_us.store(now_us + 1, Ordering::Release);
        self.trade_count.fetch_add(1, Ordering::Relaxed);
    }

    /// Get the time remaining until the next trade is allowed.
    ///
    /// Returns `Duration::ZERO` if trading is allowed now.
    pub fn time_until_next(&self) -> Duration {
        if !self.enabled {
            return Duration::ZERO;
        }

        let now_us = self.now_us();
        let last_us_stored = self.last_trade_us.load(Ordering::Acquire);
        let min_interval_us = self.min_interval.as_micros() as u64;

        // First trade is always allowed (last_us_stored == 0 means never traded)
        if last_us_stored == 0 {
            return Duration::ZERO;
        }

        // Subtract 1 to get the actual timestamp (we store now_us + 1)
        let last_us = last_us_stored.saturating_sub(1);
        let elapsed_us = now_us.saturating_sub(last_us);
        if elapsed_us >= min_interval_us {
            Duration::ZERO
        } else {
            Duration::from_micros(min_interval_us - elapsed_us)
        }
    }

    /// Get the minimum interval between trades.
    pub fn min_interval(&self) -> Duration {
        self.min_interval
    }

    /// Check if interval enforcement is enabled.
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Get the total number of trades recorded.
    pub fn trade_count(&self) -> u64 {
        self.trade_count.load(Ordering::Relaxed)
    }

    /// Get the time since the last trade.
    ///
    /// Returns `None` if no trades have been recorded.
    pub fn time_since_last_trade(&self) -> Option<Duration> {
        let last_us_stored = self.last_trade_us.load(Ordering::Acquire);
        if last_us_stored == 0 {
            return None;
        }

        // Subtract 1 to get the actual timestamp (we store now_us + 1)
        let last_us = last_us_stored.saturating_sub(1);
        let now_us = self.now_us();
        Some(Duration::from_micros(now_us.saturating_sub(last_us)))
    }

    /// Reset the interval tracker.
    ///
    /// Clears the last trade time but preserves the trade count.
    pub fn reset(&self) {
        self.last_trade_us.store(0, Ordering::Release);
    }

    /// Reset everything including trade count.
    pub fn reset_all(&self) {
        self.last_trade_us.store(0, Ordering::Release);
        self.trade_count.store(0, Ordering::Relaxed);
    }

    /// Get snapshot of current state for logging/observability.
    pub fn snapshot(&self) -> TradeIntervalSnapshot {
        TradeIntervalSnapshot {
            min_interval_ms: self.min_interval.as_millis() as u64,
            enabled: self.enabled,
            trade_count: self.trade_count.load(Ordering::Relaxed),
            can_trade: self.can_trade(),
            time_until_next_ms: self.time_until_next().as_millis() as u64,
        }
    }

    /// Get current time in microseconds since epoch.
    #[inline]
    fn now_us(&self) -> u64 {
        self.epoch.elapsed().as_micros() as u64
    }
}

impl Default for TradeInterval {
    fn default() -> Self {
        Self::new(Duration::from_millis(DEFAULT_MIN_INTERVAL_MS))
    }
}

/// Snapshot of trade interval state for observability.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TradeIntervalSnapshot {
    /// Minimum interval in milliseconds.
    pub min_interval_ms: u64,
    /// Whether enforcement is enabled.
    pub enabled: bool,
    /// Total trades recorded.
    pub trade_count: u64,
    /// Whether trading is currently allowed.
    pub can_trade: bool,
    /// Milliseconds until next trade allowed.
    pub time_until_next_ms: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn test_config_default() {
        let config = TradeIntervalConfig::default();
        assert_eq!(config.min_interval_ms, 500);
        assert!(config.enabled);
        assert_eq!(config.min_interval(), Duration::from_millis(500));
    }

    #[test]
    fn test_config_new() {
        let config = TradeIntervalConfig::new(1000);
        assert_eq!(config.min_interval_ms, 1000);
        assert!(config.enabled);
    }

    #[test]
    fn test_config_disabled() {
        let config = TradeIntervalConfig::disabled();
        assert!(!config.enabled);
        assert_eq!(config.min_interval_ms, 0);
    }

    #[test]
    fn test_interval_default() {
        let interval = TradeInterval::default();
        assert_eq!(interval.min_interval(), Duration::from_millis(500));
        assert!(interval.is_enabled());
        assert_eq!(interval.trade_count(), 0);
    }

    #[test]
    fn test_interval_first_trade_allowed() {
        let interval = TradeInterval::new(Duration::from_millis(500));

        // First trade should always be allowed
        assert!(interval.can_trade());
        assert_eq!(interval.time_until_next(), Duration::ZERO);
    }

    #[test]
    fn test_interval_record_trade() {
        let interval = TradeInterval::new(Duration::from_millis(100));

        assert_eq!(interval.trade_count(), 0);
        assert!(interval.time_since_last_trade().is_none());

        interval.record_trade();

        assert_eq!(interval.trade_count(), 1);
        assert!(interval.time_since_last_trade().is_some());
    }

    #[test]
    fn test_interval_enforces_minimum() {
        let interval = TradeInterval::new(Duration::from_millis(100));

        // First trade allowed
        assert!(interval.can_trade());
        interval.record_trade();

        // Immediately after, should not be allowed
        assert!(!interval.can_trade());

        // Time until next should be non-zero
        let wait = interval.time_until_next();
        assert!(wait > Duration::ZERO);
        assert!(wait <= Duration::from_millis(100));
    }

    #[test]
    fn test_interval_allows_after_wait() {
        let interval = TradeInterval::new(Duration::from_millis(10));

        interval.record_trade();

        // Wait for the interval to elapse
        thread::sleep(Duration::from_millis(15));

        // Should be allowed now
        assert!(interval.can_trade());
        assert_eq!(interval.time_until_next(), Duration::ZERO);
    }

    #[test]
    fn test_interval_disabled() {
        let interval = TradeInterval::disabled();

        assert!(!interval.is_enabled());

        // Should always be allowed when disabled
        interval.record_trade();
        assert!(interval.can_trade());
        assert_eq!(interval.time_until_next(), Duration::ZERO);
    }

    #[test]
    fn test_interval_from_config() {
        let config = TradeIntervalConfig::new(250);
        let interval = TradeInterval::from_config(&config);

        assert_eq!(interval.min_interval(), Duration::from_millis(250));
        assert!(interval.is_enabled());
    }

    #[test]
    fn test_interval_from_disabled_config() {
        let config = TradeIntervalConfig::disabled();
        let interval = TradeInterval::from_config(&config);

        assert!(!interval.is_enabled());
        assert!(interval.can_trade());
    }

    #[test]
    fn test_interval_reset() {
        let interval = TradeInterval::new(Duration::from_millis(1000));

        interval.record_trade();
        interval.record_trade();
        assert_eq!(interval.trade_count(), 2);
        assert!(!interval.can_trade());

        // Reset clears last trade but not count
        interval.reset();
        assert!(interval.can_trade());
        assert_eq!(interval.trade_count(), 2);
    }

    #[test]
    fn test_interval_reset_all() {
        let interval = TradeInterval::new(Duration::from_millis(1000));

        interval.record_trade();
        interval.record_trade();

        interval.reset_all();

        assert!(interval.can_trade());
        assert_eq!(interval.trade_count(), 0);
        assert!(interval.time_since_last_trade().is_none());
    }

    #[test]
    fn test_interval_snapshot() {
        let interval = TradeInterval::new(Duration::from_millis(500));

        let snap1 = interval.snapshot();
        assert_eq!(snap1.min_interval_ms, 500);
        assert!(snap1.enabled);
        assert_eq!(snap1.trade_count, 0);
        assert!(snap1.can_trade);
        assert_eq!(snap1.time_until_next_ms, 0);

        interval.record_trade();

        let snap2 = interval.snapshot();
        assert_eq!(snap2.trade_count, 1);
        assert!(!snap2.can_trade);
        assert!(snap2.time_until_next_ms > 0);
    }

    #[test]
    fn test_interval_multiple_trades() {
        let interval = TradeInterval::new(Duration::from_millis(5));

        for i in 0..5 {
            // Wait for interval
            while !interval.can_trade() {
                thread::sleep(Duration::from_millis(1));
            }

            interval.record_trade();
            assert_eq!(interval.trade_count(), i + 1);
        }

        assert_eq!(interval.trade_count(), 5);
    }

    #[test]
    fn test_interval_time_since_last_trade() {
        let interval = TradeInterval::new(Duration::from_millis(100));

        // No trades yet
        assert!(interval.time_since_last_trade().is_none());

        interval.record_trade();
        thread::sleep(Duration::from_millis(10));

        let elapsed = interval.time_since_last_trade().unwrap();
        assert!(elapsed >= Duration::from_millis(10));
        assert!(elapsed < Duration::from_millis(100));
    }

    #[test]
    fn test_interval_concurrent_access() {
        use std::sync::Arc;

        let interval = Arc::new(TradeInterval::new(Duration::from_millis(1)));
        let mut handles = vec![];

        // Spawn multiple threads checking and recording trades
        for _ in 0..4 {
            let interval = Arc::clone(&interval);
            handles.push(thread::spawn(move || {
                for _ in 0..10 {
                    if interval.can_trade() {
                        interval.record_trade();
                    }
                    thread::sleep(Duration::from_micros(100));
                }
            }));
        }

        for handle in handles {
            handle.join().unwrap();
        }

        // Should have recorded some trades
        assert!(interval.trade_count() > 0);
    }

    #[test]
    fn test_interval_zero_duration() {
        let interval = TradeInterval::new(Duration::ZERO);

        // With zero interval, should always be allowed
        interval.record_trade();
        assert!(interval.can_trade());
        assert_eq!(interval.time_until_next(), Duration::ZERO);
    }

    #[test]
    fn test_snapshot_serialization() {
        let snap = TradeIntervalSnapshot {
            min_interval_ms: 500,
            enabled: true,
            trade_count: 42,
            can_trade: false,
            time_until_next_ms: 123,
        };

        let json = serde_json::to_string(&snap).unwrap();
        let deserialized: TradeIntervalSnapshot = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.min_interval_ms, 500);
        assert!(deserialized.enabled);
        assert_eq!(deserialized.trade_count, 42);
        assert!(!deserialized.can_trade);
        assert_eq!(deserialized.time_until_next_ms, 123);
    }
}
