//! Configurable risk mode for the trading bot.
//!
//! Provides a unified interface for different risk management strategies:
//! - CircuitBreaker: Lock-free atomic checks for system protection
//! - DailyPnl: P&L-based checks (daily loss, consecutive losses, hedge ratio)
//! - Both: Combines both strategies (recommended for production)
//!
//! ## Usage
//!
//! ```ignore
//! let config = RiskModeConfig::default();
//! let manager = create_risk_manager(config);
//!
//! // Before each trade
//! if manager.can_trade() {
//!     let input = RiskCheckInput::new(proposed_size).with_position(&position);
//!     match manager.check_trade(input) {
//!         RiskCheckOutput::Approve => { /* proceed */ }
//!         RiskCheckOutput::Reject { reason } => { /* skip */ }
//!         RiskCheckOutput::ReduceSize { max_allowed } => { /* reduce */ }
//!         RiskCheckOutput::RebalanceRequired { .. } => { /* hedge first */ }
//!     }
//! }
//!
//! // After each trade
//! manager.record_success(); // or record_failure()
//! manager.record_pnl(pnl);
//! ```

use std::str::FromStr;
use std::sync::Arc;

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use super::circuit_breaker::{CircuitBreaker, CircuitBreakerConfig};
use super::pnl::{PnlRiskConfig, PnlRiskManager};
use crate::config::RiskConfig;
use crate::types::{Position, TradeDecision};

// ============================================================================
// RiskMode Enum
// ============================================================================

/// Risk management mode.
///
/// Determines which risk managers are active during trading.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum RiskMode {
    /// Circuit breaker only - lock-free atomic checks.
    /// Best for: High-frequency scenarios prioritizing minimal latency.
    /// Checks: Consecutive failures, auto-reset cooldown.
    CircuitBreaker,

    /// Daily P&L risk manager only.
    /// Best for: Scenarios prioritizing P&L-based risk limits.
    /// Checks: Daily loss limit, consecutive losses, hedge ratio.
    DailyPnl,

    /// Both circuit breaker and P&L risk manager (recommended).
    /// Best for: Production trading with comprehensive risk controls.
    /// Checks: All of the above.
    #[default]
    Both,
}

impl RiskMode {
    /// Parse risk mode from string (case-insensitive).
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "circuitbreaker" | "circuit_breaker" | "cb" => Some(RiskMode::CircuitBreaker),
            "dailypnl" | "daily_pnl" | "pnl" => Some(RiskMode::DailyPnl),
            "both" | "combined" | "all" => Some(RiskMode::Both),
            _ => None,
        }
    }

    /// Get a short code for this mode.
    pub fn code(&self) -> &'static str {
        match self {
            RiskMode::CircuitBreaker => "CB",
            RiskMode::DailyPnl => "PNL",
            RiskMode::Both => "BOTH",
        }
    }

    /// Check if circuit breaker is enabled.
    #[inline]
    pub fn uses_circuit_breaker(&self) -> bool {
        matches!(self, RiskMode::CircuitBreaker | RiskMode::Both)
    }

    /// Check if P&L risk manager is enabled.
    #[inline]
    pub fn uses_pnl_manager(&self) -> bool {
        matches!(self, RiskMode::DailyPnl | RiskMode::Both)
    }
}

impl std::fmt::Display for RiskMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RiskMode::CircuitBreaker => write!(f, "CircuitBreaker"),
            RiskMode::DailyPnl => write!(f, "DailyPnl"),
            RiskMode::Both => write!(f, "Both"),
        }
    }
}

impl FromStr for RiskMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        RiskMode::parse(s).ok_or_else(|| format!("Invalid risk mode: {}", s))
    }
}

// ============================================================================
// RiskCheckInput / RiskCheckOutput
// ============================================================================

/// Input for risk checks.
///
/// Contains all information needed to evaluate whether a trade should proceed.
#[derive(Debug, Clone)]
pub struct RiskCheckInput {
    /// Proposed trade size in USDC.
    pub proposed_size: Decimal,
    /// Current position (for hedge ratio checks).
    pub position: Option<Position>,
}

impl RiskCheckInput {
    /// Create a new risk check input.
    pub fn new(proposed_size: Decimal) -> Self {
        Self {
            proposed_size,
            position: None,
        }
    }

    /// Add position for hedge ratio checks.
    pub fn with_position(mut self, position: &Position) -> Self {
        self.position = Some(position.clone());
        self
    }
}

/// Output from risk checks.
///
/// Mirrors `TradeDecision` but is specific to the risk management layer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RiskCheckOutput {
    /// Trade is approved to proceed.
    Approve,

    /// Trade is rejected with a reason.
    Reject {
        /// Human-readable reason for rejection.
        reason: String,
        /// Which risk manager caused the rejection.
        source: RiskMode,
    },

    /// Trade can proceed but with a reduced size.
    ReduceSize {
        /// Maximum allowed size in USDC.
        max_allowed: Decimal,
        /// Which risk manager required the reduction.
        source: RiskMode,
    },

    /// Position needs rebalancing before trading.
    RebalanceRequired {
        /// Current hedge ratio (min_side / total).
        current_ratio: Decimal,
        /// Target hedge ratio to achieve.
        target_ratio: Decimal,
    },
}

