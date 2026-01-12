//! Async background processor for observability events.
//!
//! Receives `DecisionSnapshot` events from the hot path capture channel,
//! enriches them with string identifiers, buffers for batch writes, and
//! stores to ClickHouse.
//!
//! ## Architecture
//!
//! ```text
//! Hot Path                    Background Processor
//! ────────                    ────────────────────
//! [Strategy]                  [ObservabilityProcessor]
//!     │                            │
//!     │ try_send()                 │ recv()
//!     ▼                            ▼
//! [Bounded Channel] ──────► [Enrich] ──► [Buffer] ──► [ClickHouse]
//! ```
//!
//! ## Features
//!
//! - Async enrichment from hash to full strings using IdLookup trait
//! - Configurable batch size and flush interval
//! - Backpressure handling (drops oldest on buffer full)
//! - Graceful shutdown with final flush
//! - Statistics tracking for monitoring

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use clickhouse::Row;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, RwLock};
use tokio::time::interval;

use poly_common::{ClickHouseClient, ClickHouseError};

use super::capture::CaptureReceiver;
use super::types::{ActionType, Counterfactual, DecisionSnapshot, ObservabilityEvent};

/// Decision record for ClickHouse storage.
///
/// Matches the `decisions` table schema.
#[derive(Debug, Clone, Serialize, Deserialize, Row)]
pub struct DecisionRecord {
    /// Unique decision ID.
    pub decision_id: u64,
    /// Event ID string.
    pub event_id: String,
    /// Decision timestamp.
    pub timestamp: DateTime<Utc>,
    /// Decision type (Execute, Skip*, etc.).
    pub decision_type: String,
    /// YES ask price.
    #[serde(with = "rust_decimal::serde::str")]
    pub yes_ask: Decimal,
    /// NO ask price.
    #[serde(with = "rust_decimal::serde::str")]
    pub no_ask: Decimal,
    /// Combined cost (YES + NO).
    #[serde(with = "rust_decimal::serde::str")]
    pub combined_cost: Decimal,
    /// Arbitrage margin.
    #[serde(with = "rust_decimal::serde::str")]
    pub arb_margin: Decimal,
    /// Spot price at decision time.
    #[serde(with = "rust_decimal::serde::str")]
    pub spot_price: Decimal,
    /// Seconds remaining in window.
    pub time_remaining_secs: u32,
    /// Action taken (execute, skip).
    pub action: String,
    /// Reason for skip (if applicable).
    pub reason: String,
    /// Confidence score (0-100).
    pub confidence: f32,
}

impl DecisionRecord {
    /// Create from DecisionSnapshot with enriched strings.
    pub fn from_snapshot(
        snapshot: &DecisionSnapshot,
        event_id: String,
        spot_price: Option<Decimal>,
    ) -> Self {
        let action_type = ActionType::from(snapshot.action);

        Self {
            decision_id: snapshot.decision_id,
            event_id,
            timestamp: DateTime::from_timestamp_millis(snapshot.timestamp_ms)
                .unwrap_or_else(Utc::now),
            decision_type: format!("{:?}", action_type),
            yes_ask: Decimal::new(snapshot.yes_ask_bps as i64, 4),
            no_ask: Decimal::new(snapshot.no_ask_bps as i64, 4),
            combined_cost: Decimal::new(snapshot.combined_cost_bps as i64, 4),
            arb_margin: Decimal::new(snapshot.margin_bps as i64, 4),
            spot_price: spot_price.unwrap_or(Decimal::ZERO),
            time_remaining_secs: snapshot.seconds_remaining as u32,
            action: if action_type == ActionType::Execute {
                "execute".to_string()
            } else {
                "skip".to_string()
            },
            reason: Self::reason_from_action(action_type),
            confidence: snapshot.confidence as f32,
        }
    }

