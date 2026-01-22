//! Dashboard integration helpers for mode files.
//!
//! This module provides utilities for integrating dashboard capture with
//! the trading modes (live, paper, shadow). It handles:
//!
//! - Setting up the dashboard capture channel and processor
//! - P&L snapshot timer for equity curve visualization
//! - Trade event capture from the strategy loop
//!
//! ## Usage
//!
//! ```ignore
//! let dashboard = DashboardIntegration::new(config, clickhouse, shutdown_tx).await?;
//! dashboard.capture_pnl_snapshot(&state, session_id);
//! ```

use std::sync::Arc;
use std::time::Duration;

use rust_decimal::Decimal;
use tokio::sync::broadcast;
use tracing::{debug, info, warn};
use uuid::Uuid;

use poly_common::ClickHouseClient;

use crate::config::DashboardConfig;
use crate::state::GlobalState;

use super::{
    create_shared_dashboard_capture, DashboardCaptureReceiver, DashboardEvent,
    DashboardProcessorConfig, PnlSnapshot, PnlTrigger, SharedDashboardCapture,
};

/// Dashboard integration context for trading modes.
///
/// Manages the dashboard capture channel, processor, and P&L snapshot timer.
pub struct DashboardIntegration {
    /// Shared capture context for sending events.
    capture: SharedDashboardCapture,
    /// Configuration.
    config: DashboardConfig,
    /// Session ID for this run.
    session_id: Uuid,
    /// Peak P&L for drawdown calculation.
    peak_pnl: Decimal,
    /// Maximum drawdown seen.
    max_drawdown: Decimal,
    /// Background task handles.
    task_handles: Vec<tokio::task::JoinHandle<()>>,
}

impl DashboardIntegration {
    /// Create a new dashboard integration.
    ///
    /// Sets up the capture channel and processor if dashboard is enabled.
    /// Returns None if dashboard is disabled.
    pub async fn new(
        config: &DashboardConfig,
        clickhouse: Option<&ClickHouseClient>,
        shutdown_tx: &broadcast::Sender<()>,
        session_id: Uuid,
    ) -> Option<Self> {
        if !config.enabled {
            debug!("Dashboard capture disabled");
            return None;
        }

        // Create capture channel
        let (capture, receiver) = create_shared_dashboard_capture(config.capture_config());

        let mut task_handles = Vec::new();

        // Start processor if we have ClickHouse and a receiver
        if let (Some(client), Some(receiver)) = (clickhouse, receiver) {
            let processor_handle = Self::start_processor(
                config.processor_config(),
                receiver,
                client.clone(),
                shutdown_tx.subscribe(),
            );
            task_handles.push(processor_handle);
        }

        info!(
            session_id = %session_id,
            pnl_interval_secs = config.pnl_snapshot_interval_secs,
            "Dashboard integration initialized"
        );

        Some(Self {
            capture,
            config: config.clone(),
            session_id,
            peak_pnl: Decimal::ZERO,
            max_drawdown: Decimal::ZERO,
            task_handles,
        })
    }

