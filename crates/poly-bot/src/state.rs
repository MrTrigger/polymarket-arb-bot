//! Global shared state with lock-free access.
//!
//! This module provides the core shared state for the trading bot using
//! DashMap and atomics to enable lock-free reads on the hot path.
//!
//! ## Performance Requirements
//!
//! - `can_trade()` must complete in ~10ns (two atomic loads)
//! - All hot path reads must be lock-free
//! - No Mutex locks on the trading path

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

use chrono::{DateTime, Utc};
use dashmap::DashMap;
use parking_lot::RwLock;
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;

use poly_common::types::{CryptoAsset, Outcome};
use crate::dashboard::types::TradeRecord;

/// Global shared state for the trading bot.
///
/// All fields are designed for lock-free concurrent access.
#[derive(Debug)]
pub struct GlobalState {
    /// Market data (prices, order books).
    pub market_data: SharedMarketData,

    /// Control flags (trading enabled, circuit breaker).
    pub control: ControlFlags,

    /// Metrics counters for observability.
    pub metrics: MetricsCounters,
}

impl GlobalState {
    /// Create a new global state instance.
    pub fn new() -> Self {
        Self {
            market_data: SharedMarketData::new(),
            control: ControlFlags::new(),
            metrics: MetricsCounters::new(),
        }
    }

    /// Check if trading is allowed.
    ///
    /// This is the hot path check - must be ~10ns.
    /// Uses two atomic loads with Acquire ordering.
    #[inline(always)]
    pub fn can_trade(&self) -> bool {
        // First check: is trading globally enabled?
        // Second check: is circuit breaker not tripped?
        // Both must be true to trade.
        self.control.trading_enabled.load(Ordering::Acquire)
            && !self.control.circuit_breaker_tripped.load(Ordering::Acquire)
    }

    /// Enable trading.
    #[inline]
    pub fn enable_trading(&self) {
        self.control.trading_enabled.store(true, Ordering::Release);
    }

    /// Disable trading.
    #[inline]
    pub fn disable_trading(&self) {
        self.control.trading_enabled.store(false, Ordering::Release);
    }

    /// Trip the circuit breaker.
    #[inline]
    pub fn trip_circuit_breaker(&self) {
        self.control
            .circuit_breaker_tripped
            .store(true, Ordering::Release);
        self.control.circuit_breaker_trip_time.store(
            Utc::now().timestamp_millis(),
            Ordering::Release,
        );
    }

    /// Reset the circuit breaker.
    #[inline]
    pub fn reset_circuit_breaker(&self) {
        self.control
            .circuit_breaker_tripped
            .store(false, Ordering::Release);
        self.control
            .consecutive_failures
            .store(0, Ordering::Release);
    }

    /// Check if circuit breaker cooldown has elapsed.
    pub fn circuit_breaker_cooldown_elapsed(&self, cooldown: Duration) -> bool {
        if !self.control.circuit_breaker_tripped.load(Ordering::Acquire) {
            return true;
        }
        let trip_time = self.control.circuit_breaker_trip_time.load(Ordering::Acquire);
        let elapsed_ms = Utc::now().timestamp_millis() - trip_time;
        elapsed_ms >= cooldown.as_millis() as i64
    }

    /// Record a successful trade.
    #[inline]
    pub fn record_success(&self) {
        self.control
            .consecutive_failures
            .store(0, Ordering::Release);
        self.metrics.trades_executed.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a failed trade. Returns true if circuit breaker should trip.
    #[inline]
    pub fn record_failure(&self, max_failures: u32) -> bool {
        let failures = self
            .control
            .consecutive_failures
            .fetch_add(1, Ordering::AcqRel)
            + 1;
        self.metrics.trades_failed.fetch_add(1, Ordering::Relaxed);
        failures >= max_failures
    }
}

impl Default for GlobalState {
    fn default() -> Self {
        Self::new()
    }
}

/// Shared market data accessible from multiple tasks.
#[derive(Debug)]
pub struct SharedMarketData {
    /// Latest spot prices by asset.
    /// Key: CryptoAsset as string (e.g., "BTC")
    /// Value: (price, timestamp_ms)
    pub spot_prices: DashMap<String, (Decimal, i64)>,

    /// Order books by token ID.
    /// Key: token_id
    /// Value: LiveOrderBook
    pub order_books: DashMap<String, LiveOrderBook>,

