//! Anomaly detection for trading observability.
//!
//! Detects unusual market conditions and trading patterns:
//! - Extreme margins (>10% arb opportunities are suspicious)
//! - Flash crashes (sudden price drops)
//! - Latency spikes (execution delays)
//! - Spread anomalies (unusually wide or tight spreads)
//! - Volume anomalies (sudden activity changes)
//!
//! ## Architecture
//!
//! ```text
//! [Market Events] --> [AnomalyDetector] --> [Anomaly] --> [ObservabilityCapture]
//!                                                    \--> [Alerter (optional)]
//! ```
//!
//! ## Performance
//!
//! Detection is designed for hot path usage with minimal overhead:
//! - Rolling statistics use O(1) updates
//! - All prices use rust_decimal::Decimal
//! - Alerts are fire-and-forget via async channels

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tracing::{error, info, warn};

use super::capture::SharedCapture;
use super::processor::hash_string;
use super::types::ObservabilityEvent;
use crate::config::ObservabilityConfig;

/// Types of anomalies that can be detected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnomalyType {
    /// Extreme arbitrage margin (>10% is suspicious, may indicate stale data or market issue).
    ExtremeMargin,
    /// Flash crash - sudden price drop >5% in short window.
    FlashCrash,
    /// Flash spike - sudden price rise >5% in short window.
    FlashSpike,
    /// Latency spike - execution or data feed latency exceeds threshold.
    LatencySpike,
    /// Wide spread - bid-ask spread exceeds normal range.
    WideSpread,
    /// Tight spread - spread abnormally tight (may indicate wash trading).
    TightSpread,
    /// Volume spike - sudden activity increase.
    VolumeSpike,
    /// Volume drought - unusually low activity.
    VolumeDrought,
    /// Book imbalance - one side of book has much more depth.
    BookImbalance,
    /// Stale data - no updates for extended period.
    StaleData,
    /// Circuit breaker tripped.
    CircuitBreakerTrip,
    /// Connection issue detected.
    ConnectionIssue,
}

impl AnomalyType {
    /// Get string representation for storage.
    pub fn as_str(&self) -> &'static str {
        match self {
            AnomalyType::ExtremeMargin => "extreme_margin",
            AnomalyType::FlashCrash => "flash_crash",
            AnomalyType::FlashSpike => "flash_spike",
            AnomalyType::LatencySpike => "latency_spike",
            AnomalyType::WideSpread => "wide_spread",
            AnomalyType::TightSpread => "tight_spread",
            AnomalyType::VolumeSpike => "volume_spike",
            AnomalyType::VolumeDrought => "volume_drought",
            AnomalyType::BookImbalance => "book_imbalance",
            AnomalyType::StaleData => "stale_data",
            AnomalyType::CircuitBreakerTrip => "circuit_breaker_trip",
            AnomalyType::ConnectionIssue => "connection_issue",
        }
    }

    /// Parse from string representation.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "extreme_margin" => Some(AnomalyType::ExtremeMargin),
            "flash_crash" => Some(AnomalyType::FlashCrash),
            "flash_spike" => Some(AnomalyType::FlashSpike),
            "latency_spike" => Some(AnomalyType::LatencySpike),
            "wide_spread" => Some(AnomalyType::WideSpread),
            "tight_spread" => Some(AnomalyType::TightSpread),
            "volume_spike" => Some(AnomalyType::VolumeSpike),
            "volume_drought" => Some(AnomalyType::VolumeDrought),
            "book_imbalance" => Some(AnomalyType::BookImbalance),
            "stale_data" => Some(AnomalyType::StaleData),
            "circuit_breaker_trip" => Some(AnomalyType::CircuitBreakerTrip),
            "connection_issue" => Some(AnomalyType::ConnectionIssue),
            _ => None,
        }
    }
}

impl std::fmt::Display for AnomalyType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// Severity levels for anomalies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum AnomalySeverity {
    /// Informational - logged but no action needed.
    Info = 0,
    /// Low - worth noting but not urgent.
    Low = 1,
    /// Medium - should be investigated.
    Medium = 2,
    /// High - requires attention.
    High = 3,
    /// Critical - immediate action needed.
    Critical = 4,
}

impl AnomalySeverity {
    /// Convert to u8 for storage.
    pub fn to_u8(self) -> u8 {
        self as u8
    }

    /// Convert from u8.
    pub fn from_u8(v: u8) -> Self {
        match v {
            0 => AnomalySeverity::Info,
            1 => AnomalySeverity::Low,
            2 => AnomalySeverity::Medium,
            3 => AnomalySeverity::High,
            4 => AnomalySeverity::Critical,
            _ => AnomalySeverity::Info,
        }
    }

    /// Get score (0-100) for severity.
    pub fn score(self) -> u8 {
        match self {
            AnomalySeverity::Info => 10,
            AnomalySeverity::Low => 30,
            AnomalySeverity::Medium => 50,
            AnomalySeverity::High => 75,
            AnomalySeverity::Critical => 100,
        }
    }

    /// Should this trigger an alert?
    pub fn should_alert(self) -> bool {
        matches!(self, AnomalySeverity::High | AnomalySeverity::Critical)
    }
}

impl std::fmt::Display for AnomalySeverity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AnomalySeverity::Info => write!(f, "INFO"),
            AnomalySeverity::Low => write!(f, "LOW"),
            AnomalySeverity::Medium => write!(f, "MEDIUM"),
            AnomalySeverity::High => write!(f, "HIGH"),
            AnomalySeverity::Critical => write!(f, "CRITICAL"),
        }
    }
}

