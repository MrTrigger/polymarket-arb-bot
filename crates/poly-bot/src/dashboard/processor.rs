//! Background processor for dashboard events.
//!
//! Receives `DashboardEvent`s from the capture channel and writes them to ClickHouse
//! in batches. Handles graceful shutdown with final flush.
//!
//! ## Architecture
//!
//! ```text
//! Hot Path                    Background Processor
//! ────────                    ────────────────────
//! [Strategy/Executor]         [DashboardProcessor]
//!     │                            │
//!     │ try_send()                 │ recv()
//!     ▼                            ▼
//! [Bounded Channel] ──────► [Buffer by type] ──► [ClickHouse]
//! ```
//!
//! ## Features
//!
//! - Batches events by type for efficient bulk inserts
//! - Configurable batch size and flush interval
//! - Graceful shutdown with final flush
//! - Statistics tracking for monitoring

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use clickhouse::Row;
use rust_decimal::prelude::ToPrimitive;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tokio::time::interval;

use poly_common::{ClickHouseClient, ClickHouseError};

use super::capture::DashboardCaptureReceiver;
use super::types::{
    DashboardEvent, LogEntry, MarketSessionUpdate, PnlSnapshot, SessionRecord, TradeRecord,
};

// ============================================================================
// ClickHouse Row Types
// ============================================================================

/// Session record for ClickHouse storage.
///
/// Matches the `sessions` table schema.
#[derive(Debug, Clone, Serialize, Deserialize, Row)]
pub struct SessionRow {
    #[serde(with = "clickhouse::serde::uuid")]
    pub session_id: uuid::Uuid,
    pub mode: String,
    #[serde(with = "clickhouse::serde::time::datetime64::millis")]
    pub start_time: time::OffsetDateTime,
    #[serde(with = "clickhouse::serde::time::datetime64::millis::option")]
    pub end_time: Option<time::OffsetDateTime>,
    pub config_hash: String,
    pub total_pnl: f64,
    pub total_volume: f64,
    pub trades_executed: u32,
    pub trades_failed: u32,
    pub trades_skipped: u32,
    pub opportunities_detected: u64,
    pub events_processed: u64,
    pub markets_traded: Vec<String>,
    pub exit_reason: String,
}

impl From<&SessionRecord> for SessionRow {
    fn from(r: &SessionRecord) -> Self {
        Self {
            session_id: r.session_id,
            mode: r.mode.as_str().to_string(),
            start_time: datetime_to_offset(r.start_time),
            end_time: r.end_time.map(datetime_to_offset),
            config_hash: r.config_hash.clone(),
            total_pnl: r.total_pnl.to_f64().unwrap_or(0.0),
            total_volume: r.total_volume.to_f64().unwrap_or(0.0),
            trades_executed: r.trades_executed,
            trades_failed: r.trades_failed,
            trades_skipped: r.trades_skipped,
            opportunities_detected: r.opportunities_detected,
            events_processed: r.events_processed,
            markets_traded: r.markets_traded.clone(),
            exit_reason: r.exit_reason.map(|e| e.as_str().to_string()).unwrap_or_default(),
        }
    }
}

/// Trade record for ClickHouse storage.
///
/// Matches the `bot_trades` table schema.
#[derive(Debug, Clone, Serialize, Deserialize, Row)]
pub struct TradeRow {
    #[serde(with = "clickhouse::serde::uuid")]
    pub trade_id: uuid::Uuid,
    #[serde(with = "clickhouse::serde::uuid")]
    pub session_id: uuid::Uuid,
    pub decision_id: u64,
    pub event_id: String,
    pub token_id: String,
    pub outcome: String,
    pub side: String,
    pub order_type: String,
    pub requested_price: f64,
    pub requested_size: f64,
    pub fill_price: f64,
    pub fill_size: f64,
    pub slippage_bps: i32,
    pub fees: f64,
    pub total_cost: f64,
    #[serde(with = "clickhouse::serde::time::datetime64::millis")]
    pub order_time: time::OffsetDateTime,
    #[serde(with = "clickhouse::serde::time::datetime64::millis")]
    pub fill_time: time::OffsetDateTime,
    pub latency_ms: u32,
    pub spot_price_at_fill: f64,
    pub arb_margin_at_fill: f64,
    pub status: String,
}

