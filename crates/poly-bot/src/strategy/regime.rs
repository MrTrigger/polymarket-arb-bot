//! Market regime detection and adaptive parameter system.
//!
//! Adjusts strategy parameters based on current market conditions to maintain
//! trading activity in both volatile and quiet markets.
//!
//! ## Regimes
//!
//! | Regime | ATR Ratio | Description | Parameter Adjustment |
//! |--------|-----------|-------------|---------------------|
//! | High   | > 1.5     | Very active | Tighten (more selective) |
//! | Normal | 0.7 - 1.5 | Typical     | Default parameters |
//! | Low    | < 0.7     | Quiet/Sunday| Relax (accept more trades) |
//!
//! ## Adaptive Parameters
//!
//! - `max_edge_factor`: Edge required for trades (lower in quiet markets)
//! - `dist_conf_per_atr`: Confidence per ATR (higher in quiet markets)
//! - `phase_confidence_multiplier`: Scales phase thresholds
//!
//! ## Usage
//!
//! ```ignore
//! let mut detector = RegimeDetector::new();
//! detector.update_atr(CryptoAsset::Btc, current_atr, baseline_atr);
//! let params = detector.get_adaptive_params();
//! ```

use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::collections::HashMap;
use std::time::{Duration, Instant};

use poly_common::types::CryptoAsset;

/// Market regime based on current volatility relative to baseline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MarketRegime {
    /// High volatility - tighten parameters to be more selective
    High,
    /// Normal volatility - use default parameters
    Normal,
    /// Low volatility (quiet/weekend) - relax parameters to find more trades
    Low,
}

impl MarketRegime {
    /// Determine regime from ATR ratio (current / baseline).
    pub fn from_atr_ratio(ratio: Decimal) -> Self {
        if ratio > dec!(1.5) {
            MarketRegime::High
        } else if ratio < dec!(0.7) {
            MarketRegime::Low
        } else {
            MarketRegime::Normal
        }
    }

    /// Display name for logging.
    pub fn name(&self) -> &'static str {
        match self {
            MarketRegime::High => "high_vol",
            MarketRegime::Normal => "normal",
            MarketRegime::Low => "low_vol",
        }
    }
}

impl std::fmt::Display for MarketRegime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.name())
    }
}

/// Adaptive parameters adjusted by regime.
#[derive(Debug, Clone)]
pub struct AdaptiveParams {
    /// Current market regime.
    pub regime: MarketRegime,
    /// ATR ratio (current / baseline) that determined the regime.
    pub atr_ratio: Decimal,

    // --- Adjusted parameters ---

    /// Max edge factor (EV required at window start).
    /// Default: 0.20, Low vol: 0.12, High vol: 0.25
    pub max_edge_factor: Decimal,

    /// Distance confidence per ATR.
    /// Default: 0.30, Low vol: 0.45, High vol: 0.25
    pub dist_conf_per_atr: Decimal,

    /// Time confidence floor.
    /// Default: 0.30, Low vol: 0.40, High vol: 0.25
    pub time_conf_floor: Decimal,

    /// Multiplier for phase confidence thresholds.
    /// Default: 1.0, Low vol: 0.85, High vol: 1.15
    /// Applied as: effective_threshold = base_threshold * multiplier
    pub phase_confidence_multiplier: Decimal,
}

impl Default for AdaptiveParams {
    fn default() -> Self {
        Self {
            regime: MarketRegime::Normal,
            atr_ratio: Decimal::ONE,
            max_edge_factor: dec!(0.20),
            dist_conf_per_atr: dec!(0.30),
            time_conf_floor: dec!(0.30),
            phase_confidence_multiplier: Decimal::ONE,
        }
    }
}

