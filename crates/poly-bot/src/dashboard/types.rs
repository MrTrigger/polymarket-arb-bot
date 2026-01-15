//! Dashboard event types for the React trading dashboard.
//!
//! These types map to the ClickHouse dashboard tables defined in schema.sql:
//! - `sessions` - Bot session lifecycle
//! - `bot_trades` - Executed trades with fill details
//! - `pnl_snapshots` - Periodic P&L for equity curve
//! - `structured_logs` - Searchable log entries
//! - `market_sessions` - Per-market metrics within a session
//!
//! CRITICAL: Use `rust_decimal::Decimal` for all financial calculations.
//! ClickHouse storage uses Float64 because clickhouse-rs doesn't support rust_decimal.

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use poly_common::types::{CryptoAsset, Outcome};

// ============================================================================
// Session Types
// ============================================================================

/// Bot operating mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BotMode {
    /// Live trading with real money.
    Live,
    /// Paper trading (simulated fills).
    Paper,
    /// Shadow mode (observe only, no orders).
    Shadow,
    /// Backtesting against historical data.
    Backtest,
}

impl BotMode {
    /// Convert to string for ClickHouse storage.
    pub fn as_str(&self) -> &'static str {
        match self {
            BotMode::Live => "live",
            BotMode::Paper => "paper",
            BotMode::Shadow => "shadow",
            BotMode::Backtest => "backtest",
        }
    }
}

impl std::fmt::Display for BotMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// Reason for session exit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExitReason {
    /// Normal graceful shutdown.
    Graceful,
    /// Crashed or unexpected termination.
    Crash,
    /// Stopped by circuit breaker.
    CircuitBreaker,
    /// Manual stop by user.
    Manual,
}

impl ExitReason {
    /// Convert to string for ClickHouse storage.
    pub fn as_str(&self) -> &'static str {
        match self {
            ExitReason::Graceful => "graceful",
            ExitReason::Crash => "crash",
            ExitReason::CircuitBreaker => "circuit_breaker",
            ExitReason::Manual => "manual",
        }
    }
}

impl std::fmt::Display for ExitReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// Session record for tracking bot runs.
///
/// Maps to the `sessions` table in ClickHouse.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRecord {
    /// Unique session identifier.
    pub session_id: Uuid,
    /// Operating mode for this session.
    pub mode: BotMode,
    /// Session start time.
    pub start_time: DateTime<Utc>,
    /// Session end time (None if still running).
    pub end_time: Option<DateTime<Utc>>,
    /// SHA256 hash of config for reproducibility.
    pub config_hash: String,
    /// Total realized P&L for the session.
    pub total_pnl: Decimal,
    /// Total trading volume.
    pub total_volume: Decimal,
    /// Number of trades executed.
    pub trades_executed: u32,
    /// Number of trades that failed.
    pub trades_failed: u32,
    /// Number of opportunities skipped.
    pub trades_skipped: u32,
    /// Total arbitrage opportunities detected.
    pub opportunities_detected: u64,
    /// Total market events processed.
    pub events_processed: u64,
    /// List of event IDs traded in this session.
    pub markets_traded: Vec<String>,
    /// Reason for session exit (None if still running).
    pub exit_reason: Option<ExitReason>,
}

impl SessionRecord {
    /// Create a new session record.
    pub fn new(mode: BotMode, config_hash: String) -> Self {
        Self {
            session_id: Uuid::new_v4(),
            mode,
            start_time: Utc::now(),
            end_time: None,
            config_hash,
            total_pnl: Decimal::ZERO,
            total_volume: Decimal::ZERO,
            trades_executed: 0,
            trades_failed: 0,
            trades_skipped: 0,
            opportunities_detected: 0,
            events_processed: 0,
            markets_traded: Vec::new(),
            exit_reason: None,
        }
    }

    /// Mark the session as ended.
    pub fn end(&mut self, reason: ExitReason) {
        self.end_time = Some(Utc::now());
        self.exit_reason = Some(reason);
    }