impl From<&TradeRecord> for TradeRow {
    fn from(r: &TradeRecord) -> Self {
        Self {
            trade_id: r.trade_id,
            session_id: r.session_id,
            decision_id: r.decision_id,
            event_id: r.event_id.clone(),
            token_id: r.token_id.clone(),
            outcome: format!("{:?}", r.outcome).to_uppercase(),
            side: r.side.as_str().to_string(),
            order_type: r.order_type.as_str().to_string(),
            requested_price: r.requested_price.to_f64().unwrap_or(0.0),
            requested_size: r.requested_size.to_f64().unwrap_or(0.0),
            fill_price: r.fill_price.to_f64().unwrap_or(0.0),
            fill_size: r.fill_size.to_f64().unwrap_or(0.0),
            slippage_bps: r.slippage_bps,
            fees: r.fees.to_f64().unwrap_or(0.0),
            total_cost: r.total_cost.to_f64().unwrap_or(0.0),
            order_time: datetime_to_offset(r.order_time),
            fill_time: datetime_to_offset(r.fill_time),
            latency_ms: r.latency_ms,
            spot_price_at_fill: r.spot_price_at_fill.to_f64().unwrap_or(0.0),
            arb_margin_at_fill: r.arb_margin_at_fill.to_f64().unwrap_or(0.0),
            status: r.status.as_str().to_string(),
        }
    }
}

/// PnL snapshot for ClickHouse storage.
///
/// Matches the `pnl_snapshots` table schema.
#[derive(Debug, Clone, Serialize, Deserialize, Row)]
pub struct PnlRow {
    #[serde(with = "clickhouse::serde::uuid")]
    pub session_id: uuid::Uuid,
    #[serde(with = "clickhouse::serde::time::datetime64::millis")]
    pub timestamp: time::OffsetDateTime,
    pub trigger: String,
    pub realized_pnl: f64,
    pub unrealized_pnl: f64,
    pub total_pnl: f64,
    pub total_exposure: f64,
    pub yes_exposure: f64,
    pub no_exposure: f64,
    pub cumulative_volume: f64,
    pub cumulative_fees: f64,
    pub trade_count: u32,
    pub max_drawdown: f64,
    pub current_drawdown: f64,
}

impl From<&PnlSnapshot> for PnlRow {
    fn from(r: &PnlSnapshot) -> Self {
        Self {
            session_id: r.session_id,
            timestamp: datetime_to_offset(r.timestamp),
            trigger: r.trigger.as_str().to_string(),
            realized_pnl: r.realized_pnl.to_f64().unwrap_or(0.0),
            unrealized_pnl: r.unrealized_pnl.to_f64().unwrap_or(0.0),
            total_pnl: r.total_pnl.to_f64().unwrap_or(0.0),
            total_exposure: r.total_exposure.to_f64().unwrap_or(0.0),
            yes_exposure: r.yes_exposure.to_f64().unwrap_or(0.0),
            no_exposure: r.no_exposure.to_f64().unwrap_or(0.0),
            cumulative_volume: r.cumulative_volume.to_f64().unwrap_or(0.0),
            cumulative_fees: r.cumulative_fees.to_f64().unwrap_or(0.0),
            trade_count: r.trade_count,
            max_drawdown: r.max_drawdown.to_f64().unwrap_or(0.0),
            current_drawdown: r.current_drawdown.to_f64().unwrap_or(0.0),
        }
    }
}

/// Log entry for ClickHouse storage.
///
/// Matches the `structured_logs` table schema.
#[derive(Debug, Clone, Serialize, Deserialize, Row)]
pub struct LogRow {
    #[serde(with = "clickhouse::serde::uuid")]
    pub session_id: uuid::Uuid,
    #[serde(with = "clickhouse::serde::time::datetime64::millis")]
    pub timestamp: time::OffsetDateTime,
    pub level: String,
    pub target: String,
    pub message: String,
    pub event_id: Option<String>,
    pub token_id: Option<String>,
    #[serde(with = "clickhouse::serde::uuid::option")]
    pub trade_id: Option<uuid::Uuid>,
    pub fields: String,
}

impl From<&LogEntry> for LogRow {
    fn from(r: &LogEntry) -> Self {
        Self {
            session_id: r.session_id,
            timestamp: datetime_to_offset(r.timestamp),
            level: r.level.as_str().to_string(),
            target: r.target.clone(),
            message: r.message.clone(),
            event_id: r.event_id.clone(),
            token_id: r.token_id.clone(),
            trade_id: r.trade_id,
            fields: r.fields.clone(),
        }
    }
}

