//! Circuit breaker for trading system protection.
//!
//! Provides automatic fail-safe mechanism that trips after consecutive failures
//! and auto-resets after a cooldown period.
//!
//! ## Performance Requirements
//!
//! - `can_trade()` must be a single atomic load (~10ns)
//! - `record_failure()` and `record_success()` must be fast (<100ns)
//! - No heap allocations on the hot path

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, Ordering};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::config::RiskConfig;

/// Configuration for the circuit breaker.
#[derive(Debug, Clone)]
pub struct CircuitBreakerConfig {
    /// Number of consecutive failures before the circuit breaker trips.
    pub max_consecutive_failures: u32,

    /// Cooldown duration after tripping before auto-reset.
    pub cooldown: Duration,

    /// Whether auto-reset is enabled.
    pub auto_reset_enabled: bool,
}

impl Default for CircuitBreakerConfig {
    fn default() -> Self {
        Self {
            max_consecutive_failures: 3,
            cooldown: Duration::from_secs(300), // 5 minutes
            auto_reset_enabled: true,
        }
    }
}

impl CircuitBreakerConfig {
    /// Create from RiskConfig.
    pub fn from_risk_config(risk: &RiskConfig) -> Self {
        Self {
            max_consecutive_failures: risk.max_consecutive_failures,
            cooldown: Duration::from_secs(risk.circuit_breaker_cooldown_secs),
            auto_reset_enabled: true,
        }
    }

    /// Create with custom values.
    pub fn new(max_failures: u32, cooldown_secs: u64) -> Self {
        Self {
            max_consecutive_failures: max_failures,
            cooldown: Duration::from_secs(cooldown_secs),
            auto_reset_enabled: true,
        }
    }

    /// Set auto-reset behavior.
    pub fn with_auto_reset(mut self, enabled: bool) -> Self {
        self.auto_reset_enabled = enabled;
        self
    }
}

/// Circuit breaker state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CircuitBreakerState {
    /// Circuit breaker is closed - trading allowed.
    Closed,

    /// Circuit breaker is open - trading blocked.
    Open,

    /// Circuit breaker is half-open - testing recovery.
    HalfOpen,
}

impl std::fmt::Display for CircuitBreakerState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CircuitBreakerState::Closed => write!(f, "Closed"),
            CircuitBreakerState::Open => write!(f, "Open"),
            CircuitBreakerState::HalfOpen => write!(f, "HalfOpen"),
        }
    }
}

/// Reason why the circuit breaker tripped.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TripReason {
    /// Number of consecutive failures that caused the trip.
    pub failure_count: u32,

    /// Timestamp when the circuit breaker tripped.
    pub tripped_at: DateTime<Utc>,

    /// Optional description of the last failure.
    pub last_failure_description: Option<String>,
}

/// Circuit breaker statistics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CircuitBreakerStats {
    /// Current state.
    pub state: CircuitBreakerState,

    /// Total number of trips.
    pub total_trips: u64,

    /// Total number of successes recorded.
    pub total_successes: u64,

    /// Total number of failures recorded.
    pub total_failures: u64,

    /// Current consecutive failure count.
    pub consecutive_failures: u32,

    /// Time remaining in cooldown (if any).
    pub cooldown_remaining: Option<Duration>,

    /// Last trip reason (if any).
    pub last_trip_reason: Option<TripReason>,
}

/// Lock-free circuit breaker for trading protection.
///
/// The circuit breaker monitors consecutive failures and trips when
/// the threshold is exceeded. After a cooldown period, it can auto-reset.
///
/// ## States
///
/// - **Closed**: Normal operation, trading allowed
/// - **Open**: Tripped, trading blocked until cooldown expires
/// - **HalfOpen**: Testing recovery, limited trading allowed
///
/// ## Performance
///
/// - `can_trade()` is a single atomic load (~10ns)
/// - All operations are lock-free
pub struct CircuitBreaker {
    /// Configuration.
    config: CircuitBreakerConfig,

    /// Whether the circuit breaker is tripped (open).
    tripped: AtomicBool,

    /// Whether the circuit breaker is in half-open state.
    half_open: AtomicBool,

    /// Consecutive failure counter.
    consecutive_failures: AtomicU32,

    /// Timestamp when circuit breaker was tripped (millis since epoch).
    trip_time_ms: AtomicI64,

    /// Total number of times the circuit breaker has tripped.
    total_trips: AtomicU32,

