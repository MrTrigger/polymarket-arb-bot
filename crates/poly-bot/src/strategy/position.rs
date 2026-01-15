//! Phase-based position management system.
//!
//! This module implements intelligent capital allocation based on opportunity quality,
//! not pace-based throttling. Key principles:
//!
//! 1. **Reserve capital for later phases** when signals are clearer
//! 2. **Gate trades by confidence threshold** - stricter early, looser late
//! 3. **Scale position size by confidence** - bigger when more certain
//! 4. **Enforce position limits** - never go all-in on one side
//!
//! ## Market Phases
//!
//! A 15-minute market is divided into four phases:
//!
//! | Phase | Time Remaining | Budget % | Min Confidence |
//! |-------|----------------|----------|----------------|
//! | Early | 15 → 10 min    | 15%      | 0.85           |
//! | Build | 10 → 5 min     | 25%      | 0.75           |
//! | Core  | 5 → 2 min      | 30%      | 0.65           |
//! | Final | 2 → 0 min      | 30%      | 0.45           |
//!
//! Optimized via parameter sweep with dynamic ATR (18.2% ROI).

use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Market phase based on time remaining.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Phase {
    /// 15 → 10 min: High uncertainty, only exceptional signals
    Early,
    /// 10 → 5 min: Starting to see direction, selective trading
    Build,
    /// 5 → 2 min: Main trading phase, signals clearer
    Core,
    /// 2 → 0 min: Highest conviction, outcome nearly certain
    Final,
}

impl Phase {
    /// Get the phase for a given number of minutes remaining.
    pub fn from_minutes(minutes: Decimal) -> Self {
        if minutes > dec!(10) {
            Phase::Early
        } else if minutes > dec!(5) {
            Phase::Build
        } else if minutes > dec!(2) {
            Phase::Core
        } else {
            Phase::Final
        }
    }

    /// Get the phase for a given number of seconds remaining.
    pub fn from_seconds(seconds: i64) -> Self {
        let minutes = Decimal::new(seconds, 0) / dec!(60);
        Self::from_minutes(minutes)
    }

    /// Budget allocation percentage for this phase.
    pub fn budget_allocation(&self) -> Decimal {
        match self {
            Phase::Early => dec!(0.15), // 15%
            Phase::Build => dec!(0.25), // 25%
            Phase::Core => dec!(0.30),  // 30%
            Phase::Final => dec!(0.30), // 30%
        }
    }

    /// Minimum confidence threshold to trade in this phase.
    /// Optimized via parameter sweep with dynamic ATR (15m markets, 18.2% ROI).
    pub fn min_confidence(&self) -> Decimal {
        match self {
            Phase::Early => dec!(0.80), // Very selective (optimized from sweep)
            Phase::Build => dec!(0.60), // Selective (optimized from sweep)
            Phase::Core => dec!(0.50),  // Active (optimized from sweep)
            Phase::Final => dec!(0.40), // Most active (optimized from sweep)
        }
    }

    /// Display name for logging.
    pub fn name(&self) -> &'static str {
        match self {
            Phase::Early => "early",
            Phase::Build => "build",
            Phase::Core => "core",
            Phase::Final => "final",
        }
    }
}

impl std::fmt::Display for Phase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.name())
    }
}

/// Configuration for the position manager.
#[derive(Debug, Clone)]
pub struct PositionConfig {
    /// Total budget for this market (in USDC).
    pub total_budget: Decimal,
    /// Minimum order size (default $1.00).
    pub min_order_size: Decimal,
    /// Maximum single-side exposure (default 0.80 = 80%).
    pub max_single_side_exposure: Decimal,
    /// Minimum hedge ratio (default 0.20 = 20%).
    pub min_hedge_ratio: Decimal,
    /// Target number of trades per phase for base size calculation.
    pub trades_per_phase: u32,
    /// Average True Range for distance normalization.
    /// Distance confidence is measured in ATR multiples.
    pub atr: Decimal,

    // Phase-based thresholds (configurable via strategy.toml)
    /// Minimum confidence for early phase (>10 min remaining).
    pub early_threshold: Decimal,
    /// Minimum confidence for build phase (5-10 min remaining).
    pub build_threshold: Decimal,
    /// Minimum confidence for core phase (2-5 min remaining).
    pub core_threshold: Decimal,
    /// Minimum confidence for final phase (<2 min remaining).
    pub final_threshold: Decimal,
}

