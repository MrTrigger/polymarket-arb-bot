//! Counterfactual analysis for post-settlement what-if analysis.
//!
//! After a market window settles, we can calculate what the P&L would have been
//! for decisions we skipped or executed differently.
//!
//! ## Architecture
//!
//! ```text
//! Trading Loop                   CounterfactualAnalyzer
//! ────────────                   ─────────────────────
//! [Decision] ─────► record_decision() ──► [PendingDecision HashMap]
//!                                              │
//! [Window Settles] ────────────────────────────┼──► analyze_settlement()
//!                                              │           │
//!                                              ▼           ▼
//!                                    [Counterfactual] ──► [Channel/Storage]
//! ```
//!
//! ## Features
//!
//! - In-memory storage of pending decisions (decisions awaiting settlement)
//! - Automatic cleanup of expired decisions
//! - Settlement analysis with hypothetical P&L calculation
//! - Bad decision flagging for review
//! - Integration with observability channel for storage

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use poly_common::types::Outcome;

use super::capture::ObservabilityCapture;
use super::types::{ActionType, Counterfactual, ObservabilityEvent, OutcomeType};

/// Configuration for counterfactual analysis.
#[derive(Debug, Clone)]
pub struct CounterfactualConfig {
    /// Maximum number of pending decisions to track.
    pub max_pending_decisions: usize,
    /// How long to keep pending decisions before cleanup (seconds).
    pub decision_expiry_secs: u64,
    /// Minimum missed profit to flag as significant ($).
    pub significant_miss_threshold: Decimal,
    /// Minimum hypothetical size to require review.
    pub review_size_threshold: Decimal,
    /// Tolerance for toxic flow skips ($ hypothetical profit).
    /// Skips with profit below this are considered "correct".
    pub toxic_skip_tolerance: Decimal,
    /// Whether to send counterfactuals to observability channel.
    pub send_to_channel: bool,
}

impl Default for CounterfactualConfig {
    fn default() -> Self {
        Self {
            max_pending_decisions: 10_000,
            decision_expiry_secs: 1800, // 30 minutes
            significant_miss_threshold: Decimal::ONE,
            review_size_threshold: Decimal::new(10, 0), // $10
            toxic_skip_tolerance: Decimal::new(5, 0),   // $5
            send_to_channel: true,
        }
    }
}

impl CounterfactualConfig {
    /// Create from ObservabilityConfig.
    pub fn from_observability_config(config: &crate::config::ObservabilityConfig) -> Self {
        Self {
            max_pending_decisions: config.channel_buffer_size * 2,
            send_to_channel: config.capture_counterfactuals,
            ..Default::default()
        }
    }
}

/// A pending decision awaiting settlement for counterfactual analysis.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingDecision {
    /// Decision ID.
    pub decision_id: u64,
    /// Event ID.
    pub event_id: String,
    /// Window end time.
    pub window_end: DateTime<Utc>,
    /// Action taken.
    pub action: ActionType,
    /// Actual size traded (0 if skipped).
    pub actual_size: Decimal,
    /// Actual cost (0 if skipped).
    pub actual_cost: Decimal,
    /// YES ask price at decision time.
    pub yes_ask: Decimal,
    /// NO ask price at decision time.
    pub no_ask: Decimal,
    /// Hypothetical size if we had traded.
    pub hypothetical_size: Decimal,
    /// Hypothetical cost if we had traded.
    pub hypothetical_cost: Decimal,
    /// Decision margin.
    pub margin: Decimal,
    /// Confidence score.
    pub confidence: u8,
    /// Toxic severity at decision time.
    pub toxic_severity: u8,
    /// Seconds remaining at decision.
    pub seconds_remaining: i64,
    /// When the decision was made.
    pub timestamp: DateTime<Utc>,
}

impl PendingDecision {
    /// Create a new pending decision.
    pub fn new(
        decision_id: u64,
        event_id: String,
        window_end: DateTime<Utc>,
        action: ActionType,
    ) -> Self {
        Self {
            decision_id,
            event_id,
            window_end,
            action,
            actual_size: Decimal::ZERO,
            actual_cost: Decimal::ZERO,
            yes_ask: Decimal::ZERO,
            no_ask: Decimal::ZERO,
            hypothetical_size: Decimal::ZERO,
            hypothetical_cost: Decimal::ZERO,
            margin: Decimal::ZERO,
            confidence: 0,
            toxic_severity: 0,
            seconds_remaining: 0,
            timestamp: Utc::now(),
        }
    }