    /// Inventory by event ID.
    /// Key: event_id
    /// Value: InventoryPosition
    pub inventory: DashMap<String, InventoryPosition>,

    /// Shadow orders waiting for fills.
    /// Key: (event_id, outcome)
    /// Value: ShadowOrderState
    pub shadow_orders: DashMap<(String, Outcome), ShadowOrderState>,

    /// Active market windows.
    /// Key: event_id
    /// Value: ActiveWindow
    pub active_windows: DashMap<String, ActiveWindow>,

    /// Latest confidence snapshots for dashboard display.
    /// Key: event_id
    /// Value: ConfidenceSnapshot
    pub confidence_snapshots: DashMap<String, ConfidenceSnapshot>,

    /// Recent trades for dashboard display (ring buffer, max 100).
    pub recent_trades: RwLock<Vec<TradeRecord>>,
}

impl SharedMarketData {
    /// Create new shared market data.
    pub fn new() -> Self {
        Self {
            spot_prices: DashMap::new(),
            order_books: DashMap::new(),
            inventory: DashMap::new(),
            shadow_orders: DashMap::new(),
            active_windows: DashMap::new(),
            confidence_snapshots: DashMap::new(),
            recent_trades: RwLock::new(Vec::with_capacity(100)),
        }
    }

    /// Update spot price for an asset.
    #[inline]
    pub fn update_spot_price(&self, asset: &str, price: Decimal, timestamp_ms: i64) {
        self.spot_prices
            .insert(asset.to_string(), (price, timestamp_ms));
    }

    /// Get the latest spot price for an asset.
    #[inline]
    pub fn get_spot_price(&self, asset: &str) -> Option<(Decimal, i64)> {
        self.spot_prices.get(asset).map(|r| *r.value())
    }

    /// Update order book for a token.
    #[inline]
    pub fn update_order_book(&self, token_id: &str, book: LiveOrderBook) {
        self.order_books.insert(token_id.to_string(), book);
    }

    /// Get order book for a token.
    #[inline]
    pub fn get_order_book(&self, token_id: &str) -> Option<LiveOrderBook> {
        self.order_books.get(token_id).map(|r| r.value().clone())
    }

    /// Get best bid/ask for a token.
    #[inline]
    pub fn get_bbo(&self, token_id: &str) -> Option<(Decimal, Decimal)> {
        self.order_books.get(token_id).and_then(|book| {
            let b = book.value();
            if b.best_bid > Decimal::ZERO && b.best_ask > Decimal::ZERO {
                Some((b.best_bid, b.best_ask))
            } else {
                None
            }
        })
    }

    /// Update inventory for an event.
    #[inline]
    pub fn update_inventory(&self, event_id: &str, inventory: InventoryPosition) {
        self.inventory.insert(event_id.to_string(), inventory);
    }

    /// Get inventory for an event.
    #[inline]
    pub fn get_inventory(&self, event_id: &str) -> Option<InventoryPosition> {
        self.inventory.get(event_id).map(|r| r.value().clone())
    }

    /// Calculate total exposure across all positions.
    pub fn total_exposure(&self) -> Decimal {
        self.inventory
            .iter()
            .map(|r| r.value().total_exposure())
            .sum()
    }

    /// Update confidence snapshot for a market.
    #[inline]
    pub fn update_confidence(&self, event_id: &str, snapshot: ConfidenceSnapshot) {
        self.confidence_snapshots.insert(event_id.to_string(), snapshot);
    }

    /// Get confidence snapshot for a market.
    #[inline]
    pub fn get_confidence(&self, event_id: &str) -> Option<ConfidenceSnapshot> {
        self.confidence_snapshots.get(event_id).map(|r| r.value().clone())
    }

    /// Remove confidence snapshot for a market.
    #[inline]
    pub fn remove_confidence(&self, event_id: &str) {
        self.confidence_snapshots.remove(event_id);
    }

    /// Add a trade to recent trades (ring buffer, max 100).
    pub fn add_trade(&self, trade: TradeRecord) {
        let mut trades = self.recent_trades.write();
        trades.insert(0, trade);
        if trades.len() > 100 {
            trades.truncate(100);
        }
    }

    /// Get recent trades for dashboard.
    pub fn get_recent_trades(&self) -> Vec<TradeRecord> {
        self.recent_trades.read().clone()
    }
}

impl Default for SharedMarketData {
    fn default() -> Self {
        Self::new()
    }
}

/// Control flags for trading state.
///
/// All flags are atomics for lock-free access.
#[derive(Debug)]
pub struct ControlFlags {
    /// Global trading enable/disable.
    pub trading_enabled: AtomicBool,