    fn reason_from_action(action: ActionType) -> String {
        match action {
            ActionType::Execute => String::new(),
            ActionType::SkipSizing => "sizing_constraint".to_string(),
            ActionType::SkipToxic => "toxic_flow".to_string(),
            ActionType::SkipDisabled => "trading_disabled".to_string(),
            ActionType::SkipCircuitBreaker => "circuit_breaker".to_string(),
            ActionType::SkipRisk => "risk_check_failed".to_string(),
            ActionType::SkipTime => "insufficient_time".to_string(),
            ActionType::SkipConfidence => "low_confidence".to_string(),
        }
    }
}

/// Trait for looking up string identifiers from pre-hashed u64 values.
///
/// Implement this to provide the mapping from event_id hashes to full strings.
pub trait IdLookup: Send + Sync {
    /// Look up event ID from hash.
    fn lookup_event_id(&self, hash: u64) -> Option<String>;

    /// Look up YES token ID from hash.
    fn lookup_yes_token(&self, hash: u64) -> Option<String>;

    /// Look up NO token ID from hash.
    fn lookup_no_token(&self, hash: u64) -> Option<String>;

    /// Look up spot price for an asset.
    fn lookup_spot_price(&self, asset: u8) -> Option<Decimal>;
}

/// Simple in-memory ID lookup backed by HashMaps.
///
/// Thread-safe with RwLock for concurrent read/write access.
#[derive(Debug, Default)]
pub struct InMemoryIdLookup {
    event_ids: RwLock<HashMap<u64, String>>,
    yes_tokens: RwLock<HashMap<u64, String>>,
    no_tokens: RwLock<HashMap<u64, String>>,
    spot_prices: RwLock<HashMap<u8, Decimal>>,
}

impl InMemoryIdLookup {
    /// Create a new empty lookup.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register an event ID mapping.
    pub async fn register_event_id(&self, hash: u64, event_id: String) {
        self.event_ids.write().await.insert(hash, event_id);
    }

    /// Register a YES token ID mapping.
    pub async fn register_yes_token(&self, hash: u64, token_id: String) {
        self.yes_tokens.write().await.insert(hash, token_id);
    }

    /// Register a NO token ID mapping.
    pub async fn register_no_token(&self, hash: u64, token_id: String) {
        self.no_tokens.write().await.insert(hash, token_id);
    }

    /// Update spot price for an asset.
    pub async fn update_spot_price(&self, asset: u8, price: Decimal) {
        self.spot_prices.write().await.insert(asset, price);
    }

    /// Register all IDs for a market window.
    ///
    /// Computes hashes internally using FNV-1a.
    pub async fn register_market(
        &self,
        event_id: &str,
        yes_token_id: &str,
        no_token_id: &str,
    ) {
        let event_hash = hash_string(event_id);
        let yes_hash = hash_string(yes_token_id);
        let no_hash = hash_string(no_token_id);

        self.register_event_id(event_hash, event_id.to_string()).await;
        self.register_yes_token(yes_hash, yes_token_id.to_string()).await;
        self.register_no_token(no_hash, no_token_id.to_string()).await;
    }

    /// Get current number of registered events.
    pub async fn event_count(&self) -> usize {
        self.event_ids.read().await.len()
    }
}

impl IdLookup for InMemoryIdLookup {
    fn lookup_event_id(&self, hash: u64) -> Option<String> {
        // Use blocking read for sync trait method
        // In practice, the processor should use async methods directly
        self.event_ids.blocking_read().get(&hash).cloned()
    }

    fn lookup_yes_token(&self, hash: u64) -> Option<String> {
        self.yes_tokens.blocking_read().get(&hash).cloned()
    }

    fn lookup_no_token(&self, hash: u64) -> Option<String> {
        self.no_tokens.blocking_read().get(&hash).cloned()
    }

    fn lookup_spot_price(&self, asset: u8) -> Option<Decimal> {
        self.spot_prices.blocking_read().get(&asset).copied()
    }
}

/// Async-friendly lookup methods for InMemoryIdLookup.
impl InMemoryIdLookup {
    /// Async version of lookup_event_id.
    pub async fn lookup_event_id_async(&self, hash: u64) -> Option<String> {
        self.event_ids.read().await.get(&hash).cloned()
    }

