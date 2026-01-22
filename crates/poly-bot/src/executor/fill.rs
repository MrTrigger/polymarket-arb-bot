//! Fill handling for order execution.
//!
//! This module provides comprehensive fill processing:
//! - Parsing and validating fill events
//! - Updating inventory on fills
//! - Firing shadow bids on primary fills
//! - Handling partial fills
//! - Sending fill events to observability channel
//!
//! ## Fill Flow
//!
//! 1. Fill event received (from API poll or WebSocket)
//! 2. Validate fill data (order exists, size > 0)
//! 3. Update inventory (add shares, adjust cost basis)
//! 4. Fire shadow bid if this is a primary leg fill
//! 5. Send fill event to observability channel
//! 6. Update metrics (P&L, volume)
//!
//! ## Partial Fills
//!
//! Partial fills are handled by:
//! - Tracking cumulative filled size per order
//! - Firing shadow only on first partial fill
//! - Updating inventory incrementally for each partial
//! - Calculating weighted average price across partials

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tracing::{debug, info, trace, warn};

use poly_common::types::Outcome;

use super::shadow::{SharedShadowManager, ShadowError};
use super::{OrderFill, PartialOrderFill};
use crate::state::{GlobalState, InventoryPosition};

/// Configuration for fill handler.
#[derive(Debug, Clone)]
pub struct FillHandlerConfig {
    /// Whether to fire shadows on fills.
    pub enable_shadow: bool,
    /// Maximum latency for shadow fire (microseconds).
    pub shadow_latency_target_us: u64,
    /// Whether to log slow shadow fires.
    pub log_slow_shadows: bool,
    /// Minimum fill size to process (filter dust fills).
    pub min_fill_size: Decimal,
}

impl Default for FillHandlerConfig {
    fn default() -> Self {
        Self {
            enable_shadow: true,
            shadow_latency_target_us: 2000, // 2ms target
            log_slow_shadows: true,
            min_fill_size: Decimal::new(1, 2), // 0.01 minimum
        }
    }
}

/// A fill event to be processed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FillData {
    /// Order ID that was filled.
    pub order_id: String,
    /// Request ID for correlation.
    pub request_id: String,
    /// Event ID for the market.
    pub event_id: String,
    /// Token ID that was traded.
    pub token_id: String,
    /// YES or NO outcome.
    pub outcome: Outcome,
    /// Filled size.
    pub size: Decimal,
    /// Fill price.
    pub price: Decimal,
    /// Fee paid.
    pub fee: Decimal,
    /// Whether this is a partial fill.
    pub is_partial: bool,
    /// Cumulative filled size (for partial fills).
    pub cumulative_size: Decimal,
    /// Original requested size.
    pub requested_size: Decimal,
    /// Fill timestamp.
    pub timestamp: DateTime<Utc>,
    /// Sequence number for this fill (for partial fills).
    pub sequence: u32,
}

impl FillData {
    /// Create from a full OrderFill.
    pub fn from_full_fill(
        fill: &OrderFill,
        event_id: String,
        token_id: String,
        outcome: Outcome,
        requested_size: Decimal,
    ) -> Self {
        Self {
            order_id: fill.order_id.clone(),
            request_id: fill.request_id.clone(),
            event_id,
            token_id,
            outcome,
            size: fill.size,
            price: fill.price,
            fee: fill.fee,
            is_partial: false,
            cumulative_size: fill.size,
            requested_size,
            timestamp: fill.timestamp,
            sequence: 1,
        }
    }

    /// Create from a partial OrderFill.
    pub fn from_partial_fill(
        fill: &PartialOrderFill,
        event_id: String,
        token_id: String,
        outcome: Outcome,
        previous_cumulative: Decimal,
        sequence: u32,
    ) -> Self {
        let incremental_size = fill.filled_size - previous_cumulative;
        Self {
            order_id: fill.order_id.clone(),
            request_id: fill.request_id.clone(),
            event_id,
            token_id,
            outcome,
            size: incremental_size,
            price: fill.avg_price,
            fee: fill.fee,
            is_partial: true,
            cumulative_size: fill.filled_size,
            requested_size: fill.requested_size,
            timestamp: fill.timestamp,
            sequence,
        }
    }

    /// Total cost of this fill (size * price + fee).
    pub fn total_cost(&self) -> Decimal {
        self.size * self.price + self.fee
    }

