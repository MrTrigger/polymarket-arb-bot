//! Session-wide statistics and counters.
//!
//! Aggregates counters and statistics across all engines and markets
//! for the current trading session.

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};
use uuid::Uuid;

use crate::dashboard::BotMode;

/// Session-wide statistics with atomic counters for hot path.
pub struct SessionStats {
    /// Unique session ID.
    session_id: Uuid,
    /// Bot mode (live, paper, backtest).
    mode: BotMode,
    /// Session start time.
    started_at: DateTime<Utc>,
    /// Events processed (orderbook updates, price updates).
    events_processed: AtomicU64,
    /// Opportunities detected across all engines.
    opportunities_detected: AtomicU64,
    /// Trades executed (orders filled).
    trades_executed: AtomicU64,
    /// Trades failed (orders rejected/errored).
    trades_failed: AtomicU64,
    /// Trades skipped (signal present but didn't meet criteria).
    trades_skipped: AtomicU64,
    /// Shadow orders fired.
    shadow_orders_fired: AtomicU64,
    /// Shadow orders filled.
    shadow_orders_filled: AtomicU64,
    /// Circuit breaker trips.
    circuit_breaker_trips: AtomicU64,
    /// Total volume traded (in cents for atomics).
    volume_cents: AtomicU64,
    /// Markets traded (count).
    markets_traded: AtomicU64,
    /// Arbitrage opportunities detected.
    arb_opportunities: AtomicU64,
    /// Directional opportunities detected.
    directional_opportunities: AtomicU64,
    /// Maker opportunities detected.
    maker_opportunities: AtomicU64,
    /// Consecutive successes (for circuit breaker).
    consecutive_successes: AtomicU64,
    /// Consecutive failures (for circuit breaker).
    consecutive_failures: AtomicU64,
}

impl SessionStats {
    /// Create new session stats.
    pub fn new(mode: BotMode) -> Self {
        Self {
            session_id: Uuid::new_v4(),
            mode,
            started_at: Utc::now(),
            events_processed: AtomicU64::new(0),
            opportunities_detected: AtomicU64::new(0),
            trades_executed: AtomicU64::new(0),
            trades_failed: AtomicU64::new(0),
            trades_skipped: AtomicU64::new(0),
            shadow_orders_fired: AtomicU64::new(0),
            shadow_orders_filled: AtomicU64::new(0),
            circuit_breaker_trips: AtomicU64::new(0),
            volume_cents: AtomicU64::new(0),
            markets_traded: AtomicU64::new(0),
            arb_opportunities: AtomicU64::new(0),
            directional_opportunities: AtomicU64::new(0),
            maker_opportunities: AtomicU64::new(0),
            consecutive_successes: AtomicU64::new(0),
            consecutive_failures: AtomicU64::new(0),
        }
    }

    /// Get session ID.
    pub fn session_id(&self) -> Uuid {
        self.session_id
    }

    /// Get bot mode.
    pub fn mode(&self) -> BotMode {
        self.mode
    }

    /// Get session start time.
    pub fn started_at(&self) -> DateTime<Utc> {
        self.started_at
    }

    /// Get session duration in seconds.
    pub fn duration_secs(&self) -> i64 {
        (Utc::now() - self.started_at).num_seconds()
    }

    // === Event Counters ===

    /// Increment events processed.
    pub fn record_event(&self) {
        self.events_processed.fetch_add(1, Ordering::Relaxed);
    }

    /// Get events processed.
    pub fn events_processed(&self) -> u64 {
        self.events_processed.load(Ordering::Relaxed)
    }

    // === Opportunity Counters ===

