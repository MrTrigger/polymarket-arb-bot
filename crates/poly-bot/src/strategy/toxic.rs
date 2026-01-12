//! Toxic flow detection for binary options markets.
//!
//! This module identifies potentially harmful order flow that could indicate:
//! - Informed traders with inside information
//! - Large players about to move the market
//! - Front-running or spoofing activity
//!
//! ## Detection Criteria
//!
//! An order is considered potentially toxic if:
//! 1. Size is significantly larger than rolling average (>50x)
//! 2. Appears suddenly (<500ms after prior state)
//! 3. Significantly shifts the BBO
//!
//! ## Hot Path Requirements
//!
//! Toxic flow check runs on the hot path and must be fast:
//! - Target: <1μs for check
//! - Use pre-computed rolling averages
//! - Minimal allocations

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

use poly_common::types::Side;

/// Configuration for toxic flow detection.
#[derive(Debug, Clone)]
pub struct ToxicFlowConfig {
    /// Size multiplier threshold (e.g., 50x average).
    pub size_multiplier_threshold: Decimal,
    /// Time window for "sudden" appearance (milliseconds).
    pub sudden_appearance_ms: u64,
    /// Window size for rolling average calculation.
    pub rolling_window_size: usize,
    /// Minimum samples before detection is active.
    pub min_samples: usize,
    /// BBO shift threshold in basis points to consider significant.
    pub bbo_shift_threshold_bps: u32,
}

impl Default for ToxicFlowConfig {
    fn default() -> Self {
        Self {
            size_multiplier_threshold: Decimal::new(50, 0), // 50x
            sudden_appearance_ms: 500,
            rolling_window_size: 100,
            min_samples: 10,
            bbo_shift_threshold_bps: 100, // 1%
        }
    }
}

/// Severity level of toxic flow warning.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum ToxicSeverity {
    /// Low severity - single indicator triggered.
    Low,
    /// Medium severity - multiple indicators triggered.
    Medium,
    /// High severity - all indicators triggered, strong signal.
    High,
    /// Critical severity - extreme values, likely informed flow.
    Critical,
}

impl ToxicSeverity {
    /// Get a numeric score for this severity (0-100).
    pub fn score(&self) -> u8 {
        match self {
            ToxicSeverity::Low => 25,
            ToxicSeverity::Medium => 50,
            ToxicSeverity::High => 75,
            ToxicSeverity::Critical => 100,
        }
    }

    /// Check if trading should be blocked at this severity.
    pub fn should_block_trading(&self) -> bool {
        matches!(self, ToxicSeverity::High | ToxicSeverity::Critical)
    }

    /// Get recommended size multiplier adjustment.
    ///
    /// Lower multipliers reduce position size for risk management.
    pub fn size_multiplier(&self) -> Decimal {
        match self {
            ToxicSeverity::Low => Decimal::new(75, 2),      // 0.75
            ToxicSeverity::Medium => Decimal::new(50, 2),   // 0.50
            ToxicSeverity::High => Decimal::new(25, 2),     // 0.25
            ToxicSeverity::Critical => Decimal::ZERO,       // 0.00 (no trade)
        }
    }
}

impl std::fmt::Display for ToxicSeverity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ToxicSeverity::Low => write!(f, "LOW"),
            ToxicSeverity::Medium => write!(f, "MEDIUM"),
            ToxicSeverity::High => write!(f, "HIGH"),
            ToxicSeverity::Critical => write!(f, "CRITICAL"),
        }
    }
}

/// Warning about detected toxic flow.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToxicFlowWarning {
    /// Token ID where toxic flow was detected.
    pub token_id: String,
    /// Side of the toxic order (Buy/Sell).
    pub side: Side,
    /// Size of the toxic order.
    pub order_size: Decimal,
    /// Rolling average order size.
    pub avg_order_size: Decimal,
    /// Size multiplier (order_size / avg_order_size).
    pub size_multiplier: Decimal,
    /// Time since last update (milliseconds).
    pub time_since_last_ms: u64,
    /// BBO shift in basis points.
    pub bbo_shift_bps: Option<u32>,
    /// Overall severity assessment.
    pub severity: ToxicSeverity,
    /// Detection timestamp (milliseconds since epoch).
    pub detected_at_ms: i64,
    /// Indicators that triggered.
    pub indicators: ToxicIndicators,
}