/// Detected anomaly with full context.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Anomaly {
    /// Anomaly type.
    pub anomaly_type: AnomalyType,
    /// Severity level.
    pub severity: AnomalySeverity,
    /// Event ID (if applicable).
    pub event_id: Option<String>,
    /// Token ID (if applicable).
    pub token_id: Option<String>,
    /// Detection timestamp.
    pub timestamp: DateTime<Utc>,
    /// Description of the anomaly.
    pub description: String,
    /// Current value that triggered anomaly.
    pub current_value: Decimal,
    /// Expected/normal value.
    pub expected_value: Decimal,
    /// Deviation from expected (absolute or percentage).
    pub deviation: Decimal,
    /// Additional context data.
    pub context: HashMap<String, String>,
}

impl Anomaly {
    /// Create a new anomaly.
    pub fn new(
        anomaly_type: AnomalyType,
        severity: AnomalySeverity,
        description: impl Into<String>,
    ) -> Self {
        Self {
            anomaly_type,
            severity,
            event_id: None,
            token_id: None,
            timestamp: Utc::now(),
            description: description.into(),
            current_value: Decimal::ZERO,
            expected_value: Decimal::ZERO,
            deviation: Decimal::ZERO,
            context: HashMap::new(),
        }
    }

    /// Set event ID.
    pub fn with_event_id(mut self, event_id: impl Into<String>) -> Self {
        self.event_id = Some(event_id.into());
        self
    }

    /// Set token ID.
    pub fn with_token_id(mut self, token_id: impl Into<String>) -> Self {
        self.token_id = Some(token_id.into());
        self
    }

    /// Set values.
    pub fn with_values(
        mut self,
        current: Decimal,
        expected: Decimal,
        deviation: Decimal,
    ) -> Self {
        self.current_value = current;
        self.expected_value = expected;
        self.deviation = deviation;
        self
    }

    /// Add context.
    pub fn with_context(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.context.insert(key.into(), value.into());
        self
    }

    /// Convert to observability event for capture.
    pub fn to_observability_event(&self) -> ObservabilityEvent {
        // Pack deviation into data field (multiply by 10000 for bps precision)
        let data = (self.deviation * Decimal::new(10000, 0))
            .try_into()
            .unwrap_or(0i64)
            .unsigned_abs();

        let event_id_hash = self
            .event_id
            .as_ref()
            .map(|id| hash_string(id))
            .unwrap_or(0);

        ObservabilityEvent::Anomaly {
            anomaly_type: self.anomaly_type.as_str().to_string(),
            severity: self.severity.score(),
            event_id_hash,
            timestamp_ms: self.timestamp.timestamp_millis(),
            data,
        }
    }
}

/// Configuration for anomaly detection.
#[derive(Debug, Clone)]
pub struct AnomalyConfig {
    /// Enable anomaly detection.
    pub enabled: bool,
    /// Extreme margin threshold (default 0.10 = 10%).
    pub extreme_margin_threshold: Decimal,
    /// Flash crash/spike threshold (default 0.05 = 5%).
    pub flash_price_threshold: Decimal,
    /// Flash detection window (default 5 seconds).
    pub flash_window_secs: u64,
    /// Latency spike threshold (default 1000ms).
    pub latency_threshold_ms: u64,
    /// Wide spread threshold in bps (default 500 = 5%).
    pub wide_spread_threshold_bps: u32,
    /// Tight spread threshold in bps (default 10 = 0.1%).
    pub tight_spread_threshold_bps: u32,
    /// Volume spike multiplier (default 10x).
    pub volume_spike_multiplier: Decimal,
    /// Stale data threshold (default 30 seconds).
    pub stale_data_threshold_secs: u64,
    /// Book imbalance threshold (default 0.8 = 80% one-sided).
    pub book_imbalance_threshold: Decimal,
    /// Rolling window size for statistics.
    pub rolling_window_size: usize,
    /// Enable webhook alerts.
    pub alerts_enabled: bool,
    /// Webhook URL for alerts.
    pub alert_webhook_url: Option<String>,
    /// Minimum severity for alerts.
    pub alert_min_severity: AnomalySeverity,
    /// Cooldown between alerts of same type (default 60s).
    pub alert_cooldown_secs: u64,
}

impl Default for AnomalyConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            extreme_margin_threshold: Decimal::new(10, 2), // 0.10 = 10%
            flash_price_threshold: Decimal::new(5, 2),     // 0.05 = 5%
            flash_window_secs: 5,
            latency_threshold_ms: 1000,
            wide_spread_threshold_bps: 500,  // 5%
            tight_spread_threshold_bps: 10,  // 0.1%
            volume_spike_multiplier: Decimal::new(10, 0),
            stale_data_threshold_secs: 30,
            book_imbalance_threshold: Decimal::new(8, 1), // 0.8
            rolling_window_size: 100,
            alerts_enabled: false,
            alert_webhook_url: None,
            alert_min_severity: AnomalySeverity::High,
            alert_cooldown_secs: 60,
        }
    }
}

impl AnomalyConfig {
    /// Create from ObservabilityConfig.
    pub fn from_observability_config(config: &ObservabilityConfig) -> Self {
        Self {
            enabled: config.detect_anomalies,
            alerts_enabled: config.alerts_enabled,
            alert_webhook_url: config.alert_webhook_url.clone(),
            ..Default::default()
        }
    }

    /// Create disabled config.
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            ..Default::default()
        }
    }
}

/// Rolling statistics for a single metric.
#[derive(Debug)]
struct RollingStats {
    values: Vec<Decimal>,
    sum: Decimal,
    index: usize,
    count: usize,
    capacity: usize,
}

impl RollingStats {
    fn new(capacity: usize) -> Self {
        Self {
            values: vec![Decimal::ZERO; capacity],
            sum: Decimal::ZERO,
            index: 0,
            count: 0,
            capacity,
        }
    }