    /// Set actual trade info (for executed decisions).
    pub fn with_actual(mut self, size: Decimal, cost: Decimal) -> Self {
        self.actual_size = size;
        self.actual_cost = cost;
        self
    }

    /// Set hypothetical trade info (what we could have traded).
    pub fn with_hypothetical(
        mut self,
        size: Decimal,
        cost: Decimal,
        yes_ask: Decimal,
        no_ask: Decimal,
    ) -> Self {
        self.hypothetical_size = size;
        self.hypothetical_cost = cost;
        self.yes_ask = yes_ask;
        self.no_ask = no_ask;
        self
    }

    /// Set decision context.
    pub fn with_context(
        mut self,
        margin: Decimal,
        confidence: u8,
        toxic_severity: u8,
        seconds_remaining: i64,
    ) -> Self {
        self.margin = margin;
        self.confidence = confidence;
        self.toxic_severity = toxic_severity;
        self.seconds_remaining = seconds_remaining;
        self
    }

    /// Check if this decision has expired (past window_end + buffer).
    pub fn is_expired(&self, buffer_secs: u64) -> bool {
        let expiry = self.window_end + chrono::Duration::seconds(buffer_secs as i64);
        Utc::now() > expiry
    }

    /// Combined cost at decision time.
    pub fn combined_cost(&self) -> Decimal {
        self.yes_ask + self.no_ask
    }

    /// Calculate hypothetical arb P&L.
    pub fn hypothetical_pnl(&self) -> Decimal {
        // Arb P&L = size * (1.0 - combined_cost)
        self.hypothetical_size * (Decimal::ONE - self.combined_cost())
    }

    /// Calculate actual arb P&L for executed trades.
    pub fn actual_pnl(&self) -> Decimal {
        if self.actual_size.is_zero() {
            Decimal::ZERO
        } else {
            self.actual_size * (Decimal::ONE - self.combined_cost())
        }
    }
}

/// Settlement information for a market window.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settlement {
    /// Event ID.
    pub event_id: String,
    /// Which outcome won (Yes or No).
    pub winning_outcome: Outcome,
    /// Settlement timestamp.
    pub settlement_time: DateTime<Utc>,
    /// Final YES price (1.0 if Yes won, 0.0 if No won).
    pub final_yes_price: Decimal,
    /// Final NO price (1.0 if No won, 0.0 if Yes won).
    pub final_no_price: Decimal,
}

impl Settlement {
    /// Create a new settlement.
    pub fn new(event_id: String, winning_outcome: Outcome) -> Self {
        let (yes_price, no_price) = match winning_outcome {
            Outcome::Yes => (Decimal::ONE, Decimal::ZERO),
            Outcome::No => (Decimal::ZERO, Decimal::ONE),
        };

        Self {
            event_id,
            winning_outcome,
            settlement_time: Utc::now(),
            final_yes_price: yes_price,
            final_no_price: no_price,
        }
    }

    /// Create from outcome type.
    pub fn from_outcome_type(event_id: String, outcome: OutcomeType) -> Option<Self> {
        match outcome {
            OutcomeType::Yes => Some(Self::new(event_id, Outcome::Yes)),
            OutcomeType::No => Some(Self::new(event_id, Outcome::No)),
            OutcomeType::Unknown => None,
        }
    }
}

/// Statistics for counterfactual analysis.
#[derive(Debug, Default)]
pub struct CounterfactualStats {
    /// Total decisions recorded.
    pub decisions_recorded: AtomicU64,
    /// Total settlements analyzed.
    pub settlements_analyzed: AtomicU64,
    /// Total counterfactuals generated.
    pub counterfactuals_generated: AtomicU64,
    /// Total correct decisions.
    pub correct_decisions: AtomicU64,
    /// Total incorrect decisions.
    pub incorrect_decisions: AtomicU64,
    /// Total missed profit (in cents).
    pub total_missed_profit_cents: AtomicU64,
    /// Total actual profit (in cents).
    pub total_actual_profit_cents: AtomicU64,
    /// Decisions flagged for review.
    pub flagged_for_review: AtomicU64,
    /// Expired decisions cleaned up.
    pub expired_cleanups: AtomicU64,
}

impl CounterfactualStats {
    /// Create new stats.
    pub fn new() -> Self {
        Self::default()
    }

