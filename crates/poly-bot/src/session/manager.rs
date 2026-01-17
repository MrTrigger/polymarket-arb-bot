//! Unified session manager for centralized state tracking.
//!
//! The SessionManager is the single source of truth for all trading activity:
//! - Orders (with proper lifecycle state machine)
//! - Positions (consolidated inventory)
//! - P&L (realized and unrealized)
//! - Statistics (counters and computed metrics)
//!
//! It also handles persistence to ClickHouse for historical analysis.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use poly_common::types::{CryptoAsset, Outcome, Side};

use crate::dashboard::BotMode;

use super::orders::{OrderSource, OrderState, OrderSummary, OrderTracker, TrackedOrder};
use super::pnl::{CompletedTrade, PnlSnapshot, PnlSummary, PnlTracker};
use super::positions::{MarketPosition, PositionSummary, PositionTracker};
use super::stats::{EngineType, SessionStats, StatsSummary};

/// Events that can be persisted to the database.
#[derive(Debug, Clone, Serialize)]
pub enum PersistenceEvent {
    /// Order state changed.
    OrderUpdate(TrackedOrder),
    /// Position updated.
    PositionUpdate(MarketPosition),
    /// P&L snapshot taken.
    PnlSnapshot(PnlSnapshot),
    /// Trade completed.
    TradeCompleted(CompletedTrade),
    /// Session summary (periodic).
    SessionSummary(SessionSummary),
}

/// Configuration for the session manager.
#[derive(Debug, Clone)]
pub struct SessionManagerConfig {
    /// Starting balance for P&L tracking.
    pub starting_balance: Decimal,
    /// Bot mode.
    pub mode: BotMode,
    /// How often to take P&L snapshots (seconds).
    pub snapshot_interval_secs: u64,
    /// Whether to enable persistence.
    pub persistence_enabled: bool,
}

impl Default for SessionManagerConfig {
    fn default() -> Self {
        Self {
            starting_balance: dec!(1000),
            mode: BotMode::Paper,
            snapshot_interval_secs: 10,
            persistence_enabled: true,
        }
    }
}

/// Unified session manager for all state tracking.
pub struct SessionManager {
    /// Configuration.
    config: SessionManagerConfig,
    /// Order tracker.
    orders: OrderTracker,
    /// Position tracker.
    positions: PositionTracker,
    /// P&L tracker.
    pnl: PnlTracker,
    /// Session statistics.
    stats: SessionStats,
    /// Channel for persistence events.
    persistence_tx: Option<mpsc::Sender<PersistenceEvent>>,
    /// Last snapshot time.
    last_snapshot: parking_lot::RwLock<DateTime<Utc>>,
}

impl SessionManager {
    /// Create a new session manager.
    pub fn new(config: SessionManagerConfig) -> Self {
        Self {
            stats: SessionStats::new(config.mode),
            pnl: PnlTracker::new(config.starting_balance),
            orders: OrderTracker::new(),
            positions: PositionTracker::new(),
            persistence_tx: None,
            last_snapshot: parking_lot::RwLock::new(Utc::now()),
            config,
        }
    }

    /// Create with persistence channel.
    pub fn with_persistence(
        config: SessionManagerConfig,
        persistence_tx: mpsc::Sender<PersistenceEvent>,
    ) -> Self {
        Self {
            stats: SessionStats::new(config.mode),
            pnl: PnlTracker::new(config.starting_balance),
            orders: OrderTracker::new(),
            positions: PositionTracker::new(),
            persistence_tx: Some(persistence_tx),
            last_snapshot: parking_lot::RwLock::new(Utc::now()),
            config,
        }
    }

    // =========================================================================
    // Order Management
    // =========================================================================

    /// Create a new order and reserve budget.
    ///
    /// Returns the order ID for tracking.
    pub fn create_order(
        &self,
        event_id: &str,
        token_id: &str,
        asset: CryptoAsset,
        outcome: Outcome,
        side: Side,
        size: Decimal,
        price: Decimal,
        source: OrderSource,
    ) -> String {
        let order_id = self.orders.create_order(
            event_id.to_string(),
            token_id.to_string(),
            asset,
            outcome,
            side,
            size,
            price,
            source,
        );

        debug!(
            order_id = %order_id,
            event_id = %event_id,
            size = %size,
            price = %price,
            source = %source,
            "Order created, budget reserved"
        );

        // Persist
        if let Some(order) = self.orders.get_order(&order_id) {
            self.persist(PersistenceEvent::OrderUpdate(order));
        }

        order_id
    }

