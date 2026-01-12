//! Decision snapshot types for observability.
//!
//! CRITICAL: `DecisionSnapshot` must use ONLY primitive types for hot path performance.
//! No heap allocations, no String/Vec/Box types. Use pre-hashing for event IDs.


use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use poly_common::types::{CryptoAsset, Outcome};

use crate::state::WindowPhase;

/// Decision action type (fits in u8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum ActionType {
    /// Trade executed.
    Execute = 0,
    /// Skipped due to sizing constraints.
    SkipSizing = 1,
    /// Skipped due to toxic flow detection.
    SkipToxic = 2,
    /// Skipped due to trading disabled.
    SkipDisabled = 3,
    /// Skipped due to circuit breaker.
    SkipCircuitBreaker = 4,
    /// Skipped due to risk check failure.
    SkipRisk = 5,
    /// Skipped due to insufficient time.
    SkipTime = 6,
    /// Skipped due to low confidence.
    SkipConfidence = 7,
}

impl From<u8> for ActionType {
    fn from(v: u8) -> Self {
        match v {
            0 => ActionType::Execute,
            1 => ActionType::SkipSizing,
            2 => ActionType::SkipToxic,
            3 => ActionType::SkipDisabled,
            4 => ActionType::SkipCircuitBreaker,
            5 => ActionType::SkipRisk,
            6 => ActionType::SkipTime,
            7 => ActionType::SkipConfidence,
            _ => ActionType::SkipDisabled,
        }
    }
}

/// Outcome type encoded as u8.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum OutcomeType {
    /// Unknown outcome (not yet settled).
    Unknown = 0,
    /// YES outcome won.
    Yes = 1,
    /// NO outcome won.
    No = 2,
}

impl From<Outcome> for OutcomeType {
    fn from(outcome: Outcome) -> Self {
        match outcome {
            Outcome::Yes => OutcomeType::Yes,
            Outcome::No => OutcomeType::No,
        }
    }
}

impl From<Option<Outcome>> for OutcomeType {
    fn from(outcome: Option<Outcome>) -> Self {
        match outcome {
            Some(Outcome::Yes) => OutcomeType::Yes,
            Some(Outcome::No) => OutcomeType::No,
            None => OutcomeType::Unknown,
        }
    }
}

/// Minimal decision snapshot for hot path capture.
///
/// CRITICAL: This struct contains ONLY primitive types to avoid any heap allocations
/// on the hot path. The overhead of creating and sending this should be <10ns.
///
/// All identifiers are pre-hashed to u64 for zero-allocation storage.
/// Use `SnapshotBuilder` to construct from full types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(C)] // Predictable memory layout
pub struct DecisionSnapshot {
    // === Identifiers (pre-hashed) ===
    /// Decision ID (monotonic counter).
    pub decision_id: u64,
    /// Event ID hash (FNV-1a of event_id string).
    pub event_id_hash: u64,
    /// YES token ID hash.
    pub yes_token_hash: u64,
    /// NO token ID hash.
    pub no_token_hash: u64,

    // === Timing ===
    /// Timestamp (milliseconds since epoch).
    pub timestamp_ms: i64,
    /// Latency from event to decision (microseconds).
    pub latency_us: u32,
    /// Seconds remaining in window.
    pub seconds_remaining: u16,

    // === Market State (scaled integers) ===
    /// YES ask price (scaled by 10000, e.g., 0.4500 = 4500).
    pub yes_ask_bps: u16,
    /// NO ask price (scaled by 10000).
    pub no_ask_bps: u16,
    /// Combined cost (scaled by 10000).
    pub combined_cost_bps: u16,
    /// Arbitrage margin (scaled by 10000, can be negative).
    pub margin_bps: i16,
    /// Required threshold for this phase (scaled by 10000).
    pub threshold_bps: u16,

    // === Sizing ===
    /// Trade size (scaled by 100, e.g., 50.00 shares = 5000).
    pub size_cents: u32,
    /// Expected cost (scaled by 100).
    pub expected_cost_cents: u32,
    /// Expected profit (scaled by 100).
    pub expected_profit_cents: i32,

    // === Inventory (scaled integers) ===
    /// YES shares held (scaled by 100).
    pub yes_shares_cents: u32,
    /// NO shares held (scaled by 100).
    pub no_shares_cents: u32,
    /// Total exposure (scaled by 100).
    pub exposure_cents: u32,
    /// Imbalance ratio (scaled by 100, 0-100 = 0.00-1.00).
    pub imbalance_pct: u8,

    // === Signals ===
    /// Confidence score (0-100).
    pub confidence: u8,
    /// Toxic flow severity (0=None, 1=Low, 2=Medium, 3=High, 4=Critical).
    pub toxic_severity: u8,
    /// Window phase (0=Early, 1=Mid, 2=Late).
    pub phase: u8,
    /// Asset (0=BTC, 1=ETH, 2=SOL, 3=XRP).
    pub asset: u8,

