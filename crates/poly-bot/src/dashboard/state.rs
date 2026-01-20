//! Dashboard state for WebSocket broadcast.
//!
//! This module provides the `DashboardState` struct which aggregates all
//! trading state into a single serializable snapshot for the React dashboard.
//!
//! ## Performance Requirements
//!
//! - `snapshot()` must not acquire any Mutex locks
//! - Uses DashMap iteration and atomic reads (lock-free)
//! - Designed for ~500ms broadcast intervals

use std::sync::Arc;
use std::sync::atomic::Ordering;

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::state::{
    ActiveWindow, ControlFlags, GlobalState, InventoryPosition, InventoryState, LiveOrderBook,
    MetricsCounters, MetricsSnapshot,
};
use crate::dashboard::types::{LogEntry, TradeRecord};

// ============================================================================
// Main Dashboard State
// ============================================================================

/// Complete dashboard state for WebSocket broadcast.
///
/// This is the top-level struct sent to the React dashboard via WebSocket.
/// It aggregates all relevant trading state into a single JSON message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DashboardState {
    /// Snapshot timestamp.
    pub timestamp: DateTime<Utc>,

    /// Current metrics (P&L, trades, volume, etc.).
    pub metrics: MetricsSnapshotJson,

    /// Active markets with order book data.
    pub markets: Vec<ActiveMarketState>,

    /// Recent trades (last 100).
    pub recent_trades: Vec<TradeRecord>,

    /// Current positions by market.
    pub positions: Vec<PositionState>,

    /// Control flags (trading enabled, circuit breaker).
    pub control: ControlState,

    /// Recent logs (last 100).
    pub recent_logs: Vec<LogEntry>,

    /// Active anomalies.
    pub anomalies: Vec<AnomalyState>,
}

impl DashboardState {
    /// Create an empty dashboard state.
    pub fn empty() -> Self {
        Self {
            timestamp: Utc::now(),
            metrics: MetricsSnapshotJson::default(),
            markets: Vec::new(),
            recent_trades: Vec::new(),
            positions: Vec::new(),
            control: ControlState::default(),
            recent_logs: Vec::new(),
            anomalies: Vec::new(),
        }
    }

    /// Create a snapshot from GlobalState.
    ///
    /// This method is lock-free - it only uses DashMap iteration and atomic reads.
    pub fn from_global_state(state: &GlobalState) -> Self {
        Self {
            timestamp: Utc::now(),
            metrics: MetricsSnapshotJson::from_counters(&state.metrics),
            markets: Self::collect_markets(
                &state.market_data.active_windows,
                &state.market_data.order_books,
                &state.market_data.spot_prices,
                &state.market_data.confidence_snapshots,
            ),
            recent_trades: state.market_data.get_recent_trades(), // From GlobalState
            positions: Self::collect_positions(&state.market_data.inventory),
            control: ControlState::from_flags(&state.control),
            recent_logs: Vec::new(), // Populated separately via capture channel
            anomalies: Vec::new(), // Populated separately via anomaly detection
        }
    }

    /// Collect active markets with their order book state.
    fn collect_markets(
        windows: &dashmap::DashMap<String, ActiveWindow>,
        order_books: &dashmap::DashMap<String, LiveOrderBook>,
        spot_prices: &dashmap::DashMap<String, (Decimal, i64)>,
        confidence_snapshots: &dashmap::DashMap<String, crate::state::ConfidenceSnapshot>,
    ) -> Vec<ActiveMarketState> {
        let mut markets = Vec::new();

        for window_ref in windows.iter() {
            let window = window_ref.value();

            // Get order books for YES and NO tokens
            let yes_book = order_books.get(&window.yes_token_id).map(|r| r.clone());
            let no_book = order_books.get(&window.no_token_id).map(|r| r.clone());

            // Get current spot price
            let spot_price = spot_prices
                .get(window.asset.as_str())
                .map(|r| r.0)
                .unwrap_or(Decimal::ZERO);

            // Calculate arb spread if we have both books
            let (arb_spread, has_arb) = match (&yes_book, &no_book) {
                (Some(yes), Some(no)) if yes.best_ask > Decimal::ZERO && no.best_ask > Decimal::ZERO => {
                    let combined = yes.best_ask + no.best_ask;
                    let spread = Decimal::ONE - combined;
                    (spread, spread > Decimal::ZERO)
                }
                _ => (Decimal::ZERO, false),
            };

            // Get confidence snapshot if available
            let confidence = confidence_snapshots
                .get(&window.event_id)
                .map(|snap| ConfidenceData {
                    confidence: snap.confidence,
                    time_confidence: snap.time_confidence,
                    distance_confidence: snap.distance_confidence,
                    threshold: snap.threshold,
                    ev: snap.ev,
                    would_trade: snap.would_trade,
                    distance_dollars: snap.distance_dollars,
                    atr_multiple: snap.atr_multiple,
                    favorable_price: snap.favorable_price,
                });

            markets.push(ActiveMarketState {
                event_id: window.event_id.clone(),
                asset: window.asset.as_str().to_string(),
                strike_price: window.strike_price,
                window_end: window.window_end,
                seconds_remaining: window.seconds_remaining(),
                spot_price,
                yes_book: yes_book.map(OrderBookSummary::from),
                no_book: no_book.map(OrderBookSummary::from),
                arb_spread,
                has_arb_opportunity: has_arb,
                confidence,
            });
        }

        // Sort by seconds remaining (most urgent first)
        markets.sort_by(|a, b| a.seconds_remaining.cmp(&b.seconds_remaining));
        markets
    }