    /// Net proceeds for a sell (size * price - fee).
    pub fn net_proceeds(&self) -> Decimal {
        self.size * self.price - self.fee
    }

    /// Fill ratio (cumulative / requested).
    pub fn fill_ratio(&self) -> Decimal {
        if self.requested_size > Decimal::ZERO {
            self.cumulative_size / self.requested_size
        } else {
            Decimal::ZERO
        }
    }

    /// Whether this is the final fill for the order.
    pub fn is_complete(&self) -> bool {
        !self.is_partial || self.cumulative_size >= self.requested_size
    }
}

/// Result of processing a fill.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FillResult {
    /// The processed fill data.
    pub fill: FillData,
    /// Whether shadow was fired.
    pub shadow_fired: bool,
    /// Shadow fire latency (if fired).
    pub shadow_latency_us: Option<u64>,
    /// Shadow order ID (if fired).
    pub shadow_order_id: Option<String>,
    /// New inventory position after fill.
    pub inventory_after: InventorySnapshot,
    /// Processing timestamp.
    pub processed_at: DateTime<Utc>,
}

/// Snapshot of inventory for fill result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InventorySnapshot {
    /// Event ID.
    pub event_id: String,
    /// YES shares held.
    pub yes_shares: Decimal,
    /// NO shares held.
    pub no_shares: Decimal,
    /// YES cost basis.
    pub yes_cost_basis: Decimal,
    /// NO cost basis.
    pub no_cost_basis: Decimal,
    /// Total exposure.
    pub total_exposure: Decimal,
    /// Matched pairs.
    pub matched_pairs: Decimal,
}

impl InventorySnapshot {
    /// Create from InventoryPosition.
    pub fn from_position(pos: &InventoryPosition) -> Self {
        Self {
            event_id: pos.event_id.clone(),
            yes_shares: pos.yes_shares,
            no_shares: pos.no_shares,
            yes_cost_basis: pos.yes_cost_basis,
            no_cost_basis: pos.no_cost_basis,
            total_exposure: pos.total_exposure(),
            matched_pairs: pos.yes_shares.min(pos.no_shares),
        }
    }
}

/// State for tracking partial fills.
#[derive(Debug, Clone)]
pub struct PartialFillState {
    /// Order ID.
    pub order_id: String,
    /// Cumulative filled size.
    pub cumulative_size: Decimal,
    /// Weighted average price.
    pub avg_price: Decimal,
    /// Total fees paid.
    pub total_fee: Decimal,
    /// Number of partial fills.
    pub fill_count: u32,
    /// Whether shadow has been fired.
    pub shadow_fired: bool,
    /// First fill timestamp.
    pub first_fill_at: DateTime<Utc>,
    /// Last fill timestamp.
    pub last_fill_at: DateTime<Utc>,
}

impl PartialFillState {
    /// Create new state for an order.
    pub fn new(order_id: String) -> Self {
        Self {
            order_id,
            cumulative_size: Decimal::ZERO,
            avg_price: Decimal::ZERO,
            total_fee: Decimal::ZERO,
            fill_count: 0,
            shadow_fired: false,
            first_fill_at: Utc::now(),
            last_fill_at: Utc::now(),
        }
    }

    /// Update state with a new partial fill.
    pub fn update(&mut self, size: Decimal, price: Decimal, fee: Decimal) {
        // Calculate new weighted average price
        let old_value = self.cumulative_size * self.avg_price;
        let new_value = size * price;
        let new_total = self.cumulative_size + size;

        if new_total > Decimal::ZERO {
            self.avg_price = (old_value + new_value) / new_total;
        }

        self.cumulative_size = new_total;
        self.total_fee += fee;
        self.fill_count += 1;
        self.last_fill_at = Utc::now();
    }

    /// Mark shadow as fired.
    pub fn mark_shadow_fired(&mut self) {
        self.shadow_fired = true;
    }
}

/// Handler for processing fills.
pub struct FillHandler {
    /// Configuration.
    config: FillHandlerConfig,
    /// Global state for inventory updates.
    state: Arc<GlobalState>,
    /// Shadow manager for firing shadows.
    shadow_manager: Option<SharedShadowManager>,
    /// Observability channel for fill events.
    obs_sender: Option<mpsc::Sender<FillResult>>,
    /// Partial fill tracking by order ID.
    partial_fills: HashMap<String, PartialFillState>,
}

