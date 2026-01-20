//! Integration tests for multi-engine strategy system.
//!
//! These tests verify the multi-engine strategy components:
//! - Engine configuration and priority
//! - DecisionAggregator priority-based selection
//! - EngineDecision wrapping and routing
//! - Concurrent maker execution

use rust_decimal_macros::dec;

use poly_bot::config::EnginesConfig;
use poly_bot::state::WindowPhase;
use poly_bot::strategy::aggregator::{AggregatedDecision, DecisionAggregator, EngineDecision};
use poly_bot::strategy::confidence::Confidence;
use poly_bot::strategy::{
    ArbOpportunity, DirectionalOpportunity, MakerOpportunity, Signal,
};
use poly_bot::types::EngineType;
use poly_common::types::{CryptoAsset, Side};

// ============================================================================
// Test Helpers
// ============================================================================

fn create_arb_opportunity() -> ArbOpportunity {
    ArbOpportunity {
        event_id: "test-event".to_string(),
        asset: CryptoAsset::Btc,
        yes_token_id: "yes-token".to_string(),
        no_token_id: "no-token".to_string(),
        yes_ask: dec!(0.48),
        no_ask: dec!(0.49),
        combined_cost: dec!(0.97),
        margin: dec!(0.03),
        margin_bps: 300,
        max_size: dec!(100),
        seconds_remaining: 300,
        phase: WindowPhase::Mid,
        required_threshold: dec!(0.015),
        confidence: 80,
        spot_price: Some(dec!(100000)),
        strike_price: dec!(100000),
        detected_at_ms: chrono::Utc::now().timestamp_millis(),
    }
}

fn create_directional_opportunity() -> DirectionalOpportunity {
    DirectionalOpportunity {
        event_id: "test-event".to_string(),
        yes_token_id: "yes-token".to_string(),
        no_token_id: "no-token".to_string(),
        signal: Signal::StrongUp,
        up_ratio: dec!(0.78),
        down_ratio: dec!(0.22),
        confidence: Confidence::neutral(),
        spot_price: dec!(101000),
        strike_price: dec!(100000),
        distance: dec!(0.01),
        minutes_remaining: dec!(10),
        yes_ask: dec!(0.50),
        no_ask: dec!(0.48),
        combined_cost: dec!(0.98),
        yes_imbalance: dec!(0.2),
        no_imbalance: dec!(0.0),
        detected_at_ms: chrono::Utc::now().timestamp_millis(),
    }
}

fn create_maker_opportunity() -> MakerOpportunity {
    MakerOpportunity {
        event_id: "test-event".to_string(),
        token_id: "yes-token".to_string(),
        side: Side::Buy,
        price: dec!(0.48),
        size: dec!(50),
        expected_rebate: dec!(0.01),
        spread_bps: 200,
        qualifies_for_rewards: true,
        minutes_remaining: dec!(10),
        best_bid: dec!(0.45),
        best_ask: dec!(0.50),
        detected_at_ms: chrono::Utc::now().timestamp_millis(),
    }
}

// ============================================================================
// EngineType Tests
// ============================================================================

#[test]
fn test_engine_type_display() {
    assert_eq!(format!("{}", EngineType::Arbitrage), "Arbitrage");
    assert_eq!(format!("{}", EngineType::Directional), "Directional");
    assert_eq!(format!("{}", EngineType::MakerRebates), "MakerRebates");
}

#[test]
fn test_engine_type_equality() {
    assert_eq!(EngineType::Arbitrage, EngineType::Arbitrage);
    assert_ne!(EngineType::Arbitrage, EngineType::Directional);
    assert_ne!(EngineType::Directional, EngineType::MakerRebates);
}

// ============================================================================
// EnginesConfig Tests
// ============================================================================

#[test]
fn test_engines_config_default() {
    let config = EnginesConfig::default();

    // No engines enabled by default (arb fees exceed margins)
    assert!(!config.arbitrage.enabled);
    assert!(!config.directional.enabled);
    assert!(!config.maker.enabled);
}

#[test]
fn test_engines_config_arbitrage_only() {
    let config = EnginesConfig::arbitrage_only();

    assert!(config.arbitrage.enabled);
    assert!(!config.directional.enabled);
    assert!(!config.maker.enabled);
}

#[test]
fn test_engines_config_any_enabled() {
    let mut config = EnginesConfig::default();
    assert!(!config.any_enabled()); // Nothing enabled by default

    config.arbitrage.enabled = true;
    assert!(config.any_enabled()); // Arbitrage enabled

    config.arbitrage.enabled = false;
    config.directional.enabled = true;
    assert!(config.any_enabled()); // Directional enabled
}