    /// Collect current positions.
    fn collect_positions(
        inventory: &dashmap::DashMap<String, InventoryPosition>,
    ) -> Vec<PositionState> {
        inventory
            .iter()
            .filter(|r| r.value().yes_shares > Decimal::ZERO || r.value().no_shares > Decimal::ZERO)
            .map(|r| PositionState::from(r.value().clone()))
            .collect()
    }

    /// Add a recent trade.
    pub fn add_trade(&mut self, trade: TradeRecord) {
        self.recent_trades.insert(0, trade);
        if self.recent_trades.len() > 100 {
            self.recent_trades.truncate(100);
        }
    }

    /// Add a recent log entry.
    pub fn add_log(&mut self, log: LogEntry) {
        self.recent_logs.insert(0, log);
        if self.recent_logs.len() > 100 {
            self.recent_logs.truncate(100);
        }
    }

    /// Add an anomaly.
    pub fn add_anomaly(&mut self, anomaly: AnomalyState) {
        self.anomalies.push(anomaly);
    }

    /// Clear dismissed anomalies.
    pub fn clear_dismissed_anomalies(&mut self) {
        self.anomalies.retain(|a| !a.dismissed);
    }
}

// ============================================================================
// Metrics Snapshot (JSON-friendly)
// ============================================================================

/// Metrics snapshot with JSON-friendly types.
///
/// This wraps MetricsSnapshot but uses f64 for Decimals since JSON
/// doesn't have native decimal support and JavaScript uses float64.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MetricsSnapshotJson {
    /// Total events processed.
    pub events_processed: u64,

    /// Total arbitrage opportunities detected.
    pub opportunities_detected: u64,

    /// Total trades executed.
    pub trades_executed: u64,

    /// Total trades failed.
    pub trades_failed: u64,

    /// Total trades skipped.
    pub trades_skipped: u64,

    /// Total P&L in USDC (as string for precision).
    pub pnl_usdc: String,

    /// Total volume in USDC (as string for precision).
    pub volume_usdc: String,

    /// Shadow orders fired.
    pub shadow_orders_fired: u64,

    /// Shadow orders filled.
    pub shadow_orders_filled: u64,

    /// Win rate (0.0-1.0).
    pub win_rate: Option<f64>,

    /// Allocated balance for the bot (configured trading capital) in USDC.
    pub allocated_balance: String,

    /// Current/real account balance in USDC.
    pub current_balance: String,
}

impl MetricsSnapshotJson {
    /// Create from MetricsCounters (lock-free atomic reads).
    pub fn from_counters(counters: &MetricsCounters) -> Self {
        let snapshot = counters.snapshot();
        Self::from(snapshot)
    }
}

impl From<MetricsSnapshot> for MetricsSnapshotJson {
    fn from(m: MetricsSnapshot) -> Self {
        let total_trades = m.trades_executed + m.trades_failed;
        let win_rate = if total_trades > 0 {
            // Simplified win rate: successful trades / total
            Some(m.trades_executed as f64 / total_trades as f64)
        } else {
            None
        };

        Self {
            events_processed: m.events_processed,
            opportunities_detected: m.opportunities_detected,
            trades_executed: m.trades_executed,
            trades_failed: m.trades_failed,
            trades_skipped: m.trades_skipped,
            pnl_usdc: m.pnl_usdc.to_string(),
            volume_usdc: m.volume_usdc.to_string(),
            shadow_orders_fired: m.shadow_orders_fired,
            shadow_orders_filled: m.shadow_orders_filled,
            win_rate,
            allocated_balance: m.allocated_balance.to_string(),
            current_balance: m.current_balance.to_string(),
        }
    }
}

