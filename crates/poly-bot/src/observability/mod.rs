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
//!
//! ## Usage
//!
//! ```ignore
//! use poly_bot::observability::{
//!     ObservabilityCapture, CaptureConfig, DecisionSnapshot,
//!     ObservabilityProcessor, ProcessorConfig, InMemoryIdLookup,
//! };
//! use std::sync::Arc;
//!
//! // Create capture from config
//! let (capture, receiver) = ObservabilityCapture::from_config(CaptureConfig::default());
//!
//! // On hot path - ultra-fast, non-blocking
//! capture.try_capture(snapshot);
//!
//! // Background processor handles enrichment and storage
//! if let Some(rx) = receiver {
//!     let id_lookup = Arc::new(InMemoryIdLookup::new());
//!     let processor = ObservabilityProcessor::with_defaults(id_lookup);
//!     processor.run(rx, clickhouse_client, shutdown_rx).await;
//! }
//! ```

pub mod capture;
pub mod processor;
pub mod types;

pub use capture::{
    create_capture_channel, create_shared_capture, CaptureConfig, CaptureReceiver, CaptureSender,
    CaptureStats, CaptureStatsSnapshot, ObservabilityCapture, SharedCapture,
    DEFAULT_CHANNEL_CAPACITY,
};
pub use processor::{
    create_shared_processor, hash_string, spawn_processor, DecisionRecord, IdLookup,
    InMemoryIdLookup, ObservabilityProcessor, ProcessorConfig, ProcessorStats,
    ProcessorStatsSnapshot, SharedProcessor,
};
pub use types::{
    ActionType, Counterfactual, DecisionContext, DecisionSnapshot, ObservabilityEvent,
    OutcomeType, SnapshotBuilder,
};