    /// Check if the session is still running.
    pub fn is_running(&self) -> bool {
        self.end_time.is_none()
    }

    /// Duration of the session in seconds.
    pub fn duration_secs(&self) -> i64 {
        let end = self.end_time.unwrap_or_else(Utc::now);
        (end - self.start_time).num_seconds()
    }

    /// Win rate (trades with positive P&L / total trades).
    pub fn win_rate(&self) -> Option<Decimal> {
        if self.trades_executed == 0 {
            return None;
        }
        // Note: This needs actual win/loss tracking, simplified for now
        None
    }
}

// ============================================================================
// Trade Types
// ============================================================================

/// Order type for the trade.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum DashboardOrderType {
    /// Market order (immediate execution).
    Market,
    /// Limit order (price-specified).
    Limit,
    /// Shadow order (pre-hashed, conditional).
    Shadow,
}

impl DashboardOrderType {
    /// Convert to string for ClickHouse storage.
    pub fn as_str(&self) -> &'static str {
        match self {
            DashboardOrderType::Market => "MARKET",
            DashboardOrderType::Limit => "LIMIT",
            DashboardOrderType::Shadow => "SHADOW",
        }
    }
}

/// Trade execution status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum TradeStatus {
    /// Fully filled.
    Filled,
    /// Partially filled.
    Partial,
    /// Cancelled before fill.
    Cancelled,
    /// Failed to execute.
    Failed,
}

impl TradeStatus {
    /// Convert to string for ClickHouse storage.
    pub fn as_str(&self) -> &'static str {
        match self {
            TradeStatus::Filled => "FILLED",
            TradeStatus::Partial => "PARTIAL",
            TradeStatus::Cancelled => "CANCELLED",
            TradeStatus::Failed => "FAILED",
        }
    }
}

/// Trade side (buy/sell).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum TradeSide {
    /// Buy order.
    Buy,
    /// Sell order.
    Sell,
}

impl TradeSide {
    /// Convert to string for ClickHouse storage.
    pub fn as_str(&self) -> &'static str {
        match self {
            TradeSide::Buy => "BUY",
            TradeSide::Sell => "SELL",
        }
    }
}

impl From<poly_common::types::Side> for TradeSide {
    fn from(side: poly_common::types::Side) -> Self {
        match side {
            poly_common::types::Side::Buy => TradeSide::Buy,
            poly_common::types::Side::Sell => TradeSide::Sell,
        }
    }
}

/// Record of a trade executed by the bot.
///
/// Maps to the `bot_trades` table in ClickHouse.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TradeRecord {
    /// Unique trade identifier.
    pub trade_id: Uuid,
    /// Session this trade belongs to.
    pub session_id: Uuid,
    /// Decision ID that triggered this trade.
    pub decision_id: u64,
    /// Event ID for the market.
    pub event_id: String,
    /// Token ID that was traded.
    pub token_id: String,
    /// Outcome side (YES/NO).
    pub outcome: Outcome,
    /// Trade side (BUY/SELL).
    pub side: TradeSide,
    /// Order type.
    pub order_type: DashboardOrderType,
    /// Requested price.
    pub requested_price: Decimal,
    /// Requested size.
    pub requested_size: Decimal,
    /// Actual fill price.
    pub fill_price: Decimal,
    /// Actual fill size.
    pub fill_size: Decimal,
    /// Slippage in basis points.
    pub slippage_bps: i32,
    /// Trading fees paid.
    pub fees: Decimal,
    /// Total cost of the trade (price * size + fees).
    pub total_cost: Decimal,
    /// When the order was placed.
    pub order_time: DateTime<Utc>,
    /// When the fill occurred.
    pub fill_time: DateTime<Utc>,
    /// Latency from order to fill in milliseconds.
    pub latency_ms: u32,
    /// Spot price at the time of fill.
    pub spot_price_at_fill: Decimal,
    /// Arbitrage margin at time of fill.
    pub arb_margin_at_fill: Decimal,
    /// Execution status.
    pub status: TradeStatus,
}