    /// Get snapshot of stats.
    pub fn snapshot(&self) -> CounterfactualStatsSnapshot {
        CounterfactualStatsSnapshot {
            decisions_recorded: self.decisions_recorded.load(Ordering::Relaxed),
            settlements_analyzed: self.settlements_analyzed.load(Ordering::Relaxed),
            counterfactuals_generated: self.counterfactuals_generated.load(Ordering::Relaxed),
            correct_decisions: self.correct_decisions.load(Ordering::Relaxed),
            incorrect_decisions: self.incorrect_decisions.load(Ordering::Relaxed),
            total_missed_profit_cents: self.total_missed_profit_cents.load(Ordering::Relaxed),
            total_actual_profit_cents: self.total_actual_profit_cents.load(Ordering::Relaxed),
            flagged_for_review: self.flagged_for_review.load(Ordering::Relaxed),
            expired_cleanups: self.expired_cleanups.load(Ordering::Relaxed),
        }
    }

    /// Reset all counters.
    pub fn reset(&self) {
        self.decisions_recorded.store(0, Ordering::Relaxed);
        self.settlements_analyzed.store(0, Ordering::Relaxed);
        self.counterfactuals_generated.store(0, Ordering::Relaxed);
        self.correct_decisions.store(0, Ordering::Relaxed);
        self.incorrect_decisions.store(0, Ordering::Relaxed);
        self.total_missed_profit_cents.store(0, Ordering::Relaxed);
        self.total_actual_profit_cents.store(0, Ordering::Relaxed);
        self.flagged_for_review.store(0, Ordering::Relaxed);
        self.expired_cleanups.store(0, Ordering::Relaxed);
    }
}

/// Snapshot of counterfactual statistics.
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct CounterfactualStatsSnapshot {
    pub decisions_recorded: u64,
    pub settlements_analyzed: u64,
    pub counterfactuals_generated: u64,
    pub correct_decisions: u64,
    pub incorrect_decisions: u64,
    pub total_missed_profit_cents: u64,
    pub total_actual_profit_cents: u64,
    pub flagged_for_review: u64,
    pub expired_cleanups: u64,
}

impl CounterfactualStatsSnapshot {
    /// Calculate accuracy rate as percentage.
    pub fn accuracy_rate(&self) -> f64 {
        let total = self.correct_decisions + self.incorrect_decisions;
        if total == 0 {
            100.0
        } else {
            (self.correct_decisions as f64 / total as f64) * 100.0
        }
    }

    /// Get total missed profit in dollars.
    pub fn total_missed_profit(&self) -> Decimal {
        Decimal::new(self.total_missed_profit_cents as i64, 2)
    }

    /// Get total actual profit in dollars.
    pub fn total_actual_profit(&self) -> Decimal {
        Decimal::new(self.total_actual_profit_cents as i64, 2)
    }
}

/// Counterfactual analyzer for post-settlement what-if analysis.
///
/// Records decisions and analyzes them after settlement to determine
/// if the decisions were correct in hindsight.
pub struct CounterfactualAnalyzer {
    config: CounterfactualConfig,
    stats: Arc<CounterfactualStats>,
    /// Pending decisions keyed by event_id.
    /// Multiple decisions per event are stored in a Vec.
    pending: Arc<RwLock<HashMap<String, Vec<PendingDecision>>>>,
    /// Optional observability channel for sending counterfactuals.
    capture: Option<Arc<ObservabilityCapture>>,
}

impl CounterfactualAnalyzer {
    /// Create a new analyzer.
    pub fn new(config: CounterfactualConfig) -> Self {
        Self {
            config,
            stats: Arc::new(CounterfactualStats::new()),
            pending: Arc::new(RwLock::new(HashMap::new())),
            capture: None,
        }
    }

    /// Create with default config.
    pub fn with_defaults() -> Self {
        Self::new(CounterfactualConfig::default())
    }

    /// Set the observability capture channel.
    pub fn with_capture(mut self, capture: Arc<ObservabilityCapture>) -> Self {
        self.capture = Some(capture);
        self
    }

    /// Get statistics.
    pub fn stats(&self) -> &CounterfactualStats {
        &self.stats
    }

    /// Get shared stats handle.
    pub fn stats_handle(&self) -> Arc<CounterfactualStats> {
        Arc::clone(&self.stats)
    }

    /// Get stats snapshot.
    pub fn stats_snapshot(&self) -> CounterfactualStatsSnapshot {
        self.stats.snapshot()
    }