    /// Total successes recorded.
    total_successes: AtomicU32,

    /// Total failures recorded.
    total_failures: AtomicU32,
}

impl std::fmt::Debug for CircuitBreaker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CircuitBreaker")
            .field("config", &self.config)
            .field("tripped", &self.tripped.load(Ordering::Relaxed))
            .field("half_open", &self.half_open.load(Ordering::Relaxed))
            .field(
                "consecutive_failures",
                &self.consecutive_failures.load(Ordering::Relaxed),
            )
            .finish()
    }
}

impl CircuitBreaker {
    /// Create a new circuit breaker with the given configuration.
    pub fn new(config: CircuitBreakerConfig) -> Self {
        Self {
            config,
            tripped: AtomicBool::new(false),
            half_open: AtomicBool::new(false),
            consecutive_failures: AtomicU32::new(0),
            trip_time_ms: AtomicI64::new(0),
            total_trips: AtomicU32::new(0),
            total_successes: AtomicU32::new(0),
            total_failures: AtomicU32::new(0),
        }
    }

    /// Create with default configuration.
    pub fn with_defaults() -> Self {
        Self::new(CircuitBreakerConfig::default())
    }

    /// Create from RiskConfig.
    pub fn from_risk_config(risk: &RiskConfig) -> Self {
        Self::new(CircuitBreakerConfig::from_risk_config(risk))
    }

    /// Check if trading is allowed.
    ///
    /// This is the hot path - must be ~10ns.
    /// Single atomic load with Acquire ordering.
    #[inline(always)]
    pub fn can_trade(&self) -> bool {
        !self.tripped.load(Ordering::Acquire)
    }

    /// Get the current state.
    pub fn state(&self) -> CircuitBreakerState {
        if self.tripped.load(Ordering::Acquire) {
            if self.half_open.load(Ordering::Acquire) {
                CircuitBreakerState::HalfOpen
            } else {
                CircuitBreakerState::Open
            }
        } else {
            CircuitBreakerState::Closed
        }
    }

    /// Check if circuit breaker is tripped (open or half-open).
    #[inline]
    pub fn is_tripped(&self) -> bool {
        self.tripped.load(Ordering::Acquire)
    }

    /// Check if circuit breaker is in half-open state.
    #[inline]
    pub fn is_half_open(&self) -> bool {
        self.half_open.load(Ordering::Acquire)
    }

    /// Get the current consecutive failure count.
    #[inline]
    pub fn failure_count(&self) -> u32 {
        self.consecutive_failures.load(Ordering::Acquire)
    }

    /// Record a successful operation.
    ///
    /// Resets the consecutive failure counter.
    /// If in half-open state, closes the circuit breaker.
    #[inline]
    pub fn record_success(&self) {
        self.consecutive_failures.store(0, Ordering::Release);
        self.total_successes.fetch_add(1, Ordering::Relaxed);

        // If half-open, close the circuit breaker
        if self.half_open.load(Ordering::Acquire) {
            self.half_open.store(false, Ordering::Release);
            self.tripped.store(false, Ordering::Release);
        }
    }

    /// Record a failed operation.
    ///
    /// Increments the consecutive failure counter and trips the circuit
    /// breaker if the threshold is exceeded.
    ///
    /// Returns true if the circuit breaker tripped as a result.
    #[inline]
    pub fn record_failure(&self) -> bool {
        self.total_failures.fetch_add(1, Ordering::Relaxed);
        let failures = self.consecutive_failures.fetch_add(1, Ordering::AcqRel) + 1;

        // If in half-open state, any failure re-trips immediately
        if self.half_open.load(Ordering::Acquire) {
            self.trip();
            return true;
        }

        // Check if we've exceeded the threshold
        if failures >= self.config.max_consecutive_failures {
            self.trip();
            return true;
        }

        false
    }

    /// Trip the circuit breaker.
    ///
    /// Sets the circuit breaker to open state and records the trip time.
    pub fn trip(&self) {
        // Only increment trip count if we weren't already tripped
        if !self.tripped.swap(true, Ordering::AcqRel) {
            self.total_trips.fetch_add(1, Ordering::Relaxed);
        }
        self.half_open.store(false, Ordering::Release);
        self.trip_time_ms
            .store(Utc::now().timestamp_millis(), Ordering::Release);
    }