impl TradeRecord {
    /// Create a new trade record.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        session_id: Uuid,
        decision_id: u64,
        event_id: String,
        token_id: String,
        outcome: Outcome,
        side: TradeSide,
        order_type: DashboardOrderType,
        requested_price: Decimal,
        requested_size: Decimal,
    ) -> Self {
        let now = Utc::now();
        Self {
            trade_id: Uuid::new_v4(),
            session_id,
            decision_id,
            event_id,
            token_id,
            outcome,
            side,
            order_type,
            requested_price,
            requested_size,
            fill_price: Decimal::ZERO,
            fill_size: Decimal::ZERO,
            slippage_bps: 0,
            fees: Decimal::ZERO,
            total_cost: Decimal::ZERO,
            order_time: now,
            fill_time: now,
            latency_ms: 0,
            spot_price_at_fill: Decimal::ZERO,
            arb_margin_at_fill: Decimal::ZERO,
            status: TradeStatus::Failed,
        }
    }

    /// Record a successful fill.
    pub fn record_fill(
        &mut self,
        fill_price: Decimal,
        fill_size: Decimal,
        fees: Decimal,
        spot_price: Decimal,
        arb_margin: Decimal,
    ) {
        self.fill_time = Utc::now();
        self.fill_price = fill_price;
        self.fill_size = fill_size;
        self.fees = fees;
        self.total_cost = fill_price * fill_size + fees;
        self.spot_price_at_fill = spot_price;
        self.arb_margin_at_fill = arb_margin;
        self.latency_ms = (self.fill_time - self.order_time).num_milliseconds().max(0) as u32;

        // Calculate slippage
        if self.requested_price > Decimal::ZERO {
            let slippage = (fill_price - self.requested_price) / self.requested_price;
            self.slippage_bps = (slippage * Decimal::new(10000, 0))
                .try_into()
                .unwrap_or(0);
        }

        // Determine status
        self.status = if fill_size >= self.requested_size {
            TradeStatus::Filled
        } else if fill_size > Decimal::ZERO {
            TradeStatus::Partial
        } else {
            TradeStatus::Failed
        };
    }

    /// Mark as cancelled.
    pub fn cancel(&mut self) {
        self.status = TradeStatus::Cancelled;
    }

    /// Check if this was a profitable trade (simplified).
    pub fn is_profitable(&self) -> bool {
        // For arb trades, profit comes from combined cost < 1.0
        // This is a simplified check; actual P&L depends on settlement
        self.arb_margin_at_fill > Decimal::ZERO
    }
}

// ============================================================================
// P&L Snapshot Types
// ============================================================================

/// Trigger for a P&L snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PnlTrigger {
    /// Periodic snapshot (e.g., every 30 seconds).
    Periodic,
    /// Triggered by a trade.
    Trade,
    /// Triggered by market settlement.
    Settlement,
}

impl PnlTrigger {
    /// Convert to string for ClickHouse storage.
    pub fn as_str(&self) -> &'static str {
        match self {
            PnlTrigger::Periodic => "periodic",
            PnlTrigger::Trade => "trade",
            PnlTrigger::Settlement => "settlement",
        }
    }
}