#[test]
fn test_engines_config_enabled_engines() {
    let mut config = EnginesConfig::default();
    config.arbitrage.enabled = true;
    config.directional.enabled = true;
    config.maker.enabled = true;

    let enabled = config.enabled_engines();
    assert_eq!(enabled.len(), 3);
    assert!(enabled.contains(&EngineType::Arbitrage));
    assert!(enabled.contains(&EngineType::Directional));
    assert!(enabled.contains(&EngineType::MakerRebates));
}

#[test]
fn test_engines_config_priority() {
    let config = EnginesConfig::default();

    // Default priority order
    assert_eq!(config.get_priority(EngineType::Arbitrage), Some(0));
    assert_eq!(config.get_priority(EngineType::Directional), Some(1));
    assert_eq!(config.get_priority(EngineType::MakerRebates), Some(2));
}

// ============================================================================
// EngineDecision Tests
// ============================================================================

#[test]
fn test_engine_decision_type() {
    let arb = EngineDecision::Arbitrage(create_arb_opportunity());
    assert_eq!(arb.engine_type(), EngineType::Arbitrage);

    let dir = EngineDecision::Directional(create_directional_opportunity());
    assert_eq!(dir.engine_type(), EngineType::Directional);

    let maker = EngineDecision::Maker(vec![create_maker_opportunity()]);
    assert_eq!(maker.engine_type(), EngineType::MakerRebates);
}

#[test]
fn test_engine_decision_event_id() {
    let arb = EngineDecision::Arbitrage(create_arb_opportunity());
    assert_eq!(arb.event_id(), "test-event");

    let dir = EngineDecision::Directional(create_directional_opportunity());
    assert_eq!(dir.event_id(), "test-event");

    let maker = EngineDecision::Maker(vec![create_maker_opportunity()]);
    assert_eq!(maker.event_id(), "test-event");
}

#[test]
fn test_engine_decision_expected_value() {
    let arb = EngineDecision::Arbitrage(create_arb_opportunity());
    // margin (0.03) * max_size (100) = 3.0
    assert_eq!(arb.expected_value(), dec!(3));

    let dir = EngineDecision::Directional(create_directional_opportunity());
    // expected_profit = 1.0 - 0.98 = 0.02
    assert_eq!(dir.expected_value(), dec!(0.02));

    let maker = EngineDecision::Maker(vec![create_maker_opportunity()]);
    // expected_rebate = 0.01
    assert_eq!(maker.expected_value(), dec!(0.01));
}

#[test]
fn test_engine_decision_concurrency() {
    let arb = EngineDecision::Arbitrage(create_arb_opportunity());
    let dir = EngineDecision::Directional(create_directional_opportunity());
    let maker = EngineDecision::Maker(vec![create_maker_opportunity()]);

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
    let arb = EngineDecision::Arbitrage(create_arb_opportunity());
    assert!(arb.as_arbitrage().is_some());
    assert!(arb.as_directional().is_none());
    assert!(arb.as_maker().is_none());

    let dir = EngineDecision::Directional(create_directional_opportunity());
    assert!(dir.as_arbitrage().is_none());
    assert!(dir.as_directional().is_some());
    assert!(dir.as_maker().is_none());

    let maker = EngineDecision::Maker(vec![create_maker_opportunity()]);
    assert!(maker.as_arbitrage().is_none());
    assert!(maker.as_directional().is_none());
    assert!(maker.as_maker().is_some());
}

// ============================================================================
// DecisionAggregator Tests
// ============================================================================

#[test]
fn test_aggregator_default_priority() {
    let aggregator = DecisionAggregator::default();

    // Should use default priority from EnginesConfig
    assert!(aggregator.has_higher_priority(EngineType::Arbitrage, EngineType::Directional));
    assert!(aggregator.has_higher_priority(EngineType::Directional, EngineType::MakerRebates));
    assert!(aggregator.has_higher_priority(EngineType::Arbitrage, EngineType::MakerRebates));
}

#[test]
fn test_aggregator_from_config() {
    let config = EnginesConfig::default();
    let aggregator = DecisionAggregator::new(&config);

    assert!(aggregator.has_higher_priority(EngineType::Arbitrage, EngineType::Directional));
}

#[test]
fn test_aggregator_custom_priority() {
    // Custom priority: Directional > Arbitrage > Maker
    let aggregator = DecisionAggregator::with_priority(vec![
        EngineType::Directional,
        EngineType::Arbitrage,
        EngineType::MakerRebates,
    ]);

    assert!(aggregator.has_higher_priority(EngineType::Directional, EngineType::Arbitrage));
    assert!(aggregator.has_higher_priority(EngineType::Arbitrage, EngineType::MakerRebates));
}