impl ToxicFlowWarning {
    /// Check if this warning should block trading.
    #[inline]
    pub fn should_block(&self) -> bool {
        self.severity.should_block_trading()
    }

    /// Get recommended size multiplier.
    #[inline]
    pub fn size_multiplier(&self) -> Decimal {
        self.severity.size_multiplier()
    }
}

/// Individual indicators that triggered toxic detection.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct ToxicIndicators {
    /// Large order relative to average.
    pub large_order: bool,
    /// Sudden appearance (short time since last update).
    pub sudden_appearance: bool,
    /// Significant BBO shift.
    pub bbo_shift: bool,
    /// Extreme size (>100x average).
    pub extreme_size: bool,
}

impl ToxicIndicators {
    /// Count how many indicators are triggered.
    pub fn count(&self) -> u8 {
        let mut count = 0;
        if self.large_order {
            count += 1;
        }
        if self.sudden_appearance {
            count += 1;
        }
        if self.bbo_shift {
            count += 1;
        }
        if self.extreme_size {
            count += 1;
        }
        count
    }
}

/// Per-token state for tracking order flow.
#[derive(Debug, Clone)]
struct TokenFlowState {
    /// Rolling window of order sizes.
    order_sizes: VecDeque<Decimal>,
    /// Sum of sizes in window (for O(1) average).
    size_sum: Decimal,
    /// Last update timestamp (milliseconds).
    last_update_ms: i64,
    /// Previous best bid price.
    prev_best_bid: Option<Decimal>,
    /// Previous best ask price.
    prev_best_ask: Option<Decimal>,
}

impl TokenFlowState {
    fn new(window_size: usize) -> Self {
        Self {
            order_sizes: VecDeque::with_capacity(window_size),
            size_sum: Decimal::ZERO,
            last_update_ms: 0,
            prev_best_bid: None,
            prev_best_ask: None,
        }
    }

    /// Add a new order size to the rolling window.
    fn add_order(&mut self, size: Decimal, max_window: usize) {
        // Remove oldest if at capacity
        if self.order_sizes.len() >= max_window
            && let Some(old) = self.order_sizes.pop_front()
        {
            self.size_sum -= old;
        }
        self.order_sizes.push_back(size);
        self.size_sum += size;
    }

    /// Get the rolling average order size.
    #[inline]
    fn average_size(&self) -> Option<Decimal> {
        if self.order_sizes.is_empty() {
            return None;
        }
        Some(self.size_sum / Decimal::from(self.order_sizes.len()))
    }

    /// Get number of samples in the window.
    #[inline]
    fn sample_count(&self) -> usize {
        self.order_sizes.len()
    }
}

/// Toxic flow detector with rolling statistics.
#[derive(Debug)]
pub struct ToxicFlowDetector {
    /// Configuration.
    config: ToxicFlowConfig,
    /// Per-token flow state.
    token_states: std::collections::HashMap<String, TokenFlowState>,
}

impl Default for ToxicFlowDetector {
    fn default() -> Self {
        Self::new(ToxicFlowConfig::default())
    }
}

impl ToxicFlowDetector {
    /// Create a new detector with the given configuration.
    pub fn new(config: ToxicFlowConfig) -> Self {
        Self {
            config,
            token_states: std::collections::HashMap::new(),
        }
    }

    /// Get or create token state.
    fn get_or_create_state(&mut self, token_id: &str) -> &mut TokenFlowState {
        self.token_states
            .entry(token_id.to_string())
            .or_insert_with(|| TokenFlowState::new(self.config.rolling_window_size))
    }