/// Market session for ClickHouse storage.
///
/// Matches the `market_sessions` table schema.
#[derive(Debug, Clone, Serialize, Deserialize, Row)]
pub struct MarketSessionRow {
    #[serde(with = "clickhouse::serde::uuid")]
    pub session_id: uuid::Uuid,
    pub event_id: String,
    pub asset: String,
    pub strike_price: f64,
    #[serde(with = "clickhouse::serde::time::datetime64::millis")]
    pub window_start: time::OffsetDateTime,
    #[serde(with = "clickhouse::serde::time::datetime64::millis")]
    pub window_end: time::OffsetDateTime,
    #[serde(with = "clickhouse::serde::time::datetime64::millis::option")]
    pub first_trade_time: Option<time::OffsetDateTime>,
    pub yes_shares: f64,
    pub no_shares: f64,
    pub yes_cost_basis: f64,
    pub no_cost_basis: f64,
    pub realized_pnl: f64,
    pub unrealized_pnl: f64,
    pub trades_count: u32,
    pub opportunities_count: u32,
    pub skipped_count: u32,
    pub volume: f64,
    pub fees: f64,
    pub settlement_outcome: Option<String>,
    pub settlement_pnl: Option<f64>,
}

impl From<&MarketSessionUpdate> for MarketSessionRow {
    fn from(r: &MarketSessionUpdate) -> Self {
        Self {
            session_id: r.session_id,
            event_id: r.event_id.clone(),
            asset: format!("{:?}", r.asset).to_uppercase(),
            strike_price: r.strike_price.to_f64().unwrap_or(0.0),
            window_start: datetime_to_offset(r.window_start),
            window_end: datetime_to_offset(r.window_end),
            first_trade_time: r.first_trade_time.map(datetime_to_offset),
            yes_shares: r.yes_shares.to_f64().unwrap_or(0.0),
            no_shares: r.no_shares.to_f64().unwrap_or(0.0),
            yes_cost_basis: r.yes_cost_basis.to_f64().unwrap_or(0.0),
            no_cost_basis: r.no_cost_basis.to_f64().unwrap_or(0.0),
            realized_pnl: r.realized_pnl.to_f64().unwrap_or(0.0),
            unrealized_pnl: r.unrealized_pnl.to_f64().unwrap_or(0.0),
            trades_count: r.trades_count,
            opportunities_count: r.opportunities_count,
            skipped_count: r.skipped_count,
            volume: r.volume.to_f64().unwrap_or(0.0),
            fees: r.fees.to_f64().unwrap_or(0.0),
            settlement_outcome: r.settlement_outcome.map(|o| format!("{:?}", o).to_uppercase()),
            settlement_pnl: r.settlement_pnl.and_then(|d| d.to_f64()),
        }
    }
}

/// Convert chrono DateTime to time OffsetDateTime.
fn datetime_to_offset(dt: chrono::DateTime<chrono::Utc>) -> time::OffsetDateTime {
    time::OffsetDateTime::from_unix_timestamp(dt.timestamp())
        .unwrap_or_else(|_| time::OffsetDateTime::now_utc())
}

// ============================================================================
// Processor Configuration
// ============================================================================

/// Configuration for the dashboard processor.
#[derive(Debug, Clone)]
pub struct DashboardProcessorConfig {
    /// Maximum records to buffer before flush per type.
    pub batch_size: usize,
    /// Maximum time between flushes.
    pub flush_interval: Duration,
    /// Maximum buffer size (drops oldest if exceeded).
    pub max_buffer_size: usize,
}

impl Default for DashboardProcessorConfig {
    fn default() -> Self {
        Self {
            batch_size: 100,
            flush_interval: Duration::from_secs(5),
            max_buffer_size: 1000,
        }
    }
}

impl DashboardProcessorConfig {
    /// Create a test config with small values.
    #[cfg(test)]
    pub fn test_config() -> Self {
        Self {
            batch_size: 10,
            flush_interval: Duration::from_millis(100),
            max_buffer_size: 50,
        }
    }
}

// ============================================================================
// Processor Statistics
// ============================================================================

/// Statistics for the dashboard processor.
#[derive(Debug, Default)]
pub struct DashboardProcessorStats {
    /// Total events received.
    pub received: AtomicU64,
    /// Total events written to ClickHouse.
    pub written: AtomicU64,
    /// Total events dropped (buffer full).
    pub dropped: AtomicU64,
    /// Total flush operations.
    pub flushes: AtomicU64,
    /// Total write errors.
    pub write_errors: AtomicU64,
    /// Sessions written.
    pub sessions_written: AtomicU64,
    /// Trades written.
    pub trades_written: AtomicU64,
    /// PnL snapshots written.
    pub pnl_written: AtomicU64,
    /// Logs written.
    pub logs_written: AtomicU64,
    /// Market sessions written.
    pub market_sessions_written: AtomicU64,
}

