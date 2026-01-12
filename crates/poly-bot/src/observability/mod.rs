//! Observability module for fire-and-forget decision capture.
//!
//! This module provides the observability infrastructure for capturing trading decisions
//! with minimal hot path overhead (<10ns).
//!
//! ## Architecture
//!
//! The observability system uses a two-phase approach:
//!
//! 1. **Hot path capture**: `DecisionSnapshot` contains only primitives (u64, u8) to avoid
//!    any allocations on the critical trading path. Event IDs are pre-hashed to u64.
//!
//! 2. **Async enrichment**: A background processor receives snapshots via bounded channel
//!    and enriches them to full `DecisionContext` with string identifiers before storage.
//!
//! ## Performance Requirements
//!
//! - Hot path overhead: <10ns (single try_send with primitives)
//! - No heap allocations in snapshot creation
//! - Drops on channel backpressure rather than blocking

pub mod types;

pub use types::{
    ActionType, Counterfactual, DecisionContext, DecisionSnapshot, ObservabilityEvent,
    OutcomeType, SnapshotBuilder,
};