impl FillHandler {
    /// Create a new fill handler.
    pub fn new(
        config: FillHandlerConfig,
        state: Arc<GlobalState>,
        shadow_manager: Option<SharedShadowManager>,
    ) -> Self {
        Self {
            config,
            state,
            shadow_manager,
            obs_sender: None,
            partial_fills: HashMap::new(),
        }
    }

    /// Set observability channel for fill events.
    pub fn with_observability(mut self, sender: mpsc::Sender<FillResult>) -> Self {
        self.obs_sender = Some(sender);
        self
    }

    /// Process a fill event.
    ///
    /// This is the main entry point for fill processing.
    /// Returns the result of processing.
    pub async fn process_fill(&mut self, fill: FillData) -> FillResult {
        let start = Instant::now();

        // Validate fill
        if fill.size < self.config.min_fill_size {
            trace!("Ignoring dust fill: {} size={}", fill.order_id, fill.size);
            return self.create_result(fill, false, None, None);
        }

        debug!(
            order_id = %fill.order_id,
            event_id = %fill.event_id,
            outcome = ?fill.outcome,
            size = %fill.size,
            price = %fill.price,
            is_partial = fill.is_partial,
            "Processing fill"
        );

        // Update partial fill tracking
        let should_fire_shadow = self.track_partial_fill(&fill);

        // Update inventory
        self.update_inventory(&fill);

        // Fire shadow bid if appropriate
        let (shadow_fired, shadow_latency_us, shadow_order_id) = if should_fire_shadow {
            self.fire_shadow(&fill).await
        } else {
            (false, None, None)
        };

        // Create result
        let result = self.create_result(fill, shadow_fired, shadow_latency_us, shadow_order_id);

        // Log slow processing
        let elapsed_us = start.elapsed().as_micros() as u64;
        if elapsed_us > 1000 {
            debug!(
                order_id = %result.fill.order_id,
                elapsed_us = elapsed_us,
                "Slow fill processing"
            );
        }

        // Send to observability (fire-and-forget)
        if let Some(ref sender) = self.obs_sender
            && sender.try_send(result.clone()).is_err()
        {
            trace!("Observability channel full, dropping fill result");
        }

        // Update metrics
        self.update_metrics(&result);

        result
    }

    /// Process a full fill from OrderFill.
    pub async fn process_order_fill(
        &mut self,
        fill: &OrderFill,
        event_id: String,
        token_id: String,
        outcome: Outcome,
        requested_size: Decimal,
    ) -> FillResult {
        let fill_data = FillData::from_full_fill(fill, event_id, token_id, outcome, requested_size);
        self.process_fill(fill_data).await
    }

    /// Process a partial fill from PartialOrderFill.
    pub async fn process_partial_fill(
        &mut self,
        fill: &PartialOrderFill,
        event_id: String,
        token_id: String,
        outcome: Outcome,
    ) -> FillResult {
        // Get previous cumulative size for this order
        let previous = self
            .partial_fills
            .get(&fill.order_id)
            .map(|s| s.cumulative_size)
            .unwrap_or(Decimal::ZERO);

        let sequence = self
            .partial_fills
            .get(&fill.order_id)
            .map(|s| s.fill_count + 1)
            .unwrap_or(1);

        let fill_data = FillData::from_partial_fill(
            fill,
            event_id,
            token_id,
            outcome,
            previous,
            sequence,
        );

        self.process_fill(fill_data).await
    }

    /// Track partial fill state.
    /// Returns true if shadow should be fired (first fill for this order).
    fn track_partial_fill(&mut self, fill: &FillData) -> bool {
        if !fill.is_partial {
            // Full fill - always fire shadow if enabled
            return self.config.enable_shadow;
        }

        let state = self
            .partial_fills
            .entry(fill.order_id.clone())
            .or_insert_with(|| PartialFillState::new(fill.order_id.clone()));

        // Update state
        state.update(fill.size, fill.price, fill.fee);

        // Fire shadow only on first partial fill
        let should_fire = self.config.enable_shadow && !state.shadow_fired;
        if should_fire {
            state.mark_shadow_fired();
        }

        // Clean up completed orders
        if fill.is_complete() {
            self.partial_fills.remove(&fill.order_id);
        }

        should_fire
    }