    /// Record an opportunity detected.
    pub fn record_opportunity(&self, engine: EngineType) {
        self.opportunities_detected.fetch_add(1, Ordering::Relaxed);
        match engine {
            EngineType::Arbitrage => {
                self.arb_opportunities.fetch_add(1, Ordering::Relaxed);
            }
            EngineType::Directional => {
                self.directional_opportunities.fetch_add(1, Ordering::Relaxed);
            }
            EngineType::Maker => {
                self.maker_opportunities.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Get total opportunities detected.
    pub fn opportunities_detected(&self) -> u64 {
        self.opportunities_detected.load(Ordering::Relaxed)
    }

    /// Get arbitrage opportunities.
    pub fn arb_opportunities(&self) -> u64 {
        self.arb_opportunities.load(Ordering::Relaxed)
    }

    /// Get directional opportunities.
    pub fn directional_opportunities(&self) -> u64 {
        self.directional_opportunities.load(Ordering::Relaxed)
    }

    /// Get maker opportunities.
    pub fn maker_opportunities(&self) -> u64 {
        self.maker_opportunities.load(Ordering::Relaxed)
    }

    // === Trade Counters ===

    /// Record a successful trade execution.
    pub fn record_trade_executed(&self, volume: Decimal) {
        self.trades_executed.fetch_add(1, Ordering::Relaxed);
        let cents = (volume * dec!(100)).to_u64().unwrap_or(0);
        self.volume_cents.fetch_add(cents, Ordering::Relaxed);
        self.consecutive_successes.fetch_add(1, Ordering::Relaxed);
        self.consecutive_failures.store(0, Ordering::Relaxed);
    }

    /// Record a failed trade.
    pub fn record_trade_failed(&self) {
        self.trades_failed.fetch_add(1, Ordering::Relaxed);
        self.consecutive_failures.fetch_add(1, Ordering::Relaxed);
        self.consecutive_successes.store(0, Ordering::Relaxed);
    }

    /// Record a skipped trade.
    pub fn record_trade_skipped(&self) {
        self.trades_skipped.fetch_add(1, Ordering::Relaxed);
    }

    /// Get trades executed.
    pub fn trades_executed(&self) -> u64 {
        self.trades_executed.load(Ordering::Relaxed)
    }

    /// Get trades failed.
    pub fn trades_failed(&self) -> u64 {
        self.trades_failed.load(Ordering::Relaxed)
    }

    /// Get trades skipped.
    pub fn trades_skipped(&self) -> u64 {
        self.trades_skipped.load(Ordering::Relaxed)
    }

    /// Get total volume in USDC.
    pub fn volume(&self) -> Decimal {
        let cents = self.volume_cents.load(Ordering::Relaxed);
        Decimal::new(cents as i64, 2)
    }

    /// Get consecutive successes.
    pub fn consecutive_successes(&self) -> u64 {
        self.consecutive_successes.load(Ordering::Relaxed)
    }

    /// Get consecutive failures.
    pub fn consecutive_failures(&self) -> u64 {
        self.consecutive_failures.load(Ordering::Relaxed)
    }

    // === Shadow Order Counters ===

    /// Record a shadow order fired.
    pub fn record_shadow_fired(&self) {
        self.shadow_orders_fired.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a shadow order filled.
    pub fn record_shadow_filled(&self) {
        self.shadow_orders_filled.fetch_add(1, Ordering::Relaxed);
    }

    /// Get shadow orders fired.
    pub fn shadow_orders_fired(&self) -> u64 {
        self.shadow_orders_fired.load(Ordering::Relaxed)
    }

    /// Get shadow orders filled.
    pub fn shadow_orders_filled(&self) -> u64 {
        self.shadow_orders_filled.load(Ordering::Relaxed)
    }

    // === Circuit Breaker ===

    /// Record a circuit breaker trip.
    pub fn record_circuit_breaker_trip(&self) {
        self.circuit_breaker_trips.fetch_add(1, Ordering::Relaxed);
    }

    /// Get circuit breaker trips.
    pub fn circuit_breaker_trips(&self) -> u64 {
        self.circuit_breaker_trips.load(Ordering::Relaxed)
    }

    // === Markets ===

    /// Record a new market traded.
    pub fn record_market_traded(&self) {
        self.markets_traded.fetch_add(1, Ordering::Relaxed);
    }

    /// Get markets traded count.
    pub fn markets_traded(&self) -> u64 {
        self.markets_traded.load(Ordering::Relaxed)
    }

    // === Computed Metrics ===

    /// Calculate trade success rate.
    pub fn success_rate(&self) -> Decimal {
        let executed = self.trades_executed();
        let failed = self.trades_failed();
        let total = executed + failed;
        if total == 0 {
            return Decimal::ZERO;
        }
        Decimal::new(executed as i64, 0) / Decimal::new(total as i64, 0) * dec!(100)
    }

    /// Calculate opportunity conversion rate (trades / opportunities).
    pub fn conversion_rate(&self) -> Decimal {
        let opportunities = self.opportunities_detected();
        let trades = self.trades_executed();
        if opportunities == 0 {
            return Decimal::ZERO;
        }
        Decimal::new(trades as i64, 0) / Decimal::new(opportunities as i64, 0) * dec!(100)
    }

    /// Calculate events per second.
    pub fn events_per_second(&self) -> Decimal {
        let duration = self.duration_secs();
        if duration == 0 {
            return Decimal::ZERO;
        }
        Decimal::new(self.events_processed() as i64, 0) / Decimal::new(duration, 0)
    }

    /// Calculate trades per hour.
    pub fn trades_per_hour(&self) -> Decimal {
        let hours = Decimal::new(self.duration_secs(), 0) / dec!(3600);
        if hours <= Decimal::ZERO {
            return Decimal::ZERO;
        }
        Decimal::new(self.trades_executed() as i64, 0) / hours
    }

    /// Get summary.
    pub fn summary(&self) -> StatsSummary {
        StatsSummary {
            session_id: self.session_id,
            mode: self.mode,
            started_at: self.started_at,
            duration_secs: self.duration_secs(),
            events_processed: self.events_processed(),
            opportunities_detected: self.opportunities_detected(),
            arb_opportunities: self.arb_opportunities(),
            directional_opportunities: self.directional_opportunities(),
            maker_opportunities: self.maker_opportunities(),
            trades_executed: self.trades_executed(),
            trades_failed: self.trades_failed(),
            trades_skipped: self.trades_skipped(),
            volume: self.volume(),
            markets_traded: self.markets_traded(),
            shadow_orders_fired: self.shadow_orders_fired(),
            shadow_orders_filled: self.shadow_orders_filled(),
            circuit_breaker_trips: self.circuit_breaker_trips(),
            success_rate: self.success_rate(),
            conversion_rate: self.conversion_rate(),
            events_per_second: self.events_per_second(),
            trades_per_hour: self.trades_per_hour(),
        }
    }

    /// Reset all counters (for new session).
    pub fn reset(&mut self) {
        self.session_id = Uuid::new_v4();
        self.started_at = Utc::now();
        self.events_processed.store(0, Ordering::Relaxed);
        self.opportunities_detected.store(0, Ordering::Relaxed);
        self.trades_executed.store(0, Ordering::Relaxed);
        self.trades_failed.store(0, Ordering::Relaxed);
        self.trades_skipped.store(0, Ordering::Relaxed);
        self.shadow_orders_fired.store(0, Ordering::Relaxed);
        self.shadow_orders_filled.store(0, Ordering::Relaxed);
        self.circuit_breaker_trips.store(0, Ordering::Relaxed);
        self.volume_cents.store(0, Ordering::Relaxed);
        self.markets_traded.store(0, Ordering::Relaxed);
        self.arb_opportunities.store(0, Ordering::Relaxed);
        self.directional_opportunities.store(0, Ordering::Relaxed);
        self.maker_opportunities.store(0, Ordering::Relaxed);
        self.consecutive_successes.store(0, Ordering::Relaxed);
        self.consecutive_failures.store(0, Ordering::Relaxed);
    }
}

impl Default for SessionStats {
    fn default() -> Self {
        Self::new(BotMode::Paper)
    }
}

/// Engine type for opportunity tracking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum EngineType {
    Arbitrage,
    Directional,
    Maker,
}

impl std::fmt::Display for EngineType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EngineType::Arbitrage => write!(f, "arbitrage"),
            EngineType::Directional => write!(f, "directional"),
            EngineType::Maker => write!(f, "maker"),
        }
    }
}