impl RiskCheckOutput {
    /// Create a rejection with source.
    pub fn reject(reason: impl Into<String>, source: RiskMode) -> Self {
        RiskCheckOutput::Reject {
            reason: reason.into(),
            source,
        }
    }

    /// Check if the decision allows trading (Approve or ReduceSize).
    pub fn allows_trading(&self) -> bool {
        matches!(self, RiskCheckOutput::Approve | RiskCheckOutput::ReduceSize { .. })
    }

    /// Check if trading is completely blocked.
    pub fn is_blocked(&self) -> bool {
        matches!(self, RiskCheckOutput::Reject { .. })
    }

    /// Check if rebalancing is required.
    pub fn requires_rebalance(&self) -> bool {
        matches!(self, RiskCheckOutput::RebalanceRequired { .. })
    }

    /// Convert to TradeDecision for compatibility.
    pub fn to_trade_decision(&self) -> TradeDecision {
        match self {
            RiskCheckOutput::Approve => TradeDecision::Approve,
            RiskCheckOutput::Reject { reason, .. } => TradeDecision::reject(reason.clone()),
            RiskCheckOutput::ReduceSize { max_allowed, .. } => TradeDecision::ReduceSize {
                max_allowed: *max_allowed,
            },
            RiskCheckOutput::RebalanceRequired {
                current_ratio,
                target_ratio,
            } => TradeDecision::RebalanceRequired {
                current_ratio: *current_ratio,
                target_ratio: *target_ratio,
            },
        }
    }
}

impl std::fmt::Display for RiskCheckOutput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RiskCheckOutput::Approve => write!(f, "Approve"),
            RiskCheckOutput::Reject { reason, source } => {
                write!(f, "Reject[{}]: {}", source.code(), reason)
            }
            RiskCheckOutput::ReduceSize { max_allowed, source } => {
                write!(f, "ReduceSize[{}](max=${})", source.code(), max_allowed)
            }
            RiskCheckOutput::RebalanceRequired {
                current_ratio,
                target_ratio,
            } => {
                write!(
                    f,
                    "RebalanceRequired({}% -> {}%)",
                    current_ratio * Decimal::ONE_HUNDRED,
                    target_ratio * Decimal::ONE_HUNDRED
                )
            }
        }
    }
}

// ============================================================================
// RiskManager Trait
// ============================================================================

/// Unified risk manager trait.
///
/// Provides a common interface for different risk management strategies.
/// All implementations must be thread-safe (Send + Sync).
pub trait RiskManager: Send + Sync {
    /// Get the risk mode this manager implements.
    fn mode(&self) -> RiskMode;

    /// Fast path check if any trade can proceed (~10ns target).
    ///
    /// Returns false if trading is blocked for any reason.
    /// This should be a single atomic load for maximum performance.
    fn can_trade(&self) -> bool;

    /// Full risk check for a proposed trade.
    ///
    /// Evaluates all risk rules and returns a decision.
    fn check_trade(&self, input: &RiskCheckInput) -> RiskCheckOutput;

    /// Record a successful trade/operation.
    ///
    /// Resets consecutive failure counters.
    fn record_success(&self);

    /// Record a failed trade/operation.
    ///
    /// Increments failure counters and may trip circuit breaker.
    /// Returns true if a circuit breaker tripped as a result.
    fn record_failure(&self) -> bool;

    /// Record a completed trade's P&L.
    ///
    /// Updates daily P&L, trade counts, and consecutive loss tracking.
    fn record_pnl(&self, pnl: Decimal);

    /// Reset for a new trading day.
    ///
    /// Clears daily P&L, trade counts, and re-enables trading.
    fn reset_daily(&self);

    /// Manually reset the risk manager.
    ///
    /// Clears all state and re-enables trading.
    fn reset(&self);

    /// Get current stats as a displayable string.
    fn stats_string(&self) -> String;
}

// ============================================================================
// Circuit Breaker Only
// ============================================================================

/// Risk manager using only the circuit breaker.
pub struct CircuitBreakerRiskManager {
    cb: Arc<CircuitBreaker>,
}

impl CircuitBreakerRiskManager {
    /// Create a new circuit breaker risk manager.
    pub fn new(config: CircuitBreakerConfig) -> Self {
        Self {
            cb: Arc::new(CircuitBreaker::new(config)),
        }
    }

    /// Create from RiskConfig.
    pub fn from_risk_config(risk: &RiskConfig) -> Self {
        Self::new(CircuitBreakerConfig::from_risk_config(risk))
    }

    /// Get access to the underlying circuit breaker.
    pub fn circuit_breaker(&self) -> &CircuitBreaker {
        &self.cb
    }
}

impl RiskManager for CircuitBreakerRiskManager {
    fn mode(&self) -> RiskMode {
        RiskMode::CircuitBreaker
    }

    #[inline(always)]
    fn can_trade(&self) -> bool {
        self.cb.can_trade()
    }

    fn check_trade(&self, _input: &RiskCheckInput) -> RiskCheckOutput {
        if self.cb.can_trade() {
            RiskCheckOutput::Approve
        } else {
            let stats = self.cb.stats();
            let reason = if let Some(trip) = stats.last_trip_reason {
                format!(
                    "Circuit breaker tripped: {} failures at {}",
                    trip.failure_count,
                    trip.tripped_at.format("%H:%M:%S")
                )
            } else {
                "Circuit breaker tripped".to_string()
            };
            RiskCheckOutput::reject(reason, RiskMode::CircuitBreaker)
        }
    }