    /// Update inventory for a fill.
    fn update_inventory(&self, fill: &FillData) {
        // Look up condition_id from active window (needed for CTF redemption)
        let condition_id = self
            .state
            .market_data
            .active_windows
            .get(&fill.event_id)
            .map(|w| w.condition_id.clone())
            .unwrap_or_default();

        let mut position = self
            .state
            .market_data
            .get_inventory(&fill.event_id)
            .unwrap_or_else(|| InventoryPosition::new(fill.event_id.clone(), condition_id));

        let cost = fill.total_cost();

        match fill.outcome {
            Outcome::Yes => {
                position.add_yes(fill.size, cost);
            }
            Outcome::No => {
                position.add_no(fill.size, cost);
            }
        }

        self.state
            .market_data
            .update_inventory(&fill.event_id, position);

        debug!(
            event_id = %fill.event_id,
            outcome = ?fill.outcome,
            size = %fill.size,
            cost = %cost,
            "Inventory updated"
        );
    }

    /// Fire shadow bid for a fill.
    async fn fire_shadow(&self, fill: &FillData) -> (bool, Option<u64>, Option<String>) {
        let manager = match &self.shadow_manager {
            Some(m) if m.is_enabled() => m,
            _ => return (false, None, None),
        };

        let start = Instant::now();

        // Fire shadow for the opposite outcome
        let shadow_outcome = fill.outcome.opposite();

        match manager.fire(&fill.event_id, shadow_outcome) {
            Ok(result) => {
                let latency_us = start.elapsed().as_micros() as u64;

                // Log if we exceeded target
                if self.config.log_slow_shadows && latency_us > self.config.shadow_latency_target_us {
                    warn!(
                        event_id = %fill.event_id,
                        latency_us = latency_us,
                        target_us = self.config.shadow_latency_target_us,
                        "Shadow fire exceeded latency target"
                    );
                }

                info!(
                    event_id = %fill.event_id,
                    outcome = ?shadow_outcome,
                    latency_us = latency_us,
                    order_id = ?result.order_id,
                    "Shadow bid fired"
                );

                // Update metrics
                self.state
                    .metrics
                    .shadow_orders_fired
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

                (true, Some(latency_us), result.order_id)
            }
            Err(e) => {
                match &e {
                    ShadowError::NotFound { .. } => {
                        trace!(
                            event_id = %fill.event_id,
                            outcome = ?shadow_outcome,
                            "No shadow order to fire"
                        );
                    }
                    _ => {
                        warn!(
                            event_id = %fill.event_id,
                            outcome = ?shadow_outcome,
                            error = %e,
                            "Failed to fire shadow bid"
                        );
                    }
                }
                (false, None, None)
            }
        }
    }

    /// Create fill result with current inventory snapshot.
    fn create_result(
        &self,
        fill: FillData,
        shadow_fired: bool,
        shadow_latency_us: Option<u64>,
        shadow_order_id: Option<String>,
    ) -> FillResult {
        let inventory_after = self
            .state
            .market_data
            .get_inventory(&fill.event_id)
            .map(|pos| InventorySnapshot::from_position(&pos))
            .unwrap_or_else(|| InventorySnapshot {
                event_id: fill.event_id.clone(),
                yes_shares: Decimal::ZERO,
                no_shares: Decimal::ZERO,
                yes_cost_basis: Decimal::ZERO,
                no_cost_basis: Decimal::ZERO,
                total_exposure: Decimal::ZERO,
                matched_pairs: Decimal::ZERO,
            });

        FillResult {
            fill,
            shadow_fired,
            shadow_latency_us,
            shadow_order_id,
            inventory_after,
            processed_at: Utc::now(),
        }
    }