// ============================================================================
// Active Market State
// ============================================================================

/// State of an active trading market.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActiveMarketState {
    /// Event ID.
    pub event_id: String,

    /// Asset symbol (BTC, ETH, etc.).
    pub asset: String,

    /// Strike price.
    pub strike_price: Decimal,

    /// Window end time.
    pub window_end: DateTime<Utc>,

    /// Seconds remaining in window.
    pub seconds_remaining: i64,

    /// Current spot price.
    pub spot_price: Decimal,

    /// YES token order book summary.
    pub yes_book: Option<OrderBookSummary>,

    /// NO token order book summary.
    pub no_book: Option<OrderBookSummary>,

    /// Arbitrage spread (1.0 - YES_ask - NO_ask).
    /// Positive = profitable arb opportunity.
    pub arb_spread: Decimal,

    /// Whether there's currently an arb opportunity.
    pub has_arb_opportunity: bool,

    /// Confidence data for chart display.
    pub confidence: Option<ConfidenceData>,
}

/// Confidence data for dashboard chart display.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfidenceData {
    /// Current combined confidence (0.0 to 1.0).
    pub confidence: Decimal,
    /// Time confidence component (0.0 to 1.0).
    pub time_confidence: Decimal,
    /// Distance confidence component (0.0 to 1.0).
    pub distance_confidence: Decimal,
    /// Current threshold for trading (min_edge, decays over time).
    pub threshold: Decimal,
    /// Expected value (confidence - favorable_price).
    pub ev: Decimal,
    /// Whether this would qualify for a trade.
    pub would_trade: bool,
    /// Distance from strike in dollars.
    pub distance_dollars: Decimal,
    /// ATR multiple (distance / ATR).
    pub atr_multiple: Decimal,
    /// Favorable price for the dominant side.
    pub favorable_price: Decimal,
}

// ============================================================================
// Order Book Summary
// ============================================================================

/// Summary of an order book for dashboard display.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderBookSummary {
    /// Token ID.
    pub token_id: String,

    /// Best bid price.
    pub best_bid: Decimal,

    /// Best bid size.
    pub best_bid_size: Decimal,

    /// Best ask price.
    pub best_ask: Decimal,

    /// Best ask size.
    pub best_ask_size: Decimal,

    /// Spread in basis points.
    pub spread_bps: u32,

    /// Last update timestamp (ms).
    pub last_update_ms: i64,
}

impl From<LiveOrderBook> for OrderBookSummary {
    fn from(book: LiveOrderBook) -> Self {
        Self {
            spread_bps: book.spread_bps(),
            best_bid: book.best_bid,
            best_bid_size: book.best_bid_size,
            best_ask: book.best_ask,
            best_ask_size: book.best_ask_size,
            last_update_ms: book.last_update_ms,
            // Move String field last to avoid partial move issues
            token_id: book.token_id,
        }
    }
}

// ============================================================================
// Position State
// ============================================================================

/// State of a position in a market.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PositionState {
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

    /// Realized P&L.
    pub realized_pnl: Decimal,

    /// Total exposure.
    pub total_exposure: Decimal,

    /// Imbalance ratio (0.0-1.0).
    pub imbalance_ratio: Decimal,

    /// Inventory state classification.
    pub inventory_state: String,
}

impl From<InventoryPosition> for PositionState {
    fn from(inv: InventoryPosition) -> Self {
        // Calculate derived values before consuming struct fields
        let state_str = match inv.state() {
            InventoryState::Balanced => "balanced",
            InventoryState::Skewed => "skewed",
            InventoryState::Exposed => "exposed",
            InventoryState::Crisis => "crisis",
        };
        let total_exposure = inv.total_exposure();
        let imbalance_ratio = inv.imbalance_ratio();

        Self {
            yes_shares: inv.yes_shares,
            no_shares: inv.no_shares,
            yes_cost_basis: inv.yes_cost_basis,
            no_cost_basis: inv.no_cost_basis,
            realized_pnl: inv.realized_pnl,
            total_exposure,
            imbalance_ratio,
            inventory_state: state_str.to_string(),
            // Move String field last to avoid partial move issues
            event_id: inv.event_id,
        }
    }
}

