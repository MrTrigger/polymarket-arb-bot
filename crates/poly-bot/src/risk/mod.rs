//! Risk management for the trading bot.
//!
//! This module provides pre-trade validation and risk controls:
//! - Position and exposure limits
//! - Time-based restrictions
//! - Toxic flow integration
//! - Daily loss limits
//! - Circuit breaker for system protection
//! - Leg risk handling for incomplete arbitrage trades
//!
//! ## Risk Modes
//!
//! Three risk modes are supported:
//! - `CircuitBreaker`: Lock-free atomic checks for consecutive failures
//! - `DailyPnl`: P&L-based checks (daily loss, consecutive losses, hedge ratio)
//! - `Both`: Combines both risk managers (recommended for production)
//!
//! ## Hot Path Requirements
//!
//! Risk checks run before every trade and must be fast:
//! - Target: <1μs for all pre-trade checks
//! - Circuit breaker can_trade(): ~10ns (single atomic load)
//! - Avoid heap allocations
//! - Use pre-computed state where possible

pub mod checks;
pub mod circuit_breaker;
pub mod leg_risk;
pub mod mode;
pub mod pnl;

pub use checks::{
    PreTradeCheck, PreTradeRejection, RiskCheckConfig, RiskCheckResult, RiskChecker,
};
pub use circuit_breaker::{
    CircuitBreaker, CircuitBreakerConfig, CircuitBreakerState, CircuitBreakerStats,
    SharedCircuitBreaker, TripReason,
};
pub use leg_risk::{
    ChaseReason, CloseReason, LegRiskAction, LegRiskAssessment, LegRiskConfig, LegRiskManager,
    LegState, LegStatus,
};
pub use mode::{
    CompositeRiskManager, RiskCheckInput, RiskCheckOutput, RiskManager, RiskMode,
    create_risk_manager,
};
pub use pnl::{PnlRejectionReason, PnlRiskConfig, PnlRiskManager, PnlRiskStats};