    /// Record an order and check for toxic flow.
    ///
    /// Call this for every order observed (both sides).
    /// Returns Some(warning) if toxic flow is detected.
    pub fn check_order(
        &mut self,
        token_id: &str,
        side: Side,
        size: Decimal,
        current_ms: i64,
        best_bid: Option<Decimal>,
        best_ask: Option<Decimal>,
    ) -> Option<ToxicFlowWarning> {
        // Read config values before mutable borrow
        let rolling_window_size = self.config.rolling_window_size;

        let state = self.get_or_create_state(token_id);

        // Get prior values before update
        let avg_size = state.average_size();
        let sample_count = state.sample_count();
        let time_since_last = if state.last_update_ms > 0 {
            (current_ms - state.last_update_ms).max(0) as u64
        } else {
            u64::MAX // First observation, don't flag as sudden
        };
        let prev_bid = state.prev_best_bid;
        let prev_ask = state.prev_best_ask;

        // Update state
        state.add_order(size, rolling_window_size);
        state.last_update_ms = current_ms;
        state.prev_best_bid = best_bid;
        state.prev_best_ask = best_ask;

        // Skip if not enough samples
        if sample_count < self.config.min_samples {
            return None;
        }

        // Calculate indicators
        let avg = avg_size?;
        if avg <= Decimal::ZERO {
            return None;
        }

        let size_mult = size / avg;
        let mut indicators = ToxicIndicators::default();

        // Check for large order (>threshold multiple of average)
        if size_mult >= self.config.size_multiplier_threshold {
            indicators.large_order = true;
        }

        // Check for extreme size (>2x threshold)
        if size_mult >= self.config.size_multiplier_threshold * Decimal::TWO {
            indicators.extreme_size = true;
        }

        // Check for sudden appearance
        if time_since_last <= self.config.sudden_appearance_ms {
            indicators.sudden_appearance = true;
        }

        // Check for BBO shift
        let bbo_shift_bps = self.calculate_bbo_shift(
            side,
            prev_bid,
            prev_ask,
            best_bid,
            best_ask,
        );
        if let Some(shift) = bbo_shift_bps
            && shift >= self.config.bbo_shift_threshold_bps
        {
            indicators.bbo_shift = true;
        }

        // Determine severity based on indicators
        let severity = self.calculate_severity(&indicators, size_mult);

        // Only return warning if any indicator triggered
        if indicators.count() == 0 {
            return None;
        }

        Some(ToxicFlowWarning {
            token_id: token_id.to_string(),
            side,
            order_size: size,
            avg_order_size: avg,
            size_multiplier: size_mult,
            time_since_last_ms: time_since_last,
            bbo_shift_bps,
            severity,
            detected_at_ms: current_ms,
            indicators,
        })
    }

    /// Calculate BBO shift in basis points.
    fn calculate_bbo_shift(
        &self,
        side: Side,
        prev_bid: Option<Decimal>,
        prev_ask: Option<Decimal>,
        curr_bid: Option<Decimal>,
        curr_ask: Option<Decimal>,
    ) -> Option<u32> {
        match side {
            Side::Buy => {
                // Buy order affects ask side
                let prev = prev_ask?;
                let curr = curr_ask?;
                if prev <= Decimal::ZERO {
                    return None;
                }
                let shift = ((curr - prev).abs() / prev) * Decimal::new(10000, 0);
                Some(shift.try_into().unwrap_or(u32::MAX))
            }
            Side::Sell => {
                // Sell order affects bid side
                let prev = prev_bid?;
                let curr = curr_bid?;
                if prev <= Decimal::ZERO {
                    return None;
                }
                let shift = ((curr - prev).abs() / prev) * Decimal::new(10000, 0);
                Some(shift.try_into().unwrap_or(u32::MAX))
            }
        }
    }

    /// Calculate severity based on triggered indicators.
    fn calculate_severity(&self, indicators: &ToxicIndicators, size_mult: Decimal) -> ToxicSeverity {
        let count = indicators.count();

        // Critical: extreme size regardless of other indicators
        if indicators.extreme_size {
            return ToxicSeverity::Critical;
        }

        // High: 3+ indicators or large + sudden
        if count >= 3 || (indicators.large_order && indicators.sudden_appearance) {
            return ToxicSeverity::High;
        }

        // Medium: 2 indicators or very large order (>75x)
        if count >= 2 || size_mult >= Decimal::new(75, 0) {
            return ToxicSeverity::Medium;
        }

        // Low: 1 indicator
        ToxicSeverity::Low
    }

    /// Quick check if a size would be considered toxic.
    ///
    /// Use for fast pre-filtering without full state update.
    #[inline]
    pub fn would_be_large(&self, token_id: &str, size: Decimal) -> bool {
        if let Some(state) = self.token_states.get(token_id)
            && let Some(avg) = state.average_size()
            && avg > Decimal::ZERO
        {
            return size / avg >= self.config.size_multiplier_threshold;
        }
        false
    }