    /// Mark an order as pending (submitted to exchange).
    pub fn mark_order_pending(&self, order_id: &str, exchange_order_id: &str) {
        self.orders.mark_pending(order_id, exchange_order_id.to_string());

        if let Some(order) = self.orders.get_order(order_id) {
            self.persist(PersistenceEvent::OrderUpdate(order));
        }
    }

    /// Record a fill for an order.
    ///
    /// This updates the order state, position, and records metrics.
    pub fn record_fill(
        &self,
        order_id: &str,
        quantity: Decimal,
        price: Decimal,
        fee: Decimal,
    ) {
        // Update order
        if let Some((event_id, outcome, qty, cost)) =
            self.orders.record_fill(order_id, quantity, price, fee)
        {
            // Update position
            self.positions.record_fill(&event_id, outcome, qty, cost, fee);

            // Update P&L tracker with fees
            self.pnl.record_fees(fee);

            // Update stats
            self.stats.record_trade_executed(cost);

            debug!(
                order_id = %order_id,
                quantity = %quantity,
                price = %price,
                fee = %fee,
                "Fill recorded"
            );

            // Persist order update
            if let Some(order) = self.orders.get_order(order_id) {
                self.persist(PersistenceEvent::OrderUpdate(order));
            }

            // Persist position update
            if let Some(pos) = self.positions.get_position(&event_id) {
                self.persist(PersistenceEvent::PositionUpdate(pos));
            }
        }
    }

    /// Mark an order as cancelled.
    pub fn cancel_order(&self, order_id: &str, reason: Option<&str>) {
        self.orders.mark_cancelled(order_id, reason.map(|s| s.to_string()));

        if let Some(order) = self.orders.get_order(order_id) {
            debug!(order_id = %order_id, reason = ?reason, "Order cancelled");
            self.persist(PersistenceEvent::OrderUpdate(order));
        }
    }

    /// Mark an order as rejected.
    pub fn reject_order(&self, order_id: &str, reason: &str) {
        self.orders.mark_rejected(order_id, reason.to_string());
        self.stats.record_trade_failed();

        if let Some(order) = self.orders.get_order(order_id) {
            warn!(order_id = %order_id, reason = %reason, "Order rejected");
            self.persist(PersistenceEvent::OrderUpdate(order));
        }
    }

    /// Get an order by ID.
    pub fn get_order(&self, order_id: &str) -> Option<TrackedOrder> {
        self.orders.get_order(order_id)
    }

    /// Get all pending orders for a market.
    pub fn pending_orders(&self, event_id: &str) -> Vec<TrackedOrder> {
        self.orders.pending_orders(event_id)
    }

    /// Get reserved budget for a market (pending orders).
    pub fn reserved_budget(&self, event_id: &str) -> Decimal {
        self.orders.reserved_budget(event_id)
    }

    /// Get total reserved budget across all markets.
    pub fn total_reserved_budget(&self) -> Decimal {
        self.orders.total_reserved_budget()
    }

    /// Check if there are pending orders for a market.
    pub fn has_pending_orders(&self, event_id: &str) -> bool {
        !self.orders.pending_orders(event_id).is_empty()
    }

    // =========================================================================
    // Position Management
    // =========================================================================

    /// Initialize or get a market position.
    pub fn get_or_create_position(
        &self,
        event_id: &str,
        asset: CryptoAsset,
        yes_token_id: &str,
        no_token_id: &str,
        strike_price: Decimal,
    ) -> MarketPosition {
        let pos = self.positions.get_or_create(
            event_id,
            asset,
            yes_token_id,
            no_token_id,
            strike_price,
        );

        // First time seeing this market
        if pos.trade_count == 0 {
            self.stats.record_market_traded();
        }

        pos
    }

    /// Get a position by event ID.
    pub fn get_position(&self, event_id: &str) -> Option<MarketPosition> {
        self.positions.get_position(event_id)
    }

    /// Get all active positions.
    pub fn active_positions(&self) -> Vec<MarketPosition> {
        self.positions.active_positions()
    }

    /// Get total exposure across all positions.
    pub fn total_exposure(&self) -> Decimal {
        self.positions.total_exposure()
    }

    /// Get available budget (considering reserved and exposure).
    pub fn available_budget(&self) -> Decimal {
        let starting = self.config.starting_balance;
        let reserved = self.total_reserved_budget();
        let exposure = self.total_exposure();
        let realized_pnl = self.pnl.realized_pnl();

        // Available = starting + realized_pnl - reserved - exposure
        (starting + realized_pnl - reserved - exposure).max(Decimal::ZERO)
    }