#[test]
fn test_aggregate_no_opportunities() {
    let aggregator = DecisionAggregator::default();
    let result = aggregator.aggregate(None, None, vec![]);

    assert!(!result.should_act());
    assert!(result.primary.is_none());
    assert!(result.concurrent_maker.is_none());
}

#[test]
fn test_aggregate_arb_only() {
    let aggregator = DecisionAggregator::default();
    let arb = Some(create_arb_opportunity());
    let result = aggregator.aggregate(arb, None, vec![]);

    assert!(result.should_act());
    assert!(result.primary.is_some());
    assert_eq!(result.primary_engine(), Some(EngineType::Arbitrage));
}

#[test]
fn test_aggregate_directional_only() {
    let aggregator = DecisionAggregator::default();
    let dir = Some(create_directional_opportunity());
    let result = aggregator.aggregate(None, dir, vec![]);

    assert!(result.should_act());
    assert!(result.primary.is_some());
    assert_eq!(result.primary_engine(), Some(EngineType::Directional));
}

#[test]
fn test_aggregate_maker_only() {
    let aggregator = DecisionAggregator::default();
    let maker = vec![create_maker_opportunity()];
    let result = aggregator.aggregate(None, None, maker);

    assert!(result.should_act());
    assert!(result.primary.is_some());
    assert_eq!(result.primary_engine(), Some(EngineType::MakerRebates));
}

#[test]
fn test_aggregate_arb_wins_over_directional() {
    let aggregator = DecisionAggregator::default();
    let arb = Some(create_arb_opportunity());
    let dir = Some(create_directional_opportunity());
    let result = aggregator.aggregate(arb, dir, vec![]);

    assert!(result.should_act());
    assert_eq!(result.primary_engine(), Some(EngineType::Arbitrage));

    // Summary should show both detected
    assert!(result.summary.has_arb);
    assert!(result.summary.has_directional);
}

#[test]
fn test_aggregate_arb_wins_over_maker() {
    let aggregator = DecisionAggregator::default();
    let arb = Some(create_arb_opportunity());
    let maker = vec![create_maker_opportunity()];
    let result = aggregator.aggregate(arb, None, maker);

    assert!(result.should_act());
    assert_eq!(result.primary_engine(), Some(EngineType::Arbitrage));

    // Maker should be available as concurrent
    assert!(result.concurrent_maker.is_some());
}

#[test]
fn test_aggregate_directional_wins_over_maker() {
    let aggregator = DecisionAggregator::default();
    let dir = Some(create_directional_opportunity());
    let maker = vec![create_maker_opportunity()];
    let result = aggregator.aggregate(None, dir, maker);

    assert!(result.should_act());
    assert_eq!(result.primary_engine(), Some(EngineType::Directional));

    // Maker should be available as concurrent
    assert!(result.concurrent_maker.is_some());
}

#[test]
fn test_aggregate_all_opportunities() {
    let aggregator = DecisionAggregator::default();
    let arb = Some(create_arb_opportunity());
    let dir = Some(create_directional_opportunity());
    let maker = vec![create_maker_opportunity()];
    let result = aggregator.aggregate(arb, dir, maker);

    // Arb should win
    assert_eq!(result.primary_engine(), Some(EngineType::Arbitrage));

    // Maker should be concurrent
    assert!(result.concurrent_maker.is_some());

    // Summary should show all detected
    assert!(result.summary.has_arb);
    assert!(result.summary.has_directional);
    assert!(result.summary.has_maker);
    assert_eq!(result.summary.winning_engine, Some(EngineType::Arbitrage));
}

#[test]
fn test_aggregate_custom_priority_directional_wins() {
    // Custom priority: Directional > Arbitrage > Maker
    let aggregator = DecisionAggregator::with_priority(vec![
        EngineType::Directional,
        EngineType::Arbitrage,
        EngineType::MakerRebates,
    ]);

    let arb = Some(create_arb_opportunity());
    let dir = Some(create_directional_opportunity());
    let result = aggregator.aggregate(arb, dir, vec![]);

    // Directional should win with custom priority
    assert_eq!(result.primary_engine(), Some(EngineType::Directional));
}

// ============================================================================
// AggregatedDecision Tests
// ============================================================================

#[test]
fn test_aggregated_decision_should_act() {
    let empty = AggregatedDecision::default();
    assert!(!empty.should_act());

    let aggregator = DecisionAggregator::default();
    let with_arb = aggregator.aggregate(Some(create_arb_opportunity()), None, vec![]);
    assert!(with_arb.should_act());
}

#[test]
fn test_aggregated_decision_all_decisions() {
    let aggregator = DecisionAggregator::default();
    let result = aggregator.aggregate(Some(create_arb_opportunity()), None, vec![]);

    let decisions = result.all_decisions();
    assert_eq!(decisions.len(), 1);
    assert_eq!(decisions[0].engine_type(), EngineType::Arbitrage);
}