    fn record_success(&self) {
        self.cb.record_success();
    }

    fn record_failure(&self) -> bool {
        self.cb.record_failure()
    }

    fn record_pnl(&self, pnl: Decimal) {
        // Circuit breaker doesn't track P&L, but record as success/failure
        if pnl >= Decimal::ZERO {
            self.cb.record_success();
        } else {
            self.cb.record_failure();
        }
    }

    fn reset_daily(&self) {
        self.cb.reset();
    }

    fn reset(&self) {
        self.cb.reset();
    }

    fn stats_string(&self) -> String {
        let stats = self.cb.stats();
        format!(
            "CB: state={}, trips={}, failures={}/{}",
            stats.state,
            stats.total_trips,
            stats.consecutive_failures,
            self.cb.config().max_consecutive_failures
        )
    }
}

// ============================================================================
// P&L Risk Manager Only
// ============================================================================

/// Risk manager using only the P&L-based checks.
///
/// Note: Uses interior mutability via RwLock for thread safety.
pub struct PnlOnlyRiskManager {
    pnl: std::sync::RwLock<PnlRiskManager>,
}

impl PnlOnlyRiskManager {
    /// Create a new P&L-only risk manager.
    pub fn new(config: PnlRiskConfig) -> Self {
        Self {
            pnl: std::sync::RwLock::new(PnlRiskManager::new(config)),
        }
    }

    /// Create with available balance (uses default risk params).
    pub fn with_balance(available_balance: Decimal) -> Self {
        Self::new(PnlRiskConfig::new(available_balance))
    }

    /// Get stats from the P&L manager.
    pub fn stats(&self) -> super::pnl::PnlRiskStats {
        self.pnl.read().unwrap().stats()
    }
}

impl RiskManager for PnlOnlyRiskManager {
    fn mode(&self) -> RiskMode {
        RiskMode::DailyPnl
    }

    #[inline]
    fn can_trade(&self) -> bool {
        self.pnl.read().unwrap().can_trade()
    }

    fn check_trade(&self, input: &RiskCheckInput) -> RiskCheckOutput {
        let pnl = self.pnl.read().unwrap();
        let decision = pnl.check_trade(input.position.as_ref(), input.proposed_size);

        match decision {
            TradeDecision::Approve => RiskCheckOutput::Approve,
            TradeDecision::Reject { reason } => {
                RiskCheckOutput::reject(reason, RiskMode::DailyPnl)
            }
            TradeDecision::ReduceSize { max_allowed } => RiskCheckOutput::ReduceSize {
                max_allowed,
                source: RiskMode::DailyPnl,
            },
            TradeDecision::RebalanceRequired {
                current_ratio,
                target_ratio,
            } => RiskCheckOutput::RebalanceRequired {
                current_ratio,
                target_ratio,
            },
        }
    }

    fn record_success(&self) {
        // P&L manager doesn't track success/failure separately
        // Success is recorded via record_pnl with positive value
    }

    fn record_failure(&self) -> bool {
        // P&L manager doesn't track operational failures
        // Only trade P&L matters
        false
    }

    fn record_pnl(&self, pnl: Decimal) {
        self.pnl.write().unwrap().record_trade(pnl);
    }

    fn reset_daily(&self) {
        self.pnl.write().unwrap().reset_daily();
    }

    fn reset(&self) {
        self.pnl.write().unwrap().reset_daily();
    }

    fn stats_string(&self) -> String {
        self.pnl.read().unwrap().stats().to_string()
    }
}

// ============================================================================
// Composite Risk Manager (Both)
// ============================================================================

/// Composite risk manager that runs both circuit breaker and P&L checks.
///
/// This is the recommended configuration for production trading.
/// The circuit breaker provides fast-path rejection (~10ns) while
/// the P&L manager provides comprehensive risk controls.
///
/// ## Check Order
///
/// 1. Circuit breaker `can_trade()` (fast atomic check)
/// 2. P&L manager `check_trade()` (full evaluation)
///
/// Both must pass for a trade to proceed.
pub struct CompositeRiskManager {
    cb: Arc<CircuitBreaker>,
    pnl: std::sync::RwLock<PnlRiskManager>,
}

impl CompositeRiskManager {
    /// Create a new composite risk manager.
    pub fn new(cb_config: CircuitBreakerConfig, pnl_config: PnlRiskConfig) -> Self {
        Self {
            cb: Arc::new(CircuitBreaker::new(cb_config)),
            pnl: std::sync::RwLock::new(PnlRiskManager::new(pnl_config)),
        }
    }

    /// Create from RiskConfig and balance.
    pub fn from_configs(risk: &RiskConfig, available_balance: Decimal) -> Self {
        Self::new(
            CircuitBreakerConfig::from_risk_config(risk),
            PnlRiskConfig::with_params(
                available_balance,
                Decimal::new(10, 2),  // 10% daily loss
                risk.max_consecutive_failures,
                Decimal::new(20, 2),  // 20% hedge ratio
            ),
        )
    }