    /// Manually reset the circuit breaker.
    ///
    /// Sets the circuit breaker to closed state and resets the failure counter.
    pub fn reset(&self) {
        self.tripped.store(false, Ordering::Release);
        self.half_open.store(false, Ordering::Release);
        self.consecutive_failures.store(0, Ordering::Release);
    }

    /// Check if the cooldown period has elapsed.
    pub fn cooldown_elapsed(&self) -> bool {
        if !self.tripped.load(Ordering::Acquire) {
            return true;
        }

        let trip_time = self.trip_time_ms.load(Ordering::Acquire);
        let elapsed_ms = Utc::now().timestamp_millis() - trip_time;
        elapsed_ms >= self.config.cooldown.as_millis() as i64
    }

    /// Get the remaining cooldown time.
    pub fn cooldown_remaining(&self) -> Option<Duration> {
        if !self.tripped.load(Ordering::Acquire) {
            return None;
        }

        let trip_time = self.trip_time_ms.load(Ordering::Acquire);
        let elapsed_ms = Utc::now().timestamp_millis() - trip_time;
        let cooldown_ms = self.config.cooldown.as_millis() as i64;

        if elapsed_ms >= cooldown_ms {
            Some(Duration::ZERO)
        } else {
            Some(Duration::from_millis((cooldown_ms - elapsed_ms) as u64))
        }
    }

    /// Try to auto-reset if cooldown has elapsed.
    ///
    /// If auto-reset is enabled and cooldown has elapsed, transitions
    /// to half-open state to test recovery.
    ///
    /// Returns true if the circuit breaker was reset or transitioned.
    pub fn try_auto_reset(&self) -> bool {
        if !self.config.auto_reset_enabled {
            return false;
        }

        if !self.tripped.load(Ordering::Acquire) {
            return false;
        }

        if !self.cooldown_elapsed() {
            return false;
        }

        // Transition to half-open state
        self.half_open.store(true, Ordering::Release);
        self.consecutive_failures.store(0, Ordering::Release);
        true
    }

    /// Allow a single probe trade in half-open state.
    ///
    /// Returns true if a probe trade is allowed (in half-open state).
    pub fn allow_probe(&self) -> bool {
        self.half_open.load(Ordering::Acquire)
    }

    /// Get circuit breaker statistics.
    pub fn stats(&self) -> CircuitBreakerStats {
        let tripped = self.tripped.load(Ordering::Acquire);
        let state = self.state();

        let last_trip_reason = if tripped {
            let trip_time = self.trip_time_ms.load(Ordering::Acquire);
            Some(TripReason {
                failure_count: self.consecutive_failures.load(Ordering::Acquire),
                tripped_at: DateTime::from_timestamp_millis(trip_time)
                    .unwrap_or_else(Utc::now),
                last_failure_description: None,
            })
        } else {
            None
        };

        CircuitBreakerStats {
            state,
            total_trips: self.total_trips.load(Ordering::Relaxed) as u64,
            total_successes: self.total_successes.load(Ordering::Relaxed) as u64,
            total_failures: self.total_failures.load(Ordering::Relaxed) as u64,
            consecutive_failures: self.consecutive_failures.load(Ordering::Relaxed),
            cooldown_remaining: self.cooldown_remaining(),
            last_trip_reason,
        }
    }

    /// Get the configuration.
    pub fn config(&self) -> &CircuitBreakerConfig {
        &self.config
    }
}

impl Default for CircuitBreaker {
    fn default() -> Self {
        Self::with_defaults()
    }
}