#[test]
fn test_aggregated_decision_with_concurrent_maker() {
    let aggregator = DecisionAggregator::default();
    let result = aggregator.aggregate(
        Some(create_arb_opportunity()),
        None,
        vec![create_maker_opportunity()],
    );

    // Primary is arb
    assert_eq!(result.primary_engine(), Some(EngineType::Arbitrage));

    // Concurrent maker exists
    assert!(result.concurrent_maker.is_some());
    assert_eq!(result.concurrent_maker.as_ref().unwrap().len(), 1);
}

// ============================================================================
// Serialization Tests
// ============================================================================

#[test]
fn test_engine_decision_serialization() {
    let arb = EngineDecision::Arbitrage(create_arb_opportunity());
    let serialized = serde_json::to_string(&arb).unwrap();
    assert!(serialized.contains("Arbitrage"));
    assert!(serialized.contains("test-event"));

    let deserialized: EngineDecision = serde_json::from_str(&serialized).unwrap();
    assert_eq!(deserialized.engine_type(), EngineType::Arbitrage);
}

#[test]
fn test_aggregated_decision_serialization() {
    let aggregator = DecisionAggregator::default();
    let result = aggregator.aggregate(
        Some(create_arb_opportunity()),
        Some(create_directional_opportunity()),
        vec![],
    );

    let serialized = serde_json::to_string(&result).unwrap();
    assert!(serialized.contains("has_arb"));
    assert!(serialized.contains("has_directional"));

    let deserialized: AggregatedDecision = serde_json::from_str(&serialized).unwrap();
    assert!(deserialized.summary.has_arb);
    assert!(deserialized.summary.has_directional);
}

// ============================================================================
// Integration with Detectors
// ============================================================================

#[test]
fn test_detector_outputs_integrate_with_aggregator() {
    let aggregator = DecisionAggregator::default();

    // Simulate what StrategyLoop does: run detectors, aggregate results
    let arb_opp = create_arb_opportunity();
    let dir_opp = create_directional_opportunity();
    let maker_opps = vec![create_maker_opportunity()];

    // Aggregator accepts direct opportunities
    let result = aggregator.aggregate(Some(arb_opp), Some(dir_opp), maker_opps);

    // Result can be converted to EngineDecision for execution
    if let Some(ref primary) = result.primary {
        match primary {
            EngineDecision::Arbitrage(opp) => {
                assert_eq!(opp.margin, dec!(0.03));
            }
            EngineDecision::Directional(opp) => {
                assert_eq!(opp.signal, Signal::StrongUp);
            }
            EngineDecision::Maker(opps) => {
                assert!(!opps.is_empty());
            }
        }
    }
}

#[test]
fn test_engine_type_serialization() {
    let engines = [
        EngineType::Arbitrage,
        EngineType::Directional,
        EngineType::MakerRebates,
    ];

    for engine in engines {
        let serialized = serde_json::to_string(&engine).unwrap();
        let deserialized: EngineType = serde_json::from_str(&serialized).unwrap();
        assert_eq!(engine, deserialized);
    }
}

// ============================================================================
// Priority Edge Cases
// ============================================================================

#[test]
fn test_get_priority_returns_option() {
    let aggregator = DecisionAggregator::default();

    // All engines should have a priority
    assert!(aggregator.get_priority(EngineType::Arbitrage).is_some());
    assert!(aggregator.get_priority(EngineType::Directional).is_some());
    assert!(aggregator.get_priority(EngineType::MakerRebates).is_some());
}

#[test]
fn test_has_higher_priority_same_engine() {
    let aggregator = DecisionAggregator::default();

    // Same engine should not have higher priority than itself
    assert!(!aggregator.has_higher_priority(EngineType::Arbitrage, EngineType::Arbitrage));
}

#[test]
fn test_aggregator_with_empty_maker_vec() {
    let aggregator = DecisionAggregator::default();
    let result = aggregator.aggregate(Some(create_arb_opportunity()), None, vec![]);

    // Should work without concurrent maker
    assert!(result.primary.is_some());
    assert!(result.concurrent_maker.is_none());
}

#[test]
fn test_aggregator_multiple_maker_opportunities() {
    let aggregator = DecisionAggregator::default();
    let makers = vec![
        create_maker_opportunity(),
        MakerOpportunity {
            token_id: "no-token".to_string(),
            side: Side::Sell,
            ..create_maker_opportunity()
        },
    ];
    let result = aggregator.aggregate(Some(create_arb_opportunity()), None, makers);

    // Both maker opportunities should be concurrent
    assert!(result.concurrent_maker.is_some());
    assert_eq!(result.concurrent_maker.as_ref().unwrap().len(), 2);
}