    // === Decision ===
    /// Action taken (see ActionType).
    pub action: u8,
    /// Whether spot suggests YES (0=unknown, 1=yes, 2=no).
    pub spot_suggests: u8,
    /// Reserved for future use.
    pub _reserved: [u8; 2],
}

impl DecisionSnapshot {
    /// Size of the snapshot in bytes (for capacity planning).
    pub const SIZE_BYTES: usize = std::mem::size_of::<Self>();

    /// Get action as enum.
    #[inline]
    pub fn action_type(&self) -> ActionType {
        ActionType::from(self.action)
    }

    /// Get window phase as enum.
    #[inline]
    pub fn window_phase(&self) -> WindowPhase {
        match self.phase {
            0 => WindowPhase::Early,
            1 => WindowPhase::Mid,
            _ => WindowPhase::Late,
        }
    }

    /// Get asset as enum.
    #[inline]
    pub fn crypto_asset(&self) -> CryptoAsset {
        match self.asset {
            0 => CryptoAsset::Btc,
            1 => CryptoAsset::Eth,
            2 => CryptoAsset::Sol,
            _ => CryptoAsset::Xrp,
        }
    }

    /// Check if this was an executed trade.
    #[inline]
    pub fn was_executed(&self) -> bool {
        self.action == ActionType::Execute as u8
    }

    /// Get YES ask as Decimal.
    #[inline]
    pub fn yes_ask(&self) -> Decimal {
        Decimal::new(self.yes_ask_bps as i64, 4)
    }

    /// Get NO ask as Decimal.
    #[inline]
    pub fn no_ask(&self) -> Decimal {
        Decimal::new(self.no_ask_bps as i64, 4)
    }

    /// Get margin as Decimal.
    #[inline]
    pub fn margin(&self) -> Decimal {
        Decimal::new(self.margin_bps as i64, 4)
    }

    /// Get size as Decimal.
    #[inline]
    pub fn size(&self) -> Decimal {
        Decimal::new(self.size_cents as i64, 2)
    }
}


/// Builder for creating DecisionSnapshot from full types.
///
/// Handles the conversion from strings/Decimals to pre-hashed u64/scaled integers.
pub struct SnapshotBuilder {
    snapshot: DecisionSnapshot,
}

impl SnapshotBuilder {
    /// Create a new builder.
    pub fn new() -> Self {
        Self {
            snapshot: DecisionSnapshot::default(),
        }
    }

    /// Set decision ID.
    #[inline]
    pub fn decision_id(mut self, id: u64) -> Self {
        self.snapshot.decision_id = id;
        self
    }

    /// Set event ID (will be hashed).
    #[inline]
    pub fn event_id(mut self, event_id: &str) -> Self {
        self.snapshot.event_id_hash = hash_string(event_id);
        self
    }

    /// Set YES token ID (will be hashed).
    #[inline]
    pub fn yes_token_id(mut self, token_id: &str) -> Self {
        self.snapshot.yes_token_hash = hash_string(token_id);
        self
    }

    /// Set NO token ID (will be hashed).
    #[inline]
    pub fn no_token_id(mut self, token_id: &str) -> Self {
        self.snapshot.no_token_hash = hash_string(token_id);
        self
    }

    /// Set timestamp.
    #[inline]
    pub fn timestamp(mut self, ts: DateTime<Utc>) -> Self {
        self.snapshot.timestamp_ms = ts.timestamp_millis();
        self
    }

    /// Set latency in microseconds.
    #[inline]
    pub fn latency_us(mut self, us: u64) -> Self {
        self.snapshot.latency_us = us.min(u32::MAX as u64) as u32;
        self
    }

    /// Set seconds remaining.
    #[inline]
    pub fn seconds_remaining(mut self, secs: i64) -> Self {
        self.snapshot.seconds_remaining = secs.max(0).min(u16::MAX as i64) as u16;
        self
    }

    /// Set YES ask price.
    #[inline]
    pub fn yes_ask(mut self, price: Decimal) -> Self {
        self.snapshot.yes_ask_bps = decimal_to_bps(price);
        self
    }

    /// Set NO ask price.
    #[inline]
    pub fn no_ask(mut self, price: Decimal) -> Self {
        self.snapshot.no_ask_bps = decimal_to_bps(price);
        self
    }

    /// Set combined cost.
    #[inline]
    pub fn combined_cost(mut self, cost: Decimal) -> Self {
        self.snapshot.combined_cost_bps = decimal_to_bps(cost);
        self
    }