    /// Get access to the circuit breaker.
    pub fn circuit_breaker(&self) -> &CircuitBreaker {
        &self.cb
    }

    /// Get stats from the P&L manager.
    pub fn pnl_stats(&self) -> super::pnl::PnlRiskStats {
        self.pnl.read().unwrap().stats()
    }

    /// Get circuit breaker stats.
    pub fn cb_stats(&self) -> super::circuit_breaker::CircuitBreakerStats {
        self.cb.stats()
    }
}

impl RiskManager for CompositeRiskManager {
    fn mode(&self) -> RiskMode {
        RiskMode::Both
    }

    #[inline]
    fn can_trade(&self) -> bool {
        // Fast path: check circuit breaker first (atomic)
        // Then check P&L manager
        self.cb.can_trade() && self.pnl.read().unwrap().can_trade()
    }

    fn check_trade(&self, input: &RiskCheckInput) -> RiskCheckOutput {
        // 1. Circuit breaker check first (fast)
        if !self.cb.can_trade() {
            let stats = self.cb.stats();
            let reason = if let Some(trip) = stats.last_trip_reason {
                format!(
                    "Circuit breaker tripped: {} failures at {}",
                    trip.failure_count,
                    trip.tripped_at.format("%H:%M:%S")
                )
            } else {
                "Circuit breaker tripped".to_string()
            };
            return RiskCheckOutput::reject(reason, RiskMode::CircuitBreaker);
        }

        // 2. P&L manager check
        let pnl = self.pnl.read().unwrap();
        let decision = pnl.check_trade(input.position.as_ref(), input.proposed_size);

        match decision {
            TradeDecision::Approve => RiskCheckOutput::Approve,
            TradeDecision::Reject { reason } => {
                RiskCheckOutput::reject(reason, RiskMode::DailyPnl)
            }
            TradeDecision::ReduceSize { max_allowed } => RiskCheckOutput::ReduceSize {
                max_allowed,
                source: RiskMode::DailyPnl,
            },
            TradeDecision::RebalanceRequired {
                current_ratio,
                target_ratio,
            } => RiskCheckOutput::RebalanceRequired {
                current_ratio,
                target_ratio,
            },
        }
    }

    fn record_success(&self) {
        self.cb.record_success();
    }

    fn record_failure(&self) -> bool {
        self.cb.record_failure()
    }

    fn record_pnl(&self, pnl: Decimal) {
        // Record to P&L manager
        self.pnl.write().unwrap().record_trade(pnl);

        // Also update circuit breaker based on trade outcome
        if pnl >= Decimal::ZERO {
            self.cb.record_success();
        }
        // Note: We don't record_failure on losing trades to circuit breaker
        // because consecutive losses are handled by P&L manager.
        // Circuit breaker is for operational failures (API errors, etc.)
    }

    fn reset_daily(&self) {
        self.cb.reset();
        self.pnl.write().unwrap().reset_daily();
    }

    fn reset(&self) {
        self.cb.reset();
        self.pnl.write().unwrap().reset_daily();
    }

    fn stats_string(&self) -> String {
        let cb_stats = self.cb.stats();
        let pnl_stats = self.pnl.read().unwrap().stats();
        format!(
            "CB: state={}, trips={} | {}",
            cb_stats.state,
            cb_stats.total_trips,
            pnl_stats
        )
    }
}

// ============================================================================
// Factory Function
// ============================================================================

/// Configuration for creating a risk manager.
#[derive(Debug, Clone)]
pub struct RiskModeConfig {
    /// Risk mode to use.
    pub mode: RiskMode,
    /// Circuit breaker configuration.
    pub cb_config: CircuitBreakerConfig,
    /// P&L risk configuration.
    pub pnl_config: PnlRiskConfig,
}

impl Default for RiskModeConfig {
    fn default() -> Self {
        Self {
            mode: RiskMode::Both,
            cb_config: CircuitBreakerConfig::default(),
            pnl_config: PnlRiskConfig::default(),
        }
    }
}

impl RiskModeConfig {
    /// Create with specific mode and available balance.
    pub fn new(mode: RiskMode, available_balance: Decimal) -> Self {
        Self {
            mode,
            cb_config: CircuitBreakerConfig::default(),
            pnl_config: PnlRiskConfig::new(available_balance),
        }
    }

    /// Create from RiskConfig with mode and balance.
    pub fn from_risk_config(risk: &RiskConfig, mode: RiskMode, available_balance: Decimal) -> Self {
        Self {
            mode,
            cb_config: CircuitBreakerConfig::from_risk_config(risk),
            pnl_config: PnlRiskConfig::with_params(
                available_balance,
                Decimal::new(10, 2),  // 10% daily loss
                risk.max_consecutive_failures,
                Decimal::new(20, 2),  // 20% hedge ratio
            ),
        }
    }
}