    /// Circuit breaker tripped state.
    pub circuit_breaker_tripped: AtomicBool,

    /// Consecutive failure count.
    pub consecutive_failures: AtomicU32,

    /// Circuit breaker trip timestamp (milliseconds since epoch).
    pub circuit_breaker_trip_time: AtomicI64,

    /// Graceful shutdown requested.
    pub shutdown_requested: AtomicBool,
}

impl ControlFlags {
    /// Create new control flags with trading disabled.
    pub fn new() -> Self {
        Self {
            trading_enabled: AtomicBool::new(false),
            circuit_breaker_tripped: AtomicBool::new(false),
            consecutive_failures: AtomicU32::new(0),
            circuit_breaker_trip_time: AtomicI64::new(0),
            shutdown_requested: AtomicBool::new(false),
        }
    }

    /// Request graceful shutdown.
    #[inline]
    pub fn request_shutdown(&self) {
        self.shutdown_requested.store(true, Ordering::Release);
    }

    /// Check if shutdown was requested.
    #[inline]
    pub fn is_shutdown_requested(&self) -> bool {
        self.shutdown_requested.load(Ordering::Acquire)
    }
}

impl Default for ControlFlags {
    fn default() -> Self {
        Self::new()
    }
}

/// Metrics counters for observability.
///
/// Uses relaxed ordering since exact counts aren't critical.
#[derive(Debug)]
pub struct MetricsCounters {
    /// Total events processed.
    pub events_processed: AtomicU64,

    /// Total arb opportunities detected.
    pub opportunities_detected: AtomicU64,

    /// Total trades executed.
    pub trades_executed: AtomicU64,

    /// Total trades failed.
    pub trades_failed: AtomicU64,

    /// Total trades skipped (risk/toxic).
    pub trades_skipped: AtomicU64,

    /// Total P&L in cents (to avoid Decimal atomics).
    /// Divide by 100 for USDC value.
    pub pnl_cents: AtomicI64,

    /// Total volume traded in cents.
    pub volume_cents: AtomicU64,

    /// Shadow orders fired.
    pub shadow_orders_fired: AtomicU64,

    /// Shadow orders filled.
    pub shadow_orders_filled: AtomicU64,

    /// Allocated balance for the bot in cents (configured trading capital).
    /// Divide by 100 for USDC value.
    pub allocated_balance_cents: AtomicI64,

    /// Current/real account balance in cents (may differ from allocated).
    /// For paper trading, this equals allocated. For live, fetched from exchange.
    /// Divide by 100 for USDC value.
    pub current_balance_cents: AtomicI64,
}

impl MetricsCounters {
    /// Create new metrics counters.
    pub fn new() -> Self {
        Self {
            events_processed: AtomicU64::new(0),
            opportunities_detected: AtomicU64::new(0),
            trades_executed: AtomicU64::new(0),
            trades_failed: AtomicU64::new(0),
            trades_skipped: AtomicU64::new(0),
            pnl_cents: AtomicI64::new(0),
            volume_cents: AtomicU64::new(0),
            shadow_orders_fired: AtomicU64::new(0),
            shadow_orders_filled: AtomicU64::new(0),
            allocated_balance_cents: AtomicI64::new(0),
            current_balance_cents: AtomicI64::new(0),
        }
    }

    /// Increment events processed.
    #[inline]
    pub fn inc_events(&self) {
        self.events_processed.fetch_add(1, Ordering::Relaxed);
    }

    /// Increment opportunities detected.
    #[inline]
    pub fn inc_opportunities(&self) {
        self.opportunities_detected.fetch_add(1, Ordering::Relaxed);
    }

    /// Increment trades skipped.
    #[inline]
    pub fn inc_skipped(&self) {
        self.trades_skipped.fetch_add(1, Ordering::Relaxed);
    }

    /// Add to P&L (in cents).
    #[inline]
    pub fn add_pnl_cents(&self, cents: i64) {
        self.pnl_cents.fetch_add(cents, Ordering::Relaxed);
    }

    /// Add to volume (in cents).
    #[inline]
    pub fn add_volume_cents(&self, cents: u64) {
        self.volume_cents.fetch_add(cents, Ordering::Relaxed);
    }