    /// Set margin (can be negative).
    #[inline]
    pub fn margin(mut self, margin: Decimal) -> Self {
        let bps = (margin * Decimal::new(10000, 0))
            .try_into()
            .unwrap_or(0i64);
        self.snapshot.margin_bps = bps.max(i16::MIN as i64).min(i16::MAX as i64) as i16;
        self
    }

    /// Set threshold.
    #[inline]
    pub fn threshold(mut self, threshold: Decimal) -> Self {
        self.snapshot.threshold_bps = decimal_to_bps(threshold);
        self
    }

    /// Set trade size.
    #[inline]
    pub fn size(mut self, size: Decimal) -> Self {
        self.snapshot.size_cents = decimal_to_cents(size);
        self
    }

    /// Set expected cost.
    #[inline]
    pub fn expected_cost(mut self, cost: Decimal) -> Self {
        self.snapshot.expected_cost_cents = decimal_to_cents(cost);
        self
    }

    /// Set expected profit.
    #[inline]
    pub fn expected_profit(mut self, profit: Decimal) -> Self {
        let cents = (profit * Decimal::new(100, 0))
            .try_into()
            .unwrap_or(0i64);
        self.snapshot.expected_profit_cents = cents.max(i32::MIN as i64).min(i32::MAX as i64) as i32;
        self
    }

    /// Set inventory (YES shares, NO shares, exposure).
    #[inline]
    pub fn inventory(mut self, yes_shares: Decimal, no_shares: Decimal, exposure: Decimal) -> Self {
        self.snapshot.yes_shares_cents = decimal_to_cents(yes_shares);
        self.snapshot.no_shares_cents = decimal_to_cents(no_shares);
        self.snapshot.exposure_cents = decimal_to_cents(exposure);
        self
    }

    /// Set imbalance ratio (0.0 to 1.0).
    #[inline]
    pub fn imbalance(mut self, ratio: Decimal) -> Self {
        let pct = (ratio * Decimal::new(100, 0))
            .try_into()
            .unwrap_or(0i64);
        self.snapshot.imbalance_pct = pct.clamp(0, 100) as u8;
        self
    }

    /// Set confidence score (0-100).
    #[inline]
    pub fn confidence(mut self, confidence: u8) -> Self {
        self.snapshot.confidence = confidence.min(100);
        self
    }

    /// Set toxic flow severity (0=None, 1=Low, 2=Medium, 3=High, 4=Critical).
    #[inline]
    pub fn toxic_severity(mut self, severity: u8) -> Self {
        self.snapshot.toxic_severity = severity.min(4);
        self
    }

    /// Set window phase.
    #[inline]
    pub fn phase(mut self, phase: WindowPhase) -> Self {
        self.snapshot.phase = match phase {
            WindowPhase::Early => 0,
            WindowPhase::Mid => 1,
            WindowPhase::Late => 2,
        };
        self
    }

    /// Set asset.
    #[inline]
    pub fn asset(mut self, asset: CryptoAsset) -> Self {
        self.snapshot.asset = match asset {
            CryptoAsset::Btc => 0,
            CryptoAsset::Eth => 1,
            CryptoAsset::Sol => 2,
            CryptoAsset::Xrp => 3,
        };
        self
    }

    /// Set action taken.
    #[inline]
    pub fn action(mut self, action: ActionType) -> Self {
        self.snapshot.action = action as u8;
        self
    }

    /// Set spot suggests (true = YES, false = NO, None = unknown).
    #[inline]
    pub fn spot_suggests(mut self, suggests: Option<bool>) -> Self {
        self.snapshot.spot_suggests = match suggests {
            None => 0,
            Some(true) => 1,
            Some(false) => 2,
        };
        self
    }

    /// Build the snapshot.
    #[inline]
    pub fn build(self) -> DecisionSnapshot {
        self.snapshot
    }
}