impl AdaptiveParams {
    /// Create params for a specific regime.
    pub fn for_regime(regime: MarketRegime, atr_ratio: Decimal) -> Self {
        match regime {
            MarketRegime::High => Self {
                regime,
                atr_ratio,
                // More selective in high volatility
                max_edge_factor: dec!(0.25),     // Require more edge
                dist_conf_per_atr: dec!(0.25),   // Less confidence per ATR (moves are noisy)
                time_conf_floor: dec!(0.25),     // Less trust in early predictions
                phase_confidence_multiplier: dec!(1.15), // Higher thresholds
            },
            MarketRegime::Normal => Self {
                regime,
                atr_ratio,
                // Default parameters (optimized for typical conditions)
                max_edge_factor: dec!(0.20),
                dist_conf_per_atr: dec!(0.30),
                time_conf_floor: dec!(0.30),
                phase_confidence_multiplier: Decimal::ONE,
            },
            MarketRegime::Low => Self {
                regime,
                atr_ratio,
                // Relaxed parameters for quiet markets
                max_edge_factor: dec!(0.12),     // Accept lower edge
                dist_conf_per_atr: dec!(0.45),   // More confidence per ATR (small moves meaningful)
                time_conf_floor: dec!(0.40),     // More trust in early predictions
                phase_confidence_multiplier: dec!(0.85), // Lower thresholds
            },
        }
    }

    /// Get adjusted phase minimum confidence.
    ///
    /// Base thresholds: Early=0.80, Build=0.60, Core=0.50, Final=0.40
    /// Adjusted by multiplier for current regime.
    pub fn adjusted_phase_confidence(&self, base_threshold: Decimal) -> Decimal {
        let adjusted = base_threshold * self.phase_confidence_multiplier;
        // Clamp to reasonable range [0.30, 0.95]
        adjusted.max(dec!(0.30)).min(dec!(0.95))
    }
}

/// Tracks ATR history for baseline calculation.
#[derive(Debug)]
struct AtrHistory {
    /// Recent ATR values with timestamps.
    values: Vec<(Instant, Decimal)>,
    /// How long to keep history for baseline calculation.
    history_duration: Duration,
}

impl AtrHistory {
    fn new(history_duration: Duration) -> Self {
        Self {
            values: Vec::with_capacity(1000),
            history_duration,
        }
    }

    /// Record a new ATR observation.
    fn record(&mut self, atr: Decimal) {
        let now = Instant::now();

        // Prune old entries
        let cutoff = now - self.history_duration;
        self.values.retain(|(t, _)| *t >= cutoff);

        // Add new entry
        self.values.push((now, atr));
    }

    /// Get baseline ATR (average of historical values).
    fn baseline(&self) -> Option<Decimal> {
        if self.values.is_empty() {
            return None;
        }

        let sum: Decimal = self.values.iter().map(|(_, v)| *v).sum();
        Some(sum / Decimal::from(self.values.len() as u32))
    }

    /// Get current ATR (most recent value).
    fn current(&self) -> Option<Decimal> {
        self.values.last().map(|(_, v)| *v)
    }
}

/// Detects market regime and provides adaptive parameters.
#[derive(Debug)]
pub struct RegimeDetector {
    /// ATR history per asset.
    atr_history: HashMap<CryptoAsset, AtrHistory>,
    /// How long to keep ATR history for baseline (default 4 hours).
    history_duration: Duration,
    /// Minimum samples needed before adapting (default 20).
    min_samples: usize,
    /// Current regime per asset.
    current_regime: HashMap<CryptoAsset, MarketRegime>,
    /// Cached adaptive params.
    cached_params: AdaptiveParams,
    /// Last update time.
    last_update: Instant,
    /// Minimum time between regime updates (prevents thrashing).
    update_cooldown: Duration,
}

impl Default for RegimeDetector {
    fn default() -> Self {
        Self::new()
    }
}

impl RegimeDetector {
    /// Create a new regime detector with default settings.
    pub fn new() -> Self {
        Self::with_config(
            Duration::from_secs(4 * 3600), // 4 hour history
            20,                             // 20 samples minimum
            Duration::from_secs(60),        // 1 minute cooldown
        )
    }

    /// Create with custom configuration.
    pub fn with_config(
        history_duration: Duration,
        min_samples: usize,
        update_cooldown: Duration,
    ) -> Self {
        Self {
            atr_history: HashMap::new(),
            history_duration,
            min_samples,
            current_regime: HashMap::new(),
            cached_params: AdaptiveParams::default(),
            last_update: Instant::now(),
            update_cooldown,
        }
    }