    /// Get current P&L in USDC.
    pub fn pnl_usdc(&self) -> Decimal {
        let cents = self.pnl_cents.load(Ordering::Relaxed);
        Decimal::new(cents, 2)
    }

    /// Get current volume in USDC.
    pub fn volume_usdc(&self) -> Decimal {
        let cents = self.volume_cents.load(Ordering::Relaxed) as i64;
        Decimal::new(cents, 2)
    }

    /// Set the allocated balance (trading capital) in USDC.
    pub fn set_allocated_balance(&self, usdc: Decimal) {
        let cents = (usdc * Decimal::ONE_HUNDRED).to_i64().unwrap_or(0);
        self.allocated_balance_cents.store(cents, Ordering::Relaxed);
    }

    /// Set the current/real account balance in USDC.
    pub fn set_current_balance(&self, usdc: Decimal) {
        let cents = (usdc * Decimal::ONE_HUNDRED).to_i64().unwrap_or(0);
        self.current_balance_cents.store(cents, Ordering::Relaxed);
    }

    /// Get allocated balance in USDC.
    pub fn allocated_balance_usdc(&self) -> Decimal {
        let cents = self.allocated_balance_cents.load(Ordering::Relaxed);
        Decimal::new(cents, 2)
    }

    /// Get current balance in USDC.
    pub fn current_balance_usdc(&self) -> Decimal {
        let cents = self.current_balance_cents.load(Ordering::Relaxed);
        Decimal::new(cents, 2)
    }

    /// Get a snapshot of all metrics.
    pub fn snapshot(&self) -> MetricsSnapshot {
        MetricsSnapshot {
            events_processed: self.events_processed.load(Ordering::Relaxed),
            opportunities_detected: self.opportunities_detected.load(Ordering::Relaxed),
            trades_executed: self.trades_executed.load(Ordering::Relaxed),
            trades_failed: self.trades_failed.load(Ordering::Relaxed),
            trades_skipped: self.trades_skipped.load(Ordering::Relaxed),
            pnl_usdc: self.pnl_usdc(),
            volume_usdc: self.volume_usdc(),
            shadow_orders_fired: self.shadow_orders_fired.load(Ordering::Relaxed),
            shadow_orders_filled: self.shadow_orders_filled.load(Ordering::Relaxed),
            allocated_balance: self.allocated_balance_usdc(),
            current_balance: self.current_balance_usdc(),
        }
    }
}

impl Default for MetricsCounters {
    fn default() -> Self {
        Self::new()
    }
}

/// Snapshot of metrics at a point in time.
#[derive(Debug, Clone)]
pub struct MetricsSnapshot {
    pub events_processed: u64,
    pub opportunities_detected: u64,
    pub trades_executed: u64,
    pub trades_failed: u64,
    pub trades_skipped: u64,
    pub pnl_usdc: Decimal,
    pub volume_usdc: Decimal,
    pub shadow_orders_fired: u64,
    pub shadow_orders_filled: u64,
    pub allocated_balance: Decimal,
    pub current_balance: Decimal,
}

/// Live order book state for a single token.
#[derive(Debug, Clone)]
pub struct LiveOrderBook {
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
    /// Last update timestamp (milliseconds).
    pub last_update_ms: i64,
}

impl LiveOrderBook {
    /// Create a new live order book.
    pub fn new(token_id: String) -> Self {
        Self {
            token_id,
            best_bid: Decimal::ZERO,
            best_bid_size: Decimal::ZERO,
            best_ask: Decimal::ZERO,
            best_ask_size: Decimal::ZERO,
            last_update_ms: 0,
        }
    }

    /// Calculate spread in basis points.
    pub fn spread_bps(&self) -> u32 {
        if self.best_bid <= Decimal::ZERO || self.best_ask <= Decimal::ZERO {
            return 0;
        }
        let spread = self.best_ask - self.best_bid;
        let mid = (self.best_bid + self.best_ask) / Decimal::new(2, 0);
        if mid <= Decimal::ZERO {
            return 0;
        }
        // basis points = (spread / mid) * 10000
        let bps = (spread * Decimal::new(10000, 0)) / mid;
        bps.try_into().unwrap_or(u32::MAX)
    }