    fn push(&mut self, value: Decimal) {
        // Subtract old value from sum
        self.sum -= self.values[self.index];
        // Add new value
        self.values[self.index] = value;
        self.sum += value;
        // Advance index
        self.index = (self.index + 1) % self.capacity;
        if self.count < self.capacity {
            self.count += 1;
        }
    }

    fn average(&self) -> Decimal {
        if self.count == 0 {
            Decimal::ZERO
        } else {
            self.sum / Decimal::new(self.count as i64, 0)
        }
    }

    fn is_ready(&self) -> bool {
        self.count >= self.capacity / 2 // At least half full
    }
}

/// Per-token state for anomaly detection.
#[derive(Debug)]
struct TokenState {
    /// Rolling spread (in bps).
    spread_stats: RollingStats,
    /// Rolling volume.
    volume_stats: RollingStats,
    /// Last price for flash detection.
    last_price: Decimal,
    /// Last price timestamp.
    last_price_time: Instant,
    /// Last update timestamp (for stale detection).
    last_update: Instant,
}

impl TokenState {
    fn new(capacity: usize) -> Self {
        Self {
            spread_stats: RollingStats::new(capacity),
            volume_stats: RollingStats::new(capacity),
            last_price: Decimal::ZERO,
            last_price_time: Instant::now(),
            last_update: Instant::now(),
        }
    }
}

/// Statistics for anomaly detection.
#[derive(Debug, Default)]
pub struct AnomalyStats {
    /// Total anomalies detected.
    pub detected: AtomicU64,
    /// Anomalies by type.
    pub by_type: [AtomicU64; 12],
    /// Alerts sent.
    pub alerts_sent: AtomicU64,
    /// Alert failures.
    pub alert_failures: AtomicU64,
}

impl AnomalyStats {
    /// Create new stats.
    pub fn new() -> Self {
        Self::default()
    }

    /// Get snapshot.
    pub fn snapshot(&self) -> AnomalyStatsSnapshot {
        AnomalyStatsSnapshot {
            detected: self.detected.load(Ordering::Relaxed),
            alerts_sent: self.alerts_sent.load(Ordering::Relaxed),
            alert_failures: self.alert_failures.load(Ordering::Relaxed),
            by_type: [
                self.by_type[0].load(Ordering::Relaxed),
                self.by_type[1].load(Ordering::Relaxed),
                self.by_type[2].load(Ordering::Relaxed),
                self.by_type[3].load(Ordering::Relaxed),
                self.by_type[4].load(Ordering::Relaxed),
                self.by_type[5].load(Ordering::Relaxed),
                self.by_type[6].load(Ordering::Relaxed),
                self.by_type[7].load(Ordering::Relaxed),
                self.by_type[8].load(Ordering::Relaxed),
                self.by_type[9].load(Ordering::Relaxed),
                self.by_type[10].load(Ordering::Relaxed),
                self.by_type[11].load(Ordering::Relaxed),
            ],
        }
    }

    /// Reset stats.
    pub fn reset(&self) {
        self.detected.store(0, Ordering::Relaxed);
        self.alerts_sent.store(0, Ordering::Relaxed);
        self.alert_failures.store(0, Ordering::Relaxed);
        for counter in &self.by_type {
            counter.store(0, Ordering::Relaxed);
        }
    }

    fn record_anomaly(&self, anomaly_type: AnomalyType) {
        self.detected.fetch_add(1, Ordering::Relaxed);
        let idx = match anomaly_type {
            AnomalyType::ExtremeMargin => 0,
            AnomalyType::FlashCrash => 1,
            AnomalyType::FlashSpike => 2,
            AnomalyType::LatencySpike => 3,
            AnomalyType::WideSpread => 4,
            AnomalyType::TightSpread => 5,
            AnomalyType::VolumeSpike => 6,
            AnomalyType::VolumeDrought => 7,
            AnomalyType::BookImbalance => 8,
            AnomalyType::StaleData => 9,
            AnomalyType::CircuitBreakerTrip => 10,
            AnomalyType::ConnectionIssue => 11,
        };
        self.by_type[idx].fetch_add(1, Ordering::Relaxed);
    }
}

/// Snapshot of anomaly statistics.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct AnomalyStatsSnapshot {
    pub detected: u64,
    pub alerts_sent: u64,
    pub alert_failures: u64,
    pub by_type: [u64; 12],
}

impl AnomalyStatsSnapshot {
    /// Get count for specific type.
    pub fn count_for_type(&self, anomaly_type: AnomalyType) -> u64 {
        let idx = match anomaly_type {
            AnomalyType::ExtremeMargin => 0,
            AnomalyType::FlashCrash => 1,
            AnomalyType::FlashSpike => 2,
            AnomalyType::LatencySpike => 3,
            AnomalyType::WideSpread => 4,
            AnomalyType::TightSpread => 5,
            AnomalyType::VolumeSpike => 6,
            AnomalyType::VolumeDrought => 7,
            AnomalyType::BookImbalance => 8,
            AnomalyType::StaleData => 9,
            AnomalyType::CircuitBreakerTrip => 10,
            AnomalyType::ConnectionIssue => 11,
        };
        self.by_type[idx]
    }
}

/// Anomaly detector for trading observability.
pub struct AnomalyDetector {
    config: AnomalyConfig,
    stats: Arc<AnomalyStats>,
    token_states: RwLock<HashMap<String, TokenState>>,
    alert_cooldowns: RwLock<HashMap<AnomalyType, Instant>>,
    capture: Option<SharedCapture>,
}

impl AnomalyDetector {
    /// Create a new detector.
    pub fn new(config: AnomalyConfig) -> Self {
        Self {
            config,
            stats: Arc::new(AnomalyStats::new()),
            token_states: RwLock::new(HashMap::new()),
            alert_cooldowns: RwLock::new(HashMap::new()),
            capture: None,
        }
    }