// ============================================================================
// Control State
// ============================================================================

/// Control flags state for dashboard.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ControlState {
    /// Whether trading is globally enabled.
    pub trading_enabled: bool,

    /// Whether circuit breaker is tripped.
    pub circuit_breaker_tripped: bool,

    /// Consecutive failure count.
    pub consecutive_failures: u32,

    /// Circuit breaker trip time (if tripped).
    pub circuit_breaker_trip_time: Option<DateTime<Utc>>,

    /// Whether shutdown has been requested.
    pub shutdown_requested: bool,

    /// Current bot status (initializing, trading, paused, etc.).
    pub bot_status: String,

    /// Current bot mode (paper, live).
    pub current_mode: String,
}

impl ControlState {
    /// Create from ControlFlags (lock-free atomic reads).
    pub fn from_flags(flags: &ControlFlags) -> Self {
        let trip_time_ms = flags.circuit_breaker_trip_time.load(Ordering::Acquire);
        let trip_time = if trip_time_ms > 0 {
            DateTime::from_timestamp_millis(trip_time_ms)
        } else {
            None
        };

        Self {
            trading_enabled: flags.trading_enabled.load(Ordering::Acquire),
            circuit_breaker_tripped: flags.circuit_breaker_tripped.load(Ordering::Acquire),
            consecutive_failures: flags.consecutive_failures.load(Ordering::Acquire),
            circuit_breaker_trip_time: trip_time,
            shutdown_requested: flags.shutdown_requested.load(Ordering::Acquire),
            bot_status: flags.get_status().as_str().to_string(),
            current_mode: flags.get_mode().as_str().to_string(),
        }
    }
}

// ============================================================================
// Anomaly State
// ============================================================================

/// Anomaly severity level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AnomalySeverity {
    /// Low severity - informational.
    Low,
    /// Medium severity - should investigate.
    Medium,
    /// High severity - needs attention.
    High,
    /// Critical severity - immediate action required.
    Critical,
}

/// An anomaly detected by the system.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnomalyState {
    /// Unique anomaly ID.
    pub id: String,

    /// Anomaly type (e.g., "price_spike", "latency_spike").
    pub anomaly_type: String,

    /// Severity level.
    pub severity: AnomalySeverity,

    /// Human-readable message.
    pub message: String,

    /// When the anomaly was detected.
    pub detected_at: DateTime<Utc>,

    /// Associated event ID (if applicable).
    pub event_id: Option<String>,

    /// Whether the anomaly has been dismissed.
    pub dismissed: bool,
}

impl AnomalyState {
    /// Create a new anomaly.
    pub fn new(
        anomaly_type: impl Into<String>,
        severity: AnomalySeverity,
        message: impl Into<String>,
    ) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            anomaly_type: anomaly_type.into(),
            severity,
            message: message.into(),
            detected_at: Utc::now(),
            event_id: None,
            dismissed: false,
        }
    }

    /// Add event context.
    pub fn with_event(mut self, event_id: impl Into<String>) -> Self {
        self.event_id = Some(event_id.into());
        self
    }

    /// Mark as dismissed.
    pub fn dismiss(&mut self) {
        self.dismissed = true;
    }
}

// ============================================================================
// Shared State Manager
// ============================================================================

/// Shared dashboard state manager.
///
/// This wraps a DashboardState with thread-safe access for:
/// - Adding trades/logs/anomalies from capture channel
/// - Creating snapshots for WebSocket broadcast
pub struct DashboardStateManager {
    /// Recent trades (ring buffer).
    recent_trades: parking_lot::RwLock<Vec<TradeRecord>>,

    /// Recent logs (ring buffer).
    recent_logs: parking_lot::RwLock<Vec<LogEntry>>,

    /// Active anomalies.
    anomalies: parking_lot::RwLock<Vec<AnomalyState>>,
}

impl DashboardStateManager {
    /// Create a new state manager.
    pub fn new() -> Self {
        Self {
            recent_trades: parking_lot::RwLock::new(Vec::with_capacity(100)),
            recent_logs: parking_lot::RwLock::new(Vec::with_capacity(100)),
            anomalies: parking_lot::RwLock::new(Vec::new()),
        }
    }