    /// Async version of lookup_yes_token.
    pub async fn lookup_yes_token_async(&self, hash: u64) -> Option<String> {
        self.yes_tokens.read().await.get(&hash).cloned()
    }

    /// Async version of lookup_no_token.
    pub async fn lookup_no_token_async(&self, hash: u64) -> Option<String> {
        self.no_tokens.read().await.get(&hash).cloned()
    }

    /// Async version of lookup_spot_price.
    pub async fn lookup_spot_price_async(&self, asset: u8) -> Option<Decimal> {
        self.spot_prices.read().await.get(&asset).copied()
    }
}

/// FNV-1a hash for pre-hashing event IDs to u64.
///
/// Must match the hash function used in types.rs SnapshotBuilder.
#[inline]
pub fn hash_string(s: &str) -> u64 {
    const FNV_OFFSET: u64 = 0xcbf29ce484222325;
    const FNV_PRIME: u64 = 0x100000001b3;

    let mut hash = FNV_OFFSET;
    for byte in s.bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

/// Configuration for the observability processor.
#[derive(Debug, Clone)]
pub struct ProcessorConfig {
    /// Maximum records to buffer before flush.
    pub batch_size: usize,
    /// Maximum time between flushes.
    pub flush_interval: Duration,
    /// Maximum buffer size (drops oldest if exceeded).
    pub max_buffer_size: usize,
    /// Whether to process counterfactuals.
    pub process_counterfactuals: bool,
    /// Whether to process anomalies.
    pub process_anomalies: bool,
}

impl Default for ProcessorConfig {
    fn default() -> Self {
        Self {
            batch_size: 1000,
            flush_interval: Duration::from_secs(5),
            max_buffer_size: 10000,
            process_counterfactuals: true,
            process_anomalies: true,
        }
    }
}

impl ProcessorConfig {
    /// Create from ObservabilityConfig.
    pub fn from_observability_config(config: &crate::config::ObservabilityConfig) -> Self {
        Self {
            batch_size: config.batch_size,
            flush_interval: Duration::from_secs(config.flush_interval_secs),
            max_buffer_size: config.channel_buffer_size * 2, // 2x channel capacity
            process_counterfactuals: config.capture_counterfactuals,
            process_anomalies: config.detect_anomalies,
        }
    }
}

/// Statistics for the processor.
#[derive(Debug, Default)]
pub struct ProcessorStats {
    /// Total events received.
    pub received: AtomicU64,
    /// Total events processed (enriched).
    pub processed: AtomicU64,
    /// Total events written to ClickHouse.
    pub written: AtomicU64,
    /// Total events dropped (buffer full).
    pub dropped: AtomicU64,
    /// Total flush operations.
    pub flushes: AtomicU64,
    /// Total write errors.
    pub write_errors: AtomicU64,
    /// Total enrichment failures (missing ID lookup).
    pub enrichment_failures: AtomicU64,
}

impl ProcessorStats {
    /// Create new stats.
    pub fn new() -> Self {
        Self::default()
    }

    /// Get snapshot of current stats.
    pub fn snapshot(&self) -> ProcessorStatsSnapshot {
        ProcessorStatsSnapshot {
            received: self.received.load(Ordering::Relaxed),
            processed: self.processed.load(Ordering::Relaxed),
            written: self.written.load(Ordering::Relaxed),
            dropped: self.dropped.load(Ordering::Relaxed),
            flushes: self.flushes.load(Ordering::Relaxed),
            write_errors: self.write_errors.load(Ordering::Relaxed),
            enrichment_failures: self.enrichment_failures.load(Ordering::Relaxed),
        }
    }

    /// Reset all counters.
    pub fn reset(&self) {
        self.received.store(0, Ordering::Relaxed);
        self.processed.store(0, Ordering::Relaxed);
        self.written.store(0, Ordering::Relaxed);
        self.dropped.store(0, Ordering::Relaxed);
        self.flushes.store(0, Ordering::Relaxed);
        self.write_errors.store(0, Ordering::Relaxed);
        self.enrichment_failures.store(0, Ordering::Relaxed);
    }
}

/// Snapshot of processor statistics.
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct ProcessorStatsSnapshot {
    pub received: u64,
    pub processed: u64,
    pub written: u64,
    pub dropped: u64,
    pub flushes: u64,
    pub write_errors: u64,
    pub enrichment_failures: u64,
}

impl ProcessorStatsSnapshot {
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
        let total_writes = self.written + self.write_errors;
        if total_writes == 0 {
            100.0
        } else {
            (self.written as f64 / total_writes as f64) * 100.0
        }
    }
}