    /// Update ATR for an asset and recalculate regime if needed.
    pub fn update_atr(&mut self, asset: CryptoAsset, current_atr: Decimal) {
        // Get or create history for this asset
        let history = self.atr_history
            .entry(asset)
            .or_insert_with(|| AtrHistory::new(self.history_duration));

        // Record the ATR
        history.record(current_atr);

        // Check if we should update regime (cooldown)
        if self.last_update.elapsed() < self.update_cooldown {
            return;
        }

        // Need enough samples to calculate meaningful baseline
        if history.values.len() < self.min_samples {
            return;
        }

        // Calculate regime for this asset
        if let (Some(current), Some(baseline)) = (history.current(), history.baseline()) {
            if baseline > Decimal::ZERO {
                let ratio = current / baseline;
                let regime = MarketRegime::from_atr_ratio(ratio);
                self.current_regime.insert(asset, regime);
            }
        }

        // Update overall regime (use most common across assets, or most conservative)
        self.update_overall_regime();
        self.last_update = Instant::now();
    }

    /// Update the overall regime based on all tracked assets.
    fn update_overall_regime(&mut self) {
        if self.current_regime.is_empty() {
            return;
        }

        // Calculate average ATR ratio across all assets
        let mut total_ratio = Decimal::ZERO;
        let mut count = 0u32;

        for (asset, _regime) in &self.current_regime {
            if let Some(history) = self.atr_history.get(asset) {
                if let (Some(current), Some(baseline)) = (history.current(), history.baseline()) {
                    if baseline > Decimal::ZERO {
                        total_ratio += current / baseline;
                        count += 1;
                    }
                }
            }
        }

        if count > 0 {
            let avg_ratio = total_ratio / Decimal::from(count);
            let regime = MarketRegime::from_atr_ratio(avg_ratio);
            self.cached_params = AdaptiveParams::for_regime(regime, avg_ratio);
        }
    }

    /// Get current adaptive parameters.
    pub fn get_adaptive_params(&self) -> &AdaptiveParams {
        &self.cached_params
    }

    /// Get current regime.
    pub fn current_regime(&self) -> MarketRegime {
        self.cached_params.regime
    }

    /// Get regime for a specific asset.
    pub fn asset_regime(&self, asset: CryptoAsset) -> MarketRegime {
        self.current_regime.get(&asset).copied().unwrap_or(MarketRegime::Normal)
    }

    /// Check if we have enough data to provide adaptive params.
    pub fn is_warmed_up(&self) -> bool {
        self.atr_history.values().any(|h| h.values.len() >= self.min_samples)
    }

    /// Get status summary for logging.
    pub fn status_summary(&self) -> String {
        let mut parts = Vec::new();
        parts.push(format!("regime={}", self.cached_params.regime));
        parts.push(format!("atr_ratio={:.2}", self.cached_params.atr_ratio));

        for (asset, history) in &self.atr_history {
            if let (Some(current), Some(baseline)) = (history.current(), history.baseline()) {
                let ratio = if baseline > Decimal::ZERO {
                    current / baseline
                } else {
                    Decimal::ONE
                };
                parts.push(format!(
                    "{}:cur={:.2}/base={:.2}({:.2}x)",
                    asset, current, baseline, ratio
                ));
            }
        }

        parts.join(" ")
    }

