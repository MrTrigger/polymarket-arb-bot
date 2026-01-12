//! Risk management for the trading bot.
//!
//! This module provides pre-trade validation and risk controls:
//! - Position and exposure limits
//! - Time-based restrictions
//! - Toxic flow integration
//! - Daily loss limits
//! - Circuit breaker for system protection
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

pub use checks::{
    PreTradeCheck, PreTradeRejection, RiskCheckConfig, RiskCheckResult, RiskChecker,
};
pub use circuit_breaker::{
    CircuitBreaker, CircuitBreakerConfig, CircuitBreakerState, CircuitBreakerStats,
    SharedCircuitBreaker, TripReason,
};