impl Default for PositionConfig {
    fn default() -> Self {
        Self {
            total_budget: dec!(100),
            min_order_size: dec!(1),
            max_single_side_exposure: dec!(0.80),
            min_hedge_ratio: dec!(0.20),
            trades_per_phase: 15,
            atr: dec!(100), // Default ATR suitable for BTC
            // Optimized thresholds from param sweep (18.2% ROI)
            early_threshold: dec!(0.80),
            build_threshold: dec!(0.60),
            core_threshold: dec!(0.50),
            final_threshold: dec!(0.40),
        }
    }
}

impl PositionConfig {
    /// Create a new config with the given total budget.
    pub fn new(total_budget: Decimal) -> Self {
        Self {
            total_budget,
            ..Default::default()
        }
    }

    /// Create a new config with budget and ATR.
    pub fn with_atr(total_budget: Decimal, atr: Decimal) -> Self {
        Self {
            total_budget,
            atr,
            ..Default::default()
        }
    }

    /// Get the minimum confidence threshold for a given phase.
    /// Uses configurable thresholds instead of hardcoded Phase::min_confidence().
    pub fn threshold_for_phase(&self, phase: Phase) -> Decimal {
        match phase {
            Phase::Early => self.early_threshold,
            Phase::Build => self.build_threshold,
            Phase::Core => self.core_threshold,
            Phase::Final => self.final_threshold,
        }
    }

    /// Get the budget allocated to a specific phase.
    pub fn phase_budget(&self, phase: Phase) -> Decimal {
        self.total_budget * phase.budget_allocation()
    }
}

/// Reason for skipping a trade.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SkipReason {
    /// Confidence below phase threshold.
    LowConfidence,
    /// Phase budget exhausted.
    PhaseBudgetExhausted,
    /// Total budget exhausted.
    TotalBudgetExhausted,
    /// Would exceed position limits.
    PositionLimitExceeded,
    /// Order size below minimum.
    BelowMinimumSize,
}

impl std::fmt::Display for SkipReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SkipReason::LowConfidence => write!(f, "confidence below phase threshold"),
            SkipReason::PhaseBudgetExhausted => write!(f, "phase budget exhausted"),
            SkipReason::TotalBudgetExhausted => write!(f, "total budget exhausted"),
            SkipReason::PositionLimitExceeded => write!(f, "position limit exceeded"),
            SkipReason::BelowMinimumSize => write!(f, "order size below minimum"),
        }
    }
}

/// Trade decision from position manager.
#[derive(Debug, Clone)]
pub enum TradeDecision {
    /// Skip this trade opportunity.
    Skip(SkipReason),
    /// Execute a trade with the given size.
    Trade {
        /// Total size to trade (in USDC).
        size: Decimal,
        /// Confidence level (0.0 to 1.0).
        confidence: Decimal,
        /// Current phase.
        phase: Phase,
    },
}

impl TradeDecision {
    /// Check if this is a trade decision.
    pub fn is_trade(&self) -> bool {
        matches!(self, TradeDecision::Trade { .. })
    }

    /// Get the trade size if this is a trade decision.
    pub fn size(&self) -> Option<Decimal> {
        match self {
            TradeDecision::Trade { size, .. } => Some(*size),
            TradeDecision::Skip(_) => None,
        }
    }
}

/// Position manager that controls when, how much, and in what ratio to trade.
#[derive(Debug, Clone)]
pub struct PositionManager {
    /// Configuration.
    config: PositionConfig,
    /// Amount spent per phase.
    phase_spent: HashMap<Phase, Decimal>,
    /// Total amount spent.
    total_spent: Decimal,
    /// Current UP exposure (0.0 to 1.0).
    up_exposure: Decimal,
    /// Current DOWN exposure (0.0 to 1.0).
    down_exposure: Decimal,
    /// Trade count for this market.
    trade_count: u32,
}

impl PositionManager {
    /// Create a new position manager with the given config.
    pub fn new(config: PositionConfig) -> Self {
        let mut phase_spent = HashMap::new();
        phase_spent.insert(Phase::Early, Decimal::ZERO);
        phase_spent.insert(Phase::Build, Decimal::ZERO);
        phase_spent.insert(Phase::Core, Decimal::ZERO);
        phase_spent.insert(Phase::Final, Decimal::ZERO);

        Self {
            config,
            phase_spent,
            total_spent: Decimal::ZERO,
            up_exposure: Decimal::ZERO,
            down_exposure: Decimal::ZERO,
            trade_count: 0,
        }
    }

