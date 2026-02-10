//! Decision aggregator for multi-engine strategy.
//!
//! This module aggregates decisions from multiple trading engines and selects
//! the best action based on priority ordering and conflict resolution.
//!
//! ## Priority System
//!
//! Engines are evaluated in priority order (configurable via `EnginesConfig.priority`):
//! 1. Arbitrage (default highest) - Risk-free profit, always takes precedence
//! 2. Directional (default medium) - Signal-based trading
//! 3. MakerRebates (default lowest) - Passive liquidity provision
//!
//! ## Conflict Resolution
//!
//! When multiple engines produce decisions:
//! - Higher priority engine's decision wins
//! - If higher-priority engine returns None, lower-priority decisions are considered
//! - MakerRebates can run concurrently (they use passive orders)

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use super::arb::ArbOpportunity;
use super::directional::DirectionalOpportunity;
use super::maker::MakerOpportunity;
use crate::config::EnginesConfig;
use crate::types::EngineType;

/// A decision from a single engine.
///
/// Wraps the opportunity type with metadata about which engine produced it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum EngineDecision {
    /// Arbitrage opportunity detected.
    Arbitrage(ArbOpportunity),
    /// Directional opportunity detected.
    Directional(DirectionalOpportunity),
    /// Maker opportunity detected (may have multiple per market).
    Maker(Vec<MakerOpportunity>),
}

impl EngineDecision {
    /// Get the engine type that produced this decision.
    #[inline]
    pub fn engine_type(&self) -> EngineType {
        match self {
            EngineDecision::Arbitrage(_) => EngineType::Arbitrage,
            EngineDecision::Directional(_) => EngineType::Directional,
            EngineDecision::Maker(_) => EngineType::MakerRebates,
        }
    }

    /// Get the event ID for this decision.
    pub fn event_id(&self) -> &str {
        match self {
            EngineDecision::Arbitrage(opp) => &opp.event_id,
            EngineDecision::Directional(opp) => &opp.event_id,
            EngineDecision::Maker(opps) => {
                opps.first().map(|o| o.event_id.as_str()).unwrap_or("")
            }
        }
    }

    /// Get the expected profit/rebate for this decision.
    ///
    /// For arbitrage: margin * max_size
    /// For directional: expected_profit (1.0 - combined_cost)
    /// For maker: sum of expected rebates
    pub fn expected_value(&self) -> Decimal {
        match self {
            EngineDecision::Arbitrage(opp) => opp.margin * opp.max_size,
            EngineDecision::Directional(opp) => opp.expected_profit(),
            EngineDecision::Maker(opps) => {
                opps.iter().map(|o| o.expected_rebate).sum()
            }
        }
    }

    /// Check if this decision can run concurrently with another.
    ///
    /// MakerRebates can run concurrently with other strategies since they
    /// use passive limit orders that don't compete for immediate execution.
    pub fn can_run_concurrently_with(&self, other: &EngineDecision) -> bool {
        match (self, other) {
            // Maker orders can run with anything
            (EngineDecision::Maker(_), _) => true,
            (_, EngineDecision::Maker(_)) => true,
            // Arbitrage and Directional are mutually exclusive
            _ => false,
        }
    }

    /// Get a reference to the arbitrage opportunity if this is an arb decision.
    pub fn as_arbitrage(&self) -> Option<&ArbOpportunity> {
        match self {
            EngineDecision::Arbitrage(opp) => Some(opp),
            _ => None,
        }
    }

    /// Get a reference to the directional opportunity if this is a directional decision.
    pub fn as_directional(&self) -> Option<&DirectionalOpportunity> {
        match self {
            EngineDecision::Directional(opp) => Some(opp),
            _ => None,
        }
    }

    /// Get a reference to the maker opportunities if this is a maker decision.
    pub fn as_maker(&self) -> Option<&[MakerOpportunity]> {
        match self {
            EngineDecision::Maker(opps) => Some(opps),
            _ => None,
        }
    }
}