    /// Update metrics for a fill.
    fn update_metrics(&self, result: &FillResult) {
        // Track volume
        let volume_cents = (result.fill.total_cost() * Decimal::new(100, 0))
            .try_into()
            .unwrap_or(0u64);
        self.state.metrics.add_volume_cents(volume_cents);

        // Track P&L for matched pairs
        let matched = result.inventory_after.matched_pairs;
        if matched > Decimal::ZERO {
            // Each matched pair guarantees $1.00 at settlement
            // P&L = matched - cost_to_acquire
            let yes_cost_per = if result.inventory_after.yes_shares > Decimal::ZERO {
                result.inventory_after.yes_cost_basis / result.inventory_after.yes_shares
            } else {
                Decimal::ZERO
            };
            let no_cost_per = if result.inventory_after.no_shares > Decimal::ZERO {
                result.inventory_after.no_cost_basis / result.inventory_after.no_shares
            } else {
                Decimal::ZERO
            };

            let cost_per_pair = yes_cost_per + no_cost_per;
            let profit_per_pair = Decimal::ONE - cost_per_pair;
            let total_profit = matched * profit_per_pair;

            let pnl_cents = (total_profit * Decimal::new(100, 0))
                .try_into()
                .unwrap_or(0i64);

            // Only add PnL delta (avoid double counting)
            // This is a simplified approach - real implementation would track
            // PnL changes incrementally
            trace!(
                matched = %matched,
                cost_per_pair = %cost_per_pair,
                profit_per_pair = %profit_per_pair,
                pnl_cents = pnl_cents,
                "Matched pair P&L"
            );
        }
    }

    /// Clean up old partial fill state.
    pub fn cleanup_stale_partials(&mut self, max_age_secs: i64) {
        let cutoff = Utc::now() - chrono::Duration::seconds(max_age_secs);

        self.partial_fills.retain(|order_id, state| {
            if state.last_fill_at < cutoff {
                debug!(
                    order_id = %order_id,
                    last_fill_at = %state.last_fill_at,
                    "Cleaning up stale partial fill state"
                );
                false
            } else {
                true
            }
        });
    }

    /// Get partial fill state for an order.
    pub fn get_partial_state(&self, order_id: &str) -> Option<&PartialFillState> {
        self.partial_fills.get(order_id)
    }

    /// Get count of tracked partial fills.
    pub fn partial_fill_count(&self) -> usize {
        self.partial_fills.len()
    }
}

