//! Session management for unified state tracking.
//!
//! This module provides centralized tracking of all trading activity within a session:
//!
//! - **Orders**: Full lifecycle tracking (pending → filled/cancelled/rejected)
//! - **Positions**: Consolidated inventory across all markets
//! - **P&L**: Realized and unrealized profit/loss with live calculation
//! - **Statistics**: Session-wide metrics (win rate, volume, fees, etc.)
//!
//! ## Architecture
//!
//! ```text
//! SessionManager
//! ├── OrderTracker
//! │   ├── Pending orders (in-flight, reserves budget)
//! │   ├── Filled orders (completed trades)
//! │   └── Cancelled/Rejected orders (released budget)
//! ├── PositionTracker
//! │   └── Per-market inventory (single source of truth)
//! ├── PnlTracker
//! │   ├── Realized P&L (from closed positions)
//! │   ├── Unrealized P&L (mark-to-market)
//! │   └── Metrics (win rate, Sharpe, drawdown)
//! └── SessionStats
//!     └── Aggregated counters and statistics
//! ```
//!
//! ## Hot Path Considerations
//!
//! The SessionManager is designed for low-latency access:
//! - Atomic counters for frequently-updated metrics
//! - DashMap for concurrent order/position access
//! - Batch persistence to ClickHouse (fire-and-forget)

pub mod manager;
pub mod orders;
pub mod positions;
pub mod pnl;
pub mod stats;

pub use manager::{SessionManager, SessionManagerConfig, SharedSessionManager, SessionSummary, new_shared, PersistenceEvent};
pub use orders::{OrderTracker, TrackedOrder, OrderState, OrderSource, OrderSummary};
pub use positions::{PositionTracker, MarketPosition, PositionSummary, PositionState};
pub use pnl::{PnlTracker, PnlSnapshot, PnlSummary, CompletedTrade};
pub use stats::{SessionStats, StatsSummary, EngineType};