impl DashboardProcessorStats {
    /// Create new stats.
    pub fn new() -> Self {
        Self::default()
    }

    /// Get snapshot of current stats.
    pub fn snapshot(&self) -> DashboardProcessorStatsSnapshot {
        DashboardProcessorStatsSnapshot {
            received: self.received.load(Ordering::Relaxed),
            written: self.written.load(Ordering::Relaxed),
            dropped: self.dropped.load(Ordering::Relaxed),
            flushes: self.flushes.load(Ordering::Relaxed),
            write_errors: self.write_errors.load(Ordering::Relaxed),
            sessions_written: self.sessions_written.load(Ordering::Relaxed),
            trades_written: self.trades_written.load(Ordering::Relaxed),
            pnl_written: self.pnl_written.load(Ordering::Relaxed),
            logs_written: self.logs_written.load(Ordering::Relaxed),
            market_sessions_written: self.market_sessions_written.load(Ordering::Relaxed),
        }
    }

    /// Reset all counters.
    pub fn reset(&self) {
        self.received.store(0, Ordering::Relaxed);
        self.written.store(0, Ordering::Relaxed);
        self.dropped.store(0, Ordering::Relaxed);
        self.flushes.store(0, Ordering::Relaxed);
        self.write_errors.store(0, Ordering::Relaxed);
        self.sessions_written.store(0, Ordering::Relaxed);
        self.trades_written.store(0, Ordering::Relaxed);
        self.pnl_written.store(0, Ordering::Relaxed);
        self.logs_written.store(0, Ordering::Relaxed);
        self.market_sessions_written.store(0, Ordering::Relaxed);
    }
}

/// Snapshot of processor statistics.
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct DashboardProcessorStatsSnapshot {
    pub received: u64,
    pub written: u64,
    pub dropped: u64,
    pub flushes: u64,
    pub write_errors: u64,
    pub sessions_written: u64,
    pub trades_written: u64,
    pub pnl_written: u64,
    pub logs_written: u64,
    pub market_sessions_written: u64,
}

impl DashboardProcessorStatsSnapshot {
    /// Calculate drop rate as percentage.
    pub fn drop_rate(&self) -> f64 {
        if self.received == 0 {
            0.0
        } else {
            (self.dropped as f64 / self.received as f64) * 100.0
        }
    }

    /// Calculate write success rate as percentage.
    pub fn write_success_rate(&self) -> f64 {
        let total_attempts = self.written + self.write_errors;
        if total_attempts == 0 {
            100.0
        } else {
            (self.written as f64 / total_attempts as f64) * 100.0
        }
    }
}

// ============================================================================
// Event Buffers
// ============================================================================

/// Buffers for each event type.
#[derive(Default)]
struct EventBuffers {
    sessions: VecDeque<SessionRow>,
    trades: VecDeque<TradeRow>,
    pnl: VecDeque<PnlRow>,
    logs: VecDeque<LogRow>,
    market_sessions: VecDeque<MarketSessionRow>,
}

impl EventBuffers {
    fn new() -> Self {
        Self::default()
    }

    fn total_len(&self) -> usize {
        self.sessions.len()
            + self.trades.len()
            + self.pnl.len()
            + self.logs.len()
            + self.market_sessions.len()
    }

    fn is_empty(&self) -> bool {
        self.total_len() == 0
    }
}

// ============================================================================
// Dashboard Processor
// ============================================================================

/// Background processor for dashboard events.
///
/// Receives events from the capture channel, buffers by type,
/// and writes to ClickHouse in batches.
pub struct DashboardProcessor {
    config: DashboardProcessorConfig,
    stats: Arc<DashboardProcessorStats>,
}

impl DashboardProcessor {
    /// Create a new processor.
    pub fn new(config: DashboardProcessorConfig) -> Self {
        Self {
            config,
            stats: Arc::new(DashboardProcessorStats::new()),
        }
    }

    /// Create with default config.
    pub fn with_defaults() -> Self {
        Self::new(DashboardProcessorConfig::default())
    }

    /// Get statistics.
    pub fn stats(&self) -> &DashboardProcessorStats {
        &self.stats
    }

    /// Get shared stats handle.
    pub fn stats_handle(&self) -> Arc<DashboardProcessorStats> {
        Arc::clone(&self.stats)
    }

    /// Get stats snapshot.
    pub fn stats_snapshot(&self) -> DashboardProcessorStatsSnapshot {
        self.stats.snapshot()
    }