    /// Create with default config.
    pub fn with_defaults() -> Self {
        Self::new(AnomalyConfig::default())
    }

    /// Create disabled detector.
    pub fn disabled() -> Self {
        Self::new(AnomalyConfig::disabled())
    }

    /// Set capture channel for observability.
    pub fn with_capture(mut self, capture: SharedCapture) -> Self {
        self.capture = Some(capture);
        self
    }

    /// Get stats.
    pub fn stats(&self) -> &AnomalyStats {
        &self.stats
    }

    /// Get stats snapshot.
    pub fn stats_snapshot(&self) -> AnomalyStatsSnapshot {
        self.stats.snapshot()
    }

    /// Check if detection is enabled.
    pub fn is_enabled(&self) -> bool {
        self.config.enabled
    }

    /// Check for extreme margin anomaly.
    ///
    /// An arb margin > 10% is suspicious and may indicate:
    /// - Stale orderbook data
    /// - Market malfunction
    /// - Data feed issue
    pub fn check_extreme_margin(
        &self,
        event_id: &str,
        margin: Decimal,
    ) -> Option<Anomaly> {
        if !self.config.enabled || margin <= self.config.extreme_margin_threshold {
            return None;
        }

        let severity = if margin > Decimal::new(20, 2) {
            AnomalySeverity::Critical
        } else if margin > Decimal::new(15, 2) {
            AnomalySeverity::High
        } else {
            AnomalySeverity::Medium
        };

        let anomaly = Anomaly::new(
            AnomalyType::ExtremeMargin,
            severity,
            format!(
                "Extreme arbitrage margin detected: {:.2}% (threshold: {:.2}%)",
                margin * Decimal::new(100, 0),
                self.config.extreme_margin_threshold * Decimal::new(100, 0)
            ),
        )
        .with_event_id(event_id)
        .with_values(margin, self.config.extreme_margin_threshold, margin);

        self.record_and_alert(anomaly.clone());
        Some(anomaly)
    }

    /// Check for flash crash/spike anomaly.
    ///
    /// Detects sudden price movements > threshold within flash window.
    pub async fn check_flash_price(
        &self,
        token_id: &str,
        event_id: &str,
        current_price: Decimal,
    ) -> Option<Anomaly> {
        if !self.config.enabled {
            return None;
        }

        let mut states = self.token_states.write().await;
        let state = states
            .entry(token_id.to_string())
            .or_insert_with(|| TokenState::new(self.config.rolling_window_size));

        state.last_update = Instant::now();

        // Skip if no previous price
        if state.last_price == Decimal::ZERO {
            state.last_price = current_price;
            state.last_price_time = Instant::now();
            return None;
        }

        // Check if within flash window
        let elapsed = state.last_price_time.elapsed();
        if elapsed > Duration::from_secs(self.config.flash_window_secs) {
            state.last_price = current_price;
            state.last_price_time = Instant::now();
            return None;
        }

        // Calculate price change
        let prev_price = state.last_price;
        let change = if prev_price != Decimal::ZERO {
            (current_price - prev_price).abs() / prev_price
        } else {
            Decimal::ZERO
        };

        state.last_price = current_price;
        state.last_price_time = Instant::now();

        if change <= self.config.flash_price_threshold {
            return None;
        }

        let is_crash = current_price < prev_price;
        let anomaly_type = if is_crash {
            AnomalyType::FlashCrash
        } else {
            AnomalyType::FlashSpike
        };

        let severity = if change > Decimal::new(15, 2) {
            AnomalySeverity::Critical
        } else if change > Decimal::new(10, 2) {
            AnomalySeverity::High
        } else {
            AnomalySeverity::Medium
        };

        let description = if is_crash {
            format!(
                "Flash crash detected: {:.2}% drop in {}s",
                change * Decimal::new(100, 0),
                elapsed.as_secs_f64()
            )
        } else {
            format!(
                "Flash spike detected: {:.2}% rise in {}s",
                change * Decimal::new(100, 0),
                elapsed.as_secs_f64()
            )
        };

        let anomaly = Anomaly::new(anomaly_type, severity, description)
            .with_event_id(event_id)
            .with_token_id(token_id)
            .with_values(change, self.config.flash_price_threshold, change);

        self.record_and_alert(anomaly.clone());
        Some(anomaly)
    }

    /// Check for latency spike anomaly.
    pub fn check_latency_spike(
        &self,
        event_id: &str,
        latency_ms: u64,
    ) -> Option<Anomaly> {
        if !self.config.enabled || latency_ms <= self.config.latency_threshold_ms {
            return None;
        }

        let severity = if latency_ms > self.config.latency_threshold_ms * 5 {
            AnomalySeverity::Critical
        } else if latency_ms > self.config.latency_threshold_ms * 2 {
            AnomalySeverity::High
        } else {
            AnomalySeverity::Medium
        };

        let anomaly = Anomaly::new(
            AnomalyType::LatencySpike,
            severity,
            format!(
                "Latency spike detected: {}ms (threshold: {}ms)",
                latency_ms, self.config.latency_threshold_ms
            ),
        )
        .with_event_id(event_id)
        .with_values(
            Decimal::new(latency_ms as i64, 0),
            Decimal::new(self.config.latency_threshold_ms as i64, 0),
            Decimal::new((latency_ms - self.config.latency_threshold_ms) as i64, 0),
        );

        self.record_and_alert(anomaly.clone());
        Some(anomaly)
    }

