//! Session lifecycle management for the React trading dashboard.
//!
//! This module handles creating and updating session records that track
//! bot runs. Each session has a unique UUID, config hash, and aggregated
//! metrics that are updated on shutdown.
//!
//! ## Usage
//!
//! ```ignore
//! // Create session manager on startup
//! let session_manager = SessionManager::new(
//!     BotMode::Paper,
//!     &bot_config,
//!     capture.clone(),
//! );
//!
//! // Capture initial session record
//! session_manager.start();
//!
//! // ... run trading ...
//!
//! // On shutdown, finalize with aggregated metrics
//! session_manager.end(ExitReason::Graceful, &state.metrics.snapshot());
//! ```

use std::collections::HashSet;
use std::sync::Arc;

use sha3::{Digest, Keccak256};
use tracing::{info, warn};
use uuid::Uuid;

use crate::config::BotConfig;
use crate::state::MetricsSnapshot;

use super::capture::SharedDashboardCapture;
use super::types::{BotMode, DashboardEvent, ExitReason, SessionRecord};

/// Session lifecycle manager.
///
/// Handles creation and update of session records for dashboard tracking.
/// Thread-safe and can be shared across tasks.
#[derive(Debug)]
pub struct SessionManager {
    /// The session record being tracked.
    record: SessionRecord,
    /// Dashboard capture channel for sending events.
    capture: Option<SharedDashboardCapture>,
    /// Markets that have been traded (tracked for session record).
    markets_traded: HashSet<String>,
}

impl SessionManager {
    /// Create a new session manager.
    ///
    /// Generates a new session UUID and computes config hash.
    /// Does NOT send the initial session record - call `start()` for that.
    ///
    /// # Arguments
    ///
    /// * `mode` - The bot operating mode (Live, Paper, Shadow, Backtest)
    /// * `config` - Bot configuration (used for config hash)
    /// * `capture` - Optional dashboard capture channel
    pub fn new(
        mode: BotMode,
        config: &BotConfig,
        capture: Option<SharedDashboardCapture>,
    ) -> Self {
        let config_hash = compute_config_hash(config);
        let record = SessionRecord::new(mode, config_hash);

        info!(
            session_id = %record.session_id,
            mode = %mode,
            "Created new session"
        );

        Self {
            record,
            capture,
            markets_traded: HashSet::new(),
        }
    }

    /// Create a session manager without capture (for testing).
    pub fn new_without_capture(mode: BotMode, config: &BotConfig) -> Self {
        Self::new(mode, config, None)
    }

    /// Get the session ID.
    pub fn session_id(&self) -> Uuid {
        self.record.session_id
    }

    /// Get the bot mode.
    pub fn mode(&self) -> BotMode {
        self.record.mode
    }

    /// Get the config hash.
    pub fn config_hash(&self) -> &str {
        &self.record.config_hash
    }

    /// Start the session by sending the initial session record.
    ///
    /// This should be called once when the bot starts trading.
    pub fn start(&self) {
        info!(
            session_id = %self.record.session_id,
            mode = %self.record.mode,
            config_hash = %self.record.config_hash,
            "Starting session"
        );

        self.send_session_event();
    }

    /// Record that a market was traded.
    ///
    /// This tracks unique markets for the session summary.
    pub fn record_market_traded(&mut self, event_id: &str) {
        self.markets_traded.insert(event_id.to_string());
    }

    /// End the session with final metrics.
    ///
    /// Updates the session record with aggregated metrics and exit reason,
    /// then sends the final session event to ClickHouse.
    ///
    /// # Arguments
    ///
    /// * `reason` - Why the session ended (Graceful, Crash, CircuitBreaker, Manual)
    /// * `metrics` - Final metrics snapshot from GlobalState
    pub fn end(&mut self, reason: ExitReason, metrics: &MetricsSnapshot) {
        // Update record with exit info
        self.record.end(reason);

        // Update aggregated metrics from GlobalState
        self.record.total_pnl = metrics.pnl_usdc;
        self.record.total_volume = metrics.volume_usdc;
        self.record.trades_executed = metrics.trades_executed as u32;
        self.record.trades_failed = metrics.trades_failed as u32;
        self.record.trades_skipped = metrics.trades_skipped as u32;
        self.record.opportunities_detected = metrics.opportunities_detected;
        self.record.events_processed = metrics.events_processed;
        self.record.markets_traded = self.markets_traded.iter().cloned().collect();

        info!(
            session_id = %self.record.session_id,
            exit_reason = %reason,
            duration_secs = self.record.duration_secs(),
            total_pnl = %self.record.total_pnl,
            trades_executed = self.record.trades_executed,
            "Ending session"
        );

        // Send final session event
        self.send_session_event();
    }

    /// Send the current session record to the capture channel.
    fn send_session_event(&self) {
        if let Some(ref capture) = self.capture
            && capture.is_enabled()
        {
            let event = DashboardEvent::Session(self.record.clone());
            capture.try_capture(event);
        }
    }