/// Shared circuit breaker for use across threads.
pub type SharedCircuitBreaker = std::sync::Arc<CircuitBreaker>;

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    #[test]
    fn test_circuit_breaker_default() {
        let cb = CircuitBreaker::with_defaults();
        assert!(cb.can_trade());
        assert!(!cb.is_tripped());
        assert_eq!(cb.state(), CircuitBreakerState::Closed);
        assert_eq!(cb.failure_count(), 0);
    }

    #[test]
    fn test_circuit_breaker_config() {
        let config = CircuitBreakerConfig::new(5, 600);
        assert_eq!(config.max_consecutive_failures, 5);
        assert_eq!(config.cooldown, Duration::from_secs(600));
        assert!(config.auto_reset_enabled);

        let config = config.with_auto_reset(false);
        assert!(!config.auto_reset_enabled);
    }

    #[test]
    fn test_record_success() {
        let cb = CircuitBreaker::with_defaults();

        // Record some failures
        cb.record_failure();
        cb.record_failure();
        assert_eq!(cb.failure_count(), 2);

        // Success resets counter
        cb.record_success();
        assert_eq!(cb.failure_count(), 0);
    }

    #[test]
    fn test_trip_after_failures() {
        let config = CircuitBreakerConfig::new(3, 60);
        let cb = CircuitBreaker::new(config);

        assert!(cb.can_trade());
        assert!(!cb.record_failure()); // 1
        assert!(cb.can_trade());
        assert!(!cb.record_failure()); // 2
        assert!(cb.can_trade());
        assert!(cb.record_failure()); // 3 - trips
        assert!(!cb.can_trade());
        assert_eq!(cb.state(), CircuitBreakerState::Open);
    }

    #[test]
    fn test_manual_trip() {
        let cb = CircuitBreaker::with_defaults();

        assert!(cb.can_trade());
        cb.trip();
        assert!(!cb.can_trade());
        assert_eq!(cb.state(), CircuitBreakerState::Open);
    }

    #[test]
    fn test_manual_reset() {
        let cb = CircuitBreaker::with_defaults();

        cb.trip();
        assert!(!cb.can_trade());

        cb.reset();
        assert!(cb.can_trade());
        assert_eq!(cb.state(), CircuitBreakerState::Closed);
        assert_eq!(cb.failure_count(), 0);
    }

    #[test]
    fn test_cooldown_elapsed() {
        let config = CircuitBreakerConfig {
            max_consecutive_failures: 3,
            cooldown: Duration::from_millis(10), // Very short cooldown
            auto_reset_enabled: true,
        };
        let cb = CircuitBreaker::new(config);

        cb.trip();
        assert!(!cb.cooldown_elapsed());

        // Wait for cooldown
        std::thread::sleep(Duration::from_millis(15));
        assert!(cb.cooldown_elapsed());
    }

    #[test]
    fn test_cooldown_remaining() {
        let config = CircuitBreakerConfig {
            max_consecutive_failures: 3,
            cooldown: Duration::from_secs(300),
            auto_reset_enabled: true,
        };
        let cb = CircuitBreaker::new(config);

        // Not tripped - no cooldown
        assert!(cb.cooldown_remaining().is_none());

        cb.trip();

        // Should have cooldown remaining
        let remaining = cb.cooldown_remaining().unwrap();
        assert!(remaining > Duration::from_secs(290));
    }

    #[test]
    fn test_auto_reset_to_half_open() {
        let config = CircuitBreakerConfig {
            max_consecutive_failures: 3,
            cooldown: Duration::from_millis(10),
            auto_reset_enabled: true,
        };
        let cb = CircuitBreaker::new(config);

        cb.trip();
        assert_eq!(cb.state(), CircuitBreakerState::Open);

        // Wait for cooldown
        std::thread::sleep(Duration::from_millis(15));

        // Try auto-reset
        assert!(cb.try_auto_reset());
        assert_eq!(cb.state(), CircuitBreakerState::HalfOpen);
        assert!(cb.is_half_open());
        assert!(cb.allow_probe());
    }

    #[test]
    fn test_half_open_success_closes() {
        let config = CircuitBreakerConfig {
            max_consecutive_failures: 3,
            cooldown: Duration::from_millis(10),
            auto_reset_enabled: true,
        };
        let cb = CircuitBreaker::new(config);

        cb.trip();
        std::thread::sleep(Duration::from_millis(15));
        cb.try_auto_reset();

        assert_eq!(cb.state(), CircuitBreakerState::HalfOpen);

        // Success closes circuit breaker
        cb.record_success();
        assert_eq!(cb.state(), CircuitBreakerState::Closed);
        assert!(cb.can_trade());
    }

    #[test]
    fn test_half_open_failure_trips() {
        let config = CircuitBreakerConfig {
            max_consecutive_failures: 3,
            cooldown: Duration::from_millis(10),
            auto_reset_enabled: true,
        };
        let cb = CircuitBreaker::new(config);

        cb.trip();
        std::thread::sleep(Duration::from_millis(15));
        cb.try_auto_reset();

        assert_eq!(cb.state(), CircuitBreakerState::HalfOpen);

        // Failure re-trips immediately
        assert!(cb.record_failure());
        assert_eq!(cb.state(), CircuitBreakerState::Open);
        assert!(!cb.can_trade());
    }

    #[test]
    fn test_auto_reset_disabled() {
        let config = CircuitBreakerConfig {
            max_consecutive_failures: 3,
            cooldown: Duration::from_millis(10),
            auto_reset_enabled: false,
        };
        let cb = CircuitBreaker::new(config);

        cb.trip();
        std::thread::sleep(Duration::from_millis(15));

        // Auto-reset should not work
        assert!(!cb.try_auto_reset());
        assert_eq!(cb.state(), CircuitBreakerState::Open);
    }

    #[test]
    fn test_stats() {
        let config = CircuitBreakerConfig::new(3, 60);
        let cb = CircuitBreaker::new(config);

        cb.record_success();
        cb.record_failure();
        cb.record_failure();
        cb.record_success();
        cb.record_failure();
        cb.record_failure();
        cb.record_failure(); // This trips

        let stats = cb.stats();
        assert_eq!(stats.state, CircuitBreakerState::Open);
        assert_eq!(stats.total_trips, 1);
        assert_eq!(stats.total_successes, 2);
        assert_eq!(stats.total_failures, 5);
        assert!(stats.last_trip_reason.is_some());
    }

    #[test]
    fn test_trip_count_increments() {
        let config = CircuitBreakerConfig::new(1, 0);
        let cb = CircuitBreaker::new(config);

        cb.record_failure(); // Trip 1
        cb.reset();

        cb.record_failure(); // Trip 2
        cb.reset();

        let stats = cb.stats();
        assert_eq!(stats.total_trips, 2);
    }

    #[test]
    fn test_multiple_trip_calls_single_increment() {
        let cb = CircuitBreaker::with_defaults();

        cb.trip();
        cb.trip();
        cb.trip();

        let stats = cb.stats();
        assert_eq!(stats.total_trips, 1);
    }

    #[test]
    fn test_concurrent_access() {
        let cb = Arc::new(CircuitBreaker::new(CircuitBreakerConfig::new(100, 60)));

        let handles: Vec<_> = (0..10)
            .map(|i| {
                let cb = Arc::clone(&cb);
                thread::spawn(move || {
                    for _ in 0..100 {
                        // Concurrent can_trade checks
                        let _ = cb.can_trade();

                        // Alternating success/failure based on thread
                        if i % 2 == 0 {
                            cb.record_success();
                        } else {
                            cb.record_failure();
                        }
                    }
                })
            })
            .collect();

        for handle in handles {
            handle.join().unwrap();
        }

        let stats = cb.stats();
        // 5 threads recorded 100 successes each = 500
        assert_eq!(stats.total_successes, 500);
        // 5 threads recorded 100 failures each = 500
        assert_eq!(stats.total_failures, 500);
    }

    #[test]
    fn test_can_trade_is_fast() {
        let cb = CircuitBreaker::with_defaults();

        // Simple smoke test - can_trade should be very fast
        let start = std::time::Instant::now();
        for _ in 0..1_000_000 {
            let _ = cb.can_trade();
        }
        let elapsed = start.elapsed();

        // 1M calls should complete in under 100ms (100ns average)
        assert!(elapsed < Duration::from_millis(100));
    }

    #[test]
    fn test_state_display() {
        assert_eq!(CircuitBreakerState::Closed.to_string(), "Closed");
        assert_eq!(CircuitBreakerState::Open.to_string(), "Open");
        assert_eq!(CircuitBreakerState::HalfOpen.to_string(), "HalfOpen");
    }

    #[test]
    fn test_from_risk_config() {
        let risk = RiskConfig::default();
        let cb = CircuitBreaker::from_risk_config(&risk);

        assert_eq!(cb.config().max_consecutive_failures, risk.max_consecutive_failures);
        assert_eq!(cb.config().cooldown, Duration::from_secs(risk.circuit_breaker_cooldown_secs));
    }

    #[test]
    fn test_stats_serialization() {
        let config = CircuitBreakerConfig::new(3, 60);
        let cb = CircuitBreaker::new(config);

        cb.record_failure();
        cb.record_failure();
        cb.record_failure();

        let stats = cb.stats();
        let json = serde_json::to_string(&stats).unwrap();
        assert!(json.contains("\"state\":\"Open\""));
        assert!(json.contains("\"total_trips\":1"));
    }

    #[test]
    fn test_closed_state_no_cooldown_remaining() {
        let cb = CircuitBreaker::with_defaults();
        assert!(cb.cooldown_remaining().is_none());
        assert!(cb.cooldown_elapsed());
    }
}