impl Default for SnapshotBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// FNV-1a hash for pre-hashing event IDs to u64.
///
/// This is a fast, non-cryptographic hash suitable for ID deduplication.
#[inline]
fn hash_string(s: &str) -> u64 {
    // FNV-1a constants
    const FNV_OFFSET: u64 = 0xcbf29ce484222325;
    const FNV_PRIME: u64 = 0x100000001b3;

    let mut hash = FNV_OFFSET;
    for byte in s.bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

/// Convert Decimal to basis points (scaled by 10000).
#[inline]
fn decimal_to_bps(d: Decimal) -> u16 {
    let bps = (d * Decimal::new(10000, 0))
        .try_into()
        .unwrap_or(0i64);
    bps.max(0).min(u16::MAX as i64) as u16
}

/// Convert Decimal to cents (scaled by 100).
#[inline]
fn decimal_to_cents(d: Decimal) -> u32 {
    let cents = (d * Decimal::new(100, 0))
        .try_into()
        .unwrap_or(0i64);
    cents.max(0).min(u32::MAX as i64) as u32
}

/// Full decision context for async enrichment and storage.
///
/// This struct is built asynchronously from a `DecisionSnapshot` by looking up
/// string identifiers and enriching with additional context. Used for ClickHouse storage.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecisionContext {
    // === Identifiers ===
    /// Decision ID.
    pub decision_id: u64,
    /// Event ID (full string).
    pub event_id: String,
    /// YES token ID.
    pub yes_token_id: String,
    /// NO token ID.
    pub no_token_id: String,
    /// Asset.
    pub asset: CryptoAsset,

    // === Timing ===
    /// Decision timestamp.
    pub timestamp: DateTime<Utc>,
    /// Latency in microseconds.
    pub latency_us: u64,
    /// Seconds remaining.
    pub seconds_remaining: i64,
    /// Window phase.
    pub phase: WindowPhase,

    // === Market State ===
    /// YES ask price.
    pub yes_ask: Decimal,
    /// NO ask price.
    pub no_ask: Decimal,
    /// Combined cost.
    pub combined_cost: Decimal,
    /// Arbitrage margin.
    pub margin: Decimal,
    /// Required threshold.
    pub threshold: Decimal,
    /// Spot price at decision time.
    pub spot_price: Option<Decimal>,
    /// Strike price.
    pub strike_price: Decimal,

    // === Sizing ===
    /// Trade size.
    pub size: Decimal,
    /// Expected cost.
    pub expected_cost: Decimal,
    /// Expected profit.
    pub expected_profit: Decimal,

    // === Inventory State ===
    /// YES shares before trade.
    pub yes_shares: Decimal,
    /// NO shares before trade.
    pub no_shares: Decimal,
    /// Total exposure before trade.
    pub exposure: Decimal,
    /// Imbalance ratio.
    pub imbalance: Decimal,

    // === Signals ===
    /// Confidence score.
    pub confidence: u8,
    /// Toxic flow severity.
    pub toxic_severity: Option<String>,
    /// Whether spot suggests YES.
    pub spot_suggests_yes: Option<bool>,

    // === Decision ===
    /// Action taken.
    pub action: ActionType,
    /// Rejection reason (if not executed).
    pub rejection_reason: Option<String>,

    // === Order Info (if executed) ===
    /// YES order ID.
    pub yes_order_id: Option<String>,
    /// NO order ID.
    pub no_order_id: Option<String>,
    /// Actual fill size.
    pub fill_size: Option<Decimal>,
    /// Actual fill cost.
    pub fill_cost: Option<Decimal>,
}

impl DecisionContext {
    /// Create from snapshot with string lookups.
    ///
    /// This is used by the async processor to enrich snapshots.
    pub fn from_snapshot(
        snapshot: &DecisionSnapshot,
        event_id: String,
        yes_token_id: String,
        no_token_id: String,
    ) -> Self {
        Self {
            decision_id: snapshot.decision_id,
            event_id,
            yes_token_id,
            no_token_id,
            asset: snapshot.crypto_asset(),
            timestamp: DateTime::from_timestamp_millis(snapshot.timestamp_ms)
                .unwrap_or_else(Utc::now),
            latency_us: snapshot.latency_us as u64,
            seconds_remaining: snapshot.seconds_remaining as i64,
            phase: snapshot.window_phase(),
            yes_ask: snapshot.yes_ask(),
            no_ask: snapshot.no_ask(),
            combined_cost: Decimal::new(snapshot.combined_cost_bps as i64, 4),
            margin: snapshot.margin(),
            threshold: Decimal::new(snapshot.threshold_bps as i64, 4),
            spot_price: None,
            strike_price: Decimal::ZERO,
            size: snapshot.size(),
            expected_cost: Decimal::new(snapshot.expected_cost_cents as i64, 2),
            expected_profit: Decimal::new(snapshot.expected_profit_cents as i64, 2),
            yes_shares: Decimal::new(snapshot.yes_shares_cents as i64, 2),
            no_shares: Decimal::new(snapshot.no_shares_cents as i64, 2),
            exposure: Decimal::new(snapshot.exposure_cents as i64, 2),
            imbalance: Decimal::new(snapshot.imbalance_pct as i64, 2),
            confidence: snapshot.confidence,
            toxic_severity: match snapshot.toxic_severity {
                0 => None,
                1 => Some("Low".to_string()),
                2 => Some("Medium".to_string()),
                3 => Some("High".to_string()),
                4 => Some("Critical".to_string()),
                _ => None,
            },
            spot_suggests_yes: match snapshot.spot_suggests {
                0 => None,
                1 => Some(true),
                2 => Some(false),
                _ => None,
            },
            action: snapshot.action_type(),
            rejection_reason: None,
            yes_order_id: None,
            no_order_id: None,
            fill_size: None,
            fill_cost: None,
        }
    }