impl std::fmt::Debug for FillHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FillHandler")
            .field("config", &self.config)
            .field("shadow_enabled", &self.shadow_manager.is_some())
            .field("partial_fills", &self.partial_fills.len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn create_test_fill(order_id: &str, size: Decimal, is_partial: bool) -> FillData {
        FillData {
            order_id: order_id.to_string(),
            request_id: format!("req-{}", order_id),
            event_id: "event-1".to_string(),
            token_id: "token-1".to_string(),
            outcome: Outcome::Yes,
            size,
            price: dec!(0.50),
            fee: dec!(0.001),
            is_partial,
            cumulative_size: size,
            requested_size: dec!(100),
            timestamp: Utc::now(),
            sequence: 1,
        }
    }

    #[test]
    fn test_fill_data_total_cost() {
        let fill = create_test_fill("order-1", dec!(100), false);
        // cost = 100 * 0.50 + 0.001 = 50.001
        assert_eq!(fill.total_cost(), dec!(50.001));
    }

    #[test]
    fn test_fill_data_fill_ratio() {
        let mut fill = create_test_fill("order-1", dec!(50), true);
        fill.cumulative_size = dec!(50);
        fill.requested_size = dec!(100);
        // ratio = 50 / 100 = 0.5
        assert_eq!(fill.fill_ratio(), dec!(0.5));
    }

    #[test]
    fn test_fill_data_is_complete() {
        let mut fill = create_test_fill("order-1", dec!(100), false);
        assert!(fill.is_complete());

        fill.is_partial = true;
        fill.cumulative_size = dec!(50);
        fill.requested_size = dec!(100);
        assert!(!fill.is_complete());

        fill.cumulative_size = dec!(100);
        assert!(fill.is_complete());
    }

    #[test]
    fn test_partial_fill_state_update() {
        let mut state = PartialFillState::new("order-1".to_string());

        // First partial: 50 @ 0.48
        state.update(dec!(50), dec!(0.48), dec!(0.001));
        assert_eq!(state.cumulative_size, dec!(50));
        assert_eq!(state.avg_price, dec!(0.48));
        assert_eq!(state.fill_count, 1);

        // Second partial: 50 @ 0.52 (avg should be 0.50)
        state.update(dec!(50), dec!(0.52), dec!(0.001));
        assert_eq!(state.cumulative_size, dec!(100));
        assert_eq!(state.avg_price, dec!(0.50));
        assert_eq!(state.fill_count, 2);
        assert_eq!(state.total_fee, dec!(0.002));
    }

    #[test]
    fn test_partial_fill_state_shadow_fired() {
        let mut state = PartialFillState::new("order-1".to_string());
        assert!(!state.shadow_fired);

        state.mark_shadow_fired();
        assert!(state.shadow_fired);
    }

    #[test]
    fn test_fill_handler_config_default() {
        let config = FillHandlerConfig::default();
        assert!(config.enable_shadow);
        assert_eq!(config.shadow_latency_target_us, 2000);
        assert!(config.log_slow_shadows);
        assert_eq!(config.min_fill_size, dec!(0.01));
    }

    #[test]
    fn test_fill_handler_creation() {
        let config = FillHandlerConfig::default();
        let state = Arc::new(GlobalState::new());
        let handler = FillHandler::new(config, state, None);

        assert_eq!(handler.partial_fill_count(), 0);
    }

    #[test]
    fn test_fill_handler_track_partial() {
        let config = FillHandlerConfig::default();
        let state = Arc::new(GlobalState::new());
        let mut handler = FillHandler::new(config, state, None);

        // First partial - should fire shadow
        let fill1 = create_test_fill("order-1", dec!(50), true);
        let should_fire1 = handler.track_partial_fill(&fill1);
        assert!(should_fire1);
        assert_eq!(handler.partial_fill_count(), 1);

        // Second partial - should NOT fire shadow (already fired)
        let fill2 = create_test_fill("order-1", dec!(25), true);
        let should_fire2 = handler.track_partial_fill(&fill2);
        assert!(!should_fire2);

        // Check state
        let state = handler.get_partial_state("order-1").unwrap();
        assert_eq!(state.fill_count, 2);
        assert!(state.shadow_fired);
    }

    #[test]
    fn test_fill_handler_full_fill_always_fires_shadow() {
        let config = FillHandlerConfig::default();
        let state = Arc::new(GlobalState::new());
        let mut handler = FillHandler::new(config, state, None);

        let fill = create_test_fill("order-1", dec!(100), false);
        let should_fire = handler.track_partial_fill(&fill);
        assert!(should_fire);
    }

    #[test]
    fn test_fill_handler_cleanup_stale() {
        let config = FillHandlerConfig::default();
        let state = Arc::new(GlobalState::new());
        let mut handler = FillHandler::new(config, state, None);

        // Track a partial fill
        let fill = create_test_fill("order-1", dec!(50), true);
        handler.track_partial_fill(&fill);
        assert_eq!(handler.partial_fill_count(), 1);

        // Clean up with 0 max age (everything is stale)
        handler.cleanup_stale_partials(0);
        assert_eq!(handler.partial_fill_count(), 0);
    }

    #[tokio::test]
    async fn test_fill_handler_process_fill() {
        let config = FillHandlerConfig::default();
        let state = Arc::new(GlobalState::new());
        let mut handler = FillHandler::new(config, state.clone(), None);

        let fill = create_test_fill("order-1", dec!(100), false);
        let result = handler.process_fill(fill).await;

        assert_eq!(result.fill.order_id, "order-1");
        assert!(!result.shadow_fired); // No shadow manager

        // Check inventory was updated
        let inventory = state.market_data.get_inventory("event-1").unwrap();
        assert_eq!(inventory.yes_shares, dec!(100));
    }

    #[tokio::test]
    async fn test_fill_handler_inventory_update() {
        let config = FillHandlerConfig::default();
        let state = Arc::new(GlobalState::new());
        let mut handler = FillHandler::new(config, state.clone(), None);

        // YES fill
        let yes_fill = FillData {
            order_id: "order-1".to_string(),
            request_id: "req-1".to_string(),
            event_id: "event-1".to_string(),
            token_id: "token-yes".to_string(),
            outcome: Outcome::Yes,
            size: dec!(100),
            price: dec!(0.45),
            fee: dec!(0.01),
            is_partial: false,
            cumulative_size: dec!(100),
            requested_size: dec!(100),
            timestamp: Utc::now(),
            sequence: 1,
        };
        handler.process_fill(yes_fill).await;

        // NO fill
        let no_fill = FillData {
            order_id: "order-2".to_string(),
            request_id: "req-2".to_string(),
            event_id: "event-1".to_string(),
            token_id: "token-no".to_string(),
            outcome: Outcome::No,
            size: dec!(100),
            price: dec!(0.52),
            fee: dec!(0.01),
            is_partial: false,
            cumulative_size: dec!(100),
            requested_size: dec!(100),
            timestamp: Utc::now(),
            sequence: 1,
        };
        handler.process_fill(no_fill).await;

        // Check inventory
        let inventory = state.market_data.get_inventory("event-1").unwrap();
        assert_eq!(inventory.yes_shares, dec!(100));
        assert_eq!(inventory.no_shares, dec!(100));
        // YES cost: 100 * 0.45 + 0.01 = 45.01
        assert_eq!(inventory.yes_cost_basis, dec!(45.01));
        // NO cost: 100 * 0.52 + 0.01 = 52.01
        assert_eq!(inventory.no_cost_basis, dec!(52.01));
    }

    #[test]
    fn test_inventory_snapshot_from_position() {
        let mut pos = InventoryPosition::new("event-1".to_string(), "cond-1".to_string());
        pos.add_yes(dec!(100), dec!(45));
        pos.add_no(dec!(100), dec!(52));

        let snapshot = InventorySnapshot::from_position(&pos);
        assert_eq!(snapshot.yes_shares, dec!(100));
        assert_eq!(snapshot.no_shares, dec!(100));
        assert_eq!(snapshot.total_exposure, dec!(97));
        assert_eq!(snapshot.matched_pairs, dec!(100));
    }

    #[test]
    fn test_fill_data_from_order_fill() {
        let fill = OrderFill {
            request_id: "req-1".to_string(),
            order_id: "order-1".to_string(),
            size: dec!(100),
            price: dec!(0.50),
            fee: dec!(0.01),
            timestamp: Utc::now(),
        };

        let fill_data = FillData::from_full_fill(
            &fill,
            "event-1".to_string(),
            "token-1".to_string(),
            Outcome::Yes,
            dec!(100),
        );

        assert_eq!(fill_data.order_id, "order-1");
        assert!(!fill_data.is_partial);
        assert_eq!(fill_data.cumulative_size, dec!(100));
    }

    #[test]
    fn test_fill_data_from_partial_fill() {
        let fill = PartialOrderFill {
            request_id: "req-1".to_string(),
            order_id: "order-1".to_string(),
            requested_size: dec!(100),
            filled_size: dec!(75),
            avg_price: dec!(0.50),
            fee: dec!(0.01),
            timestamp: Utc::now(),
        };

        let fill_data = FillData::from_partial_fill(
            &fill,
            "event-1".to_string(),
            "token-1".to_string(),
            Outcome::Yes,
            dec!(50), // Previous cumulative
            2,
        );

        assert_eq!(fill_data.order_id, "order-1");
        assert!(fill_data.is_partial);
        assert_eq!(fill_data.size, dec!(25)); // Incremental: 75 - 50
        assert_eq!(fill_data.cumulative_size, dec!(75));
        assert_eq!(fill_data.sequence, 2);
    }

    #[tokio::test]
    async fn test_fill_handler_with_observability() {
        let config = FillHandlerConfig::default();
        let state = Arc::new(GlobalState::new());
        let (tx, mut rx) = mpsc::channel(10);

        let mut handler = FillHandler::new(config, state, None)
            .with_observability(tx);

        let fill = create_test_fill("order-1", dec!(100), false);
        handler.process_fill(fill).await;

        // Should receive fill result
        let result = rx.try_recv().unwrap();
        assert_eq!(result.fill.order_id, "order-1");
    }

    #[test]
    fn test_fill_result_serialization() {
        let fill = create_test_fill("order-1", dec!(100), false);
        let result = FillResult {
            fill,
            shadow_fired: true,
            shadow_latency_us: Some(1500),
            shadow_order_id: Some("shadow-1".to_string()),
            inventory_after: InventorySnapshot {
                event_id: "event-1".to_string(),
                yes_shares: dec!(100),
                no_shares: dec!(0),
                yes_cost_basis: dec!(50),
                no_cost_basis: dec!(0),
                total_exposure: dec!(50),
                matched_pairs: dec!(0),
            },
            processed_at: Utc::now(),
        };

        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("order-1"));
        assert!(json.contains("shadow_fired"));
    }
}