/// Result of aggregating multiple engine decisions.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AggregatedDecision {
    /// Primary decision to execute (highest priority with opportunity).
    pub primary: Option<EngineDecision>,

    /// Concurrent maker orders that can run alongside primary decision.
    /// Only populated if primary is not a maker decision and maker
    /// opportunities exist.
    pub concurrent_maker: Option<Vec<MakerOpportunity>>,

    /// Summary of what each engine detected (for logging/observability).
    pub summary: DecisionSummary,
}

impl AggregatedDecision {
    /// Check if any action should be taken.
    #[inline]
    pub fn should_act(&self) -> bool {
        self.primary.is_some()
    }

    /// Get all decisions that should execute (primary + concurrent).
    pub fn all_decisions(&self) -> Vec<&EngineDecision> {
        let mut decisions = Vec::with_capacity(2);
        if let Some(ref primary) = self.primary {
            decisions.push(primary);
        }
        decisions
    }

    /// Get the primary engine type if a decision exists.
    pub fn primary_engine(&self) -> Option<EngineType> {
        self.primary.as_ref().map(|d| d.engine_type())
    }
}

/// Summary of engine decisions for observability.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DecisionSummary {
    /// Whether arbitrage opportunity was detected.
    pub has_arb: bool,
    /// Arbitrage margin if detected (bps).
    pub arb_margin_bps: Option<i32>,

    /// Whether directional opportunity was detected.
    pub has_directional: bool,
    /// Directional signal strength if detected.
    pub directional_signal: Option<String>,

    /// Whether maker opportunities were detected.
    pub has_maker: bool,
    /// Number of maker opportunities.
    pub maker_count: usize,

    /// Which engine won the aggregation.
    pub winning_engine: Option<EngineType>,
    /// Why the winning engine was selected.
    pub selection_reason: Option<String>,
}

/// Aggregates decisions from multiple trading engines.
///
/// Uses the configured priority order to select the best action when
/// multiple opportunities exist.
#[derive(Debug, Clone)]
pub struct DecisionAggregator {
    /// Priority order for engines (index 0 = highest priority).
    priority: Vec<EngineType>,
    /// Whether to allow concurrent maker orders.
    allow_concurrent_maker: bool,
}

impl DecisionAggregator {
    /// Create a new aggregator from engines config.
    pub fn new(config: &EnginesConfig) -> Self {
        Self {
            priority: config.priority.clone(),
            allow_concurrent_maker: true,
        }
    }

    /// Create an aggregator with custom priority.
    pub fn with_priority(priority: Vec<EngineType>) -> Self {
        Self {
            priority,
            allow_concurrent_maker: true,
        }
    }

    /// Set whether to allow concurrent maker orders.
    pub fn set_allow_concurrent_maker(&mut self, allow: bool) {
        self.allow_concurrent_maker = allow;
    }

    /// Aggregate decisions from multiple engines.
    ///
    /// # Arguments
    ///
    /// * `arb` - Arbitrage decision (if any)
    /// * `directional` - Directional decision (if any)
    /// * `maker` - Maker decisions (may be empty)
    ///
    /// # Returns
    ///
    /// Aggregated decision with primary action and any concurrent makers.
    pub fn aggregate(
        &self,
        arb: Option<ArbOpportunity>,
        directional: Option<DirectionalOpportunity>,
        maker: Vec<MakerOpportunity>,
    ) -> AggregatedDecision {
        let mut summary = DecisionSummary {
            has_arb: arb.is_some(),
            arb_margin_bps: arb.as_ref().map(|o| o.margin_bps),
            has_directional: directional.is_some(),
            directional_signal: directional.as_ref().map(|o| format!("{:?}", o.signal)),
            has_maker: !maker.is_empty(),
            maker_count: maker.len(),
            ..Default::default()
        };

        // Build decisions in priority order
        let mut primary: Option<EngineDecision> = None;
        let mut concurrent_maker: Option<Vec<MakerOpportunity>> = None;

        for engine in &self.priority {
            match engine {
                EngineType::Arbitrage => {
                    if let Some(opp) = arb.clone()
                        && primary.is_none()
                    {
                        primary = Some(EngineDecision::Arbitrage(opp));
                        summary.winning_engine = Some(EngineType::Arbitrage);
                        summary.selection_reason = Some("highest priority with opportunity".to_string());
                    }
                }
                EngineType::Directional => {
                    if let Some(opp) = directional.clone()
                        && primary.is_none()
                    {
                        primary = Some(EngineDecision::Directional(opp));
                        summary.winning_engine = Some(EngineType::Directional);
                        summary.selection_reason = Some(
                            if arb.is_none() {
                                "no arbitrage, directional signal present".to_string()
                            } else {
                                "highest priority with opportunity".to_string()
                            }
                        );
                    }
                }
                EngineType::MakerRebates => {
                    if !maker.is_empty() {
                        if primary.is_none() {
                            // Maker is primary if no other opportunity
                            primary = Some(EngineDecision::Maker(maker.clone()));
                            summary.winning_engine = Some(EngineType::MakerRebates);
                            summary.selection_reason = Some("no higher-priority opportunities".to_string());
                        } else if self.allow_concurrent_maker {
                            // Maker runs concurrently with other strategies
                            concurrent_maker = Some(maker.clone());
                        }
                    }
                }
            }
        }

        AggregatedDecision {
            primary,
            concurrent_maker,
            summary,
        }
    }