    /// Check for spread anomalies.
    pub async fn check_spread(
        &self,
        token_id: &str,
        event_id: &str,
        spread_bps: u32,
    ) -> Option<Anomaly> {
        if !self.config.enabled {
            return None;
        }

        let mut states = self.token_states.write().await;
        let state = states
            .entry(token_id.to_string())
            .or_insert_with(|| TokenState::new(self.config.rolling_window_size));

        state.last_update = Instant::now();
        state.spread_stats.push(Decimal::new(spread_bps as i64, 0));

        // Check for wide spread
        if spread_bps >= self.config.wide_spread_threshold_bps {
            let severity = if spread_bps > self.config.wide_spread_threshold_bps * 2 {
                AnomalySeverity::High
            } else {
                AnomalySeverity::Medium
            };

            let anomaly = Anomaly::new(
                AnomalyType::WideSpread,
                severity,
                format!(
                    "Wide spread detected: {}bps (threshold: {}bps)",
                    spread_bps, self.config.wide_spread_threshold_bps
                ),
            )
            .with_event_id(event_id)
            .with_token_id(token_id)
            .with_values(
                Decimal::new(spread_bps as i64, 0),
                Decimal::new(self.config.wide_spread_threshold_bps as i64, 0),
                Decimal::new((spread_bps - self.config.wide_spread_threshold_bps) as i64, 0),
            );

            self.record_and_alert(anomaly.clone());
            return Some(anomaly);
        }

        // Check for tight spread (only if we have enough data)
        if spread_bps <= self.config.tight_spread_threshold_bps && state.spread_stats.is_ready() {
            let avg_spread = state.spread_stats.average();
            if avg_spread > Decimal::new(self.config.tight_spread_threshold_bps as i64 * 5, 0) {
                let anomaly = Anomaly::new(
                    AnomalyType::TightSpread,
                    AnomalySeverity::Low,
                    format!(
                        "Unusually tight spread: {}bps (avg: {:.0}bps)",
                        spread_bps, avg_spread
                    ),
                )
                .with_event_id(event_id)
                .with_token_id(token_id)
                .with_values(
                    Decimal::new(spread_bps as i64, 0),
                    avg_spread,
                    avg_spread - Decimal::new(spread_bps as i64, 0),
                );

                self.record_and_alert(anomaly.clone());
                return Some(anomaly);
            }
        }

        None
    }

    /// Check for volume anomalies.
    pub async fn check_volume(
        &self,
        token_id: &str,
        event_id: &str,
        volume: Decimal,
    ) -> Option<Anomaly> {
        if !self.config.enabled {
            return None;
        }

        let mut states = self.token_states.write().await;
        let state = states
            .entry(token_id.to_string())
            .or_insert_with(|| TokenState::new(self.config.rolling_window_size));

        state.last_update = Instant::now();

        // Get average BEFORE pushing new value for comparison
        let avg_volume = state.volume_stats.average();
        let is_ready = state.volume_stats.is_ready();

        // Now push the new value for future calculations
        state.volume_stats.push(volume);

        if !is_ready {
            return None;
        }

        if avg_volume == Decimal::ZERO {
            return None;
        }

        let ratio = volume / avg_volume;

        // Check for volume spike
        if ratio >= self.config.volume_spike_multiplier {
            let severity = if ratio > self.config.volume_spike_multiplier * Decimal::new(2, 0) {
                AnomalySeverity::High
            } else {
                AnomalySeverity::Medium
            };

            let anomaly = Anomaly::new(
                AnomalyType::VolumeSpike,
                severity,
                format!(
                    "Volume spike detected: {:.1}x average ({:.2} vs avg {:.2})",
                    ratio, volume, avg_volume
                ),
            )
            .with_event_id(event_id)
            .with_token_id(token_id)
            .with_values(volume, avg_volume, ratio);

            self.record_and_alert(anomaly.clone());
            return Some(anomaly);
        }

        // Check for volume drought (< 10% of average)
        if ratio < Decimal::new(1, 1) && avg_volume > Decimal::new(1, 0) {
            let anomaly = Anomaly::new(
                AnomalyType::VolumeDrought,
                AnomalySeverity::Low,
                format!(
                    "Volume drought detected: {:.1}% of average ({:.2} vs avg {:.2})",
                    ratio * Decimal::new(100, 0),
                    volume,
                    avg_volume
                ),
            )
            .with_event_id(event_id)
            .with_token_id(token_id)
            .with_values(volume, avg_volume, Decimal::ONE - ratio);

            self.record_and_alert(anomaly.clone());
            return Some(anomaly);
        }

        None
    }

    /// Check for book imbalance.
    pub fn check_book_imbalance(
        &self,
        token_id: &str,
        event_id: &str,
        bid_depth: Decimal,
        ask_depth: Decimal,
    ) -> Option<Anomaly> {
        if !self.config.enabled {
            return None;
        }

        let total = bid_depth + ask_depth;
        if total == Decimal::ZERO {
            return None;
        }

        let bid_ratio = bid_depth / total;
        let ask_ratio = ask_depth / total;

        let imbalance = if bid_ratio > ask_ratio {
            bid_ratio
        } else {
            ask_ratio
        };

        if imbalance < self.config.book_imbalance_threshold {
            return None;
        }

        let severity = if imbalance > Decimal::new(95, 2) {
            AnomalySeverity::High
        } else if imbalance > Decimal::new(9, 1) {
            AnomalySeverity::Medium
        } else {
            AnomalySeverity::Low
        };

        let side = if bid_ratio > ask_ratio { "bid" } else { "ask" };

        let anomaly = Anomaly::new(
            AnomalyType::BookImbalance,
            severity,
            format!(
                "Book imbalance detected: {:.1}% on {} side",
                imbalance * Decimal::new(100, 0),
                side
            ),
        )
        .with_event_id(event_id)
        .with_token_id(token_id)
        .with_values(imbalance, self.config.book_imbalance_threshold, imbalance)
        .with_context("bid_depth", bid_depth.to_string())
        .with_context("ask_depth", ask_depth.to_string());

        self.record_and_alert(anomaly.clone());
        Some(anomaly)
    }