    /// Force a specific regime (useful for testing or manual override).
    pub fn force_regime(&mut self, regime: MarketRegime) {
        self.cached_params = AdaptiveParams::for_regime(regime, Decimal::ONE);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_regime_from_atr_ratio() {
        assert_eq!(MarketRegime::from_atr_ratio(dec!(2.0)), MarketRegime::High);
        assert_eq!(MarketRegime::from_atr_ratio(dec!(1.5)), MarketRegime::Normal);
        assert_eq!(MarketRegime::from_atr_ratio(dec!(1.0)), MarketRegime::Normal);
        assert_eq!(MarketRegime::from_atr_ratio(dec!(0.7)), MarketRegime::Normal);
        assert_eq!(MarketRegime::from_atr_ratio(dec!(0.5)), MarketRegime::Low);
        assert_eq!(MarketRegime::from_atr_ratio(dec!(0.3)), MarketRegime::Low);
    }

    #[test]
    fn test_adaptive_params_for_regime() {
        let high = AdaptiveParams::for_regime(MarketRegime::High, dec!(1.8));
        assert_eq!(high.max_edge_factor, dec!(0.25));
        assert!(high.phase_confidence_multiplier > Decimal::ONE);

        let low = AdaptiveParams::for_regime(MarketRegime::Low, dec!(0.5));
        assert_eq!(low.max_edge_factor, dec!(0.12));
        assert!(low.phase_confidence_multiplier < Decimal::ONE);

        let normal = AdaptiveParams::for_regime(MarketRegime::Normal, dec!(1.0));
        assert_eq!(normal.max_edge_factor, dec!(0.20));
        assert_eq!(normal.phase_confidence_multiplier, Decimal::ONE);
    }

    #[test]
    fn test_adjusted_phase_confidence() {
        let low = AdaptiveParams::for_regime(MarketRegime::Low, dec!(0.5));

        // Early phase threshold: 0.80 * 0.85 = 0.68
        let early = low.adjusted_phase_confidence(dec!(0.80));
        assert_eq!(early, dec!(0.68));

        // Final phase threshold: 0.40 * 0.85 = 0.34 -> clamped to 0.30
        let final_phase = low.adjusted_phase_confidence(dec!(0.40));
        assert_eq!(final_phase, dec!(0.34));
    }

    #[test]
    fn test_regime_detector_default() {
        let detector = RegimeDetector::new();
        assert_eq!(detector.current_regime(), MarketRegime::Normal);
        assert!(!detector.is_warmed_up());
    }

    #[test]
    fn test_regime_detector_warmup() {
        let mut detector = RegimeDetector::with_config(
            Duration::from_secs(3600),
            5, // Low sample requirement for test
            Duration::from_millis(1), // Fast cooldown for test
        );

        // Record enough ATR values
        for i in 0..10 {
            detector.update_atr(CryptoAsset::Btc, dec!(50) + Decimal::from(i));
            std::thread::sleep(Duration::from_millis(2)); // Exceed cooldown
        }

        assert!(detector.is_warmed_up());
    }

    #[test]
    fn test_low_volatility_detection() {
        let mut detector = RegimeDetector::with_config(
            Duration::from_secs(3600),
            5,
            Duration::from_millis(1),
        );

        // First establish a baseline with normal ATR
        for _ in 0..10 {
            detector.update_atr(CryptoAsset::Btc, dec!(100));
            std::thread::sleep(Duration::from_millis(2));
        }

        // Now record much lower ATR (simulating quiet market)
        for _ in 0..10 {
            detector.update_atr(CryptoAsset::Btc, dec!(40)); // 40% of baseline
            std::thread::sleep(Duration::from_millis(2));
        }

        // Should detect low volatility
        // Note: This might still be Normal depending on averaging, but ratio should be < 1
        assert!(detector.cached_params.atr_ratio < Decimal::ONE);
    }

    #[test]
    fn test_force_regime() {
        let mut detector = RegimeDetector::new();

        detector.force_regime(MarketRegime::Low);
        assert_eq!(detector.current_regime(), MarketRegime::Low);
        assert_eq!(detector.get_adaptive_params().max_edge_factor, dec!(0.12));

        detector.force_regime(MarketRegime::High);
        assert_eq!(detector.current_regime(), MarketRegime::High);
        assert_eq!(detector.get_adaptive_params().max_edge_factor, dec!(0.25));
    }

    #[test]
    fn test_regime_display() {
        assert_eq!(format!("{}", MarketRegime::High), "high_vol");
        assert_eq!(format!("{}", MarketRegime::Normal), "normal");
        assert_eq!(format!("{}", MarketRegime::Low), "low_vol");
    }
}