    /// Record a decision for later counterfactual analysis.
    pub async fn record_decision(&self, decision: PendingDecision) {
        let event_id = decision.event_id.clone();

        let mut pending = self.pending.write().await;

        // Check if we need to enforce max pending decisions
        let total_decisions: usize = pending.values().map(|v| v.len()).sum();
        if total_decisions >= self.config.max_pending_decisions {
            // Remove oldest event's decisions
            if let Some(oldest_key) = pending
                .iter()
                .min_by_key(|(_, v)| v.first().map(|d| d.timestamp).unwrap_or(Utc::now()))
                .map(|(k, _)| k.clone())
            {
                pending.remove(&oldest_key);
            }
        }

        pending.entry(event_id).or_default().push(decision);
        self.stats.decisions_recorded.fetch_add(1, Ordering::Relaxed);
    }

    /// Analyze a settlement and generate counterfactuals for all decisions.
    pub async fn analyze_settlement(&self, settlement: Settlement) -> Vec<Counterfactual> {
        self.stats.settlements_analyzed.fetch_add(1, Ordering::Relaxed);

        // Get and remove all decisions for this event
        let decisions = {
            let mut pending = self.pending.write().await;
            pending.remove(&settlement.event_id).unwrap_or_default()
        };

        if decisions.is_empty() {
            return Vec::new();
        }

        let mut counterfactuals = Vec::with_capacity(decisions.len());

        for decision in decisions {
            let cf = self.analyze_decision(&decision, &settlement);

            // Update stats
            if cf.was_correct {
                self.stats.correct_decisions.fetch_add(1, Ordering::Relaxed);
            } else {
                self.stats.incorrect_decisions.fetch_add(1, Ordering::Relaxed);
            }

            if cf.missed_pnl > Decimal::ZERO {
                let missed_cents = (cf.missed_pnl * Decimal::new(100, 0))
                    .try_into()
                    .unwrap_or(0u64);
                self.stats
                    .total_missed_profit_cents
                    .fetch_add(missed_cents, Ordering::Relaxed);
            }

            if cf.actual_pnl > Decimal::ZERO {
                let actual_cents = (cf.actual_pnl * Decimal::new(100, 0))
                    .try_into()
                    .unwrap_or(0u64);
                self.stats
                    .total_actual_profit_cents
                    .fetch_add(actual_cents, Ordering::Relaxed);
            }

            if cf.needs_review() {
                self.stats.flagged_for_review.fetch_add(1, Ordering::Relaxed);
            }

            self.stats
                .counterfactuals_generated
                .fetch_add(1, Ordering::Relaxed);

            // Send to observability channel if configured
            if self.config.send_to_channel
                && let Some(ref capture) = self.capture
            {
                capture.try_capture_event(ObservabilityEvent::Counterfactual(cf.clone()));
            }

            counterfactuals.push(cf);
        }

        counterfactuals
    }

    /// Analyze a single decision against settlement.
    fn analyze_decision(&self, decision: &PendingDecision, settlement: &Settlement) -> Counterfactual {
        let settlement_outcome = OutcomeType::from(Some(settlement.winning_outcome));

        let mut cf = Counterfactual::new(
            decision.decision_id,
            decision.event_id.clone(),
            decision.action,
            decision.actual_size,
            settlement_outcome,
        )
        .with_hypothetical(
            decision.hypothetical_size,
            decision.hypothetical_cost,
            decision.yes_ask,
            decision.no_ask,
        )
        .with_actual(decision.actual_pnl())
        .with_decision_context(
            decision.margin,
            decision.confidence,
            decision.toxic_severity,
            decision.seconds_remaining,
        );

        // Update settlement time
        cf.settlement_time = settlement.settlement_time;

        // Assess the decision with custom toxic tolerance
        cf = self.assess_with_tolerance(cf);

        cf
    }