    /// Check for stale data.
    pub async fn check_stale_data(
        &self,
        token_id: &str,
        event_id: &str,
    ) -> Option<Anomaly> {
        if !self.config.enabled {
            return None;
        }

        let states = self.token_states.read().await;
        let state = states.get(token_id)?;

        let elapsed = state.last_update.elapsed();
        if elapsed < Duration::from_secs(self.config.stale_data_threshold_secs) {
            return None;
        }

        let severity = if elapsed > Duration::from_secs(self.config.stale_data_threshold_secs * 3) {
            AnomalySeverity::High
        } else {
            AnomalySeverity::Medium
        };

        let anomaly = Anomaly::new(
            AnomalyType::StaleData,
            severity,
            format!(
                "Stale data detected: no updates for {:.1}s (threshold: {}s)",
                elapsed.as_secs_f64(),
                self.config.stale_data_threshold_secs
            ),
        )
        .with_event_id(event_id)
        .with_token_id(token_id)
        .with_values(
            Decimal::new(elapsed.as_secs() as i64, 0),
            Decimal::new(self.config.stale_data_threshold_secs as i64, 0),
            Decimal::new((elapsed.as_secs() - self.config.stale_data_threshold_secs) as i64, 0),
        );

        self.record_and_alert(anomaly.clone());
        Some(anomaly)
    }

    /// Record circuit breaker trip.
    pub fn record_circuit_breaker_trip(&self, reason: &str) -> Anomaly {
        let anomaly = Anomaly::new(
            AnomalyType::CircuitBreakerTrip,
            AnomalySeverity::High,
            format!("Circuit breaker tripped: {}", reason),
        )
        .with_context("reason", reason.to_string());

        self.record_and_alert(anomaly.clone());
        anomaly
    }

    /// Record connection issue.
    pub fn record_connection_issue(
        &self,
        source: &str,
        error: &str,
    ) -> Anomaly {
        let anomaly = Anomaly::new(
            AnomalyType::ConnectionIssue,
            AnomalySeverity::Medium,
            format!("Connection issue with {}: {}", source, error),
        )
        .with_context("source", source.to_string())
        .with_context("error", error.to_string());

        self.record_and_alert(anomaly.clone());
        anomaly
    }

    /// Record anomaly and optionally send alert.
    fn record_and_alert(&self, anomaly: Anomaly) {
        self.stats.record_anomaly(anomaly.anomaly_type);

        // Log the anomaly
        match anomaly.severity {
            AnomalySeverity::Info => info!(
                anomaly_type = %anomaly.anomaly_type,
                severity = %anomaly.severity,
                "{}", anomaly.description
            ),
            AnomalySeverity::Low => info!(
                anomaly_type = %anomaly.anomaly_type,
                severity = %anomaly.severity,
                "{}", anomaly.description
            ),
            AnomalySeverity::Medium => warn!(
                anomaly_type = %anomaly.anomaly_type,
                severity = %anomaly.severity,
                "{}", anomaly.description
            ),
            AnomalySeverity::High | AnomalySeverity::Critical => error!(
                anomaly_type = %anomaly.anomaly_type,
                severity = %anomaly.severity,
                "{}", anomaly.description
            ),
        }

        // Send to observability capture
        if let Some(capture) = &self.capture {
            capture.try_capture_event(anomaly.to_observability_event());
        }

        // Send webhook alert if enabled
        if self.config.alerts_enabled && anomaly.severity >= self.config.alert_min_severity {
            self.maybe_send_alert(&anomaly);
        }
    }

