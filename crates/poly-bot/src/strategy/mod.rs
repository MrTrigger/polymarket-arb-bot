//! Trading strategy implementations.
//!
//! This module contains the core strategy logic for the arbitrage bot:
//! - Arbitrage detection and opportunity scoring
//! - Toxic flow detection
//! - Position sizing
//!
//! ## Hot Path Requirements
//!
//! The strategy loop runs on the hot path and must be fast:
//! - Arb detection: <1μs
//! - Toxic flow check: <1μs
//! - No allocations in critical path

pub mod arb;
pub mod toxic;

pub use arb::{ArbDetector, ArbOpportunity, ArbRejection, ArbThresholds};
pub use toxic::{
    ToxicFlowConfig, ToxicFlowDetector, ToxicFlowWarning, ToxicIndicators, ToxicSeverity,
};