    /// Run the processor loop.
    ///
    /// This should be spawned as a background task. It will run until
    /// the receiver is closed or shutdown is signaled.
    pub async fn run(
        &self,
        mut receiver: DashboardCaptureReceiver,
        client: ClickHouseClient,
        mut shutdown: broadcast::Receiver<()>,
    ) {
        let mut buffers = EventBuffers::new();
        let mut flush_timer = interval(self.config.flush_interval);
        flush_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        tracing::info!(
            batch_size = self.config.batch_size,
            flush_interval_ms = self.config.flush_interval.as_millis(),
            "Dashboard processor started"
        );

        loop {
            tokio::select! {
                // Receive event from channel
                event = receiver.recv() => {
                    match event {
                        Some(evt) => {
                            self.stats.received.fetch_add(1, Ordering::Relaxed);
                            self.buffer_event(&mut buffers, evt);

                            // Flush if any buffer reaches batch size
                            if self.should_flush(&buffers) {
                                self.flush_buffers(&mut buffers, &client).await;
                            }
                        }
                        None => {
                            // Channel closed, final flush and exit
                            tracing::info!("Dashboard capture channel closed, performing final flush");
                            self.flush_buffers(&mut buffers, &client).await;
                            break;
                        }
                    }
                }

                // Periodic flush
                _ = flush_timer.tick() => {
                    if !buffers.is_empty() {
                        self.flush_buffers(&mut buffers, &client).await;
                    }
                }

                // Shutdown signal
                _ = shutdown.recv() => {
                    tracing::info!("Dashboard processor shutdown signal received, performing final flush");
                    self.flush_buffers(&mut buffers, &client).await;
                    break;
                }
            }
        }

        let stats = self.stats_snapshot();
        tracing::info!(
            received = stats.received,
            written = stats.written,
            dropped = stats.dropped,
            write_errors = stats.write_errors,
            "Dashboard processor stopped"
        );
    }

    /// Buffer an event by type.
    fn buffer_event(&self, buffers: &mut EventBuffers, event: DashboardEvent) {
        match event {
            DashboardEvent::Session(record) => {
                self.add_to_buffer(&mut buffers.sessions, SessionRow::from(&record));
            }
            DashboardEvent::Trade(record) => {
                self.add_to_buffer(&mut buffers.trades, TradeRow::from(&record));
            }
            DashboardEvent::Pnl(record) => {
                self.add_to_buffer(&mut buffers.pnl, PnlRow::from(&record));
            }
            DashboardEvent::Log(record) => {
                self.add_to_buffer(&mut buffers.logs, LogRow::from(&record));
            }
            DashboardEvent::MarketSession(record) => {
                self.add_to_buffer(&mut buffers.market_sessions, MarketSessionRow::from(&record));
            }
        }
    }

    /// Add record to buffer with backpressure handling.
    fn add_to_buffer<T>(&self, buffer: &mut VecDeque<T>, record: T) {
        if buffer.len() >= self.config.max_buffer_size {
            buffer.pop_front();
            self.stats.dropped.fetch_add(1, Ordering::Relaxed);
        }
        buffer.push_back(record);
    }

    /// Check if any buffer should be flushed.
    fn should_flush(&self, buffers: &EventBuffers) -> bool {
        buffers.sessions.len() >= self.config.batch_size
            || buffers.trades.len() >= self.config.batch_size
            || buffers.pnl.len() >= self.config.batch_size
            || buffers.logs.len() >= self.config.batch_size
            || buffers.market_sessions.len() >= self.config.batch_size
    }

    /// Flush all buffers to ClickHouse.
    async fn flush_buffers(&self, buffers: &mut EventBuffers, client: &ClickHouseClient) {
        self.stats.flushes.fetch_add(1, Ordering::Relaxed);

        // Flush each buffer type
        self.flush_sessions(buffers, client).await;
        self.flush_trades(buffers, client).await;
        self.flush_pnl(buffers, client).await;
        self.flush_logs(buffers, client).await;
        self.flush_market_sessions(buffers, client).await;
    }

    async fn flush_sessions(&self, buffers: &mut EventBuffers, client: &ClickHouseClient) {
        if buffers.sessions.is_empty() {
            return;
        }

        let records: Vec<_> = buffers.sessions.drain(..).collect();
        let count = records.len();

        match Self::write_rows(client, "sessions", &records).await {
            Ok(()) => {
                self.stats.written.fetch_add(count as u64, Ordering::Relaxed);
                self.stats.sessions_written.fetch_add(count as u64, Ordering::Relaxed);
                tracing::debug!(count, "Flushed sessions to ClickHouse");
            }
            Err(e) => {
                self.stats.write_errors.fetch_add(count as u64, Ordering::Relaxed);
                tracing::error!(error = %e, count, "Failed to write sessions to ClickHouse");
            }
        }
    }