    /// Create with just a budget (uses default config).
    pub fn with_budget(total_budget: Decimal) -> Self {
        Self::new(PositionConfig::new(total_budget))
    }

    /// Create with budget and ATR for proper volatility normalization.
    pub fn with_budget_and_atr(total_budget: Decimal, atr: Decimal) -> Self {
        Self::new(PositionConfig::with_atr(total_budget, atr))
    }

    /// Update the ATR value (useful when ATR is calculated dynamically).
    pub fn set_atr(&mut self, atr: Decimal) {
        self.config.atr = atr;
    }

    /// Get the current ATR value.
    pub fn atr(&self) -> Decimal {
        self.config.atr
    }

    /// Calculate confidence based on time and distance.
    ///
    /// Formula: sqrt(time_confidence * distance_confidence)
    /// With boost when BOTH factors are strong.
    ///
    /// Distance is normalized by ATR to account for different asset volatilities.
    pub fn calculate_confidence(&self, distance_dollars: Decimal, minutes_remaining: Decimal) -> Decimal {
        let time_conf = Self::time_confidence(minutes_remaining);

        // Normalize distance by ATR
        let atr_multiple = if self.config.atr > Decimal::ZERO {
            distance_dollars.abs() / self.config.atr
        } else {
            Decimal::ZERO
        };
        let dist_conf = Self::distance_confidence(atr_multiple);

        // Geometric mean
        let product = time_conf * dist_conf;
        let combined = decimal_sqrt(product);

        // Boost when BOTH factors are strong
        if time_conf > dec!(0.7) && dist_conf > dec!(0.7) {
            (combined * dec!(1.2)).min(Decimal::ONE)
        } else {
            combined
        }
    }

    /// Time confidence based on minutes remaining.
    fn time_confidence(minutes: Decimal) -> Decimal {
        if minutes > dec!(12) {
            dec!(0.40)
        } else if minutes > dec!(9) {
            dec!(0.50)
        } else if minutes > dec!(6) {
            dec!(0.60)
        } else if minutes > dec!(3) {
            dec!(0.80)
        } else {
            dec!(1.00)
        }
    }

    /// Distance confidence based on ATR multiples from strike.
    ///
    /// Using ATR-normalized distance ensures consistent behavior across
    /// assets with different volatilities (BTC vs ETH vs SOL).
    ///
    /// Thresholds:
    /// - 0.25 ATR: 0.40 (small move, might be noise)
    /// - 0.50 ATR: 0.55 (moderate move)
    /// - 0.75 ATR: 0.70 (significant move)
    /// - 1.00 ATR: 0.85 (large move, likely real)
    /// - 1.50 ATR: 1.00 (very large move, high conviction)
    fn distance_confidence(atr_multiple: Decimal) -> Decimal {
        if atr_multiple > dec!(1.5) {
            dec!(1.00)
        } else if atr_multiple > dec!(1.0) {
            dec!(0.85)
        } else if atr_multiple > dec!(0.75) {
            dec!(0.70)
        } else if atr_multiple > dec!(0.50) {
            dec!(0.55)
        } else if atr_multiple > dec!(0.25) {
            dec!(0.40)
        } else {
            dec!(0.20)
        }
    }

