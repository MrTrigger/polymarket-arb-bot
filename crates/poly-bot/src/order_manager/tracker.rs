//! Pending Order Tracker - Simple tracking of in-flight orders by market.
//!
//! This is a stepping stone toward full OrderManager integration.
//! It centralizes the pending order tracking that was previously done
//! with boolean flags in TrackedMarket.

use dashmap::DashSet;
use std::sync::Arc;

/// Types of orders that can be tracked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PendingOrderType {
    /// Directional trade (UP/DOWN based on signal)
    Directional,
    /// Arbitrage trade (YES + NO hedge)
    Arbitrage,
    /// Maker order (rebate hunting)
    Maker,
}

/// Tracks pending orders by market to prevent duplicate submissions.
///
/// Thread-safe via DashSet for lock-free access on hot path.
#[derive(Debug, Clone, Default)]
pub struct PendingOrderTracker {
    /// Set of (event_id, order_type) pairs that are currently pending.
    pending: Arc<DashSet<(String, PendingOrderType)>>,
}

impl PendingOrderTracker {
    /// Create a new tracker.
    pub fn new() -> Self {
        Self {
            pending: Arc::new(DashSet::new()),
        }
    }

    /// Register a pending order for a market.
    ///
    /// Returns true if successfully registered (wasn't already pending).
    /// Returns false if already pending (duplicate).
    pub fn register(&self, event_id: &str, order_type: PendingOrderType) -> bool {
        self.pending.insert((event_id.to_string(), order_type))
    }

    /// Check if a market has a pending order of the given type.
    pub fn is_pending(&self, event_id: &str, order_type: PendingOrderType) -> bool {
        self.pending.contains(&(event_id.to_string(), order_type))
    }

    /// Check if a market has any pending order.
    pub fn has_any_pending(&self, event_id: &str) -> bool {
        self.is_pending(event_id, PendingOrderType::Directional)
            || self.is_pending(event_id, PendingOrderType::Arbitrage)
            || self.is_pending(event_id, PendingOrderType::Maker)
    }

    /// Clear a pending order for a market.
    ///
    /// Returns true if was pending (and now cleared).
    pub fn clear(&self, event_id: &str, order_type: PendingOrderType) -> bool {
        self.pending.remove(&(event_id.to_string(), order_type)).is_some()
    }

    /// Clear all pending orders for a market (e.g., when window closes).
    pub fn clear_all(&self, event_id: &str) {
        self.pending.remove(&(event_id.to_string(), PendingOrderType::Directional));
        self.pending.remove(&(event_id.to_string(), PendingOrderType::Arbitrage));
        self.pending.remove(&(event_id.to_string(), PendingOrderType::Maker));
    }

    /// Get count of pending orders (for debugging/metrics).
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_register_and_check() {
        let tracker = PendingOrderTracker::new();

        assert!(!tracker.is_pending("market-1", PendingOrderType::Directional));

        // Register returns true first time
        assert!(tracker.register("market-1", PendingOrderType::Directional));
        assert!(tracker.is_pending("market-1", PendingOrderType::Directional));

        // Register returns false for duplicate
        assert!(!tracker.register("market-1", PendingOrderType::Directional));
    }

    #[test]
    fn test_different_order_types() {
        let tracker = PendingOrderTracker::new();

        tracker.register("market-1", PendingOrderType::Directional);

        // Different order type is not pending
        assert!(!tracker.is_pending("market-1", PendingOrderType::Arbitrage));

        // Can register different type
        assert!(tracker.register("market-1", PendingOrderType::Arbitrage));
        assert!(tracker.is_pending("market-1", PendingOrderType::Arbitrage));
    }

    #[test]
    fn test_has_any_pending() {
        let tracker = PendingOrderTracker::new();

        assert!(!tracker.has_any_pending("market-1"));

        tracker.register("market-1", PendingOrderType::Directional);
        assert!(tracker.has_any_pending("market-1"));
    }

    #[test]
    fn test_clear() {
        let tracker = PendingOrderTracker::new();

        tracker.register("market-1", PendingOrderType::Directional);
        assert!(tracker.is_pending("market-1", PendingOrderType::Directional));

        assert!(tracker.clear("market-1", PendingOrderType::Directional));
        assert!(!tracker.is_pending("market-1", PendingOrderType::Directional));

        // Clear returns false if wasn't pending
        assert!(!tracker.clear("market-1", PendingOrderType::Directional));
    }

    #[test]
    fn test_clear_all() {
        let tracker = PendingOrderTracker::new();

        tracker.register("market-1", PendingOrderType::Directional);
        tracker.register("market-1", PendingOrderType::Arbitrage);

        tracker.clear_all("market-1");

        assert!(!tracker.is_pending("market-1", PendingOrderType::Directional));
        assert!(!tracker.is_pending("market-1", PendingOrderType::Arbitrage));
    }

    #[test]
    fn test_different_markets() {
        let tracker = PendingOrderTracker::new();

        tracker.register("market-1", PendingOrderType::Directional);

        // Different market is not affected
        assert!(!tracker.is_pending("market-2", PendingOrderType::Directional));
        assert!(tracker.register("market-2", PendingOrderType::Directional));
    }
}