    /// Assess decision with custom thresholds.
    fn assess_with_tolerance(&self, mut cf: Counterfactual) -> Counterfactual {
        let (was_correct, reason) = match cf.original_action {
            ActionType::Execute => {
                if cf.actual_pnl >= Decimal::ZERO {
                    (true, "Executed profitable trade".to_string())
                } else {
                    (false, format!("Executed losing trade: ${}", cf.actual_pnl))
                }
            }
            ActionType::SkipSizing => {
                if cf.hypothetical_pnl > Decimal::ZERO {
                    (
                        false,
                        format!("Missed ${} profit due to sizing", cf.hypothetical_pnl),
                    )
                } else {
                    (true, "Correctly skipped due to sizing".to_string())
                }
            }
            ActionType::SkipToxic => {
                // Use configurable tolerance for toxic flow skips
                if cf.hypothetical_pnl > self.config.toxic_skip_tolerance {
                    (
                        false,
                        format!(
                            "Missed ${} profit due to toxic flow (>${} tolerance)",
                            cf.hypothetical_pnl, self.config.toxic_skip_tolerance
                        ),
                    )
                } else {
                    (true, "Correctly avoided potential toxic flow".to_string())
                }
            }
            ActionType::SkipRisk => {
                if cf.hypothetical_pnl > Decimal::ZERO {
                    (
                        false,
                        format!("Missed ${} profit due to risk check", cf.hypothetical_pnl),
                    )
                } else {
                    (true, "Correctly skipped risky trade".to_string())
                }
            }
            ActionType::SkipCircuitBreaker => {
                // Circuit breaker skips are usually justified
                if cf.hypothetical_pnl > Decimal::new(10, 0) {
                    (
                        false,
                        format!(
                            "Missed ${} profit due to circuit breaker",
                            cf.hypothetical_pnl
                        ),
                    )
                } else {
                    (
                        true,
                        "Circuit breaker prevented potential loss".to_string(),
                    )
                }
            }
            _ => {
                // Other skips (disabled, time, confidence)
                if cf.hypothetical_pnl > Decimal::ZERO {
                    (false, format!("Missed ${} profit", cf.hypothetical_pnl))
                } else {
                    (true, "Correctly skipped".to_string())
                }
            }
        };

        cf.was_correct = was_correct;
        cf.assessment_reason = reason;
        cf
    }

    /// Clean up expired pending decisions.
    pub async fn cleanup_expired(&self) -> usize {
        let mut pending = self.pending.write().await;
        let expiry_secs = self.config.decision_expiry_secs;

        let mut expired_count = 0;

        // Remove expired decisions
        pending.retain(|_, decisions| {
            let initial_len = decisions.len();
            decisions.retain(|d| !d.is_expired(expiry_secs));
            expired_count += initial_len - decisions.len();
            !decisions.is_empty()
        });

        if expired_count > 0 {
            self.stats
                .expired_cleanups
                .fetch_add(expired_count as u64, Ordering::Relaxed);
        }

        expired_count
    }

    /// Get count of pending decisions.
    pub async fn pending_count(&self) -> usize {
        self.pending.read().await.values().map(|v| v.len()).sum()
    }

    /// Get pending decisions for an event (for testing).
    pub async fn get_pending(&self, event_id: &str) -> Vec<PendingDecision> {
        self.pending
            .read()
            .await
            .get(event_id)
            .cloned()
            .unwrap_or_default()
    }

    /// Run background cleanup task.
    ///
    /// Should be spawned as a background task.
    pub async fn run_cleanup_loop(
        &self,
        cleanup_interval: Duration,
        mut shutdown: tokio::sync::broadcast::Receiver<()>,
    ) {
        let mut cleanup_timer = tokio::time::interval(cleanup_interval);
        cleanup_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                _ = cleanup_timer.tick() => {
                    let cleaned = self.cleanup_expired().await;
                    if cleaned > 0 {
                        tracing::debug!(cleaned, "Cleaned up expired counterfactual decisions");
                    }
                }
                _ = shutdown.recv() => {
                    tracing::info!("Counterfactual cleanup loop shutting down");
                    break;
                }
            }
        }
    }
}

/// Shared analyzer for use across tasks.
pub type SharedCounterfactualAnalyzer = Arc<CounterfactualAnalyzer>;

