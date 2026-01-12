//! Risk management for the trading bot.
//!
//! This module provides pre-trade validation and risk controls:
//! - Position and exposure limits
//! - Time-based restrictions
//! - Toxic flow integration
//! - Daily loss limits
//!
//! ## Hot Path Requirements
//!
//! Risk checks run before every trade and must be fast:
//! - Target: <1μs for all pre-trade checks
//! - Avoid heap allocations
//! - Use pre-computed state where possible

pub mod checks;

pub use checks::{
    PreTradeCheck, PreTradeRejection, RiskCheckConfig, RiskCheckResult, RiskChecker,
};