/// Summary of session statistics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatsSummary {
    pub session_id: Uuid,
    pub mode: BotMode,
    pub started_at: DateTime<Utc>,
    pub duration_secs: i64,
    pub events_processed: u64,
    pub opportunities_detected: u64,
    pub arb_opportunities: u64,
    pub directional_opportunities: u64,
    pub maker_opportunities: u64,
    pub trades_executed: u64,
    pub trades_failed: u64,
    pub trades_skipped: u64,
    pub volume: Decimal,
    pub markets_traded: u64,
    pub shadow_orders_fired: u64,
    pub shadow_orders_filled: u64,
    pub circuit_breaker_trips: u64,
    pub success_rate: Decimal,
    pub conversion_rate: Decimal,
    pub events_per_second: Decimal,
    pub trades_per_hour: Decimal,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_session_stats() {
        let stats = SessionStats::new(BotMode::Paper);

        // Record some activity
        stats.record_event();
        stats.record_event();
        stats.record_opportunity(EngineType::Directional);
        stats.record_trade_executed(dec!(100));
        stats.record_trade_failed();

        assert_eq!(stats.events_processed(), 2);
        assert_eq!(stats.opportunities_detected(), 1);
        assert_eq!(stats.directional_opportunities(), 1);
        assert_eq!(stats.trades_executed(), 1);
        assert_eq!(stats.trades_failed(), 1);
        assert_eq!(stats.volume(), dec!(100));
        assert_eq!(stats.success_rate(), dec!(50)); // 1 success, 1 failure
    }

    #[test]
    fn test_consecutive_tracking() {
        let stats = SessionStats::new(BotMode::Paper);

        // Record successes
        stats.record_trade_executed(dec!(10));
        stats.record_trade_executed(dec!(10));
        stats.record_trade_executed(dec!(10));
        assert_eq!(stats.consecutive_successes(), 3);
        assert_eq!(stats.consecutive_failures(), 0);

        // Record failure - resets success streak
        stats.record_trade_failed();
        assert_eq!(stats.consecutive_successes(), 0);
        assert_eq!(stats.consecutive_failures(), 1);
    }
}