    /// Start the dashboard processor as a background task.
    fn start_processor(
        config: DashboardProcessorConfig,
        receiver: DashboardCaptureReceiver,
        client: ClickHouseClient,
        shutdown_rx: broadcast::Receiver<()>,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let processor = super::DashboardProcessor::new(config);
            processor.run(receiver, client, shutdown_rx).await;
        })
    }

    /// Start the P&L snapshot timer as a background task.
    ///
    /// Captures periodic P&L snapshots at the configured interval.
    pub fn start_pnl_timer(
        &self,
        state: Arc<GlobalState>,
        shutdown_rx: broadcast::Receiver<()>,
    ) -> tokio::task::JoinHandle<()> {
        let capture = self.capture.clone();
        let interval_secs = self.config.pnl_snapshot_interval_secs;
        let session_id = self.session_id;

        tokio::spawn(async move {
            Self::pnl_timer_loop(capture, state, session_id, interval_secs, shutdown_rx).await;
        })
    }

    /// P&L timer loop that captures periodic snapshots.
    async fn pnl_timer_loop(
        capture: SharedDashboardCapture,
        state: Arc<GlobalState>,
        session_id: Uuid,
        interval_secs: u64,
        mut shutdown_rx: broadcast::Receiver<()>,
    ) {
        let mut interval = tokio::time::interval(Duration::from_secs(interval_secs));
        let mut peak_pnl = Decimal::ZERO;
        let mut max_drawdown = Decimal::ZERO;

        loop {
            tokio::select! {
                _ = interval.tick() => {
                    let snapshot = Self::create_pnl_snapshot(
                        &state,
                        session_id,
                        PnlTrigger::Periodic,
                        &mut peak_pnl,
                        &mut max_drawdown,
                    );
                    capture.try_capture(DashboardEvent::Pnl(snapshot));
                }
                _ = shutdown_rx.recv() => {
                    debug!("P&L timer received shutdown signal");
                    break;
                }
            }
        }
    }

    /// Create a P&L snapshot from current state.
    fn create_pnl_snapshot(
        state: &GlobalState,
        session_id: Uuid,
        trigger: PnlTrigger,
        peak_pnl: &mut Decimal,
        max_drawdown: &mut Decimal,
    ) -> PnlSnapshot {
        let metrics = state.metrics.snapshot();

        // Calculate exposures from inventory
        let (yes_exposure, no_exposure) = Self::calculate_exposures(&state.market_data.inventory);
        let total_exposure = yes_exposure + no_exposure;

        // Use realized P&L from metrics as total (unrealized calculation needs market prices)
        let realized_pnl = metrics.pnl_usdc;
        let unrealized_pnl = Decimal::ZERO; // Would need current market prices
        let total_pnl = realized_pnl + unrealized_pnl;

        // Update peak and drawdown
        if total_pnl > *peak_pnl {
            *peak_pnl = total_pnl;
        }
        let current_drawdown = *peak_pnl - total_pnl;
        if current_drawdown > *max_drawdown {
            *max_drawdown = current_drawdown;
        }

        PnlSnapshot {
            session_id,
            timestamp: chrono::Utc::now(),
            trigger,
            realized_pnl,
            unrealized_pnl,
            total_pnl,
            total_exposure,
            yes_exposure,
            no_exposure,
            cumulative_volume: metrics.volume_usdc,
            cumulative_fees: Decimal::ZERO, // Would need to track fees
            trade_count: metrics.trades_executed as u32,
            max_drawdown: *max_drawdown,
            current_drawdown,
        }
    }

    /// Calculate YES and NO exposures from inventory state.
    fn calculate_exposures(
        inventory: &dashmap::DashMap<String, crate::state::InventoryPosition>,
    ) -> (Decimal, Decimal) {
        let mut yes_exposure = Decimal::ZERO;
        let mut no_exposure = Decimal::ZERO;

        for entry in inventory.iter() {
            let pos = entry.value();
            yes_exposure += pos.yes_shares * Decimal::new(5, 1); // Approximate at 0.50
            no_exposure += pos.no_shares * Decimal::new(5, 1);
        }

        (yes_exposure, no_exposure)
    }

    /// Get the capture context for manual event capture.
    pub fn capture(&self) -> &SharedDashboardCapture {
        &self.capture
    }

    /// Capture a P&L snapshot manually (e.g., after a trade).
    pub fn capture_pnl_snapshot(&mut self, state: &GlobalState, trigger: PnlTrigger) {
        let snapshot = Self::create_pnl_snapshot(
            state,
            self.session_id,
            trigger,
            &mut self.peak_pnl,
            &mut self.max_drawdown,
        );
        self.capture.try_capture(DashboardEvent::Pnl(snapshot));
    }

    /// Shutdown and wait for background tasks.
    pub async fn shutdown(self, timeout: Duration) {
        for handle in self.task_handles {
            match tokio::time::timeout(timeout, handle).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => warn!("Dashboard task panicked: {}", e),
                Err(_) => warn!("Dashboard task timed out during shutdown"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_calculate_exposures_empty() {
        let inventory = dashmap::DashMap::new();
        let (yes, no) = DashboardIntegration::calculate_exposures(&inventory);
        assert_eq!(yes, Decimal::ZERO);
        assert_eq!(no, Decimal::ZERO);
    }

    #[test]
    fn test_calculate_exposures_with_positions() {
        use crate::state::InventoryPosition;

        let inventory = dashmap::DashMap::new();
        inventory.insert(
            "event-1".to_string(),
            InventoryPosition {
                event_id: "event-1".to_string(),
                condition_id: "cond-1".to_string(),
                yes_shares: Decimal::new(100, 0),
                no_shares: Decimal::new(50, 0),
                yes_cost_basis: Decimal::new(45, 0),
                no_cost_basis: Decimal::new(25, 0),
                realized_pnl: Decimal::ZERO,
            },
        );

        let (yes, no) = DashboardIntegration::calculate_exposures(&inventory);
        // 100 * 0.5 = 50
        assert_eq!(yes, Decimal::new(50, 0));
        // 50 * 0.5 = 25
        assert_eq!(no, Decimal::new(25, 0));
    }

    #[tokio::test]
    async fn test_dashboard_integration_disabled() {
        let config = DashboardConfig::disabled();
        let (shutdown_tx, _) = broadcast::channel(1);
        let session_id = Uuid::new_v4();

        let integration =
            DashboardIntegration::new(&config, None, &shutdown_tx, session_id).await;

        assert!(integration.is_none());
    }

    #[tokio::test]
    async fn test_dashboard_integration_enabled_no_clickhouse() {
        let config = DashboardConfig::default();
        let (shutdown_tx, _) = broadcast::channel(1);
        let session_id = Uuid::new_v4();

        let integration =
            DashboardIntegration::new(&config, None, &shutdown_tx, session_id).await;

        assert!(integration.is_some());
        let integration = integration.unwrap();
        assert!(integration.capture.is_enabled());
    }

    #[test]
    fn test_create_pnl_snapshot() {
        let state = GlobalState::new();
        let session_id = Uuid::new_v4();
        let mut peak_pnl = Decimal::ZERO;
        let mut max_drawdown = Decimal::ZERO;

        let snapshot = DashboardIntegration::create_pnl_snapshot(
            &state,
            session_id,
            PnlTrigger::Periodic,
            &mut peak_pnl,
            &mut max_drawdown,
        );

        assert_eq!(snapshot.session_id, session_id);
        assert_eq!(snapshot.trigger, PnlTrigger::Periodic);
        assert_eq!(snapshot.realized_pnl, Decimal::ZERO);
        assert_eq!(snapshot.trade_count, 0);
    }

    #[test]
    fn test_drawdown_calculation() {
        let state = GlobalState::new();
        let session_id = Uuid::new_v4();
        let mut peak_pnl = Decimal::new(100, 0); // Peak of $100
        let mut max_drawdown = Decimal::new(20, 0); // Previous max drawdown

        // With P&L at 0 (from empty state), drawdown should be 100
        let snapshot = DashboardIntegration::create_pnl_snapshot(
            &state,
            session_id,
            PnlTrigger::Periodic,
            &mut peak_pnl,
            &mut max_drawdown,
        );

        assert_eq!(snapshot.current_drawdown, Decimal::new(100, 0));
        assert_eq!(snapshot.max_drawdown, Decimal::new(100, 0)); // Updated from 20 to 100
    }
}