    /// Add a trade to recent trades.
    pub fn add_trade(&self, trade: TradeRecord) {
        let mut trades = self.recent_trades.write();
        trades.insert(0, trade);
        if trades.len() > 100 {
            trades.truncate(100);
        }
    }

    /// Add a log entry to recent logs.
    pub fn add_log(&self, log: LogEntry) {
        let mut logs = self.recent_logs.write();
        logs.insert(0, log);
        if logs.len() > 100 {
            logs.truncate(100);
        }
    }

    /// Add an anomaly.
    pub fn add_anomaly(&self, anomaly: AnomalyState) {
        let mut anomalies = self.anomalies.write();
        anomalies.push(anomaly);
    }

    /// Dismiss an anomaly by ID.
    pub fn dismiss_anomaly(&self, id: &str) {
        let mut anomalies = self.anomalies.write();
        if let Some(anomaly) = anomalies.iter_mut().find(|a| a.id == id) {
            anomaly.dismiss();
        }
    }

    /// Clear all dismissed anomalies.
    pub fn clear_dismissed_anomalies(&self) {
        let mut anomalies = self.anomalies.write();
        anomalies.retain(|a| !a.dismissed);
    }

    /// Create a full dashboard snapshot.
    ///
    /// Combines GlobalState (lock-free reads) with managed state (brief RwLock reads).
    pub fn snapshot(&self, global_state: &GlobalState) -> DashboardState {
        let mut state = DashboardState::from_global_state(global_state);

        // Add managed state (brief lock acquisitions)
        state.recent_trades = self.recent_trades.read().clone();
        state.recent_logs = self.recent_logs.read().clone();
        state.anomalies = self.anomalies.read().clone();

        state
    }

    /// Get trade count.
    pub fn trade_count(&self) -> usize {
        self.recent_trades.read().len()
    }

    /// Get log count.
    pub fn log_count(&self) -> usize {
        self.recent_logs.read().len()
    }

    /// Get anomaly count.
    pub fn anomaly_count(&self) -> usize {
        self.anomalies.read().len()
    }
}

impl Default for DashboardStateManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Shared reference to DashboardStateManager.
pub type SharedDashboardStateManager = Arc<DashboardStateManager>;