    /// Get the priority rank of an engine (0 = highest).
    #[inline]
    pub fn get_priority(&self, engine: EngineType) -> Option<usize> {
        self.priority.iter().position(|e| *e == engine)
    }

    /// Check if engine A has higher priority than engine B.
    #[inline]
    pub fn has_higher_priority(&self, a: EngineType, b: EngineType) -> bool {
        match (self.get_priority(a), self.get_priority(b)) {
            (Some(pa), Some(pb)) => pa < pb,
            (Some(_), None) => true,  // A is in list, B is not
            (None, Some(_)) => false, // B is in list, A is not
            (None, None) => false,    // Neither in list
        }
    }
}

impl Default for DecisionAggregator {
    fn default() -> Self {
        Self {
            priority: vec![
                EngineType::Arbitrage,
                EngineType::Directional,
                EngineType::MakerRebates,
            ],
            allow_concurrent_maker: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::WindowPhase;
    use crate::strategy::confidence::Confidence;
    use crate::strategy::signal::Signal;
    use poly_common::types::{CryptoAsset, Side};
    use rust_decimal_macros::dec;

    fn create_test_arb() -> ArbOpportunity {
        ArbOpportunity {
            event_id: "test-event".to_string(),
            asset: CryptoAsset::Btc,
            yes_token_id: "yes-token".to_string(),
            no_token_id: "no-token".to_string(),
            yes_ask: dec!(0.48),
            no_ask: dec!(0.50),
            combined_cost: dec!(0.98),
            margin: dec!(0.02),
            margin_bps: 200,
            max_size: dec!(100),
            seconds_remaining: 300,
            phase: WindowPhase::Mid,
            required_threshold: dec!(0.015),
            confidence: 80,
            spot_price: Some(dec!(100000)),
            strike_price: dec!(100000),
            detected_at_ms: 0,
        }
    }

    fn create_test_directional() -> DirectionalOpportunity {
        DirectionalOpportunity {
            event_id: "test-event".to_string(),
            yes_token_id: "yes-token".to_string(),
            no_token_id: "no-token".to_string(),
            signal: Signal::StrongUp,
            confidence: Confidence::neutral(),
            spot_price: dec!(100500),
            strike_price: dec!(100000),
            distance: dec!(0.005),
            minutes_remaining: dec!(5),
            yes_ask: dec!(0.48),
            no_ask: dec!(0.50),
            combined_cost: dec!(0.98),
            yes_imbalance: dec!(0),
            no_imbalance: dec!(0),
            detected_at_ms: 0,
        }
    }

    fn create_test_maker() -> MakerOpportunity {
        MakerOpportunity {
            event_id: "test-event".to_string(),
            token_id: "yes-token".to_string(),
            side: Side::Buy,
            price: dec!(0.45),
            size: dec!(50),
            expected_rebate: dec!(0.01),
            spread_bps: 100,
            qualifies_for_rewards: true,
            minutes_remaining: dec!(5),
            best_bid: dec!(0.44),
            best_ask: dec!(0.46),
            detected_at_ms: 0,
        }
    }

    // =========================================================================
    // EngineDecision Tests
    // =========================================================================

    #[test]
    fn test_engine_decision_type() {
        let arb = EngineDecision::Arbitrage(create_test_arb());
        assert_eq!(arb.engine_type(), EngineType::Arbitrage);

        let dir = EngineDecision::Directional(create_test_directional());
        assert_eq!(dir.engine_type(), EngineType::Directional);

        let maker = EngineDecision::Maker(vec![create_test_maker()]);
        assert_eq!(maker.engine_type(), EngineType::MakerRebates);
    }

    #[test]
    fn test_engine_decision_event_id() {
        let arb = EngineDecision::Arbitrage(create_test_arb());
        assert_eq!(arb.event_id(), "test-event");

        let maker_empty = EngineDecision::Maker(vec![]);
        assert_eq!(maker_empty.event_id(), "");
    }

    #[test]
    fn test_engine_decision_expected_value() {
        let arb = EngineDecision::Arbitrage(create_test_arb());
        // margin * max_size = 0.02 * 100 = 2.00
        assert_eq!(arb.expected_value(), dec!(2));

        let dir = EngineDecision::Directional(create_test_directional());
        // 1.0 - 0.98 = 0.02
        assert_eq!(dir.expected_value(), dec!(0.02));

        let maker = EngineDecision::Maker(vec![create_test_maker()]);
        // sum of rebates = 0.01
        assert_eq!(maker.expected_value(), dec!(0.01));
    }

    #[test]
    fn test_engine_decision_concurrency() {
        let arb = EngineDecision::Arbitrage(create_test_arb());
        let dir = EngineDecision::Directional(create_test_directional());
        let maker = EngineDecision::Maker(vec![create_test_maker()]);

        // Maker can run with anything
        assert!(maker.can_run_concurrently_with(&arb));
        assert!(maker.can_run_concurrently_with(&dir));
        assert!(arb.can_run_concurrently_with(&maker));
        assert!(dir.can_run_concurrently_with(&maker));

        // Arb and Directional are mutually exclusive
        assert!(!arb.can_run_concurrently_with(&dir));
        assert!(!dir.can_run_concurrently_with(&arb));
    }

    #[test]
    fn test_engine_decision_accessors() {
        let arb = EngineDecision::Arbitrage(create_test_arb());
        assert!(arb.as_arbitrage().is_some());
        assert!(arb.as_directional().is_none());
        assert!(arb.as_maker().is_none());

        let dir = EngineDecision::Directional(create_test_directional());
        assert!(dir.as_arbitrage().is_none());
        assert!(dir.as_directional().is_some());
        assert!(dir.as_maker().is_none());

        let maker = EngineDecision::Maker(vec![create_test_maker()]);
        assert!(maker.as_arbitrage().is_none());
        assert!(maker.as_directional().is_none());
        assert!(maker.as_maker().is_some());
        assert_eq!(maker.as_maker().unwrap().len(), 1);
    }

    // =========================================================================
    // AggregatedDecision Tests
    // =========================================================================

    #[test]
    fn test_aggregated_decision_should_act() {
        let empty = AggregatedDecision::default();
        assert!(!empty.should_act());

        let with_primary = AggregatedDecision {
            primary: Some(EngineDecision::Arbitrage(create_test_arb())),
            ..Default::default()
        };
        assert!(with_primary.should_act());
    }

    #[test]
    fn test_aggregated_decision_primary_engine() {
        let empty = AggregatedDecision::default();
        assert!(empty.primary_engine().is_none());

        let with_arb = AggregatedDecision {
            primary: Some(EngineDecision::Arbitrage(create_test_arb())),
            ..Default::default()
        };
        assert_eq!(with_arb.primary_engine(), Some(EngineType::Arbitrage));
    }

    // =========================================================================
    // DecisionAggregator Tests
    // =========================================================================

    #[test]
    fn test_aggregator_default() {
        let agg = DecisionAggregator::default();
        assert_eq!(agg.priority.len(), 3);
        assert_eq!(agg.priority[0], EngineType::Arbitrage);
        assert_eq!(agg.priority[1], EngineType::Directional);
        assert_eq!(agg.priority[2], EngineType::MakerRebates);
        assert!(agg.allow_concurrent_maker);
    }

    #[test]
    fn test_aggregator_with_priority() {
        let agg = DecisionAggregator::with_priority(vec![
            EngineType::Directional,
            EngineType::Arbitrage,
        ]);
        assert_eq!(agg.priority[0], EngineType::Directional);
        assert_eq!(agg.priority[1], EngineType::Arbitrage);
    }

    #[test]
    fn test_aggregator_get_priority() {
        let agg = DecisionAggregator::default();
        assert_eq!(agg.get_priority(EngineType::Arbitrage), Some(0));
        assert_eq!(agg.get_priority(EngineType::Directional), Some(1));
        assert_eq!(agg.get_priority(EngineType::MakerRebates), Some(2));
    }

    #[test]
    fn test_aggregator_has_higher_priority() {
        let agg = DecisionAggregator::default();
        assert!(agg.has_higher_priority(EngineType::Arbitrage, EngineType::Directional));
        assert!(agg.has_higher_priority(EngineType::Directional, EngineType::MakerRebates));
        assert!(agg.has_higher_priority(EngineType::Arbitrage, EngineType::MakerRebates));

        assert!(!agg.has_higher_priority(EngineType::Directional, EngineType::Arbitrage));
        assert!(!agg.has_higher_priority(EngineType::MakerRebates, EngineType::Directional));
    }

    // =========================================================================
    // aggregate() Tests
    // =========================================================================

    #[test]
    fn test_aggregate_no_opportunities() {
        let agg = DecisionAggregator::default();
        let result = agg.aggregate(None, None, vec![]);

        assert!(!result.should_act());
        assert!(result.primary.is_none());
        assert!(result.concurrent_maker.is_none());
        assert!(!result.summary.has_arb);
        assert!(!result.summary.has_directional);
        assert!(!result.summary.has_maker);
    }

    #[test]
    fn test_aggregate_arb_only() {
        let agg = DecisionAggregator::default();
        let result = agg.aggregate(Some(create_test_arb()), None, vec![]);

        assert!(result.should_act());
        assert_eq!(result.primary_engine(), Some(EngineType::Arbitrage));
        assert!(result.summary.has_arb);
        assert_eq!(result.summary.arb_margin_bps, Some(200));
        assert_eq!(result.summary.winning_engine, Some(EngineType::Arbitrage));
    }

    #[test]
    fn test_aggregate_directional_only() {
        let agg = DecisionAggregator::default();
        let result = agg.aggregate(None, Some(create_test_directional()), vec![]);

        assert!(result.should_act());
        assert_eq!(result.primary_engine(), Some(EngineType::Directional));
        assert!(result.summary.has_directional);
        assert!(result.summary.directional_signal.is_some());
        assert_eq!(result.summary.winning_engine, Some(EngineType::Directional));
    }

    #[test]
    fn test_aggregate_maker_only() {
        let agg = DecisionAggregator::default();
        let result = agg.aggregate(None, None, vec![create_test_maker()]);

        assert!(result.should_act());
        assert_eq!(result.primary_engine(), Some(EngineType::MakerRebates));
        assert!(result.summary.has_maker);
        assert_eq!(result.summary.maker_count, 1);
        assert_eq!(result.summary.winning_engine, Some(EngineType::MakerRebates));
        // No concurrent maker since maker is primary
        assert!(result.concurrent_maker.is_none());
    }

    #[test]
    fn test_aggregate_arb_wins_over_directional() {
        let agg = DecisionAggregator::default();
        let result = agg.aggregate(
            Some(create_test_arb()),
            Some(create_test_directional()),
            vec![],
        );

        // Arb should win due to higher priority
        assert_eq!(result.primary_engine(), Some(EngineType::Arbitrage));
        assert!(result.summary.has_arb);
        assert!(result.summary.has_directional);
        assert_eq!(result.summary.winning_engine, Some(EngineType::Arbitrage));
    }

    #[test]
    fn test_aggregate_arb_wins_over_maker() {
        let agg = DecisionAggregator::default();
        let result = agg.aggregate(
            Some(create_test_arb()),
            None,
            vec![create_test_maker()],
        );

        // Arb is primary, maker runs concurrently
        assert_eq!(result.primary_engine(), Some(EngineType::Arbitrage));
        assert!(result.concurrent_maker.is_some());
        assert_eq!(result.concurrent_maker.as_ref().unwrap().len(), 1);
    }

    #[test]
    fn test_aggregate_directional_wins_over_maker() {
        let agg = DecisionAggregator::default();
        let result = agg.aggregate(
            None,
            Some(create_test_directional()),
            vec![create_test_maker()],
        );

        // Directional is primary, maker runs concurrently
        assert_eq!(result.primary_engine(), Some(EngineType::Directional));
        assert!(result.concurrent_maker.is_some());
    }

    #[test]
    fn test_aggregate_all_opportunities() {
        let agg = DecisionAggregator::default();
        let result = agg.aggregate(
            Some(create_test_arb()),
            Some(create_test_directional()),
            vec![create_test_maker()],
        );

        // Arb wins, maker runs concurrently
        assert_eq!(result.primary_engine(), Some(EngineType::Arbitrage));
        assert!(result.concurrent_maker.is_some());
        assert!(result.summary.has_arb);
        assert!(result.summary.has_directional);
        assert!(result.summary.has_maker);
    }

    #[test]
    fn test_aggregate_custom_priority() {
        // Directional first, then arb
        let agg = DecisionAggregator::with_priority(vec![
            EngineType::Directional,
            EngineType::Arbitrage,
            EngineType::MakerRebates,
        ]);

        let result = agg.aggregate(
            Some(create_test_arb()),
            Some(create_test_directional()),
            vec![],
        );

        // Directional should win due to higher priority in this config
        assert_eq!(result.primary_engine(), Some(EngineType::Directional));
    }

    #[test]
    fn test_aggregate_no_concurrent_maker_when_disabled() {
        let mut agg = DecisionAggregator::default();
        agg.set_allow_concurrent_maker(false);

        let result = agg.aggregate(
            Some(create_test_arb()),
            None,
            vec![create_test_maker()],
        );

        // Arb is primary, but no concurrent maker
        assert_eq!(result.primary_engine(), Some(EngineType::Arbitrage));
        assert!(result.concurrent_maker.is_none());
    }

    // =========================================================================
    // Serialization Tests
    // =========================================================================

    #[test]
    fn test_engine_decision_serialization() {
        let arb = EngineDecision::Arbitrage(create_test_arb());
        let json = serde_json::to_string(&arb).unwrap();
        assert!(json.contains("Arbitrage"));

        let parsed: EngineDecision = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.engine_type(), EngineType::Arbitrage);
    }

    #[test]
    fn test_aggregated_decision_serialization() {
        let decision = AggregatedDecision {
            primary: Some(EngineDecision::Arbitrage(create_test_arb())),
            concurrent_maker: Some(vec![create_test_maker()]),
            summary: DecisionSummary {
                has_arb: true,
                arb_margin_bps: Some(200),
                winning_engine: Some(EngineType::Arbitrage),
                ..Default::default()
            },
        };

        let json = serde_json::to_string(&decision).unwrap();
        assert!(json.contains("Arbitrage"));
        assert!(json.contains("200"));

        let parsed: AggregatedDecision = serde_json::from_str(&json).unwrap();
        assert!(parsed.should_act());
    }

    // =========================================================================
    // DecisionSummary Tests
    // =========================================================================

    #[test]
    fn test_decision_summary_default() {
        let summary = DecisionSummary::default();
        assert!(!summary.has_arb);
        assert!(!summary.has_directional);
        assert!(!summary.has_maker);
        assert!(summary.winning_engine.is_none());
    }
}