    /// Check if we can place an order of given size for a market.
    pub fn can_place_order(&self, event_id: &str, size: Decimal, max_per_market: Decimal) -> bool {
        // Check total available budget
        if size > self.available_budget() {
            return false;
        }

        // Check per-market limit (position + reserved + new order)
        let position_exposure = self.positions
            .get_position(event_id)
            .map(|p| p.total_exposure())
            .unwrap_or(Decimal::ZERO);
        let reserved = self.reserved_budget(event_id);
        let total_for_market = position_exposure + reserved + size;

        if total_for_market > max_per_market {
            return false;
        }

        true
    }

    // =========================================================================
    // P&L Management
    // =========================================================================

    /// Record realized P&L (from settlement or position close).
    pub fn record_realized_pnl(&self, event_id: &str, pnl: Decimal) {
        self.pnl.record_realized_pnl(pnl);

        info!(event_id = %event_id, pnl = %pnl, "Realized P&L recorded");

        // Remove position if settled
        if let Some(pos) = self.positions.remove_position(event_id) {
            self.persist(PersistenceEvent::PositionUpdate(pos));
        }
    }

    /// Record a completed trade for win/loss tracking.
    pub fn record_completed_trade(&self, trade: CompletedTrade) {
        self.persist(PersistenceEvent::TradeCompleted(trade.clone()));
        self.pnl.record_completed_trade(trade);
    }

    /// Take a P&L snapshot (call periodically).
    pub fn take_pnl_snapshot(&self, unrealized_pnl: Decimal) {
        let exposure = self.total_exposure();
        self.pnl.take_snapshot(unrealized_pnl, exposure);

        if let Some(snapshot) = self.pnl.latest_snapshot() {
            self.persist(PersistenceEvent::PnlSnapshot(snapshot));
        }

        *self.last_snapshot.write() = Utc::now();
    }

    /// Check if it's time for a P&L snapshot.
    pub fn should_take_snapshot(&self) -> bool {
        let last = *self.last_snapshot.read();
        let elapsed = (Utc::now() - last).num_seconds() as u64;
        elapsed >= self.config.snapshot_interval_secs
    }

    /// Maybe take a snapshot if interval has passed.
    pub fn maybe_take_snapshot(&self, unrealized_pnl: Decimal) {
        if self.should_take_snapshot() {
            self.take_pnl_snapshot(unrealized_pnl);
        }
    }

    /// Get P&L summary.
    pub fn pnl_summary(&self) -> PnlSummary {
        self.pnl.summary()
    }

    // =========================================================================
    // Statistics
    // =========================================================================

    /// Record an event processed.
    pub fn record_event(&self) {
        self.stats.record_event();
    }

    /// Record an opportunity detected.
    pub fn record_opportunity(&self, engine: EngineType) {
        self.stats.record_opportunity(engine);
    }

    /// Record a trade skipped.
    pub fn record_trade_skipped(&self) {
        self.stats.record_trade_skipped();
    }

    /// Record a circuit breaker trip.
    pub fn record_circuit_breaker_trip(&self) {
        self.stats.record_circuit_breaker_trip();
    }

    /// Record shadow order fired.
    pub fn record_shadow_fired(&self) {
        self.stats.record_shadow_fired();
    }

    /// Record shadow order filled.
    pub fn record_shadow_filled(&self) {
        self.stats.record_shadow_filled();
    }

    /// Get consecutive failures (for circuit breaker logic).
    pub fn consecutive_failures(&self) -> u64 {
        self.stats.consecutive_failures()
    }

    /// Get stats summary.
    pub fn stats_summary(&self) -> StatsSummary {
        self.stats.summary()
    }

    // =========================================================================
    // Summary & Persistence
    // =========================================================================

    /// Get full session summary.
    pub fn summary(&self) -> SessionSummary {
        SessionSummary {
            stats: self.stats.summary(),
            orders: self.orders.summary(),
            positions: self.positions.summary(),
            pnl: self.pnl.summary(),
            available_budget: self.available_budget(),
            reserved_budget: self.total_reserved_budget(),
        }
    }

    /// Persist an event to the database.
    fn persist(&self, event: PersistenceEvent) {
        if !self.config.persistence_enabled {
            return;
        }

        if let Some(tx) = &self.persistence_tx {
            // Fire and forget - don't block hot path
            let _ = tx.try_send(event);
        }
    }