/// P&L snapshot for equity curve visualization.
///
/// Maps to the `pnl_snapshots` table in ClickHouse.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PnlSnapshot {
    /// Session this snapshot belongs to.
    pub session_id: Uuid,
    /// Snapshot timestamp.
    pub timestamp: DateTime<Utc>,
    /// What triggered this snapshot.
    pub trigger: PnlTrigger,
    /// Realized P&L (closed positions).
    pub realized_pnl: Decimal,
    /// Unrealized P&L (open positions at current prices).
    pub unrealized_pnl: Decimal,
    /// Total P&L (realized + unrealized).
    pub total_pnl: Decimal,
    /// Total position exposure.
    pub total_exposure: Decimal,
    /// Exposure to YES outcomes.
    pub yes_exposure: Decimal,
    /// Exposure to NO outcomes.
    pub no_exposure: Decimal,
    /// Cumulative trading volume.
    pub cumulative_volume: Decimal,
    /// Cumulative fees paid.
    pub cumulative_fees: Decimal,
    /// Total trade count.
    pub trade_count: u32,
    /// Maximum drawdown seen.
    pub max_drawdown: Decimal,
    /// Current drawdown from peak.
    pub current_drawdown: Decimal,
}

impl PnlSnapshot {
    /// Create a new P&L snapshot.
    pub fn new(session_id: Uuid, trigger: PnlTrigger) -> Self {
        Self {
            session_id,
            timestamp: Utc::now(),
            trigger,
            realized_pnl: Decimal::ZERO,
            unrealized_pnl: Decimal::ZERO,
            total_pnl: Decimal::ZERO,
            total_exposure: Decimal::ZERO,
            yes_exposure: Decimal::ZERO,
            no_exposure: Decimal::ZERO,
            cumulative_volume: Decimal::ZERO,
            cumulative_fees: Decimal::ZERO,
            trade_count: 0,
            max_drawdown: Decimal::ZERO,
            current_drawdown: Decimal::ZERO,
        }
    }
}

// ============================================================================
// Log Types
// ============================================================================

/// Log level for structured logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum LogLevel {
    /// Trace level (most verbose).
    Trace,
    /// Debug level.
    Debug,
    /// Info level.
    Info,
    /// Warning level.
    Warn,
    /// Error level (most severe).
    Error,
}

impl LogLevel {
    /// Convert to string for ClickHouse storage.
    pub fn as_str(&self) -> &'static str {
        match self {
            LogLevel::Trace => "TRACE",
            LogLevel::Debug => "DEBUG",
            LogLevel::Info => "INFO",
            LogLevel::Warn => "WARN",
            LogLevel::Error => "ERROR",
        }
    }
}

impl From<tracing::Level> for LogLevel {
    fn from(level: tracing::Level) -> Self {
        match level {
            tracing::Level::TRACE => LogLevel::Trace,
            tracing::Level::DEBUG => LogLevel::Debug,
            tracing::Level::INFO => LogLevel::Info,
            tracing::Level::WARN => LogLevel::Warn,
            tracing::Level::ERROR => LogLevel::Error,
        }
    }
}

/// Structured log entry for searchable logging.
///
/// Maps to the `structured_logs` table in ClickHouse.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEntry {
    /// Session this log belongs to.
    pub session_id: Uuid,
    /// Log timestamp.
    pub timestamp: DateTime<Utc>,
    /// Log level.
    pub level: LogLevel,
    /// Module path (e.g., "poly_bot::strategy::arb").
    pub target: String,
    /// Log message.
    pub message: String,
    /// Associated event ID (if applicable).
    pub event_id: Option<String>,
    /// Associated token ID (if applicable).
    pub token_id: Option<String>,
    /// Associated trade ID (if applicable).
    pub trade_id: Option<Uuid>,
    /// Additional structured fields as JSON.
    pub fields: String,
}

impl LogEntry {
    /// Create a new log entry.
    pub fn new(session_id: Uuid, level: LogLevel, target: String, message: String) -> Self {
        Self {
            session_id,
            timestamp: Utc::now(),
            level,
            target,
            message,
            event_id: None,
            token_id: None,
            trade_id: None,
            fields: "{}".to_string(),
        }
    }

    /// Add event context.
    pub fn with_event(mut self, event_id: impl Into<String>) -> Self {
        self.event_id = Some(event_id.into());
        self
    }

    /// Add token context.
    pub fn with_token(mut self, token_id: impl Into<String>) -> Self {
        self.token_id = Some(token_id.into());
        self
    }