    /// Send alert if not in cooldown.
    fn maybe_send_alert(&self, anomaly: &Anomaly) {
        // Check cooldown (sync for simplicity)
        let should_alert = {
            let cooldowns = self.alert_cooldowns.blocking_read();
            if let Some(last_alert) = cooldowns.get(&anomaly.anomaly_type) {
                last_alert.elapsed() > Duration::from_secs(self.config.alert_cooldown_secs)
            } else {
                true
            }
        };

        if !should_alert {
            return;
        }

        // Update cooldown
        {
            let mut cooldowns = self.alert_cooldowns.blocking_write();
            cooldowns.insert(anomaly.anomaly_type, Instant::now());
        }

        // Send webhook if configured
        if let Some(url) = &self.config.alert_webhook_url {
            let url = url.clone();
            let payload = serde_json::json!({
                "text": format!(
                    "[{}] {} - {}",
                    anomaly.severity,
                    anomaly.anomaly_type,
                    anomaly.description
                ),
                "anomaly_type": anomaly.anomaly_type.as_str(),
                "severity": anomaly.severity.to_string(),
                "description": anomaly.description,
                "timestamp": anomaly.timestamp.to_rfc3339(),
            });

            // Fire and forget
            tokio::spawn(async move {
                if let Err(e) = send_webhook(&url, &payload).await {
                    warn!(error = %e, "Failed to send anomaly webhook");
                }
            });

            self.stats.alerts_sent.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Clean up old token states.
    pub async fn cleanup_stale_states(&self, max_age_secs: u64) {
        let mut states = self.token_states.write().await;
        let threshold = Duration::from_secs(max_age_secs);

        states.retain(|_, state| state.last_update.elapsed() < threshold);
    }
}

/// Send webhook alert.
async fn send_webhook(url: &str, payload: &serde_json::Value) -> Result<(), reqwest::Error> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()?;

    client
        .post(url)
        .json(payload)
        .send()
        .await?
        .error_for_status()?;

    Ok(())
}

/// Shared anomaly detector for use across tasks.
pub type SharedAnomalyDetector = Arc<AnomalyDetector>;

/// Create a shared anomaly detector.
pub fn create_shared_detector(config: AnomalyConfig) -> SharedAnomalyDetector {
    Arc::new(AnomalyDetector::new(config))
}

/// Create shared detector with capture.
pub fn create_shared_detector_with_capture(
    config: AnomalyConfig,
    capture: SharedCapture,
) -> SharedAnomalyDetector {
    Arc::new(AnomalyDetector::new(config).with_capture(capture))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn test_anomaly_type_str() {
        assert_eq!(AnomalyType::ExtremeMargin.as_str(), "extreme_margin");
        assert_eq!(AnomalyType::FlashCrash.as_str(), "flash_crash");
        assert_eq!(AnomalyType::parse("flash_crash"), Some(AnomalyType::FlashCrash));
        assert_eq!(AnomalyType::parse("unknown"), None);
    }

    #[test]
    fn test_anomaly_severity() {
        assert!(AnomalySeverity::Critical > AnomalySeverity::High);
        assert!(AnomalySeverity::High > AnomalySeverity::Medium);
        assert_eq!(AnomalySeverity::Critical.score(), 100);
        assert!(AnomalySeverity::High.should_alert());
        assert!(!AnomalySeverity::Medium.should_alert());
    }

    #[test]
    fn test_anomaly_severity_conversion() {
        assert_eq!(AnomalySeverity::from_u8(0), AnomalySeverity::Info);
        assert_eq!(AnomalySeverity::from_u8(4), AnomalySeverity::Critical);
        assert_eq!(AnomalySeverity::from_u8(99), AnomalySeverity::Info);
        assert_eq!(AnomalySeverity::Critical.to_u8(), 4);
    }

    #[test]
    fn test_anomaly_creation() {
        let anomaly = Anomaly::new(
            AnomalyType::ExtremeMargin,
            AnomalySeverity::High,
            "Test anomaly",
        )
        .with_event_id("event123")
        .with_token_id("token456")
        .with_values(dec!(0.15), dec!(0.10), dec!(0.05))
        .with_context("key", "value");

        assert_eq!(anomaly.anomaly_type, AnomalyType::ExtremeMargin);
        assert_eq!(anomaly.severity, AnomalySeverity::High);
        assert_eq!(anomaly.event_id, Some("event123".to_string()));
        assert_eq!(anomaly.token_id, Some("token456".to_string()));
        assert_eq!(anomaly.current_value, dec!(0.15));
        assert_eq!(anomaly.context.get("key"), Some(&"value".to_string()));
    }

    #[test]
    fn test_anomaly_to_observability_event() {
        let anomaly = Anomaly::new(
            AnomalyType::FlashCrash,
            AnomalySeverity::Critical,
            "Flash crash",
        )
        .with_event_id("event123")
        .with_values(dec!(0.10), dec!(0.05), dec!(0.05));

        let event = anomaly.to_observability_event();

        match event {
            ObservabilityEvent::Anomaly {
                anomaly_type,
                severity,
                event_id_hash,
                ..
            } => {
                assert_eq!(anomaly_type, "flash_crash");
                assert_eq!(severity, 100); // Critical score
                assert_ne!(event_id_hash, 0);
            }
            _ => panic!("Wrong event type"),
        }
    }

    #[test]
    fn test_anomaly_config_default() {
        let config = AnomalyConfig::default();

        assert!(config.enabled);
        assert_eq!(config.extreme_margin_threshold, dec!(0.10));
        assert_eq!(config.flash_price_threshold, dec!(0.05));
        assert!(!config.alerts_enabled);
    }

    #[test]
    fn test_anomaly_config_disabled() {
        let config = AnomalyConfig::disabled();
        assert!(!config.enabled);
    }

    #[test]
    fn test_rolling_stats() {
        let mut stats = RollingStats::new(5);

        stats.push(dec!(10));
        stats.push(dec!(20));
        stats.push(dec!(30));

        assert_eq!(stats.count, 3);
        assert_eq!(stats.average(), dec!(20)); // (10 + 20 + 30) / 3

        // Fill to capacity
        stats.push(dec!(40));
        stats.push(dec!(50));

        assert_eq!(stats.count, 5);
        assert!(stats.is_ready());
        assert_eq!(stats.average(), dec!(30)); // (10 + 20 + 30 + 40 + 50) / 5

        // Push more, should wrap
        stats.push(dec!(60));
        assert_eq!(stats.count, 5);
        assert_eq!(stats.average(), dec!(40)); // (20 + 30 + 40 + 50 + 60) / 5
    }

    #[test]
    fn test_detector_check_extreme_margin() {
        let detector = AnomalyDetector::with_defaults();

        // Below threshold - no anomaly
        assert!(detector.check_extreme_margin("event1", dec!(0.05)).is_none());

        // Above threshold - anomaly detected
        let anomaly = detector.check_extreme_margin("event1", dec!(0.15)).unwrap();
        assert_eq!(anomaly.anomaly_type, AnomalyType::ExtremeMargin);
        assert_eq!(anomaly.severity, AnomalySeverity::Medium);

        // Way above threshold - critical
        let anomaly = detector.check_extreme_margin("event1", dec!(0.25)).unwrap();
        assert_eq!(anomaly.severity, AnomalySeverity::Critical);
    }

    #[test]
    fn test_detector_check_latency_spike() {
        let detector = AnomalyDetector::with_defaults();

        // Below threshold
        assert!(detector.check_latency_spike("event1", 500).is_none());

        // Above threshold
        let anomaly = detector.check_latency_spike("event1", 1500).unwrap();
        assert_eq!(anomaly.anomaly_type, AnomalyType::LatencySpike);
        assert_eq!(anomaly.severity, AnomalySeverity::Medium);

        // Way above threshold
        let anomaly = detector.check_latency_spike("event1", 6000).unwrap();
        assert_eq!(anomaly.severity, AnomalySeverity::Critical);
    }

    #[test]
    fn test_detector_check_book_imbalance() {
        let detector = AnomalyDetector::with_defaults();

        // Balanced book
        assert!(detector
            .check_book_imbalance("token1", "event1", dec!(100), dec!(100))
            .is_none());

        // Slight imbalance
        assert!(detector
            .check_book_imbalance("token1", "event1", dec!(60), dec!(40))
            .is_none());

        // Significant imbalance
        let anomaly = detector
            .check_book_imbalance("token1", "event1", dec!(90), dec!(10))
            .unwrap();
        assert_eq!(anomaly.anomaly_type, AnomalyType::BookImbalance);
    }

    #[test]
    fn test_detector_record_circuit_breaker() {
        let detector = AnomalyDetector::with_defaults();

        let anomaly = detector.record_circuit_breaker_trip("consecutive failures");

        assert_eq!(anomaly.anomaly_type, AnomalyType::CircuitBreakerTrip);
        assert_eq!(anomaly.severity, AnomalySeverity::High);
        assert!(anomaly.description.contains("consecutive failures"));
    }

    #[test]
    fn test_detector_record_connection_issue() {
        let detector = AnomalyDetector::with_defaults();

        let anomaly = detector.record_connection_issue("Binance WS", "connection timeout");

        assert_eq!(anomaly.anomaly_type, AnomalyType::ConnectionIssue);
        assert_eq!(anomaly.severity, AnomalySeverity::Medium);
        assert!(anomaly.description.contains("Binance WS"));
    }

    #[test]
    fn test_detector_disabled() {
        let detector = AnomalyDetector::disabled();

        assert!(!detector.is_enabled());
        assert!(detector.check_extreme_margin("event1", dec!(0.99)).is_none());
        assert!(detector.check_latency_spike("event1", 100000).is_none());
    }

    #[test]
    fn test_anomaly_stats() {
        let stats = AnomalyStats::new();

        stats.record_anomaly(AnomalyType::ExtremeMargin);
        stats.record_anomaly(AnomalyType::ExtremeMargin);
        stats.record_anomaly(AnomalyType::FlashCrash);

        let snapshot = stats.snapshot();

        assert_eq!(snapshot.detected, 3);
        assert_eq!(snapshot.count_for_type(AnomalyType::ExtremeMargin), 2);
        assert_eq!(snapshot.count_for_type(AnomalyType::FlashCrash), 1);
    }

    #[test]
    fn test_anomaly_stats_reset() {
        let stats = AnomalyStats::new();

        stats.record_anomaly(AnomalyType::ExtremeMargin);
        assert_eq!(stats.snapshot().detected, 1);

        stats.reset();
        assert_eq!(stats.snapshot().detected, 0);
    }

    #[tokio::test]
    async fn test_detector_check_spread() {
        let detector = AnomalyDetector::with_defaults();

        // Normal spread
        assert!(detector
            .check_spread("token1", "event1", 100)
            .await
            .is_none());

        // Wide spread
        let anomaly = detector
            .check_spread("token1", "event1", 600)
            .await
            .unwrap();
        assert_eq!(anomaly.anomaly_type, AnomalyType::WideSpread);
    }

    #[tokio::test]
    async fn test_detector_check_volume() {
        let config = AnomalyConfig {
            rolling_window_size: 5,
            volume_spike_multiplier: dec!(3),
            ..Default::default()
        };
        let detector = AnomalyDetector::new(config);

        // Build up baseline (need at least half window = 3 samples)
        for _ in 0..3 {
            detector
                .check_volume("token1", "event1", dec!(100))
                .await;
        }

        // Volume spike
        let anomaly = detector
            .check_volume("token1", "event1", dec!(500))
            .await
            .unwrap();
        assert_eq!(anomaly.anomaly_type, AnomalyType::VolumeSpike);
    }

    #[tokio::test]
    async fn test_detector_check_flash_price() {
        let config = AnomalyConfig {
            flash_window_secs: 10,
            flash_price_threshold: dec!(0.05),
            ..Default::default()
        };
        let detector = AnomalyDetector::new(config);

        // First price - no anomaly
        assert!(detector
            .check_flash_price("token1", "event1", dec!(100))
            .await
            .is_none());

        // Small change - no anomaly
        assert!(detector
            .check_flash_price("token1", "event1", dec!(102))
            .await
            .is_none());

        // Large drop - flash crash
        let anomaly = detector
            .check_flash_price("token1", "event1", dec!(90))
            .await
            .unwrap();
        assert_eq!(anomaly.anomaly_type, AnomalyType::FlashCrash);
    }

    #[tokio::test]
    async fn test_detector_cleanup_stale_states() {
        let detector = AnomalyDetector::with_defaults();

        // Add some states
        detector
            .check_spread("token1", "event1", 100)
            .await;
        detector
            .check_spread("token2", "event1", 100)
            .await;

        // States should exist
        assert_eq!(detector.token_states.read().await.len(), 2);

        // Cleanup with very short max age (0 seconds means all stale)
        detector.cleanup_stale_states(0).await;

        // States should be cleaned up
        assert_eq!(detector.token_states.read().await.len(), 0);
    }

    #[test]
    fn test_create_shared_detector() {
        let detector = create_shared_detector(AnomalyConfig::default());
        assert!(detector.is_enabled());
    }

    #[test]
    fn test_anomaly_serialization() {
        let anomaly = Anomaly::new(
            AnomalyType::ExtremeMargin,
            AnomalySeverity::High,
            "Test",
        )
        .with_event_id("event1")
        .with_values(dec!(0.15), dec!(0.10), dec!(0.05));

        let json = serde_json::to_string(&anomaly).unwrap();
        assert!(json.contains("extreme_margin"));
        assert!(json.contains("HIGH"));

        let decoded: Anomaly = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.anomaly_type, AnomalyType::ExtremeMargin);
        assert_eq!(decoded.severity, AnomalySeverity::High);
    }
}