    /// Add spot price context.
    pub fn with_spot_price(mut self, price: Decimal) -> Self {
        self.spot_price = Some(price);
        self
    }

    /// Add strike price context.
    pub fn with_strike_price(mut self, price: Decimal) -> Self {
        self.strike_price = price;
        self
    }

    /// Add rejection reason.
    pub fn with_rejection(mut self, reason: String) -> Self {
        self.rejection_reason = Some(reason);
        self
    }

    /// Add order IDs.
    pub fn with_orders(mut self, yes_order_id: String, no_order_id: String) -> Self {
        self.yes_order_id = Some(yes_order_id);
        self.no_order_id = Some(no_order_id);
        self
    }

    /// Add fill info.
    pub fn with_fill(mut self, size: Decimal, cost: Decimal) -> Self {
        self.fill_size = Some(size);
        self.fill_cost = Some(cost);
        self
    }
}

/// Counterfactual analysis for post-settlement what-if.
///
/// After a market window settles, we can calculate what the P&L would have been
/// for decisions we skipped or executed differently.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Counterfactual {
    /// Original decision ID.
    pub decision_id: u64,
    /// Event ID.
    pub event_id: String,
    /// Original action taken.
    pub original_action: ActionType,
    /// Original size (0 if skipped).
    pub original_size: Decimal,
    /// Actual settlement outcome.
    pub settlement_outcome: OutcomeType,
    /// Settlement timestamp.
    pub settlement_time: DateTime<Utc>,

    // === Hypothetical Analysis ===
    /// Hypothetical size if we had traded.
    pub hypothetical_size: Decimal,
    /// Hypothetical cost if we had traded.
    pub hypothetical_cost: Decimal,
    /// Hypothetical P&L based on settlement.
    pub hypothetical_pnl: Decimal,
    /// Actual P&L (0 if skipped).
    pub actual_pnl: Decimal,
    /// Missed profit (hypothetical - actual).
    pub missed_pnl: Decimal,

    // === Decision Quality ===
    /// Was this a good decision in hindsight?
    pub was_correct: bool,
    /// Reason for the assessment.
    pub assessment_reason: String,

    // === Context at Decision Time ===
    /// Margin at decision time.
    pub decision_margin: Decimal,
    /// Confidence at decision time.
    pub decision_confidence: u8,
    /// Toxic severity at decision time.
    pub decision_toxic_severity: u8,
    /// Time remaining at decision.
    pub decision_seconds_remaining: i64,
}

impl Counterfactual {
    /// Create a new counterfactual analysis.
    pub fn new(
        decision_id: u64,
        event_id: String,
        original_action: ActionType,
        original_size: Decimal,
        settlement_outcome: OutcomeType,
    ) -> Self {
        Self {
            decision_id,
            event_id,
            original_action,
            original_size,
            settlement_outcome,
            settlement_time: Utc::now(),
            hypothetical_size: Decimal::ZERO,
            hypothetical_cost: Decimal::ZERO,
            hypothetical_pnl: Decimal::ZERO,
            actual_pnl: Decimal::ZERO,
            missed_pnl: Decimal::ZERO,
            was_correct: false,
            assessment_reason: String::new(),
            decision_margin: Decimal::ZERO,
            decision_confidence: 0,
            decision_toxic_severity: 0,
            decision_seconds_remaining: 0,
        }
    }

    /// Calculate P&L for arb trade based on settlement.
    ///
    /// In arbitrage, we buy both YES and NO at combined cost < 1.0.
    /// At settlement, one side pays out $1.00 per share.
    /// P&L = shares * 1.0 - combined_cost
    pub fn calculate_arb_pnl(
        size: Decimal,
        yes_price: Decimal,
        no_price: Decimal,
    ) -> Decimal {
        // Arb P&L = size * (1.0 - combined_cost)
        let combined_cost = yes_price + no_price;
        size * (Decimal::ONE - combined_cost)
    }

    /// Set hypothetical trade info.
    pub fn with_hypothetical(
        mut self,
        size: Decimal,
        cost: Decimal,
        yes_price: Decimal,
        no_price: Decimal,
    ) -> Self {
        self.hypothetical_size = size;
        self.hypothetical_cost = cost;
        self.hypothetical_pnl = Self::calculate_arb_pnl(size, yes_price, no_price);
        self
    }

    /// Set actual trade info.
    pub fn with_actual(mut self, pnl: Decimal) -> Self {
        self.actual_pnl = pnl;
        self.missed_pnl = self.hypothetical_pnl - self.actual_pnl;
        self
    }