    /// Add trade context.
    pub fn with_trade(mut self, trade_id: Uuid) -> Self {
        self.trade_id = Some(trade_id);
        self
    }

    /// Add structured fields.
    pub fn with_fields(mut self, fields: impl Serialize) -> Self {
        self.fields = serde_json::to_string(&fields).unwrap_or_else(|_| "{}".to_string());
        self
    }
}

// ============================================================================
// Market Session Types
// ============================================================================

/// Per-market metrics within a trading session.
///
/// Maps to the `market_sessions` table in ClickHouse.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketSessionUpdate {
    /// Session this belongs to.
    pub session_id: Uuid,
    /// Event ID for the market.
    pub event_id: String,
    /// Asset this market tracks.
    pub asset: CryptoAsset,
    /// Strike price.
    pub strike_price: Decimal,
    /// Window start time.
    pub window_start: DateTime<Utc>,
    /// Window end time.
    pub window_end: DateTime<Utc>,
    /// Time of first trade in this market (if any).
    pub first_trade_time: Option<DateTime<Utc>>,
    /// YES shares held.
    pub yes_shares: Decimal,
    /// NO shares held.
    pub no_shares: Decimal,
    /// Cost basis for YES shares.
    pub yes_cost_basis: Decimal,
    /// Cost basis for NO shares.
    pub no_cost_basis: Decimal,
    /// Realized P&L from this market.
    pub realized_pnl: Decimal,
    /// Unrealized P&L at current prices.
    pub unrealized_pnl: Decimal,
    /// Number of trades in this market.
    pub trades_count: u32,
    /// Number of opportunities detected.
    pub opportunities_count: u32,
    /// Number of opportunities skipped.
    pub skipped_count: u32,
    /// Trading volume in this market.
    pub volume: Decimal,
    /// Fees paid in this market.
    pub fees: Decimal,
    /// Settlement outcome (None if not settled).
    pub settlement_outcome: Option<Outcome>,
    /// P&L from settlement (None if not settled).
    pub settlement_pnl: Option<Decimal>,
}

impl MarketSessionUpdate {
    /// Create a new market session record.
    pub fn new(
        session_id: Uuid,
        event_id: String,
        asset: CryptoAsset,
        strike_price: Decimal,
        window_start: DateTime<Utc>,
        window_end: DateTime<Utc>,
    ) -> Self {
        Self {
            session_id,
            event_id,
            asset,
            strike_price,
            window_start,
            window_end,
            first_trade_time: None,
            yes_shares: Decimal::ZERO,
            no_shares: Decimal::ZERO,
            yes_cost_basis: Decimal::ZERO,
            no_cost_basis: Decimal::ZERO,
            realized_pnl: Decimal::ZERO,
            unrealized_pnl: Decimal::ZERO,
            trades_count: 0,
            opportunities_count: 0,
            skipped_count: 0,
            volume: Decimal::ZERO,
            fees: Decimal::ZERO,
            settlement_outcome: None,
            settlement_pnl: None,
        }
    }

    /// Record a trade in this market.
    pub fn record_trade(&mut self, outcome: Outcome, size: Decimal, cost: Decimal, fees: Decimal) {
        if self.first_trade_time.is_none() {
            self.first_trade_time = Some(Utc::now());
        }

        match outcome {
            Outcome::Yes => {
                self.yes_shares += size;
                self.yes_cost_basis += cost;
            }
            Outcome::No => {
                self.no_shares += size;
                self.no_cost_basis += cost;
            }
        }

        self.trades_count += 1;
        self.volume += cost;
        self.fees += fees;
    }

    /// Record an opportunity (detected or skipped).
    pub fn record_opportunity(&mut self, was_skipped: bool) {
        self.opportunities_count += 1;
        if was_skipped {
            self.skipped_count += 1;
        }
    }