/// Background processor for observability events.
///
/// Receives events from the capture channel, enriches them with string IDs,
/// buffers for batch writes, and stores to ClickHouse.
pub struct ObservabilityProcessor {
    config: ProcessorConfig,
    stats: Arc<ProcessorStats>,
    id_lookup: Arc<InMemoryIdLookup>,
}

impl ObservabilityProcessor {
    /// Create a new processor.
    pub fn new(config: ProcessorConfig, id_lookup: Arc<InMemoryIdLookup>) -> Self {
        Self {
            config,
            stats: Arc::new(ProcessorStats::new()),
            id_lookup,
        }
    }

    /// Create with default config.
    pub fn with_defaults(id_lookup: Arc<InMemoryIdLookup>) -> Self {
        Self::new(ProcessorConfig::default(), id_lookup)
    }

    /// Get statistics.
    pub fn stats(&self) -> &ProcessorStats {
        &self.stats
    }

    /// Get shared stats handle.
    pub fn stats_handle(&self) -> Arc<ProcessorStats> {
        Arc::clone(&self.stats)
    }

    /// Get stats snapshot.
    pub fn stats_snapshot(&self) -> ProcessorStatsSnapshot {
        self.stats.snapshot()
    }

    /// Run the processor loop.
    ///
    /// This should be spawned as a background task. It will run until
    /// the receiver is closed or shutdown is signaled.
    pub async fn run(
        &self,
        mut receiver: CaptureReceiver,
        client: ClickHouseClient,
        mut shutdown: broadcast::Receiver<()>,
    ) {
        let mut buffer: VecDeque<DecisionRecord> = VecDeque::with_capacity(self.config.batch_size);
        let mut flush_timer = interval(self.config.flush_interval);
        flush_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        tracing::info!(
            batch_size = self.config.batch_size,
            flush_interval_ms = self.config.flush_interval.as_millis(),
            "Observability processor started"
        );

        loop {
            tokio::select! {
                // Receive event from channel
                event = receiver.recv() => {
                    match event {
                        Some(evt) => {
                            self.stats.received.fetch_add(1, Ordering::Relaxed);
                            if let Some(record) = self.process_event(evt).await {
                                self.add_to_buffer(&mut buffer, record);
                            }

                            // Flush if batch size reached
                            if buffer.len() >= self.config.batch_size {
                                self.flush_buffer(&mut buffer, &client).await;
                            }
                        }
                        None => {
                            // Channel closed, final flush and exit
                            tracing::info!("Capture channel closed, performing final flush");
                            self.flush_buffer(&mut buffer, &client).await;
                            break;
                        }
                    }
                }

                // Periodic flush
                _ = flush_timer.tick() => {
                    if !buffer.is_empty() {
                        self.flush_buffer(&mut buffer, &client).await;
                    }
                }

                // Shutdown signal
                _ = shutdown.recv() => {
                    tracing::info!("Shutdown signal received, performing final flush");
                    self.flush_buffer(&mut buffer, &client).await;
                    break;
                }
            }
        }

        let stats = self.stats_snapshot();
        tracing::info!(
            received = stats.received,
            processed = stats.processed,
            written = stats.written,
            dropped = stats.dropped,
            write_errors = stats.write_errors,
            "Observability processor stopped"
        );
    }