    /// Set decision context.
    pub fn with_decision_context(
        mut self,
        margin: Decimal,
        confidence: u8,
        toxic_severity: u8,
        seconds_remaining: i64,
    ) -> Self {
        self.decision_margin = margin;
        self.decision_confidence = confidence;
        self.decision_toxic_severity = toxic_severity;
        self.decision_seconds_remaining = seconds_remaining;
        self
    }

    /// Assess whether the decision was correct.
    ///
    /// A decision is "correct" if:
    /// - Execute: We made positive P&L (or minimized loss)
    /// - Skip: We would have lost money, or the skip reason was valid
    pub fn assess(mut self) -> Self {
        let (was_correct, reason) = match self.original_action {
            ActionType::Execute => {
                if self.actual_pnl >= Decimal::ZERO {
                    (true, "Executed profitable trade".to_string())
                } else {
                    (false, format!("Executed losing trade: ${}", self.actual_pnl))
                }
            }
            ActionType::SkipSizing => {
                if self.hypothetical_pnl > Decimal::ZERO {
                    (false, format!("Missed ${} profit due to sizing", self.hypothetical_pnl))
                } else {
                    (true, "Correctly skipped due to sizing".to_string())
                }
            }
            ActionType::SkipToxic => {
                // Toxic flow skips are usually correct if margin was small
                if self.hypothetical_pnl > Decimal::new(5, 0) {
                    (false, format!("Missed ${} profit due to toxic flow", self.hypothetical_pnl))
                } else {
                    (true, "Correctly avoided potential toxic flow".to_string())
                }
            }
            ActionType::SkipRisk => {
                if self.hypothetical_pnl > Decimal::ZERO {
                    (false, format!("Missed ${} profit due to risk check", self.hypothetical_pnl))
                } else {
                    (true, "Correctly skipped risky trade".to_string())
                }
            }
            _ => {
                // Other skips (disabled, circuit breaker, etc.)
                if self.hypothetical_pnl > Decimal::ZERO {
                    (false, format!("Missed ${} profit", self.hypothetical_pnl))
                } else {
                    (true, "Correctly skipped".to_string())
                }
            }
        };

        self.was_correct = was_correct;
        self.assessment_reason = reason;
        self
    }

    /// Check if this was a significant miss (>$1 missed profit).
    pub fn is_significant_miss(&self) -> bool {
        self.missed_pnl > Decimal::ONE
    }

    /// Check if this warrants review (bad decision on significant trade).
    pub fn needs_review(&self) -> bool {
        !self.was_correct && self.hypothetical_size > Decimal::new(10, 0)
    }
}