    async fn flush_trades(&self, buffers: &mut EventBuffers, client: &ClickHouseClient) {
        if buffers.trades.is_empty() {
            return;
        }

        let records: Vec<_> = buffers.trades.drain(..).collect();
        let count = records.len();

        match Self::write_rows(client, "bot_trades", &records).await {
            Ok(()) => {
                self.stats.written.fetch_add(count as u64, Ordering::Relaxed);
                self.stats.trades_written.fetch_add(count as u64, Ordering::Relaxed);
                tracing::debug!(count, "Flushed trades to ClickHouse");
            }
            Err(e) => {
                self.stats.write_errors.fetch_add(count as u64, Ordering::Relaxed);
                tracing::error!(error = %e, count, "Failed to write trades to ClickHouse");
            }
        }
    }

    async fn flush_pnl(&self, buffers: &mut EventBuffers, client: &ClickHouseClient) {
        if buffers.pnl.is_empty() {
            return;
        }

        let records: Vec<_> = buffers.pnl.drain(..).collect();
        let count = records.len();

        match Self::write_rows(client, "pnl_snapshots", &records).await {
            Ok(()) => {
                self.stats.written.fetch_add(count as u64, Ordering::Relaxed);
                self.stats.pnl_written.fetch_add(count as u64, Ordering::Relaxed);
                tracing::debug!(count, "Flushed PnL snapshots to ClickHouse");
            }
            Err(e) => {
                self.stats.write_errors.fetch_add(count as u64, Ordering::Relaxed);
                tracing::error!(error = %e, count, "Failed to write PnL snapshots to ClickHouse");
            }
        }
    }

    async fn flush_logs(&self, buffers: &mut EventBuffers, client: &ClickHouseClient) {
        if buffers.logs.is_empty() {
            return;
        }

        let records: Vec<_> = buffers.logs.drain(..).collect();
        let count = records.len();

        match Self::write_rows(client, "structured_logs", &records).await {
            Ok(()) => {
                self.stats.written.fetch_add(count as u64, Ordering::Relaxed);
                self.stats.logs_written.fetch_add(count as u64, Ordering::Relaxed);
                tracing::debug!(count, "Flushed logs to ClickHouse");
            }
            Err(e) => {
                self.stats.write_errors.fetch_add(count as u64, Ordering::Relaxed);
                tracing::error!(error = %e, count, "Failed to write logs to ClickHouse");
            }
        }
    }

    async fn flush_market_sessions(&self, buffers: &mut EventBuffers, client: &ClickHouseClient) {
        if buffers.market_sessions.is_empty() {
            return;
        }

        let records: Vec<_> = buffers.market_sessions.drain(..).collect();
        let count = records.len();

        match Self::write_rows(client, "market_sessions", &records).await {
            Ok(()) => {
                self.stats.written.fetch_add(count as u64, Ordering::Relaxed);
                self.stats.market_sessions_written.fetch_add(count as u64, Ordering::Relaxed);
                tracing::debug!(count, "Flushed market sessions to ClickHouse");
            }
            Err(e) => {
                self.stats.write_errors.fetch_add(count as u64, Ordering::Relaxed);
                tracing::error!(error = %e, count, "Failed to write market sessions to ClickHouse");
            }
        }
    }

    /// Write rows to ClickHouse.
    async fn write_rows<T: Row + Serialize>(
        client: &ClickHouseClient,
        table: &str,
        records: &[T],
    ) -> Result<(), ClickHouseError> {
        if records.is_empty() {
            return Ok(());
        }

        let mut insert = client.inner().insert(table)?;
        for record in records {
            insert.write(record).await?;
        }
        insert.end().await?;

        Ok(())
    }
}

/// Shared processor for use across tasks.
pub type SharedDashboardProcessor = Arc<DashboardProcessor>;

/// Create a shared processor.
pub fn create_shared_dashboard_processor(
    config: DashboardProcessorConfig,
) -> SharedDashboardProcessor {
    Arc::new(DashboardProcessor::new(config))
}