    /// Process a single event, returning a record if successful.
    async fn process_event(&self, event: ObservabilityEvent) -> Option<DecisionRecord> {
        match event {
            ObservabilityEvent::Decision(snapshot) => {
                self.enrich_decision(&snapshot).await
            }
            ObservabilityEvent::Counterfactual(cf) => {
                if self.config.process_counterfactuals {
                    self.process_counterfactual(&cf).await
                } else {
                    None
                }
            }
            ObservabilityEvent::Anomaly { anomaly_type, severity, event_id_hash, timestamp_ms, data } => {
                if self.config.process_anomalies {
                    self.process_anomaly(&anomaly_type, severity, event_id_hash, timestamp_ms, data).await
                } else {
                    None
                }
            }
        }
    }

    /// Enrich a decision snapshot to a full record.
    async fn enrich_decision(&self, snapshot: &DecisionSnapshot) -> Option<DecisionRecord> {
        // Look up event ID from hash
        let event_id = match self.id_lookup.lookup_event_id_async(snapshot.event_id_hash).await {
            Some(id) => id,
            None => {
                self.stats.enrichment_failures.fetch_add(1, Ordering::Relaxed);
                // Use hash as fallback identifier
                format!("unknown_{:016x}", snapshot.event_id_hash)
            }
        };

        // Look up spot price
        let spot_price = self.id_lookup.lookup_spot_price_async(snapshot.asset).await;

        self.stats.processed.fetch_add(1, Ordering::Relaxed);

        Some(DecisionRecord::from_snapshot(snapshot, event_id, spot_price))
    }

    /// Process a counterfactual (converts to decision record for storage).
    async fn process_counterfactual(&self, cf: &Counterfactual) -> Option<DecisionRecord> {
        // For now, store counterfactuals as decision records with special type
        self.stats.processed.fetch_add(1, Ordering::Relaxed);

        Some(DecisionRecord {
            decision_id: cf.decision_id,
            event_id: cf.event_id.clone(),
            timestamp: cf.settlement_time,
            decision_type: format!("Counterfactual_{:?}", cf.original_action),
            yes_ask: Decimal::ZERO, // Not available in counterfactual
            no_ask: Decimal::ZERO,
            combined_cost: cf.hypothetical_cost,
            arb_margin: cf.decision_margin,
            spot_price: Decimal::ZERO,
            time_remaining_secs: cf.decision_seconds_remaining as u32,
            action: if cf.was_correct { "correct" } else { "incorrect" }.to_string(),
            reason: cf.assessment_reason.clone(),
            confidence: cf.decision_confidence as f32,
        })
    }

    /// Process an anomaly (converts to decision record for storage).
    async fn process_anomaly(
        &self,
        anomaly_type: &str,
        severity: u8,
        event_id_hash: u64,
        timestamp_ms: i64,
        _data: u64,
    ) -> Option<DecisionRecord> {
        let event_id = self.id_lookup.lookup_event_id_async(event_id_hash).await
            .unwrap_or_else(|| format!("unknown_{:016x}", event_id_hash));

        self.stats.processed.fetch_add(1, Ordering::Relaxed);

        Some(DecisionRecord {
            decision_id: 0, // Anomalies don't have decision IDs
            event_id,
            timestamp: DateTime::from_timestamp_millis(timestamp_ms).unwrap_or_else(Utc::now),
            decision_type: format!("Anomaly_{}", anomaly_type),
            yes_ask: Decimal::ZERO,
            no_ask: Decimal::ZERO,
            combined_cost: Decimal::ZERO,
            arb_margin: Decimal::ZERO,
            spot_price: Decimal::ZERO,
            time_remaining_secs: 0,
            action: "anomaly".to_string(),
            reason: anomaly_type.to_string(),
            confidence: severity as f32,
        })
    }