    /// Check if the session is still running.
    pub fn is_running(&self) -> bool {
        self.record.is_running()
    }

    /// Get the session duration in seconds.
    pub fn duration_secs(&self) -> i64 {
        self.record.duration_secs()
    }

    /// Get a clone of the current session record (for inspection).
    pub fn record(&self) -> SessionRecord {
        self.record.clone()
    }
}

/// Compute a Keccak256 hash of the relevant config fields.
///
/// This hash is used to track config changes across sessions and
/// enable reproducibility analysis.
fn compute_config_hash(config: &BotConfig) -> String {
    let mut hasher = Keccak256::new();

    // Include trading mode
    hasher.update(config.mode.to_string().as_bytes());

    // Include assets
    for asset in &config.assets {
        hasher.update(asset.as_bytes());
    }

    // Include key trading parameters
    hasher.update(config.trading.min_margin_early.to_string().as_bytes());
    hasher.update(config.trading.min_margin_mid.to_string().as_bytes());
    hasher.update(config.trading.min_margin_late.to_string().as_bytes());
    hasher.update(config.trading.max_position_per_market.to_string().as_bytes());
    hasher.update(config.trading.max_total_exposure.to_string().as_bytes());
    hasher.update(config.trading.base_order_size.to_string().as_bytes());

    // Include risk parameters
    hasher.update(config.risk.max_consecutive_failures.to_string().as_bytes());
    hasher.update(config.risk.max_daily_loss.to_string().as_bytes());
    hasher.update(config.risk.max_imbalance_ratio.to_string().as_bytes());

    // Include shadow bid config
    hasher.update(if config.shadow.enabled { b"1" } else { b"0" });
    hasher.update(config.shadow.price_offset_bps.to_string().as_bytes());

    // Include engine config
    hasher.update(if config.engines.arbitrage.enabled { b"1" } else { b"0" });
    hasher.update(if config.engines.directional.enabled { b"1" } else { b"0" });
    hasher.update(if config.engines.maker.enabled { b"1" } else { b"0" });

    // Return first 16 hex characters (64 bits, enough for tracking)
    let result = hasher.finalize();
    hex::encode(&result[..8])
}

/// Shared session manager for use across async tasks.
pub type SharedSessionManager = Arc<std::sync::RwLock<SessionManager>>;

/// Create a shared session manager.
pub fn create_shared_session_manager(
    mode: BotMode,
    config: &BotConfig,
    capture: Option<SharedDashboardCapture>,
) -> SharedSessionManager {
    Arc::new(std::sync::RwLock::new(SessionManager::new(
        mode, config, capture,
    )))
}

/// Helper to end a shared session manager.
///
/// Handles the lock and logs any errors.
pub fn end_shared_session(
    session: &SharedSessionManager,
    reason: ExitReason,
    metrics: &MetricsSnapshot,
) {
    match session.write() {
        Ok(mut session) => {
            session.end(reason, metrics);
        }
        Err(e) => {
            warn!("Failed to acquire session lock for shutdown: {}", e);
        }
    }
}