    /// Check if the book is valid (has both bid and ask).
    pub fn is_valid(&self) -> bool {
        self.best_bid > Decimal::ZERO && self.best_ask > Decimal::ZERO
    }
}

/// Inventory position for a market.
#[derive(Debug, Clone)]
pub struct InventoryPosition {
    /// Event ID.
    pub event_id: String,
    /// YES shares held.
    pub yes_shares: Decimal,
    /// NO shares held.
    pub no_shares: Decimal,
    /// Cost basis for YES shares.
    pub yes_cost_basis: Decimal,
    /// Cost basis for NO shares.
    pub no_cost_basis: Decimal,
    /// Realized P&L.
    pub realized_pnl: Decimal,
}

impl InventoryPosition {
    /// Create a new empty position.
    pub fn new(event_id: String) -> Self {
        Self {
            event_id,
            yes_shares: Decimal::ZERO,
            no_shares: Decimal::ZERO,
            yes_cost_basis: Decimal::ZERO,
            no_cost_basis: Decimal::ZERO,
            realized_pnl: Decimal::ZERO,
        }
    }

    /// Total exposure (cost basis of both sides).
    pub fn total_exposure(&self) -> Decimal {
        self.yes_cost_basis + self.no_cost_basis
    }

    /// Calculate imbalance ratio (0.0 = balanced, 1.0 = fully one-sided).
    pub fn imbalance_ratio(&self) -> Decimal {
        let total = self.yes_shares + self.no_shares;
        if total <= Decimal::ZERO {
            return Decimal::ZERO;
        }
        let max_side = self.yes_shares.max(self.no_shares);
        let min_side = self.yes_shares.min(self.no_shares);
        (max_side - min_side) / total
    }

    /// Get inventory state classification.
    pub fn state(&self) -> InventoryState {
        let ratio = self.imbalance_ratio();
        if ratio <= Decimal::new(2, 1) {
            // <= 0.2
            InventoryState::Balanced
        } else if ratio <= Decimal::new(5, 1) {
            // <= 0.5
            InventoryState::Skewed
        } else if ratio <= Decimal::new(8, 1) {
            // <= 0.8
            InventoryState::Exposed
        } else {
            InventoryState::Crisis
        }
    }

    /// Add YES shares.
    pub fn add_yes(&mut self, shares: Decimal, cost: Decimal) {
        self.yes_shares += shares;
        self.yes_cost_basis += cost;
    }

    /// Add NO shares.
    pub fn add_no(&mut self, shares: Decimal, cost: Decimal) {
        self.no_shares += shares;
        self.no_cost_basis += cost;
    }
}

/// Inventory state classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InventoryState {
    /// Well-balanced YES/NO position.
    Balanced,
    /// Slightly imbalanced.
    Skewed,
    /// Significantly imbalanced, should reduce.
    Exposed,
    /// Critical imbalance, must hedge immediately.
    Crisis,
}

/// Shadow order waiting for primary fill.
#[derive(Debug, Clone)]
pub struct ShadowOrderState {
    /// Event ID.
    pub event_id: String,
    /// Outcome (YES/NO).
    pub outcome: Outcome,
    /// Shadow bid price.
    pub price: Decimal,
    /// Shadow bid size.
    pub size: Decimal,
    /// Creation timestamp.
    pub created_at: DateTime<Utc>,
    /// Pre-computed order hash for fast signing.
    pub pre_hash: Option<[u8; 32]>,
}

/// Active market window.
#[derive(Debug, Clone)]
pub struct ActiveWindow {
    /// Event ID.
    pub event_id: String,
    /// Asset being tracked.
    pub asset: CryptoAsset,
    /// YES token ID.
    pub yes_token_id: String,
    /// NO token ID.
    pub no_token_id: String,
    /// Strike price.
    pub strike_price: Decimal,
    /// Window end time.
    pub window_end: DateTime<Utc>,
}

impl ActiveWindow {
    /// Seconds remaining in this window.
    pub fn seconds_remaining(&self) -> i64 {
        (self.window_end - Utc::now()).num_seconds().max(0)
    }

    /// Check if window has expired.
    pub fn is_expired(&self) -> bool {
        Utc::now() >= self.window_end
    }

    /// Get the window phase based on time remaining.
    pub fn phase(&self, early_threshold_secs: u64, mid_threshold_secs: u64) -> WindowPhase {
        let remaining = self.seconds_remaining() as u64;
        if remaining > early_threshold_secs {
            WindowPhase::Early
        } else if remaining > mid_threshold_secs {
            WindowPhase::Mid
        } else {
            WindowPhase::Late
        }
    }
}