    /// Add record to buffer with backpressure handling.
    fn add_to_buffer(&self, buffer: &mut VecDeque<DecisionRecord>, record: DecisionRecord) {
        // Drop oldest if buffer full
        if buffer.len() >= self.config.max_buffer_size {
            buffer.pop_front();
            self.stats.dropped.fetch_add(1, Ordering::Relaxed);
        }

        buffer.push_back(record);
    }

    /// Flush buffer to ClickHouse.
    async fn flush_buffer(&self, buffer: &mut VecDeque<DecisionRecord>, client: &ClickHouseClient) {
        if buffer.is_empty() {
            return;
        }

        let records: Vec<DecisionRecord> = buffer.drain(..).collect();
        let count = records.len();

        self.stats.flushes.fetch_add(1, Ordering::Relaxed);

        match self.write_records(client, &records).await {
            Ok(()) => {
                self.stats.written.fetch_add(count as u64, Ordering::Relaxed);
                tracing::debug!(count, "Flushed decisions to ClickHouse");
            }
            Err(e) => {
                self.stats.write_errors.fetch_add(count as u64, Ordering::Relaxed);
                tracing::error!(error = %e, count, "Failed to write decisions to ClickHouse");
            }
        }
    }

    /// Write records to ClickHouse.
    async fn write_records(
        &self,
        client: &ClickHouseClient,
        records: &[DecisionRecord],
    ) -> Result<(), ClickHouseError> {
        if records.is_empty() {
            return Ok(());
        }

        let mut insert = client.inner().insert("decisions")?;
        for record in records {
            insert.write(record).await?;
        }
        insert.end().await?;

        Ok(())
    }
}

/// Shared processor for use across tasks.
pub type SharedProcessor = Arc<ObservabilityProcessor>;

/// Create a shared processor.
pub fn create_shared_processor(
    config: ProcessorConfig,
    id_lookup: Arc<InMemoryIdLookup>,
) -> SharedProcessor {
    Arc::new(ObservabilityProcessor::new(config, id_lookup))
}