/// Event types for the observability channel.
#[derive(Debug, Clone)]
pub enum ObservabilityEvent {
    /// Trading decision snapshot.
    Decision(DecisionSnapshot),
    /// Counterfactual analysis result.
    Counterfactual(Counterfactual),
    /// Anomaly detected.
    Anomaly {
        /// Anomaly type.
        anomaly_type: String,
        /// Severity (0-100).
        severity: u8,
        /// Event ID (if applicable).
        event_id_hash: u64,
        /// Timestamp.
        timestamp_ms: i64,
        /// Additional data as packed u64.
        data: u64,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn test_decision_snapshot_size() {
        // Ensure the snapshot fits in expected size for memory planning
        assert!(DecisionSnapshot::SIZE_BYTES <= 128);
        println!("DecisionSnapshot size: {} bytes", DecisionSnapshot::SIZE_BYTES);
    }

    #[test]
    fn test_snapshot_builder_basic() {
        let snapshot = SnapshotBuilder::new()
            .decision_id(42)
            .event_id("event123")
            .yes_token_id("yes_token")
            .no_token_id("no_token")
            .timestamp(Utc::now())
            .latency_us(150)
            .seconds_remaining(300)
            .yes_ask(dec!(0.45))
            .no_ask(dec!(0.52))
            .margin(dec!(0.03))
            .size(dec!(50.0))
            .confidence(75)
            .action(ActionType::Execute)
            .build();

        assert_eq!(snapshot.decision_id, 42);
        assert_eq!(snapshot.yes_ask_bps, 4500);
        assert_eq!(snapshot.no_ask_bps, 5200);
        assert_eq!(snapshot.margin_bps, 300);
        assert_eq!(snapshot.size_cents, 5000);
        assert_eq!(snapshot.confidence, 75);
        assert_eq!(snapshot.action, ActionType::Execute as u8);
    }

    #[test]
    fn test_snapshot_builder_inventory() {
        let snapshot = SnapshotBuilder::new()
            .inventory(dec!(100.0), dec!(80.0), dec!(170.0))
            .imbalance(dec!(0.11))
            .build();

        assert_eq!(snapshot.yes_shares_cents, 10000);
        assert_eq!(snapshot.no_shares_cents, 8000);
        assert_eq!(snapshot.exposure_cents, 17000);
        assert_eq!(snapshot.imbalance_pct, 11);
    }

    #[test]
    fn test_hash_string() {
        let hash1 = hash_string("event123");
        let hash2 = hash_string("event123");
        let hash3 = hash_string("event456");

        assert_eq!(hash1, hash2);
        assert_ne!(hash1, hash3);
    }

    #[test]
    fn test_action_type_conversion() {
        assert_eq!(ActionType::from(0), ActionType::Execute);
        assert_eq!(ActionType::from(1), ActionType::SkipSizing);
        assert_eq!(ActionType::from(2), ActionType::SkipToxic);
        assert_eq!(ActionType::from(99), ActionType::SkipDisabled); // default
    }

    #[test]
    fn test_outcome_type_conversion() {
        assert_eq!(OutcomeType::from(Outcome::Yes), OutcomeType::Yes);
        assert_eq!(OutcomeType::from(Outcome::No), OutcomeType::No);
        assert_eq!(OutcomeType::from(None), OutcomeType::Unknown);
    }

    #[test]
    fn test_snapshot_accessors() {
        let snapshot = SnapshotBuilder::new()
            .yes_ask(dec!(0.4567))
            .no_ask(dec!(0.5123))
            .margin(dec!(0.0310))
            .size(dec!(75.50))
            .phase(WindowPhase::Mid)
            .asset(CryptoAsset::Eth)
            .action(ActionType::SkipToxic)
            .build();

        assert_eq!(snapshot.yes_ask(), dec!(0.4567));
        assert_eq!(snapshot.no_ask(), dec!(0.5123));
        assert_eq!(snapshot.margin(), dec!(0.031));
        assert_eq!(snapshot.size(), dec!(75.5));
        assert_eq!(snapshot.window_phase(), WindowPhase::Mid);
        assert_eq!(snapshot.crypto_asset(), CryptoAsset::Eth);
        assert_eq!(snapshot.action_type(), ActionType::SkipToxic);
        assert!(!snapshot.was_executed());
    }

    #[test]
    fn test_decision_context_from_snapshot() {
        let snapshot = SnapshotBuilder::new()
            .decision_id(1)
            .yes_ask(dec!(0.45))
            .no_ask(dec!(0.52))
            .size(dec!(50.0))
            .confidence(80)
            .action(ActionType::Execute)
            .asset(CryptoAsset::Btc)
            .phase(WindowPhase::Late)
            .build();

        let context = DecisionContext::from_snapshot(
            &snapshot,
            "event_abc".to_string(),
            "yes_token_123".to_string(),
            "no_token_456".to_string(),
        );

        assert_eq!(context.decision_id, 1);
        assert_eq!(context.event_id, "event_abc");
        assert_eq!(context.yes_token_id, "yes_token_123");
        assert_eq!(context.asset, CryptoAsset::Btc);
        assert_eq!(context.yes_ask, dec!(0.45));
        assert_eq!(context.size, dec!(50.0));
        assert_eq!(context.action, ActionType::Execute);
        assert_eq!(context.phase, WindowPhase::Late);
    }

    #[test]
    fn test_decision_context_with_enrichment() {
        let snapshot = SnapshotBuilder::new()
            .action(ActionType::Execute)
            .build();

        let context = DecisionContext::from_snapshot(
            &snapshot,
            "event1".to_string(),
            "yes1".to_string(),
            "no1".to_string(),
        )
        .with_spot_price(dec!(100500))
        .with_strike_price(dec!(100000))
        .with_orders("order123".to_string(), "order456".to_string())
        .with_fill(dec!(50), dec!(48.50));

        assert_eq!(context.spot_price, Some(dec!(100500)));
        assert_eq!(context.strike_price, dec!(100000));
        assert_eq!(context.yes_order_id, Some("order123".to_string()));
        assert_eq!(context.fill_size, Some(dec!(50)));
    }

    #[test]
    fn test_counterfactual_arb_pnl() {
        // Buy 100 shares at 0.45 YES + 0.52 NO = 0.97 combined
        // Profit = 100 * (1.0 - 0.97) = $3.00
        let pnl = Counterfactual::calculate_arb_pnl(dec!(100), dec!(0.45), dec!(0.52));
        assert_eq!(pnl, dec!(3));
    }

    #[test]
    fn test_counterfactual_assessment_execute_profit() {
        let cf = Counterfactual::new(
            1,
            "event1".to_string(),
            ActionType::Execute,
            dec!(100),
            OutcomeType::Yes,
        )
        .with_actual(dec!(3.00))
        .assess();

        assert!(cf.was_correct);
        assert!(cf.assessment_reason.contains("profitable"));
    }

    #[test]
    fn test_counterfactual_assessment_skip_miss() {
        let cf = Counterfactual::new(
            1,
            "event1".to_string(),
            ActionType::SkipSizing,
            dec!(0),
            OutcomeType::Yes,
        )
        .with_hypothetical(dec!(100), dec!(97), dec!(0.45), dec!(0.52))
        .with_actual(dec!(0))
        .assess();

        assert!(!cf.was_correct);
        assert!(cf.assessment_reason.contains("Missed"));
        assert!(cf.is_significant_miss());
    }

    #[test]
    fn test_counterfactual_needs_review() {
        // SkipToxic is considered correct if P&L <= $5, so we need larger size
        // with 200 shares at 0.03 margin = $6 profit > $5 threshold
        let cf = Counterfactual::new(
            1,
            "event1".to_string(),
            ActionType::SkipToxic,
            dec!(0),
            OutcomeType::Yes,
        )
        .with_hypothetical(dec!(200), dec!(194), dec!(0.45), dec!(0.52))
        .with_actual(dec!(0))
        .assess();

        // P&L = 200 * (1.0 - 0.97) = $6 > $5, so was_correct = false
        // hypothetical_size = 200 > 10, so needs_review = true
        assert!(!cf.was_correct);
        assert!(cf.needs_review());
    }

    #[test]
    fn test_negative_margin() {
        let snapshot = SnapshotBuilder::new()
            .margin(dec!(-0.02))
            .build();

        assert_eq!(snapshot.margin_bps, -200);
        assert_eq!(snapshot.margin(), dec!(-0.02));
    }

    #[test]
    fn test_snapshot_default() {
        let snapshot = DecisionSnapshot::default();
        assert_eq!(snapshot.decision_id, 0);
        assert_eq!(snapshot.action, 0);
        assert_eq!(snapshot._reserved, [0; 2]);
    }

    #[test]
    fn test_observability_event_variants() {
        let snapshot = DecisionSnapshot::default();
        let event = ObservabilityEvent::Decision(snapshot);

        match event {
            ObservabilityEvent::Decision(s) => assert_eq!(s.decision_id, 0),
            _ => panic!("Wrong variant"),
        }

        let cf = Counterfactual::new(
            1,
            "e1".to_string(),
            ActionType::Execute,
            dec!(100),
            OutcomeType::Yes,
        );
        let event = ObservabilityEvent::Counterfactual(cf);

        match event {
            ObservabilityEvent::Counterfactual(c) => assert_eq!(c.decision_id, 1),
            _ => panic!("Wrong variant"),
        }
    }

    #[test]
    fn test_context_serialization() {
        let context = DecisionContext::from_snapshot(
            &DecisionSnapshot::default(),
            "event1".to_string(),
            "yes1".to_string(),
            "no1".to_string(),
        );

        let json = serde_json::to_string(&context).unwrap();
        assert!(json.contains("event1"));

        let decoded: DecisionContext = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.event_id, "event1");
    }