    /// Flush a summary to persistence.
    pub fn flush_summary(&self) {
        self.persist(PersistenceEvent::SessionSummary(self.summary()));
    }

    /// Reset for a new session.
    pub fn reset(&self) {
        self.orders.clear();
        self.positions.clear();
        self.pnl.reset();
        // Note: stats.reset() requires &mut, handle separately if needed
        *self.last_snapshot.write() = Utc::now();

        info!("Session manager reset");
    }

    // =========================================================================
    // Accessors
    // =========================================================================

    /// Get the order tracker (for advanced queries).
    pub fn orders(&self) -> &OrderTracker {
        &self.orders
    }

    /// Get the position tracker.
    pub fn positions(&self) -> &PositionTracker {
        &self.positions
    }

    /// Get the P&L tracker.
    pub fn pnl(&self) -> &PnlTracker {
        &self.pnl
    }

    /// Get session stats.
    pub fn stats(&self) -> &SessionStats {
        &self.stats
    }

    /// Get session ID.
    pub fn session_id(&self) -> uuid::Uuid {
        self.stats.session_id()
    }

    /// Get bot mode.
    pub fn mode(&self) -> BotMode {
        self.config.mode
    }

    /// Get starting balance.
    pub fn starting_balance(&self) -> Decimal {
        self.config.starting_balance
    }
}

impl Default for SessionManager {
    fn default() -> Self {
        Self::new(SessionManagerConfig::default())
    }
}

/// Full session summary combining all trackers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSummary {
    pub stats: StatsSummary,
    pub orders: OrderSummary,
    pub positions: PositionSummary,
    pub pnl: PnlSummary,
    pub available_budget: Decimal,
    pub reserved_budget: Decimal,
}

/// Create a shared session manager.
pub type SharedSessionManager = Arc<SessionManager>;

/// Create a new shared session manager.
pub fn new_shared(config: SessionManagerConfig) -> SharedSessionManager {
    Arc::new(SessionManager::new(config))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_order_flow() {
        let manager = SessionManager::new(SessionManagerConfig {
            starting_balance: dec!(1000),
            ..Default::default()
        });

        // Create order
        let order_id = manager.create_order(
            "event1",
            "token1",
            CryptoAsset::Btc,
            Outcome::Yes,
            Side::Buy,
            dec!(100),
            dec!(0.50),
            OrderSource::Directional,
        );

        // Check budget reserved
        assert_eq!(manager.reserved_budget("event1"), dec!(100));
        assert_eq!(manager.available_budget(), dec!(900));

        // Record fill
        manager.record_fill(&order_id, dec!(200), dec!(0.50), dec!(0.50));

        // Check position created
        let pos = manager.get_position("event1").unwrap();
        assert_eq!(pos.yes_shares, dec!(200));

        // Budget should be released
        assert_eq!(manager.reserved_budget("event1"), dec!(0));
    }

    #[test]
    fn test_can_place_order() {
        let manager = SessionManager::new(SessionManagerConfig {
            starting_balance: dec!(100),
            ..Default::default()
        });

        // Can place small order
        assert!(manager.can_place_order("event1", dec!(50), dec!(100)));

        // Can't exceed available budget
        assert!(!manager.can_place_order("event1", dec!(150), dec!(200)));

        // Create order to use some budget
        manager.create_order(
            "event1",
            "token1",
            CryptoAsset::Btc,
            Outcome::Yes,
            Side::Buy,
            dec!(60),
            dec!(0.50),
            OrderSource::Directional,
        );

        // Now exceeds per-market limit
        assert!(!manager.can_place_order("event1", dec!(50), dec!(100)));

        // But can still place on different market
        assert!(manager.can_place_order("event2", dec!(30), dec!(100)));
    }

    #[test]
    fn test_session_summary() {
        let manager = SessionManager::new(SessionManagerConfig {
            starting_balance: dec!(1000),
            ..Default::default()
        });

        // Do some activity
        let order_id = manager.create_order(
            "event1",
            "token1",
            CryptoAsset::Btc,
            Outcome::Yes,
            Side::Buy,
            dec!(100),
            dec!(0.50),
            OrderSource::Directional,
        );
        manager.record_fill(&order_id, dec!(100), dec!(0.50), dec!(0.50));
        manager.record_opportunity(EngineType::Directional);

        let summary = manager.summary();
        assert_eq!(summary.stats.trades_executed, 1);
        assert_eq!(summary.stats.directional_opportunities, 1);
        assert_eq!(summary.positions.active_markets, 1);
    }
}