/// Create a shared analyzer.
pub fn create_shared_analyzer(config: CounterfactualConfig) -> SharedCounterfactualAnalyzer {
    Arc::new(CounterfactualAnalyzer::new(config))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn test_counterfactual_config_default() {
        let config = CounterfactualConfig::default();

        assert_eq!(config.max_pending_decisions, 10_000);
        assert_eq!(config.decision_expiry_secs, 1800);
        assert_eq!(config.significant_miss_threshold, Decimal::ONE);
        assert_eq!(config.review_size_threshold, dec!(10));
        assert_eq!(config.toxic_skip_tolerance, dec!(5));
        assert!(config.send_to_channel);
    }

    #[test]
    fn test_pending_decision_new() {
        let decision = PendingDecision::new(
            1,
            "event1".to_string(),
            Utc::now() + chrono::Duration::minutes(15),
            ActionType::Execute,
        );

        assert_eq!(decision.decision_id, 1);
        assert_eq!(decision.event_id, "event1");
        assert_eq!(decision.action, ActionType::Execute);
        assert_eq!(decision.actual_size, Decimal::ZERO);
    }

    #[test]
    fn test_pending_decision_with_actual() {
        let decision = PendingDecision::new(
            1,
            "event1".to_string(),
            Utc::now(),
            ActionType::Execute,
        )
        .with_actual(dec!(100), dec!(97));

        assert_eq!(decision.actual_size, dec!(100));
        assert_eq!(decision.actual_cost, dec!(97));
    }

    #[test]
    fn test_pending_decision_with_hypothetical() {
        let decision = PendingDecision::new(
            1,
            "event1".to_string(),
            Utc::now(),
            ActionType::SkipSizing,
        )
        .with_hypothetical(dec!(100), dec!(97), dec!(0.45), dec!(0.52));

        assert_eq!(decision.hypothetical_size, dec!(100));
        assert_eq!(decision.hypothetical_cost, dec!(97));
        assert_eq!(decision.yes_ask, dec!(0.45));
        assert_eq!(decision.no_ask, dec!(0.52));
        assert_eq!(decision.combined_cost(), dec!(0.97));
    }

    #[test]
    fn test_pending_decision_hypothetical_pnl() {
        let decision = PendingDecision::new(
            1,
            "event1".to_string(),
            Utc::now(),
            ActionType::SkipSizing,
        )
        .with_hypothetical(dec!(100), dec!(97), dec!(0.45), dec!(0.52));

        // P&L = 100 * (1.0 - 0.97) = $3.00
        assert_eq!(decision.hypothetical_pnl(), dec!(3));
    }

    #[test]
    fn test_pending_decision_actual_pnl() {
        let decision = PendingDecision::new(
            1,
            "event1".to_string(),
            Utc::now(),
            ActionType::Execute,
        )
        .with_actual(dec!(100), dec!(97))
        .with_hypothetical(dec!(100), dec!(97), dec!(0.45), dec!(0.52));

        assert_eq!(decision.actual_pnl(), dec!(3));
    }

    #[test]
    fn test_pending_decision_actual_pnl_zero_size() {
        let decision = PendingDecision::new(
            1,
            "event1".to_string(),
            Utc::now(),
            ActionType::SkipSizing,
        )
        .with_hypothetical(dec!(100), dec!(97), dec!(0.45), dec!(0.52));

        // No actual trade, so P&L is 0
        assert_eq!(decision.actual_pnl(), Decimal::ZERO);
    }

    #[test]
    fn test_pending_decision_is_expired() {
        let past = Utc::now() - chrono::Duration::hours(1);
        let decision = PendingDecision::new(
            1,
            "event1".to_string(),
            past,
            ActionType::Execute,
        );

        assert!(decision.is_expired(60));
    }

    #[test]
    fn test_pending_decision_not_expired() {
        let future = Utc::now() + chrono::Duration::hours(1);
        let decision = PendingDecision::new(
            1,
            "event1".to_string(),
            future,
            ActionType::Execute,
        );

        assert!(!decision.is_expired(60));
    }

    #[test]
    fn test_settlement_new() {
        let settlement = Settlement::new("event1".to_string(), Outcome::Yes);

        assert_eq!(settlement.event_id, "event1");
        assert_eq!(settlement.winning_outcome, Outcome::Yes);
        assert_eq!(settlement.final_yes_price, Decimal::ONE);
        assert_eq!(settlement.final_no_price, Decimal::ZERO);
    }

    #[test]
    fn test_settlement_no_wins() {
        let settlement = Settlement::new("event1".to_string(), Outcome::No);

        assert_eq!(settlement.final_yes_price, Decimal::ZERO);
        assert_eq!(settlement.final_no_price, Decimal::ONE);
    }

    #[test]
    fn test_settlement_from_outcome_type() {
        let settlement = Settlement::from_outcome_type("event1".to_string(), OutcomeType::Yes);
        assert!(settlement.is_some());
        assert_eq!(settlement.unwrap().winning_outcome, Outcome::Yes);

        let settlement = Settlement::from_outcome_type("event1".to_string(), OutcomeType::Unknown);
        assert!(settlement.is_none());
    }

    #[test]
    fn test_counterfactual_stats_snapshot() {
        let stats = CounterfactualStats::new();

        stats.decisions_recorded.store(100, Ordering::Relaxed);
        stats.correct_decisions.store(80, Ordering::Relaxed);
        stats.incorrect_decisions.store(20, Ordering::Relaxed);

        let snapshot = stats.snapshot();

        assert_eq!(snapshot.decisions_recorded, 100);
        assert_eq!(snapshot.correct_decisions, 80);
        assert_eq!(snapshot.incorrect_decisions, 20);
        assert!((snapshot.accuracy_rate() - 80.0).abs() < 0.001);
    }

    #[test]
    fn test_counterfactual_stats_reset() {
        let stats = CounterfactualStats::new();

        stats.decisions_recorded.store(100, Ordering::Relaxed);
        stats.correct_decisions.store(80, Ordering::Relaxed);

        stats.reset();

        assert_eq!(stats.decisions_recorded.load(Ordering::Relaxed), 0);
        assert_eq!(stats.correct_decisions.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn test_analyzer_record_decision() {
        let analyzer = CounterfactualAnalyzer::with_defaults();

        let decision = PendingDecision::new(
            1,
            "event1".to_string(),
            Utc::now() + chrono::Duration::minutes(15),
            ActionType::Execute,
        );

        analyzer.record_decision(decision).await;

        assert_eq!(analyzer.pending_count().await, 1);
        assert_eq!(analyzer.stats_snapshot().decisions_recorded, 1);
    }

    #[tokio::test]
    async fn test_analyzer_multiple_decisions_per_event() {
        let analyzer = CounterfactualAnalyzer::with_defaults();

        let window_end = Utc::now() + chrono::Duration::minutes(15);

        for i in 0..3 {
            let decision = PendingDecision::new(
                i,
                "event1".to_string(),
                window_end,
                ActionType::Execute,
            );
            analyzer.record_decision(decision).await;
        }

        let pending = analyzer.get_pending("event1").await;
        assert_eq!(pending.len(), 3);
    }

    #[tokio::test]
    async fn test_analyzer_analyze_settlement_execute_profit() {
        let analyzer = CounterfactualAnalyzer::with_defaults();

        let decision = PendingDecision::new(
            1,
            "event1".to_string(),
            Utc::now() + chrono::Duration::minutes(15),
            ActionType::Execute,
        )
        .with_actual(dec!(100), dec!(97))
        .with_hypothetical(dec!(100), dec!(97), dec!(0.45), dec!(0.52))
        .with_context(dec!(0.03), 75, 0, 300);

        analyzer.record_decision(decision).await;

        let settlement = Settlement::new("event1".to_string(), Outcome::Yes);
        let counterfactuals = analyzer.analyze_settlement(settlement).await;

        assert_eq!(counterfactuals.len(), 1);
        let cf = &counterfactuals[0];
        assert!(cf.was_correct);
        assert!(cf.assessment_reason.contains("profitable"));
        assert_eq!(cf.actual_pnl, dec!(3));
    }

    #[tokio::test]
    async fn test_analyzer_analyze_settlement_skip_miss() {
        let analyzer = CounterfactualAnalyzer::with_defaults();

        let decision = PendingDecision::new(
            1,
            "event1".to_string(),
            Utc::now() + chrono::Duration::minutes(15),
            ActionType::SkipSizing,
        )
        .with_hypothetical(dec!(100), dec!(97), dec!(0.45), dec!(0.52))
        .with_context(dec!(0.03), 75, 0, 300);

        analyzer.record_decision(decision).await;

        let settlement = Settlement::new("event1".to_string(), Outcome::Yes);
        let counterfactuals = analyzer.analyze_settlement(settlement).await;

        assert_eq!(counterfactuals.len(), 1);
        let cf = &counterfactuals[0];
        assert!(!cf.was_correct);
        assert!(cf.assessment_reason.contains("Missed"));
        assert_eq!(cf.hypothetical_pnl, dec!(3));
    }

    #[tokio::test]
    async fn test_analyzer_toxic_skip_within_tolerance() {
        let analyzer = CounterfactualAnalyzer::with_defaults();

        // Skip with small hypothetical profit ($2) within $5 tolerance
        let decision = PendingDecision::new(
            1,
            "event1".to_string(),
            Utc::now() + chrono::Duration::minutes(15),
            ActionType::SkipToxic,
        )
        .with_hypothetical(dec!(100), dec!(98), dec!(0.47), dec!(0.51))
        .with_context(dec!(0.02), 50, 3, 300);

        analyzer.record_decision(decision).await;

        let settlement = Settlement::new("event1".to_string(), Outcome::Yes);
        let counterfactuals = analyzer.analyze_settlement(settlement).await;

        let cf = &counterfactuals[0];
        // P&L = 100 * (1.0 - 0.98) = $2.00 < $5 tolerance
        assert!(cf.was_correct);
        assert!(cf.assessment_reason.contains("toxic flow"));
    }

    #[tokio::test]
    async fn test_analyzer_toxic_skip_exceeds_tolerance() {
        let analyzer = CounterfactualAnalyzer::with_defaults();

        // Skip with larger hypothetical profit ($6) exceeding $5 tolerance
        let decision = PendingDecision::new(
            1,
            "event1".to_string(),
            Utc::now() + chrono::Duration::minutes(15),
            ActionType::SkipToxic,
        )
        .with_hypothetical(dec!(200), dec!(188), dec!(0.45), dec!(0.49))
        .with_context(dec!(0.06), 50, 3, 300);

        analyzer.record_decision(decision).await;

        let settlement = Settlement::new("event1".to_string(), Outcome::Yes);
        let counterfactuals = analyzer.analyze_settlement(settlement).await;

        let cf = &counterfactuals[0];
        // P&L = 200 * (1.0 - 0.94) = $12.00 > $5 tolerance
        assert!(!cf.was_correct);
        assert!(cf.assessment_reason.contains("tolerance"));
    }

    #[tokio::test]
    async fn test_analyzer_cleanup_expired() {
        let analyzer = CounterfactualAnalyzer::new(CounterfactualConfig {
            decision_expiry_secs: 0, // Expire immediately
            ..Default::default()
        });

        let past = Utc::now() - chrono::Duration::hours(1);
        let decision = PendingDecision::new(
            1,
            "event1".to_string(),
            past,
            ActionType::Execute,
        );

        analyzer.record_decision(decision).await;
        assert_eq!(analyzer.pending_count().await, 1);

        let cleaned = analyzer.cleanup_expired().await;
        assert_eq!(cleaned, 1);
        assert_eq!(analyzer.pending_count().await, 0);
        assert_eq!(analyzer.stats_snapshot().expired_cleanups, 1);
    }

    #[tokio::test]
    async fn test_analyzer_max_pending_decisions() {
        let analyzer = CounterfactualAnalyzer::new(CounterfactualConfig {
            max_pending_decisions: 3,
            ..Default::default()
        });

        // Add 4 decisions across different events
        for i in 0..4 {
            let decision = PendingDecision::new(
                i as u64,
                format!("event{}", i),
                Utc::now() + chrono::Duration::minutes(15),
                ActionType::Execute,
            );
            // Small delay to ensure different timestamps
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            analyzer.record_decision(decision).await;
        }

        // Should have removed oldest to stay at limit
        assert_eq!(analyzer.pending_count().await, 3);
    }

    #[tokio::test]
    async fn test_analyzer_stats_accumulation() {
        let analyzer = CounterfactualAnalyzer::with_defaults();

        // Record multiple decisions
        for i in 0..3 {
            let decision = PendingDecision::new(
                i as u64,
                "event1".to_string(),
                Utc::now() + chrono::Duration::minutes(15),
                if i % 2 == 0 {
                    ActionType::Execute
                } else {
                    ActionType::SkipSizing
                },
            )
            .with_actual(dec!(100), dec!(97))
            .with_hypothetical(dec!(100), dec!(97), dec!(0.45), dec!(0.52))
            .with_context(dec!(0.03), 75, 0, 300);

            analyzer.record_decision(decision).await;
        }

        let settlement = Settlement::new("event1".to_string(), Outcome::Yes);
        let _ = analyzer.analyze_settlement(settlement).await;

        let stats = analyzer.stats_snapshot();
        assert_eq!(stats.decisions_recorded, 3);
        assert_eq!(stats.settlements_analyzed, 1);
        assert_eq!(stats.counterfactuals_generated, 3);
    }

    #[test]
    fn test_counterfactual_stats_total_profit() {
        let snapshot = CounterfactualStatsSnapshot {
            total_missed_profit_cents: 1500, // $15.00
            total_actual_profit_cents: 2000, // $20.00
            ..Default::default()
        };

        assert_eq!(snapshot.total_missed_profit(), dec!(15));
        assert_eq!(snapshot.total_actual_profit(), dec!(20));
    }
}