    #[test]
    fn test_counterfactual_serialization() {
        let cf = Counterfactual::new(
            42,
            "event1".to_string(),
            ActionType::SkipToxic,
            dec!(0),
            OutcomeType::No,
        );

        let json = serde_json::to_string(&cf).unwrap();
        assert!(json.contains("\"decision_id\":42"));

        let decoded: Counterfactual = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.decision_id, 42);
        assert_eq!(decoded.settlement_outcome, OutcomeType::No);
    }

    #[test]
    fn test_spot_suggests_encoding() {
        let snapshot_unknown = SnapshotBuilder::new()
            .spot_suggests(None)
            .build();
        assert_eq!(snapshot_unknown.spot_suggests, 0);

        let snapshot_yes = SnapshotBuilder::new()
            .spot_suggests(Some(true))
            .build();
        assert_eq!(snapshot_yes.spot_suggests, 1);

        let snapshot_no = SnapshotBuilder::new()
            .spot_suggests(Some(false))
            .build();
        assert_eq!(snapshot_no.spot_suggests, 2);
    }

    #[test]
    fn test_phase_encoding() {
        let early = SnapshotBuilder::new().phase(WindowPhase::Early).build();
        let mid = SnapshotBuilder::new().phase(WindowPhase::Mid).build();
        let late = SnapshotBuilder::new().phase(WindowPhase::Late).build();

        assert_eq!(early.phase, 0);
        assert_eq!(mid.phase, 1);
        assert_eq!(late.phase, 2);

        assert_eq!(early.window_phase(), WindowPhase::Early);
        assert_eq!(mid.window_phase(), WindowPhase::Mid);
        assert_eq!(late.window_phase(), WindowPhase::Late);
    }
}