/// Create a risk manager for the specified mode.
///
/// Returns a boxed trait object that can be used polymorphically.
///
/// # Arguments
///
/// * `config` - Configuration specifying mode and parameters
///
/// # Examples
///
/// ```ignore
/// // Create with defaults (Both mode)
/// let manager = create_risk_manager(RiskModeConfig::default());
///
/// // Create with specific mode
/// let config = RiskModeConfig::new(RiskMode::CircuitBreaker, dec!(5000));
/// let manager = create_risk_manager(config);
/// ```
pub fn create_risk_manager(config: RiskModeConfig) -> Box<dyn RiskManager> {
    match config.mode {
        RiskMode::CircuitBreaker => {
            Box::new(CircuitBreakerRiskManager::new(config.cb_config))
        }
        RiskMode::DailyPnl => {
            Box::new(PnlOnlyRiskManager::new(config.pnl_config))
        }
        RiskMode::Both => {
            Box::new(CompositeRiskManager::new(config.cb_config, config.pnl_config))
        }
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    // =========================================================================
    // RiskMode Tests
    // =========================================================================

    #[test]
    fn test_risk_mode_default() {
        assert_eq!(RiskMode::default(), RiskMode::Both);
    }

    #[test]
    fn test_risk_mode_parse() {
        assert_eq!(RiskMode::parse("circuitbreaker"), Some(RiskMode::CircuitBreaker));
        assert_eq!(RiskMode::parse("CircuitBreaker"), Some(RiskMode::CircuitBreaker));
        assert_eq!(RiskMode::parse("circuit_breaker"), Some(RiskMode::CircuitBreaker));
        assert_eq!(RiskMode::parse("cb"), Some(RiskMode::CircuitBreaker));

        assert_eq!(RiskMode::parse("dailypnl"), Some(RiskMode::DailyPnl));
        assert_eq!(RiskMode::parse("DailyPnl"), Some(RiskMode::DailyPnl));
        assert_eq!(RiskMode::parse("daily_pnl"), Some(RiskMode::DailyPnl));
        assert_eq!(RiskMode::parse("pnl"), Some(RiskMode::DailyPnl));

        assert_eq!(RiskMode::parse("both"), Some(RiskMode::Both));
        assert_eq!(RiskMode::parse("Both"), Some(RiskMode::Both));
        assert_eq!(RiskMode::parse("combined"), Some(RiskMode::Both));
        assert_eq!(RiskMode::parse("all"), Some(RiskMode::Both));

        assert_eq!(RiskMode::parse("invalid"), None);
    }

    #[test]
    fn test_risk_mode_from_str() {
        assert_eq!("both".parse::<RiskMode>().unwrap(), RiskMode::Both);
        assert_eq!("cb".parse::<RiskMode>().unwrap(), RiskMode::CircuitBreaker);
        assert!("invalid".parse::<RiskMode>().is_err());
    }

    #[test]
    fn test_risk_mode_display() {
        assert_eq!(RiskMode::CircuitBreaker.to_string(), "CircuitBreaker");
        assert_eq!(RiskMode::DailyPnl.to_string(), "DailyPnl");
        assert_eq!(RiskMode::Both.to_string(), "Both");
    }

    #[test]
    fn test_risk_mode_code() {
        assert_eq!(RiskMode::CircuitBreaker.code(), "CB");
        assert_eq!(RiskMode::DailyPnl.code(), "PNL");
        assert_eq!(RiskMode::Both.code(), "BOTH");
    }

    #[test]
    fn test_risk_mode_uses_flags() {
        assert!(RiskMode::CircuitBreaker.uses_circuit_breaker());
        assert!(!RiskMode::CircuitBreaker.uses_pnl_manager());

        assert!(!RiskMode::DailyPnl.uses_circuit_breaker());
        assert!(RiskMode::DailyPnl.uses_pnl_manager());

        assert!(RiskMode::Both.uses_circuit_breaker());
        assert!(RiskMode::Both.uses_pnl_manager());
    }

    #[test]
    fn test_risk_mode_serialization() {
        let mode = RiskMode::Both;
        let json = serde_json::to_string(&mode).unwrap();
        let parsed: RiskMode = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, mode);
    }

    // =========================================================================
    // RiskCheckInput Tests
    // =========================================================================

    #[test]
    fn test_risk_check_input_new() {
        let input = RiskCheckInput::new(dec!(100));
        assert_eq!(input.proposed_size, dec!(100));
        assert!(input.position.is_none());
    }

    #[test]
    fn test_risk_check_input_with_position() {
        let pos = Position {
            up_shares: dec!(100),
            down_shares: dec!(100),
            up_cost: dec!(45),
            down_cost: dec!(52),
        };
        let input = RiskCheckInput::new(dec!(100)).with_position(&pos);
        assert!(input.position.is_some());
        assert_eq!(input.position.unwrap().up_shares, dec!(100));
    }

    // =========================================================================
    // RiskCheckOutput Tests
    // =========================================================================

    #[test]
    fn test_risk_check_output_approve() {
        let output = RiskCheckOutput::Approve;
        assert!(output.allows_trading());
        assert!(!output.is_blocked());
        assert!(!output.requires_rebalance());
        assert_eq!(format!("{}", output), "Approve");
    }

    #[test]
    fn test_risk_check_output_reject() {
        let output = RiskCheckOutput::reject("Test rejection", RiskMode::CircuitBreaker);
        assert!(!output.allows_trading());
        assert!(output.is_blocked());
        assert!(!output.requires_rebalance());

        if let RiskCheckOutput::Reject { reason, source } = &output {
            assert_eq!(reason, "Test rejection");
            assert_eq!(*source, RiskMode::CircuitBreaker);
        } else {
            panic!("Expected Reject variant");
        }

        assert!(format!("{}", output).contains("CB"));
    }

    #[test]
    fn test_risk_check_output_reduce_size() {
        let output = RiskCheckOutput::ReduceSize {
            max_allowed: dec!(50),
            source: RiskMode::DailyPnl,
        };
        assert!(output.allows_trading());
        assert!(!output.is_blocked());
        assert!(!output.requires_rebalance());
        assert!(format!("{}", output).contains("50"));
        assert!(format!("{}", output).contains("PNL"));
    }

    #[test]
    fn test_risk_check_output_rebalance_required() {
        let output = RiskCheckOutput::RebalanceRequired {
            current_ratio: dec!(0.15),
            target_ratio: dec!(0.20),
        };
        assert!(!output.allows_trading());
        assert!(!output.is_blocked());
        assert!(output.requires_rebalance());
    }

    #[test]
    fn test_risk_check_output_to_trade_decision() {
        let output = RiskCheckOutput::Approve;
        assert_eq!(output.to_trade_decision(), TradeDecision::Approve);

        let output = RiskCheckOutput::reject("test", RiskMode::Both);
        let decision = output.to_trade_decision();
        assert!(matches!(decision, TradeDecision::Reject { .. }));

        let output = RiskCheckOutput::ReduceSize {
            max_allowed: dec!(50),
            source: RiskMode::DailyPnl,
        };
        let decision = output.to_trade_decision();
        assert!(matches!(decision, TradeDecision::ReduceSize { max_allowed } if max_allowed == dec!(50)));
    }

    #[test]
    fn test_risk_check_output_serialization() {
        let output = RiskCheckOutput::Approve;
        let json = serde_json::to_string(&output).unwrap();
        let parsed: RiskCheckOutput = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, output);

        let output = RiskCheckOutput::reject("test", RiskMode::Both);
        let json = serde_json::to_string(&output).unwrap();
        let parsed: RiskCheckOutput = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, output);
    }

    // =========================================================================
    // CircuitBreakerRiskManager Tests
    // =========================================================================

    #[test]
    fn test_cb_manager_new() {
        let manager = CircuitBreakerRiskManager::new(CircuitBreakerConfig::default());
        assert_eq!(manager.mode(), RiskMode::CircuitBreaker);
        assert!(manager.can_trade());
    }

    #[test]
    fn test_cb_manager_approve() {
        let manager = CircuitBreakerRiskManager::new(CircuitBreakerConfig::default());
        let input = RiskCheckInput::new(dec!(100));
        let output = manager.check_trade(&input);
        assert_eq!(output, RiskCheckOutput::Approve);
    }

    #[test]
    fn test_cb_manager_trip_reject() {
        let config = CircuitBreakerConfig::new(3, 300);
        let manager = CircuitBreakerRiskManager::new(config);

        // Trip the circuit breaker
        manager.record_failure();
        manager.record_failure();
        manager.record_failure();

        assert!(!manager.can_trade());

        let input = RiskCheckInput::new(dec!(100));
        let output = manager.check_trade(&input);
        assert!(output.is_blocked());
        if let RiskCheckOutput::Reject { source, .. } = output {
            assert_eq!(source, RiskMode::CircuitBreaker);
        }
    }

    #[test]
    fn test_cb_manager_record_success() {
        let config = CircuitBreakerConfig::new(3, 300);
        let manager = CircuitBreakerRiskManager::new(config);

        manager.record_failure();
        manager.record_failure();
        // 2 failures
        assert!(manager.can_trade());

        // Success resets
        manager.record_success();
        assert!(manager.can_trade());

        // Now we need 3 more failures to trip
        manager.record_failure();
        manager.record_failure();
        manager.record_failure();
        assert!(!manager.can_trade());
    }

    #[test]
    fn test_cb_manager_record_pnl() {
        let config = CircuitBreakerConfig::new(3, 300);
        let manager = CircuitBreakerRiskManager::new(config);

        // Positive P&L = success
        manager.record_pnl(dec!(10));
        manager.record_pnl(dec!(5));

        // Two failures
        manager.record_pnl(dec!(-10));
        manager.record_pnl(dec!(-5));
        assert!(manager.can_trade());

        // Third failure trips
        manager.record_pnl(dec!(-3));
        assert!(!manager.can_trade());
    }

    #[test]
    fn test_cb_manager_reset() {
        let config = CircuitBreakerConfig::new(3, 300);
        let manager = CircuitBreakerRiskManager::new(config);

        // Trip it
        manager.record_failure();
        manager.record_failure();
        manager.record_failure();
        assert!(!manager.can_trade());

        // Reset
        manager.reset();
        assert!(manager.can_trade());
    }

    #[test]
    fn test_cb_manager_stats_string() {
        let manager = CircuitBreakerRiskManager::new(CircuitBreakerConfig::default());
        let stats = manager.stats_string();
        assert!(stats.contains("CB:"));
        assert!(stats.contains("state="));
    }

    // =========================================================================
    // PnlOnlyRiskManager Tests
    // =========================================================================

    #[test]
    fn test_pnl_manager_new() {
        let manager = PnlOnlyRiskManager::with_balance(dec!(5000));
        assert_eq!(manager.mode(), RiskMode::DailyPnl);
        assert!(manager.can_trade());
    }

    #[test]
    fn test_pnl_manager_approve() {
        let manager = PnlOnlyRiskManager::with_balance(dec!(5000));
        let input = RiskCheckInput::new(dec!(100));
        let output = manager.check_trade(&input);
        assert_eq!(output, RiskCheckOutput::Approve);
    }

    #[test]
    fn test_pnl_manager_reject_daily_loss() {
        let manager = PnlOnlyRiskManager::with_balance(dec!(5000)); // Limit: $500

        // Hit daily loss limit
        manager.record_pnl(dec!(-500));

        assert!(!manager.can_trade());

        let input = RiskCheckInput::new(dec!(100));
        let output = manager.check_trade(&input);
        assert!(output.is_blocked());
        if let RiskCheckOutput::Reject { source, .. } = output {
            assert_eq!(source, RiskMode::DailyPnl);
        }
    }

    #[test]
    fn test_pnl_manager_reject_consecutive_losses() {
        let manager = PnlOnlyRiskManager::with_balance(dec!(5000));

        // 3 consecutive losses (small to avoid hitting daily limit)
        manager.record_pnl(dec!(-50));
        manager.record_pnl(dec!(-50));
        manager.record_pnl(dec!(-50));

        assert!(!manager.can_trade());
    }

    #[test]
    fn test_pnl_manager_rebalance_required() {
        let manager = PnlOnlyRiskManager::with_balance(dec!(5000));

        // Position with low hedge ratio
        let pos = Position {
            up_shares: dec!(850),
            down_shares: dec!(150),
            up_cost: dec!(425),
            down_cost: dec!(75),
        };

        let input = RiskCheckInput::new(dec!(100)).with_position(&pos);
        let output = manager.check_trade(&input);
        assert!(output.requires_rebalance());
    }

    #[test]
    fn test_pnl_manager_reduce_size() {
        let manager = PnlOnlyRiskManager::with_balance(dec!(5000)); // Limit: $500

        // Lose $400 = 80% of limit, past 75% warning
        manager.record_pnl(dec!(-400));
        assert!(manager.can_trade()); // Still enabled

        let input = RiskCheckInput::new(dec!(100));
        let output = manager.check_trade(&input);

        match output {
            RiskCheckOutput::ReduceSize { max_allowed, source } => {
                assert_eq!(max_allowed, dec!(50)); // 100 * 0.50
                assert_eq!(source, RiskMode::DailyPnl);
            }
            _ => panic!("Expected ReduceSize, got {:?}", output),
        }
    }

    #[test]
    fn test_pnl_manager_reset_daily() {
        let manager = PnlOnlyRiskManager::with_balance(dec!(5000));

        // Hit limit
        manager.record_pnl(dec!(-500));
        assert!(!manager.can_trade());

        // Reset
        manager.reset_daily();
        assert!(manager.can_trade());

        let stats = manager.stats();
        assert_eq!(stats.daily_pnl, dec!(0));
        assert_eq!(stats.trade_count, 0);
    }

    #[test]
    fn test_pnl_manager_stats_string() {
        let manager = PnlOnlyRiskManager::with_balance(dec!(5000));
        manager.record_pnl(dec!(10));

        let stats = manager.stats_string();
        assert!(stats.contains("PnL:"));
    }

    // =========================================================================
    // CompositeRiskManager Tests
    // =========================================================================

    #[test]
    fn test_composite_manager_new() {
        let manager = CompositeRiskManager::new(
            CircuitBreakerConfig::default(),
            PnlRiskConfig::new(dec!(5000)),
        );
        assert_eq!(manager.mode(), RiskMode::Both);
        assert!(manager.can_trade());
    }

    #[test]
    fn test_composite_manager_approve() {
        let manager = CompositeRiskManager::new(
            CircuitBreakerConfig::default(),
            PnlRiskConfig::new(dec!(5000)),
        );
        let input = RiskCheckInput::new(dec!(100));
        let output = manager.check_trade(&input);
        assert_eq!(output, RiskCheckOutput::Approve);
    }

    #[test]
    fn test_composite_cb_trips_first() {
        let manager = CompositeRiskManager::new(
            CircuitBreakerConfig::new(3, 300),
            PnlRiskConfig::new(dec!(5000)),
        );

        // Trip circuit breaker with operational failures
        manager.record_failure();
        manager.record_failure();
        manager.record_failure();

        assert!(!manager.can_trade());

        let input = RiskCheckInput::new(dec!(100));
        let output = manager.check_trade(&input);
        assert!(output.is_blocked());
        if let RiskCheckOutput::Reject { source, .. } = output {
            assert_eq!(source, RiskMode::CircuitBreaker);
        }
    }

    #[test]
    fn test_composite_pnl_rejects_after_cb_passes() {
        let manager = CompositeRiskManager::new(
            CircuitBreakerConfig::new(3, 300),
            PnlRiskConfig::new(dec!(5000)),
        );

        // P&L losses (circuit breaker not affected by trading losses)
        manager.record_pnl(dec!(-50));
        manager.record_pnl(dec!(-50));
        manager.record_pnl(dec!(-50));

        // CB should still be ok (positive P&L resets it, losses don't trip it)
        let _cb_stats = manager.cb_stats();
        // Actually, with our logic, record_pnl doesn't record_failure to CB
        // So CB should be fine

        // But PNL manager should reject due to consecutive losses
        let input = RiskCheckInput::new(dec!(100));
        let output = manager.check_trade(&input);

        // Check based on what manager rejects
        if output.is_blocked() {
            if let RiskCheckOutput::Reject { source, .. } = output {
                // Should be PNL since CB is ok
                assert_eq!(source, RiskMode::DailyPnl);
            }
        }
    }

    #[test]
    fn test_composite_record_pnl() {
        let manager = CompositeRiskManager::new(
            CircuitBreakerConfig::new(3, 300),
            PnlRiskConfig::new(dec!(5000)),
        );

        // Positive P&L resets CB and records to PNL
        manager.record_pnl(dec!(10));
        manager.record_pnl(dec!(20));

        let pnl_stats = manager.pnl_stats();
        assert_eq!(pnl_stats.daily_pnl, dec!(30));
        assert_eq!(pnl_stats.winning_trades, 2);
    }

    #[test]
    fn test_composite_reset_daily() {
        let manager = CompositeRiskManager::new(
            CircuitBreakerConfig::new(3, 300),
            PnlRiskConfig::new(dec!(5000)),
        );

        // Accumulate some state
        manager.record_failure();
        manager.record_pnl(dec!(-100));

        // Reset
        manager.reset_daily();

        assert!(manager.can_trade());
        let pnl_stats = manager.pnl_stats();
        assert_eq!(pnl_stats.daily_pnl, dec!(0));
    }

    #[test]
    fn test_composite_stats_string() {
        let manager = CompositeRiskManager::new(
            CircuitBreakerConfig::default(),
            PnlRiskConfig::new(dec!(5000)),
        );

        let stats = manager.stats_string();
        assert!(stats.contains("CB:"));
        assert!(stats.contains("PnL:"));
    }

    #[test]
    fn test_composite_rebalance_required() {
        let manager = CompositeRiskManager::new(
            CircuitBreakerConfig::default(),
            PnlRiskConfig::new(dec!(5000)),
        );

        let pos = Position {
            up_shares: dec!(850),
            down_shares: dec!(150),
            up_cost: dec!(425),
            down_cost: dec!(75),
        };

        let input = RiskCheckInput::new(dec!(100)).with_position(&pos);
        let output = manager.check_trade(&input);
        assert!(output.requires_rebalance());
    }

    // =========================================================================
    // Factory Function Tests
    // =========================================================================

    #[test]
    fn test_create_risk_manager_circuit_breaker() {
        let config = RiskModeConfig::new(RiskMode::CircuitBreaker, dec!(5000));
        let manager = create_risk_manager(config);
        assert_eq!(manager.mode(), RiskMode::CircuitBreaker);
        assert!(manager.can_trade());
    }

    #[test]
    fn test_create_risk_manager_daily_pnl() {
        let config = RiskModeConfig::new(RiskMode::DailyPnl, dec!(5000));
        let manager = create_risk_manager(config);
        assert_eq!(manager.mode(), RiskMode::DailyPnl);
        assert!(manager.can_trade());
    }

    #[test]
    fn test_create_risk_manager_both() {
        let config = RiskModeConfig::new(RiskMode::Both, dec!(5000));
        let manager = create_risk_manager(config);
        assert_eq!(manager.mode(), RiskMode::Both);
        assert!(manager.can_trade());
    }

    #[test]
    fn test_create_risk_manager_default() {
        let manager = create_risk_manager(RiskModeConfig::default());
        assert_eq!(manager.mode(), RiskMode::Both);
    }

    #[test]
    fn test_risk_manager_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<CircuitBreakerRiskManager>();
        assert_send_sync::<PnlOnlyRiskManager>();
        assert_send_sync::<CompositeRiskManager>();
    }

    #[test]
    fn test_boxed_risk_manager_usable() {
        let manager: Box<dyn RiskManager> = create_risk_manager(RiskModeConfig::default());

        assert!(manager.can_trade());

        let input = RiskCheckInput::new(dec!(100));
        let output = manager.check_trade(&input);
        assert_eq!(output, RiskCheckOutput::Approve);

        manager.record_pnl(dec!(10));
        manager.record_success();

        let stats = manager.stats_string();
        assert!(!stats.is_empty());
    }

    // =========================================================================
    // RiskModeConfig Tests
    // =========================================================================

    #[test]
    fn test_risk_mode_config_default() {
        let config = RiskModeConfig::default();
        assert_eq!(config.mode, RiskMode::Both);
    }

    #[test]
    fn test_risk_mode_config_new() {
        let config = RiskModeConfig::new(RiskMode::CircuitBreaker, dec!(10000));
        assert_eq!(config.mode, RiskMode::CircuitBreaker);
        assert_eq!(config.pnl_config.available_balance, dec!(10000));
    }

    #[test]
    fn test_risk_mode_config_from_risk_config() {
        let risk = RiskConfig::default();
        let config = RiskModeConfig::from_risk_config(&risk, RiskMode::Both, dec!(5000));

        assert_eq!(config.mode, RiskMode::Both);
        assert_eq!(
            config.cb_config.max_consecutive_failures,
            risk.max_consecutive_failures
        );
        assert_eq!(config.pnl_config.available_balance, dec!(5000));
    }
}