/// Create a shared dashboard state manager.
pub fn create_shared_dashboard_state_manager() -> SharedDashboardStateManager {
    Arc::new(DashboardStateManager::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dashboard::types::LogLevel;
    use rust_decimal_macros::dec;

    #[test]
    fn test_dashboard_state_empty() {
        let state = DashboardState::empty();
        assert!(state.markets.is_empty());
        assert!(state.recent_trades.is_empty());
        assert!(state.positions.is_empty());
        assert!(state.recent_logs.is_empty());
        assert!(state.anomalies.is_empty());
    }

    #[test]
    fn test_dashboard_state_from_global_state() {
        let global = GlobalState::new();
        global.enable_trading();

        let state = DashboardState::from_global_state(&global);
        assert!(state.control.trading_enabled);
        assert!(!state.control.circuit_breaker_tripped);
    }

    #[test]
    fn test_metrics_snapshot_json() {
        let counters = MetricsCounters::new();
        counters.events_processed.store(100, Ordering::Relaxed);
        counters.trades_executed.store(50, Ordering::Relaxed);
        counters.trades_failed.store(5, Ordering::Relaxed);
        counters.add_pnl_cents(5000); // $50.00

        let json = MetricsSnapshotJson::from_counters(&counters);
        assert_eq!(json.events_processed, 100);
        assert_eq!(json.trades_executed, 50);
        assert_eq!(json.trades_failed, 5);
        assert_eq!(json.pnl_usdc, "50.00");

        // Win rate: 50 / 55 ≈ 0.909
        let win_rate = json.win_rate.unwrap();
        assert!(win_rate > 0.9 && win_rate < 0.92);
    }

    #[test]
    fn test_order_book_summary() {
        let book = LiveOrderBook {
            token_id: "token123".to_string(),
            best_bid: dec!(0.45),
            best_bid_size: dec!(100),
            best_ask: dec!(0.55),
            best_ask_size: dec!(200),
            last_update_ms: 1234567890,
        };

        let summary = OrderBookSummary::from(book);
        assert_eq!(summary.token_id, "token123");
        assert_eq!(summary.best_bid, dec!(0.45));
        assert_eq!(summary.best_ask, dec!(0.55));
        assert_eq!(summary.spread_bps, 2000); // (0.55-0.45)/0.50 * 10000
    }

    #[test]
    fn test_position_state() {
        let mut inv = InventoryPosition::new("event1".to_string());
        inv.add_yes(dec!(100), dec!(45));
        inv.add_no(dec!(100), dec!(50));

        let state = PositionState::from(inv);
        assert_eq!(state.event_id, "event1");
        assert_eq!(state.yes_shares, dec!(100));
        assert_eq!(state.no_shares, dec!(100));
        assert_eq!(state.total_exposure, dec!(95));
        assert_eq!(state.inventory_state, "balanced");
    }

    #[test]
    fn test_control_state() {
        let flags = ControlFlags::new();
        flags.trading_enabled.store(true, Ordering::Release);
        flags.consecutive_failures.store(2, Ordering::Release);

        let state = ControlState::from_flags(&flags);
        assert!(state.trading_enabled);
        assert!(!state.circuit_breaker_tripped);
        assert_eq!(state.consecutive_failures, 2);
        assert!(state.circuit_breaker_trip_time.is_none());
    }

    #[test]
    fn test_anomaly_state() {
        let anomaly = AnomalyState::new("price_spike", AnomalySeverity::High, "BTC price jumped 5%")
            .with_event("event123");

        assert_eq!(anomaly.anomaly_type, "price_spike");
        assert_eq!(anomaly.severity, AnomalySeverity::High);
        assert_eq!(anomaly.event_id, Some("event123".to_string()));
        assert!(!anomaly.dismissed);
    }

    #[test]
    fn test_anomaly_dismiss() {
        let mut anomaly =
            AnomalyState::new("latency_spike", AnomalySeverity::Medium, "Order latency > 500ms");
        assert!(!anomaly.dismissed);

        anomaly.dismiss();
        assert!(anomaly.dismissed);
    }

    #[test]
    fn test_dashboard_state_manager_trades() {
        let manager = DashboardStateManager::new();

        // Add some trades
        for i in 0..150 {
            let trade = TradeRecord::new(
                uuid::Uuid::new_v4(),
                i,
                format!("event{}", i),
                format!("token{}", i),
                poly_common::types::Outcome::Yes,
                crate::dashboard::types::TradeSide::Buy,
                crate::dashboard::types::DashboardOrderType::Market,
                dec!(0.45),
                dec!(100),
            );
            manager.add_trade(trade);
        }

        // Should be capped at 100
        assert_eq!(manager.trade_count(), 100);

        // Most recent should be first
        let trades = manager.recent_trades.read();
        assert!(trades[0].decision_id > trades[99].decision_id);
    }

    #[test]
    fn test_dashboard_state_manager_logs() {
        let manager = DashboardStateManager::new();

        for i in 0..150 {
            let log = LogEntry::new(
                uuid::Uuid::new_v4(),
                LogLevel::Info,
                "test".to_string(),
                format!("message {}", i),
            );
            manager.add_log(log);
        }

        assert_eq!(manager.log_count(), 100);
    }

    #[test]
    fn test_dashboard_state_manager_anomalies() {
        let manager = DashboardStateManager::new();

        let anomaly1 = AnomalyState::new("type1", AnomalySeverity::Low, "msg1");
        let id1 = anomaly1.id.clone();
        let anomaly2 = AnomalyState::new("type2", AnomalySeverity::High, "msg2");

        manager.add_anomaly(anomaly1);
        manager.add_anomaly(anomaly2);
        assert_eq!(manager.anomaly_count(), 2);

        // Dismiss first anomaly
        manager.dismiss_anomaly(&id1);
        assert_eq!(manager.anomaly_count(), 2); // Still 2

        // Clear dismissed
        manager.clear_dismissed_anomalies();
        assert_eq!(manager.anomaly_count(), 1);
    }

    #[test]
    fn test_dashboard_state_serialization() {
        let state = DashboardState::empty();
        let json = serde_json::to_string(&state).unwrap();
        assert!(json.contains("\"metrics\""));
        assert!(json.contains("\"markets\""));
        assert!(json.contains("\"control\""));

        // Deserialize back
        let parsed: DashboardState = serde_json::from_str(&json).unwrap();
        assert!(parsed.markets.is_empty());
    }

    #[test]
    fn test_metrics_snapshot_json_no_trades() {
        let counters = MetricsCounters::new();
        let json = MetricsSnapshotJson::from_counters(&counters);

        // No trades = no win rate
        assert!(json.win_rate.is_none());
    }
}