    /// Decide whether to trade and with what size.
    ///
    /// # Arguments
    /// * `distance_dollars` - Distance from strike in dollars (for confidence calculation)
    /// * `seconds_remaining` - Seconds left in the market window
    /// * `favorable_price` - Price of the dominant side we're buying (for edge calculation)
    /// * `max_edge_factor` - Maximum edge required at window start (from config)
    /// * `window_duration_secs` - Total window duration in seconds (e.g., 900 for 15min)
    pub fn should_trade(
        &self,
        distance_dollars: Decimal,
        seconds_remaining: i64,
        favorable_price: Decimal,
        max_edge_factor: Decimal,
        window_duration_secs: i64,
    ) -> TradeDecision {
        let minutes = Decimal::new(seconds_remaining, 0) / dec!(60);
        let phase = Phase::from_minutes(minutes);
        let confidence = self.calculate_confidence(distance_dollars, minutes);

        // 1. Check confidence threshold
        // When max_edge_factor > 0: use edge-based (required = price + edge)
        // When max_edge_factor == 0: use phase-based (legacy mode, best in backtest)
        let required_confidence = if max_edge_factor > Decimal::ZERO {
            // Edge-based: required confidence = favorable_price + min_edge
            // min_edge = max_edge_factor * (seconds_remaining / window_duration_secs)
            let time_factor = if window_duration_secs > 0 {
                Decimal::new(seconds_remaining, 0) / Decimal::new(window_duration_secs, 0)
            } else {
                Decimal::ZERO
            };
            let min_edge = max_edge_factor * time_factor;
            favorable_price + min_edge
        } else {
            // Legacy phase-based thresholds (configurable via PositionConfig)
            self.config.threshold_for_phase(phase)
        };

        if confidence < required_confidence {
            return TradeDecision::Skip(SkipReason::LowConfidence);
        }

        // 2. Check total budget
        let total_remaining = self.config.total_budget - self.total_spent;
        if total_remaining <= Decimal::ZERO {
            return TradeDecision::Skip(SkipReason::TotalBudgetExhausted);
        }

        // 3. Check phase budget
        let phase_budget = self.config.phase_budget(phase);
        let phase_spent = self.phase_spent.get(&phase).copied().unwrap_or(Decimal::ZERO);
        let phase_remaining = phase_budget - phase_spent;
        if phase_remaining <= Decimal::ZERO {
            return TradeDecision::Skip(SkipReason::PhaseBudgetExhausted);
        }

        // 4. Calculate size
        let size = self.calculate_size(phase_remaining, total_remaining, confidence);

        // 5. Check minimum size
        if size < self.config.min_order_size {
            return TradeDecision::Skip(SkipReason::BelowMinimumSize);
        }

        TradeDecision::Trade {
            size,
            confidence,
            phase,
        }
    }

    /// Calculate trade size based on phase budget, confidence, and limits.
    fn calculate_size(
        &self,
        phase_remaining: Decimal,
        total_remaining: Decimal,
        confidence: Decimal,
    ) -> Decimal {
        // Base size: phase remaining / trades per phase
        let trades_per_phase = Decimal::new(self.config.trades_per_phase as i64, 0);
        let base_size = phase_remaining / trades_per_phase;

        // Confidence multiplier: 0.5x (low) to 2.0x (high)
        let multiplier = dec!(0.5) + (confidence * dec!(1.5));
        let mut size = base_size * multiplier;

        // Apply limits
        size = size.min(phase_remaining); // Don't exceed phase budget
        size = size.min(total_remaining * dec!(0.15)); // Max 15% of remaining
        size = size.max(self.config.min_order_size); // Minimum order size

        size
    }

    /// Record a completed trade.
    pub fn record_trade(
        &mut self,
        size: Decimal,
        up_amount: Decimal,
        down_amount: Decimal,
        seconds_remaining: i64,
    ) {
        let phase = Phase::from_seconds(seconds_remaining);

        // Update phase spent
        *self.phase_spent.entry(phase).or_insert(Decimal::ZERO) += size;

        // Update total spent
        self.total_spent += size;

        // Update exposure ratios
        let total_up = self.up_exposure * (self.total_spent - size) + up_amount;
        let total_down = self.down_exposure * (self.total_spent - size) + down_amount;
        if self.total_spent > Decimal::ZERO {
            self.up_exposure = total_up / self.total_spent;
            self.down_exposure = total_down / self.total_spent;
        }

        self.trade_count += 1;
    }

    /// Get current phase spending summary.
    pub fn phase_summary(&self) -> HashMap<Phase, (Decimal, Decimal)> {
        let mut summary = HashMap::new();
        for phase in [Phase::Early, Phase::Build, Phase::Core, Phase::Final] {
            let budget = self.config.phase_budget(phase);
            let spent = self.phase_spent.get(&phase).copied().unwrap_or(Decimal::ZERO);
            summary.insert(phase, (spent, budget));
        }
        summary
    }

    /// Get total budget remaining.
    pub fn budget_remaining(&self) -> Decimal {
        self.config.total_budget - self.total_spent
    }

    /// Get current exposure ratios.
    pub fn exposure(&self) -> (Decimal, Decimal) {
        (self.up_exposure, self.down_exposure)
    }

    /// Get trade count.
    pub fn trade_count(&self) -> u32 {
        self.trade_count
    }