/// Window time phase for threshold selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum WindowPhase {
    /// >5 minutes remaining (higher threshold).
    Early,
    /// 2-5 minutes remaining (medium threshold).
    Mid,
    /// <2 minutes remaining (lower threshold).
    Late,
}

/// Confidence snapshot for dashboard display.
///
/// Captures the current confidence calculation state for a market
/// so the dashboard can display confidence graphs and thresholds.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ConfidenceSnapshot {
    /// Event ID this snapshot belongs to.
    pub event_id: String,
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
    /// Snapshot timestamp (milliseconds since epoch).
    pub timestamp_ms: i64,
}

impl ConfidenceSnapshot {
    /// Create a new confidence snapshot.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        event_id: String,
        confidence: Decimal,
        time_confidence: Decimal,
        distance_confidence: Decimal,
        threshold: Decimal,
        ev: Decimal,
        would_trade: bool,
        distance_dollars: Decimal,
        atr_multiple: Decimal,
        favorable_price: Decimal,
    ) -> Self {
        Self {
            event_id,
            confidence,
            time_confidence,
            distance_confidence,
            threshold,
            ev,
            would_trade,
            distance_dollars,
            atr_multiple,
            favorable_price,
            timestamp_ms: Utc::now().timestamp_millis(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;
    use std::sync::Arc;
    use std::thread;

    #[test]
    fn test_can_trade_default() {
        let state = GlobalState::new();
        // Trading disabled by default
        assert!(!state.can_trade());
    }

    #[test]
    fn test_can_trade_enabled() {
        let state = GlobalState::new();
        state.enable_trading();
        assert!(state.can_trade());
    }

    #[test]
    fn test_can_trade_circuit_breaker() {
        let state = GlobalState::new();
        state.enable_trading();
        assert!(state.can_trade());

        state.trip_circuit_breaker();
        assert!(!state.can_trade());

        state.reset_circuit_breaker();
        assert!(state.can_trade());
    }

    #[test]
    fn test_consecutive_failures() {
        let state = GlobalState::new();

        assert!(!state.record_failure(3)); // 1 failure
        assert!(!state.record_failure(3)); // 2 failures
        assert!(state.record_failure(3)); // 3 failures - should trip

        state.record_success();
        // Counter reset
        assert!(!state.record_failure(3)); // 1 failure again
    }

    #[test]
    fn test_spot_price_update() {
        let state = GlobalState::new();
        state
            .market_data
            .update_spot_price("BTC", dec!(50000.50), 1234567890);

        let (price, ts) = state.market_data.get_spot_price("BTC").unwrap();
        assert_eq!(price, dec!(50000.50));
        assert_eq!(ts, 1234567890);
    }

    #[test]
    fn test_order_book_update() {
        let state = GlobalState::new();
        let book = LiveOrderBook {
            token_id: "token123".to_string(),
            best_bid: dec!(0.45),
            best_bid_size: dec!(100),
            best_ask: dec!(0.55),
            best_ask_size: dec!(200),
            last_update_ms: 1234567890,
        };

        state.market_data.update_order_book("token123", book.clone());

        let retrieved = state.market_data.get_order_book("token123").unwrap();
        assert_eq!(retrieved.best_bid, dec!(0.45));
        assert_eq!(retrieved.best_ask, dec!(0.55));
    }

    #[test]
    fn test_bbo() {
        let state = GlobalState::new();
        let book = LiveOrderBook {
            token_id: "token123".to_string(),
            best_bid: dec!(0.45),
            best_bid_size: dec!(100),
            best_ask: dec!(0.55),
            best_ask_size: dec!(200),
            last_update_ms: 1234567890,
        };

        state.market_data.update_order_book("token123", book);

        let (bid, ask) = state.market_data.get_bbo("token123").unwrap();
        assert_eq!(bid, dec!(0.45));
        assert_eq!(ask, dec!(0.55));
    }

    #[test]
    fn test_inventory_imbalance() {
        let mut inv = InventoryPosition::new("event1".to_string());
        assert_eq!(inv.imbalance_ratio(), Decimal::ZERO);

        inv.add_yes(dec!(100), dec!(45));
        inv.add_no(dec!(100), dec!(45));
        // Balanced: imbalance = 0
        assert_eq!(inv.imbalance_ratio(), Decimal::ZERO);
        assert_eq!(inv.state(), InventoryState::Balanced);

        inv.add_yes(dec!(50), dec!(22.5));
        // YES=150, NO=100, total=250
        // imbalance = (150-100)/250 = 0.2
        assert_eq!(inv.imbalance_ratio(), dec!(0.2));
        assert_eq!(inv.state(), InventoryState::Balanced);
    }

    #[test]
    fn test_inventory_states() {
        let mut inv = InventoryPosition::new("event1".to_string());

        // Skewed: 0.2 < ratio <= 0.5
        inv.yes_shares = dec!(70);
        inv.no_shares = dec!(30);
        assert_eq!(inv.imbalance_ratio(), dec!(0.4));
        assert_eq!(inv.state(), InventoryState::Skewed);

        // Exposed: 0.5 < ratio <= 0.8
        inv.yes_shares = dec!(85);
        inv.no_shares = dec!(15);
        assert_eq!(inv.imbalance_ratio(), dec!(0.7));
        assert_eq!(inv.state(), InventoryState::Exposed);

        // Crisis: ratio > 0.8
        inv.yes_shares = dec!(95);
        inv.no_shares = dec!(5);
        assert_eq!(inv.imbalance_ratio(), dec!(0.9));
        assert_eq!(inv.state(), InventoryState::Crisis);
    }

    #[test]
    fn test_live_order_book_spread() {
        let book = LiveOrderBook {
            token_id: "test".to_string(),
            best_bid: dec!(0.45),
            best_bid_size: dec!(100),
            best_ask: dec!(0.55),
            best_ask_size: dec!(200),
            last_update_ms: 0,
        };

        // spread = 0.10, mid = 0.50
        // bps = (0.10 / 0.50) * 10000 = 2000
        assert_eq!(book.spread_bps(), 2000);
    }

    #[test]
    fn test_live_order_book_validity() {
        let mut book = LiveOrderBook::new("test".to_string());
        assert!(!book.is_valid());

        book.best_bid = dec!(0.45);
        assert!(!book.is_valid());

        book.best_ask = dec!(0.55);
        assert!(book.is_valid());
    }

    #[test]
    fn test_metrics_counters() {
        let metrics = MetricsCounters::new();

        metrics.inc_events();
        metrics.inc_events();
        metrics.inc_opportunities();
        metrics.add_pnl_cents(500);
        metrics.add_volume_cents(10000);

        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.events_processed, 2);
        assert_eq!(snapshot.opportunities_detected, 1);
        assert_eq!(snapshot.pnl_usdc, dec!(5.00));
        assert_eq!(snapshot.volume_usdc, dec!(100.00));
    }

    #[test]
    fn test_total_exposure() {
        let state = GlobalState::new();

        let mut inv1 = InventoryPosition::new("event1".to_string());
        inv1.yes_cost_basis = dec!(100);
        inv1.no_cost_basis = dec!(50);

        let mut inv2 = InventoryPosition::new("event2".to_string());
        inv2.yes_cost_basis = dec!(75);
        inv2.no_cost_basis = dec!(25);

        state.market_data.update_inventory("event1", inv1);
        state.market_data.update_inventory("event2", inv2);

        // Total: 100 + 50 + 75 + 25 = 250
        assert_eq!(state.market_data.total_exposure(), dec!(250));
    }

    #[test]
    fn test_concurrent_access() {
        let state = Arc::new(GlobalState::new());
        state.enable_trading();

        let handles: Vec<_> = (0..10)
            .map(|i| {
                let state = Arc::clone(&state);
                thread::spawn(move || {
                    for j in 0..100 {
                        // Concurrent can_trade checks
                        let _ = state.can_trade();

                        // Concurrent price updates
                        state.market_data.update_spot_price(
                            "BTC",
                            Decimal::new(50000 + i * 100 + j, 0),
                            i * 1000 + j,
                        );

                        // Concurrent metrics updates
                        state.metrics.inc_events();
                    }
                })
            })
            .collect();

        for handle in handles {
            handle.join().unwrap();
        }

        // Should have processed 1000 events total
        assert_eq!(
            state.metrics.events_processed.load(Ordering::Relaxed),
            1000
        );
    }

    #[test]
    fn test_shutdown_flag() {
        let state = GlobalState::new();
        assert!(!state.control.is_shutdown_requested());

        state.control.request_shutdown();
        assert!(state.control.is_shutdown_requested());
    }
}