    /// Record settlement.
    pub fn record_settlement(&mut self, outcome: Outcome) {
        self.settlement_outcome = Some(outcome);

        // Calculate settlement P&L
        let settlement_value = match outcome {
            Outcome::Yes => self.yes_shares,
            Outcome::No => self.no_shares,
        };
        let total_cost = self.yes_cost_basis + self.no_cost_basis;
        self.settlement_pnl = Some(settlement_value - total_cost);
    }

    /// Total position cost basis.
    pub fn total_cost_basis(&self) -> Decimal {
        self.yes_cost_basis + self.no_cost_basis
    }

    /// Check if we have any position in this market.
    pub fn has_position(&self) -> bool {
        self.yes_shares > Decimal::ZERO || self.no_shares > Decimal::ZERO
    }
}

// ============================================================================
// Dashboard Event Enum
// ============================================================================

/// Event types for the dashboard capture channel.
///
/// These events are sent from the hot path to the background processor
/// for ClickHouse storage.
#[derive(Debug, Clone)]
pub enum DashboardEvent {
    /// Session lifecycle event.
    Session(SessionRecord),
    /// Trade executed.
    Trade(TradeRecord),
    /// P&L snapshot.
    Pnl(PnlSnapshot),
    /// Log entry.
    Log(LogEntry),
    /// Market session update.
    MarketSession(MarketSessionUpdate),
}