    /// Get the current average order size for a token.
    pub fn average_order_size(&self, token_id: &str) -> Option<Decimal> {
        self.token_states.get(token_id)?.average_size()
    }

    /// Get the sample count for a token.
    pub fn sample_count(&self, token_id: &str) -> usize {
        self.token_states
            .get(token_id)
            .map(|s| s.sample_count())
            .unwrap_or(0)
    }

    /// Reset state for a specific token.
    pub fn reset_token(&mut self, token_id: &str) {
        self.token_states.remove(token_id);
    }

    /// Reset all state.
    pub fn reset(&mut self) {
        self.token_states.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn make_detector() -> ToxicFlowDetector {
        ToxicFlowDetector::new(ToxicFlowConfig {
            size_multiplier_threshold: dec!(50),
            sudden_appearance_ms: 500,
            rolling_window_size: 100,
            min_samples: 10,
            bbo_shift_threshold_bps: 100,
        })
    }

    fn seed_detector(detector: &mut ToxicFlowDetector, token_id: &str, avg_size: Decimal, count: usize) {
        for i in 0..count {
            detector.check_order(
                token_id,
                Side::Buy,
                avg_size,
                i as i64 * 1000, // 1 second apart
                Some(dec!(0.50)),
                Some(dec!(0.51)),
            );
        }
    }

    #[test]
    fn test_config_default() {
        let config = ToxicFlowConfig::default();
        assert_eq!(config.size_multiplier_threshold, dec!(50));
        assert_eq!(config.sudden_appearance_ms, 500);
        assert_eq!(config.rolling_window_size, 100);
        assert_eq!(config.min_samples, 10);
    }

    #[test]
    fn test_severity_levels() {
        assert!(ToxicSeverity::Low < ToxicSeverity::Medium);
        assert!(ToxicSeverity::Medium < ToxicSeverity::High);
        assert!(ToxicSeverity::High < ToxicSeverity::Critical);

        assert_eq!(ToxicSeverity::Low.score(), 25);
        assert_eq!(ToxicSeverity::Critical.score(), 100);

        assert!(!ToxicSeverity::Low.should_block_trading());
        assert!(!ToxicSeverity::Medium.should_block_trading());
        assert!(ToxicSeverity::High.should_block_trading());
        assert!(ToxicSeverity::Critical.should_block_trading());
    }

    #[test]
    fn test_severity_size_multiplier() {
        assert_eq!(ToxicSeverity::Low.size_multiplier(), dec!(0.75));
        assert_eq!(ToxicSeverity::Medium.size_multiplier(), dec!(0.50));
        assert_eq!(ToxicSeverity::High.size_multiplier(), dec!(0.25));
        assert_eq!(ToxicSeverity::Critical.size_multiplier(), dec!(0));
    }

    #[test]
    fn test_no_detection_before_min_samples() {
        let mut detector = make_detector();

        // Add 9 orders (below min_samples of 10)
        for i in 0..9 {
            let result = detector.check_order(
                "token1",
                Side::Buy,
                dec!(100),
                i * 1000,
                Some(dec!(0.50)),
                Some(dec!(0.51)),
            );
            assert!(result.is_none(), "Should not detect before min_samples");
        }

        // 10th order enables detection
        assert_eq!(detector.sample_count("token1"), 9);
    }

    #[test]
    fn test_detect_large_order() {
        let mut detector = make_detector();

        // Seed with normal orders (avg = 100)
        seed_detector(&mut detector, "token1", dec!(100), 15);

        // Large order: 5000 = 50x average (exactly at threshold)
        let result = detector.check_order(
            "token1",
            Side::Buy,
            dec!(5000),
            20_000, // 20 seconds later
            Some(dec!(0.50)),
            Some(dec!(0.51)),
        );

        assert!(result.is_some());
        let warning = result.unwrap();
        assert!(warning.indicators.large_order);
        assert_eq!(warning.order_size, dec!(5000));
        assert!(warning.size_multiplier >= dec!(50));
    }

    #[test]
    fn test_detect_extreme_size() {
        let mut detector = make_detector();

        // Seed with normal orders (avg = 100)
        seed_detector(&mut detector, "token1", dec!(100), 15);

        // Extreme order: 10000 = 100x average (2x threshold)
        let result = detector.check_order(
            "token1",
            Side::Buy,
            dec!(10000),
            20_000,
            Some(dec!(0.50)),
            Some(dec!(0.51)),
        );

        assert!(result.is_some());
        let warning = result.unwrap();
        assert!(warning.indicators.extreme_size);
        assert_eq!(warning.severity, ToxicSeverity::Critical);
    }

    #[test]
    fn test_detect_sudden_appearance() {
        let mut detector = make_detector();

        // Seed with normal orders
        seed_detector(&mut detector, "token1", dec!(100), 15);

        // Record last update time
        let last_ms = 14_000i64;

        // Sudden large order: within 500ms of last
        // Note: we need a large order for detection since sudden alone isn't enough
        // unless combined with other factors
        let result = detector.check_order(
            "token1",
            Side::Buy,
            dec!(5000), // 50x
            last_ms + 100, // 100ms later (sudden)
            Some(dec!(0.50)),
            Some(dec!(0.51)),
        );

        assert!(result.is_some());
        let warning = result.unwrap();
        assert!(warning.indicators.sudden_appearance);
        assert!(warning.indicators.large_order);
        // Large + sudden = High severity
        assert!(warning.severity >= ToxicSeverity::High);
    }

    #[test]
    fn test_detect_bbo_shift() {
        let mut detector = make_detector();

        // Seed with normal orders at stable BBO
        seed_detector(&mut detector, "token1", dec!(100), 15);

        // Large order with BBO shift
        // Previous ask was 0.51, now 0.54 = 5.88% shift (588 bps)
        let result = detector.check_order(
            "token1",
            Side::Buy,
            dec!(5000),
            20_000,
            Some(dec!(0.50)),
            Some(dec!(0.54)), // Shifted from 0.51
        );

        assert!(result.is_some());
        let warning = result.unwrap();
        assert!(warning.indicators.bbo_shift);
        assert!(warning.bbo_shift_bps.is_some());
        assert!(warning.bbo_shift_bps.unwrap() >= 100);
    }

    #[test]
    fn test_no_detection_for_normal_orders() {
        let mut detector = make_detector();

        // Seed with normal orders
        seed_detector(&mut detector, "token1", dec!(100), 15);

        // Normal order: not large, not sudden
        let result = detector.check_order(
            "token1",
            Side::Buy,
            dec!(100), // Same as average
            20_000,    // Not sudden
            Some(dec!(0.50)),
            Some(dec!(0.51)), // No BBO shift
        );

        assert!(result.is_none(), "Should not flag normal orders");
    }

    #[test]
    fn test_severity_calculation() {
        // 1 indicator = Low
        let mut indicators = ToxicIndicators::default();
        indicators.large_order = true;

        let detector = make_detector();
        let severity = detector.calculate_severity(&indicators, dec!(50));
        assert_eq!(severity, ToxicSeverity::Low);

        // 2 indicators = Medium
        indicators.bbo_shift = true;
        let severity = detector.calculate_severity(&indicators, dec!(50));
        assert_eq!(severity, ToxicSeverity::Medium);

        // Large + sudden = High
        indicators.bbo_shift = false;
        indicators.sudden_appearance = true;
        let severity = detector.calculate_severity(&indicators, dec!(50));
        assert_eq!(severity, ToxicSeverity::High);

        // Extreme size = Critical
        indicators.extreme_size = true;
        let severity = detector.calculate_severity(&indicators, dec!(100));
        assert_eq!(severity, ToxicSeverity::Critical);
    }

    #[test]
    fn test_would_be_large() {
        let mut detector = make_detector();

        // No state yet - false
        assert!(!detector.would_be_large("token1", dec!(5000)));

        // Seed
        seed_detector(&mut detector, "token1", dec!(100), 15);

        // Check sizes
        assert!(!detector.would_be_large("token1", dec!(100)));  // 1x
        assert!(!detector.would_be_large("token1", dec!(4900))); // 49x
        assert!(detector.would_be_large("token1", dec!(5000)));  // 50x
        assert!(detector.would_be_large("token1", dec!(10000))); // 100x
    }

    #[test]
    fn test_average_order_size() {
        let mut detector = make_detector();

        // No state
        assert!(detector.average_order_size("token1").is_none());

        // Seed with avg = 100
        seed_detector(&mut detector, "token1", dec!(100), 10);

        let avg = detector.average_order_size("token1").unwrap();
        assert_eq!(avg, dec!(100));
    }

    #[test]
    fn test_rolling_window() {
        let mut detector = ToxicFlowDetector::new(ToxicFlowConfig {
            rolling_window_size: 5, // Small window for testing
            min_samples: 3,
            ..Default::default()
        });

        // Add 5 orders of size 100
        for i in 0..5 {
            detector.check_order(
                "token1",
                Side::Buy,
                dec!(100),
                i * 1000,
                None,
                None,
            );
        }
        assert_eq!(detector.average_order_size("token1"), Some(dec!(100)));
        assert_eq!(detector.sample_count("token1"), 5);

        // Add order of size 200 - oldest (100) should be removed
        // New window: 100, 100, 100, 100, 200 = 600/5 = 120
        detector.check_order(
            "token1",
            Side::Buy,
            dec!(200),
            5000,
            None,
            None,
        );
        assert_eq!(detector.sample_count("token1"), 5);
        assert_eq!(detector.average_order_size("token1"), Some(dec!(120)));
    }

    #[test]
    fn test_reset_token() {
        let mut detector = make_detector();
        seed_detector(&mut detector, "token1", dec!(100), 10);
        seed_detector(&mut detector, "token2", dec!(200), 10);

        assert!(detector.average_order_size("token1").is_some());
        assert!(detector.average_order_size("token2").is_some());

        detector.reset_token("token1");

        assert!(detector.average_order_size("token1").is_none());
        assert!(detector.average_order_size("token2").is_some());
    }

    #[test]
    fn test_reset_all() {
        let mut detector = make_detector();
        seed_detector(&mut detector, "token1", dec!(100), 10);
        seed_detector(&mut detector, "token2", dec!(200), 10);

        detector.reset();

        assert!(detector.average_order_size("token1").is_none());
        assert!(detector.average_order_size("token2").is_none());
    }

    #[test]
    fn test_indicators_count() {
        let mut indicators = ToxicIndicators::default();
        assert_eq!(indicators.count(), 0);

        indicators.large_order = true;
        assert_eq!(indicators.count(), 1);

        indicators.sudden_appearance = true;
        assert_eq!(indicators.count(), 2);

        indicators.bbo_shift = true;
        assert_eq!(indicators.count(), 3);

        indicators.extreme_size = true;
        assert_eq!(indicators.count(), 4);
    }

    #[test]
    fn test_warning_helpers() {
        let warning = ToxicFlowWarning {
            token_id: "token1".to_string(),
            side: Side::Buy,
            order_size: dec!(5000),
            avg_order_size: dec!(100),
            size_multiplier: dec!(50),
            time_since_last_ms: 1000,
            bbo_shift_bps: Some(100),
            severity: ToxicSeverity::High,
            detected_at_ms: 12345,
            indicators: ToxicIndicators {
                large_order: true,
                sudden_appearance: true,
                ..Default::default()
            },
        };

        assert!(warning.should_block());
        assert_eq!(warning.size_multiplier(), dec!(0.25));
    }

    #[test]
    fn test_severity_display() {
        assert_eq!(ToxicSeverity::Low.to_string(), "LOW");
        assert_eq!(ToxicSeverity::Medium.to_string(), "MEDIUM");
        assert_eq!(ToxicSeverity::High.to_string(), "HIGH");
        assert_eq!(ToxicSeverity::Critical.to_string(), "CRITICAL");
    }

    #[test]
    fn test_multiple_tokens() {
        let mut detector = make_detector();

        // Different tokens with different averages
        seed_detector(&mut detector, "token1", dec!(100), 15);
        seed_detector(&mut detector, "token2", dec!(1000), 15);

        // 5000 is large for token1 (50x) but small for token2 (5x)
        let result1 = detector.check_order(
            "token1",
            Side::Buy,
            dec!(5000),
            20_000,
            None,
            None,
        );
        assert!(result1.is_some());

        let result2 = detector.check_order(
            "token2",
            Side::Buy,
            dec!(5000),
            20_000,
            None,
            None,
        );
        assert!(result2.is_none());
    }
}