    /// Check if we're at position limits for a given direction.
    pub fn at_position_limit(&self, favor_up: bool) -> bool {
        if favor_up {
            self.up_exposure >= self.config.max_single_side_exposure
        } else {
            self.down_exposure >= self.config.max_single_side_exposure
        }
    }

    /// Reset for a new market.
    pub fn reset(&mut self) {
        for spent in self.phase_spent.values_mut() {
            *spent = Decimal::ZERO;
        }
        self.total_spent = Decimal::ZERO;
        self.up_exposure = Decimal::ZERO;
        self.down_exposure = Decimal::ZERO;
        self.trade_count = 0;
    }
}

/// Approximate square root for Decimal using Newton's method.
fn decimal_sqrt(x: Decimal) -> Decimal {
    if x <= Decimal::ZERO {
        return Decimal::ZERO;
    }

    // Convert to f64, compute sqrt, convert back
    let x_f64: f64 = x.try_into().unwrap_or(0.0);
    let sqrt_f64 = x_f64.sqrt();

    Decimal::try_from(sqrt_f64).unwrap_or(Decimal::ZERO)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_phase_from_minutes() {
        assert_eq!(Phase::from_minutes(dec!(14)), Phase::Early);
        assert_eq!(Phase::from_minutes(dec!(10)), Phase::Build);
        assert_eq!(Phase::from_minutes(dec!(8)), Phase::Build);
        assert_eq!(Phase::from_minutes(dec!(5)), Phase::Core);
        assert_eq!(Phase::from_minutes(dec!(3)), Phase::Core);
        assert_eq!(Phase::from_minutes(dec!(2)), Phase::Final);
        assert_eq!(Phase::from_minutes(dec!(1)), Phase::Final);
    }

    #[test]
    fn test_phase_from_seconds() {
        assert_eq!(Phase::from_seconds(900), Phase::Early);  // 15 min
        assert_eq!(Phase::from_seconds(600), Phase::Build);  // 10 min
        assert_eq!(Phase::from_seconds(300), Phase::Core);   // 5 min
        assert_eq!(Phase::from_seconds(120), Phase::Final);  // 2 min
        assert_eq!(Phase::from_seconds(60), Phase::Final);   // 1 min
    }

    #[test]
    fn test_phase_budget_allocation() {
        assert_eq!(Phase::Early.budget_allocation(), dec!(0.15));
        assert_eq!(Phase::Build.budget_allocation(), dec!(0.25));
        assert_eq!(Phase::Core.budget_allocation(), dec!(0.30));
        assert_eq!(Phase::Final.budget_allocation(), dec!(0.30));

        // Should sum to 100%
        let total = Phase::Early.budget_allocation()
            + Phase::Build.budget_allocation()
            + Phase::Core.budget_allocation()
            + Phase::Final.budget_allocation();
        assert_eq!(total, Decimal::ONE);
    }

    #[test]
    fn test_phase_min_confidence() {
        // Optimized thresholds from param sweep with dynamic ATR (18.2% ROI)
        assert_eq!(Phase::Early.min_confidence(), dec!(0.80));
        assert_eq!(Phase::Build.min_confidence(), dec!(0.60));
        assert_eq!(Phase::Core.min_confidence(), dec!(0.50));
        assert_eq!(Phase::Final.min_confidence(), dec!(0.40));
    }

    #[test]
    fn test_config_phase_budget() {
        let config = PositionConfig::new(dec!(1000));
        assert_eq!(config.phase_budget(Phase::Early), dec!(150));
        assert_eq!(config.phase_budget(Phase::Build), dec!(250));
        assert_eq!(config.phase_budget(Phase::Core), dec!(300));
        assert_eq!(config.phase_budget(Phase::Final), dec!(300));
    }

    #[test]
    fn test_time_confidence() {
        assert_eq!(PositionManager::time_confidence(dec!(14)), dec!(0.40));
        assert_eq!(PositionManager::time_confidence(dec!(10)), dec!(0.50));
        assert_eq!(PositionManager::time_confidence(dec!(7)), dec!(0.60));
        assert_eq!(PositionManager::time_confidence(dec!(4)), dec!(0.80));
        assert_eq!(PositionManager::time_confidence(dec!(2)), dec!(1.00));
    }

    #[test]
    fn test_distance_confidence() {
        // Now takes ATR multiples instead of raw dollar amounts
        assert_eq!(PositionManager::distance_confidence(dec!(2.0)), dec!(1.00));  // 2.0 ATR
        assert_eq!(PositionManager::distance_confidence(dec!(1.2)), dec!(0.85));  // 1.2 ATR
        assert_eq!(PositionManager::distance_confidence(dec!(0.8)), dec!(0.70));  // 0.8 ATR
        assert_eq!(PositionManager::distance_confidence(dec!(0.6)), dec!(0.55));  // 0.6 ATR
        assert_eq!(PositionManager::distance_confidence(dec!(0.3)), dec!(0.40));  // 0.3 ATR
        assert_eq!(PositionManager::distance_confidence(dec!(0.1)), dec!(0.20));  // 0.1 ATR
    }

    #[test]
    fn test_calculate_confidence() {
        // Default ATR is $100
        let pm = PositionManager::with_budget(dec!(1000));

        // Early market, close to strike -> low confidence
        // $15 = 0.15 ATR -> dist_conf=0.20, time_conf=0.40 -> sqrt(0.08) ≈ 0.28
        let conf1 = pm.calculate_confidence(dec!(15), dec!(14));
        assert!(conf1 < dec!(0.50), "Expected low confidence, got {}", conf1);

        // Late market, far from strike -> high confidence
        // $150 = 1.5 ATR -> dist_conf=1.00, time_conf=1.00 -> 1.0 (with boost)
        let conf2 = pm.calculate_confidence(dec!(150), dec!(2));
        assert!(conf2 > dec!(0.90), "Expected high confidence, got {}", conf2);

        // Late market, at strike -> moderate (time high, distance low)
        // $5 = 0.05 ATR -> dist_conf=0.20, time_conf=1.00 -> sqrt(0.20) ≈ 0.45
        let conf3 = pm.calculate_confidence(dec!(5), dec!(1));
        assert!(conf3 < dec!(0.60), "Expected moderate confidence, got {}", conf3);
    }

    #[test]
    fn test_should_trade_low_confidence_skips() {
        let pm = PositionManager::with_budget(dec!(1000));

        // Early phase with low distance -> confidence < required (price + edge)
        // At 14 min: edge = 0.20 * (840/900) = 0.187
        // Price 0.50 + edge 0.187 = 0.687 required
        // Confidence ~0.25 (low distance) < 0.687 -> skip
        let decision = pm.should_trade(dec!(25), 840, dec!(0.50), dec!(0.20), 900);
        assert!(matches!(decision, TradeDecision::Skip(SkipReason::LowConfidence)));
    }

    #[test]
    fn test_should_trade_high_confidence_trades() {
        let pm = PositionManager::with_budget(dec!(1000));

        // Final phase with high distance and low favorable price
        // At 1.5 min: edge = 0.20 * (90/900) = 0.02
        // Price 0.30 + edge 0.02 = 0.32 required
        // $150 = 1.5 ATR -> dist_conf=1.00, with time_conf=1.00 -> confidence ~0.90
        // 0.90 > 0.32 -> should trade
        let decision = pm.should_trade(dec!(150), 90, dec!(0.30), dec!(0.20), 900);
        assert!(decision.is_trade(), "Expected trade, got {:?}", decision);
    }

    #[test]
    fn test_should_trade_budget_exhausted() {
        let config = PositionConfig {
            total_budget: dec!(100),
            min_order_size: dec!(1),
            ..Default::default()
        };
        let mut pm = PositionManager::new(config);

        // Spend all budget
        pm.total_spent = dec!(100);

        let decision = pm.should_trade(dec!(80), 90, dec!(0.30), dec!(0.20), 900);
        assert!(matches!(decision, TradeDecision::Skip(SkipReason::TotalBudgetExhausted)));
    }

    #[test]
    fn test_record_trade() {
        let mut pm = PositionManager::with_budget(dec!(1000));

        // Record a trade in Core phase
        pm.record_trade(dec!(50), dec!(39), dec!(11), 240); // 4 min

        assert_eq!(pm.total_spent, dec!(50));
        assert_eq!(pm.trade_count, 1);

        let phase_spent = pm.phase_spent.get(&Phase::Core).copied().unwrap();
        assert_eq!(phase_spent, dec!(50));
    }

    #[test]
    fn test_phase_budget_limits() {
        let mut pm = PositionManager::with_budget(dec!(1000));

        // Exhaust Final phase budget ($300)
        *pm.phase_spent.get_mut(&Phase::Final).unwrap() = dec!(300);

        // At 1 min with $150 distance (1.5 ATR): confidence is high
        // Edge = 0.20 * (60/900) = 0.013
        // Price 0.30 + edge 0.013 = 0.313 required
        // But Final phase budget is exhausted
        let decision = pm.should_trade(dec!(150), 60, dec!(0.30), dec!(0.20), 900);
        assert!(matches!(decision, TradeDecision::Skip(SkipReason::PhaseBudgetExhausted)),
            "Expected PhaseBudgetExhausted, got {:?}", decision);
    }

    #[test]
    fn test_sizing_scales_with_confidence() {
        let pm = PositionManager::with_budget(dec!(1000));

        // Low confidence -> smaller size
        let size_low = pm.calculate_size(dec!(300), dec!(1000), dec!(0.40));

        // High confidence -> larger size
        let size_high = pm.calculate_size(dec!(300), dec!(1000), dec!(0.90));

        assert!(size_high > size_low, "High conf size {} should > low conf size {}", size_high, size_low);
    }

    #[test]
    fn test_budget_remaining() {
        let mut pm = PositionManager::with_budget(dec!(1000));
        assert_eq!(pm.budget_remaining(), dec!(1000));

        pm.total_spent = dec!(300);
        assert_eq!(pm.budget_remaining(), dec!(700));
    }

    #[test]
    fn test_reset() {
        let mut pm = PositionManager::with_budget(dec!(1000));

        // Add some state
        pm.total_spent = dec!(500);
        pm.trade_count = 10;
        *pm.phase_spent.get_mut(&Phase::Core).unwrap() = dec!(200);

        // Reset
        pm.reset();

        assert_eq!(pm.total_spent, Decimal::ZERO);
        assert_eq!(pm.trade_count, 0);
        assert_eq!(pm.phase_spent.get(&Phase::Core).copied().unwrap(), Decimal::ZERO);
    }

    #[test]
    fn test_spec_example_simulation() {
        // Test with edge-based thresholds
        // Budget: $1,000 | Default ATR: $100
        // max_edge_factor: 0.20, window_duration: 900 secs
        // favorable_price: 0.50 for all tests
        let pm = PositionManager::with_budget(dec!(1000));

        // [14.0m] edge=0.187, need=0.687 | Dist: +$15 (0.15 ATR) | conf=0.28 -> SKIP
        let d1 = pm.should_trade(dec!(15), 840, dec!(0.50), dec!(0.20), 900);
        assert!(matches!(d1, TradeDecision::Skip(SkipReason::LowConfidence)));

        // [12.0m] edge=0.16, need=0.66 | Dist: +$22 (0.22 ATR) | conf=0.28 -> SKIP
        let d2 = pm.should_trade(dec!(22), 720, dec!(0.50), dec!(0.20), 900);
        assert!(matches!(d2, TradeDecision::Skip(SkipReason::LowConfidence)));

        // [10.0m] edge=0.133, need=0.633 | Dist: +$35 (0.35 ATR) | conf=0.45 -> SKIP
        let d3 = pm.should_trade(dec!(35), 600, dec!(0.50), dec!(0.20), 900);
        assert!(matches!(d3, TradeDecision::Skip(SkipReason::LowConfidence)));

        // [8.5m] edge=0.113, need=0.613 | Dist: +$60 (0.60 ATR) | conf=0.52 -> SKIP
        let d4 = pm.should_trade(dec!(60), 510, dec!(0.50), dec!(0.20), 900);
        assert!(matches!(d4, TradeDecision::Skip(SkipReason::LowConfidence)),
            "Expected skip at 8.5m with $60 distance, got {:?}", d4);

        // [2.0m] edge=0.027, need=0.527 | Dist: +$70 (0.70 ATR) | conf=0.74 -> TRADE
        let d5 = pm.should_trade(dec!(70), 120, dec!(0.50), dec!(0.20), 900);
        assert!(d5.is_trade(), "Expected trade at 2m with $70 distance, got {:?}", d5);

        // [1.0m] edge=0.013, need=0.513 | Dist: +$100 (1.0 ATR) | conf=0.84 -> TRADE
        let d6 = pm.should_trade(dec!(100), 60, dec!(0.50), dec!(0.20), 900);
        assert!(d6.is_trade(), "Expected trade at 1m with $100 distance, got {:?}", d6);
    }
}