/// Helper to record a market traded on shared session manager.
pub fn record_market_on_session(session: &SharedSessionManager, event_id: &str) {
    match session.write() {
        Ok(mut session) => {
            session.record_market_traded(event_id);
        }
        Err(e) => {
            warn!("Failed to acquire session lock to record market: {}", e);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn make_test_config() -> BotConfig {
        BotConfig::default()
    }

    fn make_test_metrics() -> MetricsSnapshot {
        MetricsSnapshot {
            pnl_usdc: dec!(123.45),
            volume_usdc: dec!(5000.00),
            trades_executed: 42,
            trades_failed: 3,
            trades_skipped: 15,
            opportunities_detected: 100,
            events_processed: 10000,
            shadow_orders_fired: 5,
            shadow_orders_filled: 3,
            allocated_balance: dec!(1000.00),
            current_balance: dec!(1000.00),
        }
    }

    #[test]
    fn test_session_manager_new() {
        let config = make_test_config();
        let manager = SessionManager::new_without_capture(BotMode::Paper, &config);

        assert!(manager.is_running());
        assert_eq!(manager.mode(), BotMode::Paper);
        assert!(!manager.config_hash().is_empty());
        assert_eq!(manager.config_hash().len(), 16); // 8 bytes = 16 hex chars
    }

    #[test]
    fn test_session_manager_session_id() {
        let config = make_test_config();
        let manager = SessionManager::new_without_capture(BotMode::Live, &config);

        // Session ID should be a valid UUID
        let session_id = manager.session_id();
        assert_ne!(session_id, Uuid::nil());
    }

    #[test]
    fn test_session_manager_start() {
        let config = make_test_config();
        let manager = SessionManager::new_without_capture(BotMode::Paper, &config);

        // Just verify it doesn't panic without capture
        manager.start();
        assert!(manager.is_running());
    }

    #[test]
    fn test_session_manager_end() {
        let config = make_test_config();
        let mut manager = SessionManager::new_without_capture(BotMode::Paper, &config);
        let metrics = make_test_metrics();

        manager.start();
        manager.record_market_traded("event-123");
        manager.record_market_traded("event-456");
        manager.end(ExitReason::Graceful, &metrics);

        assert!(!manager.is_running());
        let record = manager.record();
        assert_eq!(record.exit_reason, Some(ExitReason::Graceful));
        assert_eq!(record.total_pnl, dec!(123.45));
        assert_eq!(record.trades_executed, 42);
        assert_eq!(record.markets_traded.len(), 2);
        assert!(record.markets_traded.contains(&"event-123".to_string()));
    }

    #[test]
    fn test_session_manager_duration() {
        let config = make_test_config();
        let manager = SessionManager::new_without_capture(BotMode::Paper, &config);

        // Duration should be >= 0
        assert!(manager.duration_secs() >= 0);
    }

    #[test]
    fn test_compute_config_hash_deterministic() {
        let config = make_test_config();

        let hash1 = compute_config_hash(&config);
        let hash2 = compute_config_hash(&config);

        assert_eq!(hash1, hash2);
        assert_eq!(hash1.len(), 16);
    }

    #[test]
    fn test_compute_config_hash_different_configs() {
        let config1 = make_test_config();
        let mut config2 = make_test_config();

        config2.trading.max_position_per_market = dec!(999999);

        let hash1 = compute_config_hash(&config1);
        let hash2 = compute_config_hash(&config2);

        assert_ne!(hash1, hash2);
    }

    #[test]
    fn test_compute_config_hash_different_modes() {
        let mut config1 = make_test_config();
        let mut config2 = make_test_config();

        config1.mode = crate::config::TradingMode::Paper;
        config2.mode = crate::config::TradingMode::Live;

        let hash1 = compute_config_hash(&config1);
        let hash2 = compute_config_hash(&config2);

        assert_ne!(hash1, hash2);
    }

    #[test]
    fn test_record_market_traded() {
        let config = make_test_config();
        let mut manager = SessionManager::new_without_capture(BotMode::Paper, &config);

        // Recording same market twice should only count once
        manager.record_market_traded("event-123");
        manager.record_market_traded("event-123");
        manager.record_market_traded("event-456");

        assert_eq!(manager.markets_traded.len(), 2);
    }

    #[test]
    fn test_create_shared_session_manager() {
        let config = make_test_config();
        let session = create_shared_session_manager(BotMode::Paper, &config, None);

        {
            let guard = session.read().unwrap();
            assert!(guard.is_running());
            assert_eq!(guard.mode(), BotMode::Paper);
        }
    }

    #[test]
    fn test_end_shared_session() {
        let config = make_test_config();
        let metrics = make_test_metrics();
        let session = create_shared_session_manager(BotMode::Paper, &config, None);

        end_shared_session(&session, ExitReason::Manual, &metrics);

        let guard = session.read().unwrap();
        assert!(!guard.is_running());
        assert_eq!(guard.record().exit_reason, Some(ExitReason::Manual));
    }

    #[test]
    fn test_record_market_on_session() {
        let config = make_test_config();
        let session = create_shared_session_manager(BotMode::Paper, &config, None);

        record_market_on_session(&session, "event-789");

        let guard = session.read().unwrap();
        assert!(guard.markets_traded.contains("event-789"));
    }

    #[test]
    fn test_exit_reasons() {
        let config = make_test_config();
        let metrics = make_test_metrics();

        // Test each exit reason
        for reason in [
            ExitReason::Graceful,
            ExitReason::Crash,
            ExitReason::CircuitBreaker,
            ExitReason::Manual,
        ] {
            let mut manager = SessionManager::new_without_capture(BotMode::Paper, &config);
            manager.end(reason, &metrics);
            assert_eq!(manager.record().exit_reason, Some(reason));
        }
    }

    #[test]
    fn test_session_with_capture() {
        use super::super::capture::{create_shared_dashboard_capture, DashboardCaptureConfig};

        let config = make_test_config();
        let (capture, mut receiver) = create_shared_dashboard_capture(
            DashboardCaptureConfig::test_config(100),
        );

        let manager = SessionManager::new(BotMode::Paper, &config, Some(capture));
        manager.start();

        // Should have received the session event
        let receiver = receiver.as_mut().unwrap();
        let event = receiver.try_recv();
        assert!(event.is_ok());
        assert_eq!(event.unwrap().event_type(), "session");
    }

    #[test]
    fn test_session_manager_record_clone() {
        let config = make_test_config();
        let manager = SessionManager::new_without_capture(BotMode::Shadow, &config);

        let record = manager.record();
        assert_eq!(record.mode, BotMode::Shadow);
        assert_eq!(record.session_id, manager.session_id());
    }
}