impl DashboardEvent {
    /// Get the event type name for logging.
    pub fn event_type(&self) -> &'static str {
        match self {
            DashboardEvent::Session(_) => "session",
            DashboardEvent::Trade(_) => "trade",
            DashboardEvent::Pnl(_) => "pnl",
            DashboardEvent::Log(_) => "log",
            DashboardEvent::MarketSession(_) => "market_session",
        }
    }

    /// Get the session ID for this event.
    pub fn session_id(&self) -> Uuid {
        match self {
            DashboardEvent::Session(s) => s.session_id,
            DashboardEvent::Trade(t) => t.session_id,
            DashboardEvent::Pnl(p) => p.session_id,
            DashboardEvent::Log(l) => l.session_id,
            DashboardEvent::MarketSession(m) => m.session_id,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn test_session_record_new() {
        let session = SessionRecord::new(BotMode::Paper, "abc123".to_string());
        assert!(session.is_running());
        assert_eq!(session.mode, BotMode::Paper);
        assert_eq!(session.config_hash, "abc123");
        assert_eq!(session.total_pnl, Decimal::ZERO);
        assert!(session.exit_reason.is_none());
    }

    #[test]
    fn test_session_record_end() {
        let mut session = SessionRecord::new(BotMode::Live, "hash".to_string());
        assert!(session.is_running());

        session.end(ExitReason::Graceful);
        assert!(!session.is_running());
        assert_eq!(session.exit_reason, Some(ExitReason::Graceful));
        assert!(session.end_time.is_some());
    }

    #[test]
    fn test_trade_record_fill() {
        let mut trade = TradeRecord::new(
            Uuid::new_v4(),
            1,
            "event123".to_string(),
            "token456".to_string(),
            Outcome::Yes,
            TradeSide::Buy,
            DashboardOrderType::Market,
            dec!(0.45),
            dec!(100),
        );

        trade.record_fill(
            dec!(0.46),  // fill_price
            dec!(100),   // fill_size
            dec!(0.46),  // fees
            dec!(100500), // spot_price
            dec!(0.03),  // arb_margin
        );

        assert_eq!(trade.status, TradeStatus::Filled);
        assert_eq!(trade.fill_price, dec!(0.46));
        assert_eq!(trade.fill_size, dec!(100));
        assert!(trade.slippage_bps > 0); // Bought higher than requested
    }

    #[test]
    fn test_pnl_snapshot_new() {
        let snapshot = PnlSnapshot::new(Uuid::new_v4(), PnlTrigger::Periodic);
        assert_eq!(snapshot.trigger, PnlTrigger::Periodic);
        assert_eq!(snapshot.total_pnl, Decimal::ZERO);
    }

    #[test]
    fn test_log_entry_with_context() {
        let entry = LogEntry::new(
            Uuid::new_v4(),
            LogLevel::Info,
            "poly_bot::strategy".to_string(),
            "Arb opportunity detected".to_string(),
        )
        .with_event("event123")
        .with_token("token456");

        assert_eq!(entry.event_id, Some("event123".to_string()));
        assert_eq!(entry.token_id, Some("token456".to_string()));
    }

    #[test]
    fn test_market_session_update() {
        let mut market = MarketSessionUpdate::new(
            Uuid::new_v4(),
            "event123".to_string(),
            CryptoAsset::Btc,
            dec!(100000),
            Utc::now(),
            Utc::now() + chrono::Duration::minutes(15),
        );

        assert!(!market.has_position());

        market.record_trade(Outcome::Yes, dec!(100), dec!(45), dec!(0.45));
        market.record_trade(Outcome::No, dec!(100), dec!(52), dec!(0.52));

        assert!(market.has_position());
        assert_eq!(market.trades_count, 2);
        assert_eq!(market.yes_shares, dec!(100));
        assert_eq!(market.no_shares, dec!(100));
        assert_eq!(market.total_cost_basis(), dec!(97));
    }

    #[test]
    fn test_market_session_settlement() {
        let mut market = MarketSessionUpdate::new(
            Uuid::new_v4(),
            "event123".to_string(),
            CryptoAsset::Btc,
            dec!(100000),
            Utc::now(),
            Utc::now() + chrono::Duration::minutes(15),
        );

        market.record_trade(Outcome::Yes, dec!(100), dec!(45), dec!(0.45));
        market.record_trade(Outcome::No, dec!(100), dec!(52), dec!(0.52));

        // YES wins: 100 * $1.00 = $100, cost = $97, P&L = $3
        market.record_settlement(Outcome::Yes);

        assert_eq!(market.settlement_outcome, Some(Outcome::Yes));
        assert_eq!(market.settlement_pnl, Some(dec!(3)));
    }

    #[test]
    fn test_dashboard_event_type() {
        let session = SessionRecord::new(BotMode::Paper, "hash".to_string());
        let event = DashboardEvent::Session(session);
        assert_eq!(event.event_type(), "session");
    }

    #[test]
    fn test_bot_mode_display() {
        assert_eq!(BotMode::Live.as_str(), "live");
        assert_eq!(BotMode::Paper.as_str(), "paper");
        assert_eq!(format!("{}", BotMode::Shadow), "shadow");
    }

    #[test]
    fn test_log_level_ordering() {
        assert!(LogLevel::Error > LogLevel::Warn);
        assert!(LogLevel::Warn > LogLevel::Info);
        assert!(LogLevel::Info > LogLevel::Debug);
        assert!(LogLevel::Debug > LogLevel::Trace);
    }

    #[test]
    fn test_trade_side_from_common() {
        let buy: TradeSide = poly_common::types::Side::Buy.into();
        let sell: TradeSide = poly_common::types::Side::Sell.into();
        assert_eq!(buy, TradeSide::Buy);
        assert_eq!(sell, TradeSide::Sell);
    }

    #[test]
    fn test_trade_record_cancel() {
        let mut trade = TradeRecord::new(
            Uuid::new_v4(),
            1,
            "event123".to_string(),
            "token456".to_string(),
            Outcome::Yes,
            TradeSide::Buy,
            DashboardOrderType::Limit,
            dec!(0.45),
            dec!(100),
        );

        trade.cancel();
        assert_eq!(trade.status, TradeStatus::Cancelled);
    }

    #[test]
    fn test_serialization() {
        let session = SessionRecord::new(BotMode::Paper, "hash".to_string());
        let json = serde_json::to_string(&session).unwrap();
        let parsed: SessionRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.mode, session.mode);
        assert_eq!(parsed.config_hash, session.config_hash);
    }
}