/// Spawn the processor as a background task.
///
/// Returns a handle that can be used to wait for completion.
pub fn spawn_dashboard_processor(
    processor: &DashboardProcessor,
    receiver: DashboardCaptureReceiver,
    client: ClickHouseClient,
    shutdown: broadcast::Receiver<()>,
) -> tokio::task::JoinHandle<()> {
    let config = processor.config.clone();
    let stats = processor.stats_handle();

    tokio::spawn(async move {
        let proc = DashboardProcessor {
            config,
            stats,
        };
        proc.run(receiver, client, shutdown).await;
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dashboard::types::{BotMode, DashboardOrderType, LogLevel, PnlTrigger, TradeSide};
    use chrono::Utc;
    use poly_common::types::{CryptoAsset, Outcome as CommonOutcome};
    use rust_decimal_macros::dec;
    use uuid::Uuid;

    fn make_test_session() -> SessionRecord {
        SessionRecord::new(BotMode::Paper, "test_hash".to_string())
    }

    fn make_test_trade() -> TradeRecord {
        let mut trade = TradeRecord::new(
            Uuid::new_v4(),
            1,
            "event123".to_string(),
            "token456".to_string(),
            CommonOutcome::Yes,
            TradeSide::Buy,
            DashboardOrderType::Market,
            dec!(0.45),
            dec!(100),
        );
        trade.record_fill(dec!(0.46), dec!(100), dec!(0.46), dec!(100000), dec!(0.03));
        trade
    }

    fn make_test_pnl() -> PnlSnapshot {
        PnlSnapshot::new(Uuid::new_v4(), PnlTrigger::Periodic)
    }

    fn make_test_log() -> LogEntry {
        LogEntry::new(
            Uuid::new_v4(),
            LogLevel::Info,
            "poly_bot::test".to_string(),
            "Test message".to_string(),
        )
    }

    fn make_test_market_session() -> MarketSessionUpdate {
        MarketSessionUpdate::new(
            Uuid::new_v4(),
            "event123".to_string(),
            CryptoAsset::Btc,
            dec!(100000),
            Utc::now(),
            Utc::now() + chrono::Duration::minutes(15),
        )
    }

    #[test]
    fn test_session_row_conversion() {
        let session = make_test_session();
        let row = SessionRow::from(&session);

        assert_eq!(row.session_id, session.session_id);
        assert_eq!(row.mode, "paper");
        assert_eq!(row.config_hash, "test_hash");
    }

    #[test]
    fn test_trade_row_conversion() {
        let trade = make_test_trade();
        let row = TradeRow::from(&trade);

        assert_eq!(row.trade_id, trade.trade_id);
        assert_eq!(row.event_id, "event123");
        assert_eq!(row.outcome, "YES");
        assert_eq!(row.side, "BUY");
        assert_eq!(row.status, "FILLED");
    }

    #[test]
    fn test_pnl_row_conversion() {
        let pnl = make_test_pnl();
        let row = PnlRow::from(&pnl);

        assert_eq!(row.session_id, pnl.session_id);
        assert_eq!(row.trigger, "periodic");
    }

    #[test]
    fn test_log_row_conversion() {
        let log = make_test_log();
        let row = LogRow::from(&log);

        assert_eq!(row.session_id, log.session_id);
        assert_eq!(row.level, "INFO");
        assert_eq!(row.target, "poly_bot::test");
        assert_eq!(row.message, "Test message");
    }

    #[test]
    fn test_market_session_row_conversion() {
        let market = make_test_market_session();
        let row = MarketSessionRow::from(&market);

        assert_eq!(row.session_id, market.session_id);
        assert_eq!(row.event_id, "event123");
        assert_eq!(row.asset, "BTC");
    }

    #[test]
    fn test_processor_config_default() {
        let config = DashboardProcessorConfig::default();

        assert_eq!(config.batch_size, 100);
        assert_eq!(config.flush_interval, Duration::from_secs(5));
        assert_eq!(config.max_buffer_size, 1000);
    }

    #[test]
    fn test_processor_stats_snapshot() {
        let stats = DashboardProcessorStats::new();

        stats.received.store(100, Ordering::Relaxed);
        stats.written.store(90, Ordering::Relaxed);
        stats.dropped.store(5, Ordering::Relaxed);
        stats.write_errors.store(5, Ordering::Relaxed);

        let snapshot = stats.snapshot();

        assert_eq!(snapshot.received, 100);
        assert_eq!(snapshot.written, 90);
        assert_eq!(snapshot.dropped, 5);
        assert_eq!(snapshot.write_errors, 5);
    }

    #[test]
    fn test_processor_stats_drop_rate() {
        let snapshot = DashboardProcessorStatsSnapshot {
            received: 100,
            dropped: 10,
            ..Default::default()
        };

        assert!((snapshot.drop_rate() - 10.0).abs() < 0.001);
    }

    #[test]
    fn test_processor_stats_write_success_rate() {
        let snapshot = DashboardProcessorStatsSnapshot {
            written: 90,
            write_errors: 10,
            ..Default::default()
        };

        assert!((snapshot.write_success_rate() - 90.0).abs() < 0.001);
    }

    #[test]
    fn test_processor_stats_reset() {
        let stats = DashboardProcessorStats::new();

        stats.received.store(100, Ordering::Relaxed);
        stats.written.store(90, Ordering::Relaxed);

        stats.reset();

        assert_eq!(stats.received.load(Ordering::Relaxed), 0);
        assert_eq!(stats.written.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn test_event_buffers() {
        let buffers = EventBuffers::new();

        assert!(buffers.is_empty());
        assert_eq!(buffers.total_len(), 0);
    }

    #[test]
    fn test_processor_creation() {
        let processor = DashboardProcessor::with_defaults();

        let stats = processor.stats_snapshot();
        assert_eq!(stats.received, 0);
        assert_eq!(stats.written, 0);
    }

    #[test]
    fn test_processor_buffer_event() {
        let processor = DashboardProcessor::with_defaults();
        let mut buffers = EventBuffers::new();

        // Buffer a session event
        processor.buffer_event(&mut buffers, DashboardEvent::Session(make_test_session()));
        assert_eq!(buffers.sessions.len(), 1);

        // Buffer a trade event
        processor.buffer_event(&mut buffers, DashboardEvent::Trade(make_test_trade()));
        assert_eq!(buffers.trades.len(), 1);

        // Buffer a PnL event
        processor.buffer_event(&mut buffers, DashboardEvent::Pnl(make_test_pnl()));
        assert_eq!(buffers.pnl.len(), 1);

        // Buffer a log event
        processor.buffer_event(&mut buffers, DashboardEvent::Log(make_test_log()));
        assert_eq!(buffers.logs.len(), 1);

        // Buffer a market session event
        processor.buffer_event(&mut buffers, DashboardEvent::MarketSession(make_test_market_session()));
        assert_eq!(buffers.market_sessions.len(), 1);

        assert_eq!(buffers.total_len(), 5);
    }

    #[test]
    fn test_processor_buffer_backpressure() {
        let config = DashboardProcessorConfig {
            max_buffer_size: 3,
            ..Default::default()
        };
        let processor = DashboardProcessor::new(config);
        let mut buffers = EventBuffers::new();

        // Add 4 sessions to buffer with max size 3
        for _ in 0..4 {
            processor.buffer_event(&mut buffers, DashboardEvent::Session(make_test_session()));
        }

        // Should have dropped one
        assert_eq!(buffers.sessions.len(), 3);
        assert_eq!(processor.stats_snapshot().dropped, 1);
    }

    #[test]
    fn test_processor_should_flush() {
        let config = DashboardProcessorConfig {
            batch_size: 2,
            ..Default::default()
        };
        let processor = DashboardProcessor::new(config);
        let mut buffers = EventBuffers::new();

        // Should not flush with 1 event
        processor.buffer_event(&mut buffers, DashboardEvent::Session(make_test_session()));
        assert!(!processor.should_flush(&buffers));

        // Should flush with 2 events
        processor.buffer_event(&mut buffers, DashboardEvent::Session(make_test_session()));
        assert!(processor.should_flush(&buffers));
    }

    #[tokio::test]
    async fn test_processor_receive_events() {
        use tokio::sync::mpsc;

        let (tx, rx) = mpsc::channel(10);
        let (_shutdown_tx, shutdown_rx) = broadcast::channel(1);

        let processor = DashboardProcessor::with_defaults();

        // Send some events
        tx.send(DashboardEvent::Session(make_test_session())).await.unwrap();
        tx.send(DashboardEvent::Trade(make_test_trade())).await.unwrap();

        // Close the channel
        drop(tx);

        // Create a mock client (won't actually connect)
        let client = poly_common::ClickHouseClient::with_defaults();

        // Run processor - it should receive events and exit when channel closes
        // Note: This will try to write to ClickHouse and fail, but we're testing
        // the receive and buffer logic, not the actual writes
        processor.run(rx, client, shutdown_rx).await;

        let stats = processor.stats_snapshot();
        assert_eq!(stats.received, 2);
        // Write errors expected since no ClickHouse running
        assert!(stats.write_errors > 0 || stats.flushes > 0);
    }

    #[test]
    fn test_datetime_conversion() {
        let now = chrono::Utc::now();
        let offset = datetime_to_offset(now);

        assert_eq!(offset.unix_timestamp(), now.timestamp());
    }

    #[test]
    fn test_create_shared_processor() {
        let processor = create_shared_dashboard_processor(DashboardProcessorConfig::default());

        assert_eq!(processor.stats_snapshot().received, 0);
    }
}