/// Spawn the processor as a background task.
///
/// Returns a handle that can be used to wait for completion.
pub fn spawn_processor(
    processor: &ObservabilityProcessor,
    receiver: CaptureReceiver,
    client: ClickHouseClient,
    shutdown: broadcast::Receiver<()>,
) -> tokio::task::JoinHandle<()> {
    let config = processor.config.clone();
    let stats = processor.stats_handle();
    let id_lookup = Arc::clone(&processor.id_lookup);

    tokio::spawn(async move {
        let proc = ObservabilityProcessor {
            config,
            stats,
            id_lookup,
        };
        proc.run(receiver, client, shutdown).await;
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn test_hash_string() {
        let hash1 = hash_string("event123");
        let hash2 = hash_string("event123");
        let hash3 = hash_string("event456");

        assert_eq!(hash1, hash2);
        assert_ne!(hash1, hash3);
    }

    #[test]
    fn test_decision_record_from_snapshot() {
        let snapshot = DecisionSnapshot {
            decision_id: 42,
            event_id_hash: hash_string("event1"),
            timestamp_ms: 1700000000000,
            yes_ask_bps: 4500,
            no_ask_bps: 5200,
            combined_cost_bps: 9700,
            margin_bps: 300,
            seconds_remaining: 120,
            confidence: 75,
            action: ActionType::Execute as u8,
            ..Default::default()
        };

        let record = DecisionRecord::from_snapshot(&snapshot, "event1".to_string(), Some(dec!(100000)));

        assert_eq!(record.decision_id, 42);
        assert_eq!(record.event_id, "event1");
        assert_eq!(record.yes_ask, dec!(0.45));
        assert_eq!(record.no_ask, dec!(0.52));
        assert_eq!(record.arb_margin, dec!(0.03));
        assert_eq!(record.spot_price, dec!(100000));
        assert_eq!(record.time_remaining_secs, 120);
        assert_eq!(record.action, "execute");
        assert_eq!(record.confidence, 75.0);
    }

    #[test]
    fn test_decision_record_skip_action() {
        let snapshot = DecisionSnapshot {
            action: ActionType::SkipToxic as u8,
            ..Default::default()
        };

        let record = DecisionRecord::from_snapshot(&snapshot, "event1".to_string(), None);

        assert_eq!(record.action, "skip");
        assert_eq!(record.reason, "toxic_flow");
    }

    #[tokio::test]
    async fn test_in_memory_id_lookup() {
        let lookup = InMemoryIdLookup::new();

        lookup.register_market("event123", "yes_token", "no_token").await;
        lookup.update_spot_price(0, dec!(100000)).await;

        let event_hash = hash_string("event123");
        let yes_hash = hash_string("yes_token");

        assert_eq!(lookup.lookup_event_id_async(event_hash).await, Some("event123".to_string()));
        assert_eq!(lookup.lookup_yes_token_async(yes_hash).await, Some("yes_token".to_string()));
        assert_eq!(lookup.lookup_spot_price_async(0).await, Some(dec!(100000)));
        assert_eq!(lookup.lookup_event_id_async(0).await, None);
    }

    #[tokio::test]
    async fn test_in_memory_id_lookup_event_count() {
        let lookup = InMemoryIdLookup::new();

        assert_eq!(lookup.event_count().await, 0);

        lookup.register_market("event1", "yes1", "no1").await;
        assert_eq!(lookup.event_count().await, 1);

        lookup.register_market("event2", "yes2", "no2").await;
        assert_eq!(lookup.event_count().await, 2);
    }

    #[test]
    fn test_processor_config_default() {
        let config = ProcessorConfig::default();

        assert_eq!(config.batch_size, 1000);
        assert_eq!(config.flush_interval, Duration::from_secs(5));
        assert_eq!(config.max_buffer_size, 10000);
        assert!(config.process_counterfactuals);
        assert!(config.process_anomalies);
    }

    #[test]
    fn test_processor_stats_snapshot() {
        let stats = ProcessorStats::new();

        stats.received.store(100, Ordering::Relaxed);
        stats.processed.store(95, Ordering::Relaxed);
        stats.written.store(90, Ordering::Relaxed);
        stats.dropped.store(5, Ordering::Relaxed);
        stats.write_errors.store(5, Ordering::Relaxed);

        let snapshot = stats.snapshot();

        assert_eq!(snapshot.received, 100);
        assert_eq!(snapshot.processed, 95);
        assert_eq!(snapshot.written, 90);
        assert_eq!(snapshot.dropped, 5);
        assert_eq!(snapshot.write_errors, 5);
    }

    #[test]
    fn test_processor_stats_drop_rate() {
        let snapshot = ProcessorStatsSnapshot {
            received: 100,
            dropped: 10,
            ..Default::default()
        };

        assert!((snapshot.drop_rate() - 10.0).abs() < 0.001);
    }

    #[test]
    fn test_processor_stats_write_success_rate() {
        let snapshot = ProcessorStatsSnapshot {
            written: 90,
            write_errors: 10,
            ..Default::default()
        };

        assert!((snapshot.write_success_rate() - 90.0).abs() < 0.001);
    }

    #[test]
    fn test_processor_stats_reset() {
        let stats = ProcessorStats::new();

        stats.received.store(100, Ordering::Relaxed);
        stats.written.store(90, Ordering::Relaxed);

        stats.reset();

        assert_eq!(stats.received.load(Ordering::Relaxed), 0);
        assert_eq!(stats.written.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn test_processor_creation() {
        let id_lookup = Arc::new(InMemoryIdLookup::new());
        let processor = ObservabilityProcessor::with_defaults(id_lookup);

        let stats = processor.stats_snapshot();
        assert_eq!(stats.received, 0);
        assert_eq!(stats.written, 0);
    }

    #[tokio::test]
    async fn test_processor_enrich_decision() {
        let id_lookup = Arc::new(InMemoryIdLookup::new());
        id_lookup.register_market("event123", "yes123", "no123").await;
        id_lookup.update_spot_price(0, dec!(50000)).await;

        let processor = ObservabilityProcessor::with_defaults(id_lookup);

        let snapshot = DecisionSnapshot {
            decision_id: 1,
            event_id_hash: hash_string("event123"),
            asset: 0, // BTC
            action: ActionType::Execute as u8,
            ..Default::default()
        };

        let record = processor.enrich_decision(&snapshot).await.unwrap();

        assert_eq!(record.decision_id, 1);
        assert_eq!(record.event_id, "event123");
        assert_eq!(record.spot_price, dec!(50000));
    }

    #[tokio::test]
    async fn test_processor_enrich_decision_unknown_event() {
        let id_lookup = Arc::new(InMemoryIdLookup::new());
        let processor = ObservabilityProcessor::with_defaults(id_lookup);

        let snapshot = DecisionSnapshot {
            decision_id: 1,
            event_id_hash: 0xDEADBEEF,
            ..Default::default()
        };

        let record = processor.enrich_decision(&snapshot).await.unwrap();

        // Should use fallback identifier
        assert!(record.event_id.starts_with("unknown_"));
        assert_eq!(processor.stats_snapshot().enrichment_failures, 1);
    }

    #[tokio::test]
    async fn test_processor_add_to_buffer_backpressure() {
        let id_lookup = Arc::new(InMemoryIdLookup::new());
        let config = ProcessorConfig {
            max_buffer_size: 3,
            ..Default::default()
        };
        let processor = ObservabilityProcessor::new(config, id_lookup);

        let mut buffer = VecDeque::new();

        // Add 4 records to buffer with max size 3
        for i in 0..4 {
            let record = DecisionRecord {
                decision_id: i,
                event_id: format!("event{}", i),
                ..Default::default()
            };
            processor.add_to_buffer(&mut buffer, record);
        }

        // Should have dropped one
        assert_eq!(buffer.len(), 3);
        assert_eq!(processor.stats_snapshot().dropped, 1);

        // First record should be gone (oldest dropped)
        assert_eq!(buffer.front().unwrap().decision_id, 1);
    }

    #[tokio::test]
    async fn test_processor_process_counterfactual() {
        let id_lookup = Arc::new(InMemoryIdLookup::new());
        let processor = ObservabilityProcessor::with_defaults(id_lookup);

        let cf = Counterfactual::new(
            42,
            "event1".to_string(),
            ActionType::SkipToxic,
            dec!(0),
            super::super::types::OutcomeType::Yes,
        );

        let record = processor.process_counterfactual(&cf).await.unwrap();

        assert_eq!(record.decision_id, 42);
        assert_eq!(record.event_id, "event1");
        assert!(record.decision_type.contains("Counterfactual"));
    }

    #[tokio::test]
    async fn test_processor_process_anomaly() {
        let id_lookup = Arc::new(InMemoryIdLookup::new());
        id_lookup.register_market("event123", "yes", "no").await;

        let processor = ObservabilityProcessor::with_defaults(id_lookup);

        let event_hash = hash_string("event123");
        let record = processor.process_anomaly(
            "flash_crash",
            90,
            event_hash,
            1700000000000,
            0,
        ).await.unwrap();

        assert_eq!(record.event_id, "event123");
        assert!(record.decision_type.contains("Anomaly"));
        assert_eq!(record.reason, "flash_crash");
        assert_eq!(record.confidence, 90.0);
    }

    impl Default for DecisionRecord {
        fn default() -> Self {
            Self {
                decision_id: 0,
                event_id: String::new(),
                timestamp: Utc::now(),
                decision_type: String::new(),
                yes_ask: Decimal::ZERO,
                no_ask: Decimal::ZERO,
                combined_cost: Decimal::ZERO,
                arb_margin: Decimal::ZERO,
                spot_price: Decimal::ZERO,
                time_remaining_secs: 0,
                action: String::new(),
                reason: String::new(),
                confidence: 0.0,
            }
        }
    }
}
