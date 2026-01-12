//! Leg risk handling for arbitrage positions.
//!
//! When executing an arbitrage trade, we buy both YES and NO shares.
//! "Leg risk" occurs when one leg fills but the other does not, leaving
//! us with directional exposure instead of the intended hedged position.
//!
//! ## Leg Risk Scenarios
//!
//! 1. **Partial Fill**: First leg fills, second leg fails/times out
//! 2. **Price Movement**: Market moves before second leg can fill
//! 3. **Liquidity Dry-up**: Counterparty size disappears mid-execution
//!
//! ## Response Strategy
//!
//! - **Monitor**: Track fill status of both legs
//! - **Calculate**: Compute exposure and potential loss
//! - **Chase**: Aggressively pursue second leg with price improvement
//! - **Emergency Close**: If exposure exceeds threshold, close position at market

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use poly_common::types::Outcome;

use crate::config::RiskConfig;
use crate::executor::OrderRequest;
use crate::types::Inventory;

/// Configuration for leg risk handling.
#[derive(Debug, Clone)]
pub struct LegRiskConfig {
    /// Exposure threshold for emergency close (USDC).
    pub emergency_close_threshold: Decimal,
    /// Maximum time to wait for second leg before acting (milliseconds).
    pub max_wait_ms: u64,
    /// Time pressure threshold - seconds remaining in window to trigger aggressive chase.
    pub time_pressure_secs: i64,
    /// Maximum price improvement for aggressive chase (decimal).
    pub max_chase_improvement: Decimal,
    /// Chase step size (decimal price increment).
    pub chase_step_size: Decimal,
    /// Minimum profit to maintain during chase (decimal).
    pub min_chase_profit: Decimal,
}

impl Default for LegRiskConfig {
    fn default() -> Self {
        Self {
            emergency_close_threshold: Decimal::new(200, 0), // $200
            max_wait_ms: 5000,                               // 5 seconds
            time_pressure_secs: 60,                          // 1 minute
            max_chase_improvement: Decimal::new(5, 2),       // 0.05 (5 cents)
            chase_step_size: Decimal::new(1, 3),             // 0.001
            min_chase_profit: Decimal::new(5, 3),            // 0.005 (0.5%)
        }
    }
}

impl LegRiskConfig {
    /// Create from RiskConfig.
    pub fn from_risk_config(risk: &RiskConfig) -> Self {
        Self {
            emergency_close_threshold: risk.emergency_close_threshold,
            ..Default::default()
        }
    }
}

/// Represents the state of a two-legged trade.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LegState {
    /// Event ID for the market.
    pub event_id: String,
    /// YES leg status.
    pub yes_leg: LegStatus,
    /// NO leg status.
    pub no_leg: LegStatus,
    /// Trade initiation time.
    pub started_at: DateTime<Utc>,
    /// Intended trade size.
    pub intended_size: Decimal,
    /// Original YES ask price (for profit calculation).
    pub original_yes_ask: Decimal,
    /// Original NO ask price (for profit calculation).
    pub original_no_ask: Decimal,
}

impl LegState {
    /// Create a new leg state for a trade.
    pub fn new(
        event_id: String,
        intended_size: Decimal,
        original_yes_ask: Decimal,
        original_no_ask: Decimal,
    ) -> Self {
        Self {
            event_id,
            yes_leg: LegStatus::Pending,
            no_leg: LegStatus::Pending,
            started_at: Utc::now(),
            intended_size,
            original_yes_ask,
            original_no_ask,
        }
    }

    /// Update YES leg status.
    pub fn update_yes(&mut self, status: LegStatus) {
        self.yes_leg = status;
    }

    /// Update NO leg status.
    pub fn update_no(&mut self, status: LegStatus) {
        self.no_leg = status;
    }

    /// Check if both legs are filled (trade complete).
    pub fn is_complete(&self) -> bool {
        matches!(self.yes_leg, LegStatus::Filled { .. })
            && matches!(self.no_leg, LegStatus::Filled { .. })
    }

    /// Check if we have leg risk (one filled, one not).
    pub fn has_leg_risk(&self) -> bool {
        let yes_filled = matches!(self.yes_leg, LegStatus::Filled { .. });
        let no_filled = matches!(self.no_leg, LegStatus::Filled { .. });
        (yes_filled && !no_filled) || (!yes_filled && no_filled)
    }

    /// Get the filled leg, if exactly one is filled.
    pub fn filled_leg(&self) -> Option<(Outcome, &LegStatus)> {
        let yes_filled = matches!(self.yes_leg, LegStatus::Filled { .. });
        let no_filled = matches!(self.no_leg, LegStatus::Filled { .. });

        if yes_filled && !no_filled {
            Some((Outcome::Yes, &self.yes_leg))
        } else if no_filled && !yes_filled {
            Some((Outcome::No, &self.no_leg))
        } else {
            None
        }
    }

    /// Get the unfilled leg, if exactly one is unfilled.
    pub fn unfilled_leg(&self) -> Option<Outcome> {
        let yes_filled = matches!(self.yes_leg, LegStatus::Filled { .. });
        let no_filled = matches!(self.no_leg, LegStatus::Filled { .. });

        if yes_filled && !no_filled {
            Some(Outcome::No)
        } else if no_filled && !yes_filled {
            Some(Outcome::Yes)
        } else {
            None
        }
    }

    /// Time elapsed since trade started.
    pub fn elapsed_ms(&self) -> i64 {
        let now = Utc::now();
        (now - self.started_at).num_milliseconds()
    }

    /// Get total filled size.
    pub fn total_filled_size(&self) -> Decimal {
        let yes_size = match &self.yes_leg {
            LegStatus::Filled { size, .. } | LegStatus::PartialFill { filled_size: size, .. } => {
                *size
            }
            _ => Decimal::ZERO,
        };
        let no_size = match &self.no_leg {
            LegStatus::Filled { size, .. } | LegStatus::PartialFill { filled_size: size, .. } => {
                *size
            }
            _ => Decimal::ZERO,
        };
        yes_size + no_size
    }

    /// Calculate current exposure (cost of unhedged position).
    pub fn calculate_exposure(&self) -> Decimal {
        match (&self.yes_leg, &self.no_leg) {
            (LegStatus::Filled { size, price, .. }, status) if !matches!(status, LegStatus::Filled { .. }) => {
                // YES filled, NO not filled - exposed to NO outcome
                *size * *price
            }
            (status, LegStatus::Filled { size, price, .. }) if !matches!(status, LegStatus::Filled { .. }) => {
                // NO filled, YES not filled - exposed to YES outcome
                *size * *price
            }
            _ => Decimal::ZERO,
        }
    }

    /// Calculate potential loss if the unfilled leg doesn't complete.
    ///
    /// If we hold YES shares and market resolves to NO, we lose the cost basis.
    /// If we hold NO shares and market resolves to YES, we lose the cost basis.
    pub fn potential_loss(&self, current_yes_price: Decimal, current_no_price: Decimal) -> Decimal {
        match (&self.yes_leg, &self.no_leg) {
            (LegStatus::Filled { size, price, .. }, _) if !matches!(self.no_leg, LegStatus::Filled { .. }) => {
                // YES filled, NO not - worst case YES becomes worthless
                let cost = *size * *price;
                let market_value = *size * current_yes_price;
                cost - market_value // Loss if we sold now
            }
            (_, LegStatus::Filled { size, price, .. }) if !matches!(self.yes_leg, LegStatus::Filled { .. }) => {
                // NO filled, YES not - worst case NO becomes worthless
                let cost = *size * *price;
                let market_value = *size * current_no_price;
                cost - market_value // Loss if we sold now
            }
            _ => Decimal::ZERO,
        }
    }
}

/// Status of a single leg.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum LegStatus {
    /// Order not yet submitted.
    Pending,
    /// Order submitted, waiting for fill.
    Submitted {
        order_id: String,
        submitted_at: DateTime<Utc>,
    },
    /// Partially filled.
    PartialFill {
        order_id: String,
        filled_size: Decimal,
        avg_price: Decimal,
    },
    /// Fully filled.
    Filled {
        order_id: String,
        size: Decimal,
        price: Decimal,
        fee: Decimal,
    },
    /// Failed (rejected or timed out).
    Failed {
        reason: String,
        at: DateTime<Utc>,
    },
    /// Cancelled.
    Cancelled {
        order_id: String,
        filled_size: Decimal,
    },
}

impl LegStatus {
    /// Check if this leg is in a terminal state.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            LegStatus::Filled { .. } | LegStatus::Failed { .. } | LegStatus::Cancelled { .. }
        )
    }

    /// Get order ID if available.
    pub fn order_id(&self) -> Option<&str> {
        match self {
            LegStatus::Submitted { order_id, .. }
            | LegStatus::PartialFill { order_id, .. }
            | LegStatus::Filled { order_id, .. }
            | LegStatus::Cancelled { order_id, .. } => Some(order_id),
            _ => None,
        }
    }

    /// Get filled size.
    pub fn filled_size(&self) -> Decimal {
        match self {
            LegStatus::Filled { size, .. } => *size,
            LegStatus::PartialFill { filled_size, .. } => *filled_size,
            LegStatus::Cancelled { filled_size, .. } => *filled_size,
            _ => Decimal::ZERO,
        }
    }
}

/// Action to take in response to leg risk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum LegRiskAction {
    /// Wait - still within acceptable parameters.
    Wait {
        exposure: Decimal,
        elapsed_ms: i64,
    },
    /// Aggressively chase the second leg.
    AggressiveChase {
        outcome: Outcome,
        target_size: Decimal,
        max_price: Decimal,
        reason: ChaseReason,
    },
    /// Emergency close - sell the filled leg to exit exposure.
    EmergencyClose {
        outcome: Outcome,
        size: Decimal,
        reason: CloseReason,
    },
    /// Trade complete - no action needed.
    Complete,
}

/// Reason for aggressive chase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChaseReason {
    /// Normal chase - second leg not filled yet.
    Normal,
    /// Time pressure - window closing soon.
    TimePressure,
    /// Exposure threshold approached.
    ExposureLimit,
}

/// Reason for emergency close.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CloseReason {
    /// Exposure exceeded threshold.
    ExposureExceeded,
    /// Timeout waiting for second leg.
    Timeout,
    /// Market window closing.
    WindowClosing,
    /// Second leg completely failed.
    LegFailed,
}

/// Result of leg risk assessment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LegRiskAssessment {
    /// Recommended action.
    pub action: LegRiskAction,
    /// Current exposure amount.
    pub exposure: Decimal,
    /// Potential loss at current prices.
    pub potential_loss: Decimal,
    /// Time elapsed since trade started.
    pub elapsed_ms: i64,
    /// Seconds remaining in window.
    pub seconds_remaining: i64,
    /// Assessment timestamp.
    pub assessed_at: DateTime<Utc>,
}

/// Leg risk manager handles detection and response to leg risk conditions.
pub struct LegRiskManager {
    /// Configuration.
    config: LegRiskConfig,
}

impl LegRiskManager {
    /// Create a new leg risk manager.
    pub fn new(config: LegRiskConfig) -> Self {
        Self { config }
    }

    /// Assess current leg risk situation and recommend action.
    pub fn assess(
        &self,
        state: &LegState,
        seconds_remaining: i64,
        current_yes_price: Decimal,
        current_no_price: Decimal,
    ) -> LegRiskAssessment {
        let exposure = state.calculate_exposure();
        let potential_loss = state.potential_loss(current_yes_price, current_no_price);
        let elapsed_ms = state.elapsed_ms();

        // Determine action
        let action = self.determine_action(
            state,
            exposure,
            elapsed_ms,
            seconds_remaining,
            current_yes_price,
            current_no_price,
        );

        LegRiskAssessment {
            action,
            exposure,
            potential_loss,
            elapsed_ms,
            seconds_remaining,
            assessed_at: Utc::now(),
        }
    }

    /// Determine the appropriate action based on current state.
    fn determine_action(
        &self,
        state: &LegState,
        exposure: Decimal,
        elapsed_ms: i64,
        seconds_remaining: i64,
        current_yes_price: Decimal,
        current_no_price: Decimal,
    ) -> LegRiskAction {
        // Check if complete
        if state.is_complete() {
            return LegRiskAction::Complete;
        }

        // No leg risk if neither leg filled
        if !state.has_leg_risk() {
            return LegRiskAction::Wait {
                exposure: Decimal::ZERO,
                elapsed_ms,
            };
        }

        // Get the unfilled leg
        let unfilled = match state.unfilled_leg() {
            Some(o) => o,
            None => return LegRiskAction::Complete,
        };

        // Check for failed leg
        let unfilled_status = match unfilled {
            Outcome::Yes => &state.yes_leg,
            Outcome::No => &state.no_leg,
        };
        if let LegStatus::Failed { .. } = unfilled_status {
            // Second leg completely failed - emergency close
            let (filled_outcome, filled_status) = state.filled_leg().unwrap();
            let size = filled_status.filled_size();
            return LegRiskAction::EmergencyClose {
                outcome: filled_outcome,
                size,
                reason: CloseReason::LegFailed,
            };
        }

        // Check exposure threshold
        if exposure >= self.config.emergency_close_threshold {
            let (filled_outcome, filled_status) = state.filled_leg().unwrap();
            let size = filled_status.filled_size();
            warn!(
                event_id = %state.event_id,
                exposure = %exposure,
                threshold = %self.config.emergency_close_threshold,
                "Exposure threshold exceeded, emergency close"
            );
            return LegRiskAction::EmergencyClose {
                outcome: filled_outcome,
                size,
                reason: CloseReason::ExposureExceeded,
            };
        }

        // Check timeout
        if elapsed_ms > self.config.max_wait_ms as i64 {
            let (filled_outcome, filled_status) = state.filled_leg().unwrap();
            let size = filled_status.filled_size();
            warn!(
                event_id = %state.event_id,
                elapsed_ms = elapsed_ms,
                max_wait_ms = self.config.max_wait_ms,
                "Timeout waiting for second leg, emergency close"
            );
            return LegRiskAction::EmergencyClose {
                outcome: filled_outcome,
                size,
                reason: CloseReason::Timeout,
            };
        }

        // Check window closing
        if seconds_remaining <= 0 {
            let (filled_outcome, filled_status) = state.filled_leg().unwrap();
            let size = filled_status.filled_size();
            return LegRiskAction::EmergencyClose {
                outcome: filled_outcome,
                size,
                reason: CloseReason::WindowClosing,
            };
        }

        // Calculate max price for chase
        let (target_size, max_price, reason) = self.calculate_chase_params(
            state,
            unfilled,
            seconds_remaining,
            exposure,
            current_yes_price,
            current_no_price,
        );

        // Within acceptable parameters but should chase
        if target_size > Decimal::ZERO {
            LegRiskAction::AggressiveChase {
                outcome: unfilled,
                target_size,
                max_price,
                reason,
            }
        } else {
            // Just wait
            LegRiskAction::Wait {
                exposure,
                elapsed_ms,
            }
        }
    }

    /// Calculate parameters for aggressive chase.
    fn calculate_chase_params(
        &self,
        state: &LegState,
        unfilled_outcome: Outcome,
        seconds_remaining: i64,
        exposure: Decimal,
        current_yes_price: Decimal,
        current_no_price: Decimal,
    ) -> (Decimal, Decimal, ChaseReason) {
        // Get filled leg info
        let (_, filled_status) = match state.filled_leg() {
            Some(f) => f,
            None => return (Decimal::ZERO, Decimal::ZERO, ChaseReason::Normal),
        };

        let filled_size = filled_status.filled_size();
        let filled_price = match filled_status {
            LegStatus::Filled { price, .. } => *price,
            LegStatus::PartialFill { avg_price, .. } => *avg_price,
            _ => return (Decimal::ZERO, Decimal::ZERO, ChaseReason::Normal),
        };

        // Target size is what we need to complete the hedge
        let target_size = filled_size;

        // Determine reason and aggressiveness
        let reason = if seconds_remaining < self.config.time_pressure_secs {
            ChaseReason::TimePressure
        } else if exposure >= self.config.emergency_close_threshold / Decimal::TWO {
            ChaseReason::ExposureLimit
        } else {
            ChaseReason::Normal
        };

        // Calculate max price we can pay while maintaining profit
        // For arb: YES + NO < $1.00
        // If we filled YES at Y, max NO price = 1.0 - Y - min_profit
        let max_price = match unfilled_outcome {
            Outcome::Yes => {
                // Filled NO, need YES
                // Max YES = 1.0 - filled_NO_price - min_profit
                Decimal::ONE - filled_price - self.config.min_chase_profit
            }
            Outcome::No => {
                // Filled YES, need NO
                // Max NO = 1.0 - filled_YES_price - min_profit
                Decimal::ONE - filled_price - self.config.min_chase_profit
            }
        };

        // Clamp to reasonable bounds
        let current_price = match unfilled_outcome {
            Outcome::Yes => current_yes_price,
            Outcome::No => current_no_price,
        };

        // Max improvement from current price
        let max_from_current = current_price + self.config.max_chase_improvement;
        let clamped_max = max_price.min(max_from_current).min(Decimal::ONE);

        // Ensure we have room to chase
        if clamped_max <= current_price {
            return (Decimal::ZERO, Decimal::ZERO, reason);
        }

        (target_size, clamped_max, reason)
    }

    /// Create an IOC order request for emergency close.
    pub fn create_emergency_close_order(
        &self,
        request_id: String,
        event_id: String,
        token_id: String,
        outcome: Outcome,
        size: Decimal,
        min_price: Decimal,
    ) -> OrderRequest {
        debug!(
            event_id = %event_id,
            outcome = ?outcome,
            size = %size,
            min_price = %min_price,
            "Creating emergency close order"
        );

        // IOC sell order at minimum acceptable price
        OrderRequest::ioc(
            request_id,
            event_id,
            token_id,
            outcome,
            poly_common::types::Side::Sell,
            size,
            min_price,
        )
    }

    /// Create an aggressive chase order.
    pub fn create_chase_order(
        &self,
        request_id: String,
        event_id: String,
        token_id: String,
        outcome: Outcome,
        size: Decimal,
        price: Decimal,
    ) -> OrderRequest {
        debug!(
            event_id = %event_id,
            outcome = ?outcome,
            size = %size,
            price = %price,
            "Creating aggressive chase order"
        );

        // IOC buy order at aggressive price
        OrderRequest::ioc(
            request_id,
            event_id,
            token_id,
            outcome,
            poly_common::types::Side::Buy,
            size,
            price,
        )
    }

    /// Check if inventory has leg risk (imbalanced YES/NO position).
    pub fn inventory_has_leg_risk(&self, inventory: &Inventory) -> bool {
        let imbalance = inventory.imbalance_ratio();
        // Consider it leg risk if significantly imbalanced (>0.5)
        imbalance > Decimal::new(5, 1)
    }

    /// Calculate inventory exposure from imbalance.
    pub fn inventory_exposure(&self, inventory: &Inventory) -> Decimal {
        // Exposure is the cost basis of the unmatched position
        let unmatched_yes = inventory.unmatched_yes();
        let unmatched_no = inventory.unmatched_no();

        if unmatched_yes > Decimal::ZERO {
            // YES-heavy, calculate YES exposure
            let yes_cost_per = if inventory.yes_shares > Decimal::ZERO {
                inventory.yes_cost_basis / inventory.yes_shares
            } else {
                Decimal::ZERO
            };
            unmatched_yes * yes_cost_per
        } else if unmatched_no > Decimal::ZERO {
            // NO-heavy, calculate NO exposure
            let no_cost_per = if inventory.no_shares > Decimal::ZERO {
                inventory.no_cost_basis / inventory.no_shares
            } else {
                Decimal::ZERO
            };
            unmatched_no * no_cost_per
        } else {
            Decimal::ZERO
        }
    }

    /// Determine if emergency close is needed for inventory.
    pub fn should_emergency_close_inventory(&self, inventory: &Inventory) -> bool {
        let exposure = self.inventory_exposure(inventory);
        exposure >= self.config.emergency_close_threshold
    }
}

impl std::fmt::Debug for LegRiskManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LegRiskManager")
            .field("config", &self.config)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::OrderType;
    use rust_decimal_macros::dec;

    fn create_test_state() -> LegState {
        LegState::new(
            "event-1".to_string(),
            dec!(100),
            dec!(0.45),
            dec!(0.52),
        )
    }

    #[test]
    fn test_leg_state_new() {
        let state = create_test_state();
        assert_eq!(state.event_id, "event-1");
        assert!(matches!(state.yes_leg, LegStatus::Pending));
        assert!(matches!(state.no_leg, LegStatus::Pending));
        assert_eq!(state.intended_size, dec!(100));
    }

    #[test]
    fn test_leg_state_update() {
        let mut state = create_test_state();

        state.update_yes(LegStatus::Filled {
            order_id: "order-1".to_string(),
            size: dec!(100),
            price: dec!(0.45),
            fee: dec!(0.01),
        });

        assert!(matches!(state.yes_leg, LegStatus::Filled { .. }));
        assert!(state.has_leg_risk());
        assert!(!state.is_complete());
    }

    #[test]
    fn test_leg_state_complete() {
        let mut state = create_test_state();

        state.update_yes(LegStatus::Filled {
            order_id: "order-1".to_string(),
            size: dec!(100),
            price: dec!(0.45),
            fee: dec!(0.01),
        });
        state.update_no(LegStatus::Filled {
            order_id: "order-2".to_string(),
            size: dec!(100),
            price: dec!(0.52),
            fee: dec!(0.01),
        });

        assert!(state.is_complete());
        assert!(!state.has_leg_risk());
    }

    #[test]
    fn test_leg_state_exposure() {
        let mut state = create_test_state();

        // YES filled at 0.45 for 100 shares
        state.update_yes(LegStatus::Filled {
            order_id: "order-1".to_string(),
            size: dec!(100),
            price: dec!(0.45),
            fee: dec!(0.01),
        });

        // Exposure = 100 * 0.45 = 45
        assert_eq!(state.calculate_exposure(), dec!(45));
    }

    #[test]
    fn test_leg_state_no_leg_risk_when_neither_filled() {
        let state = create_test_state();
        assert!(!state.has_leg_risk());
        assert_eq!(state.calculate_exposure(), Decimal::ZERO);
    }

    #[test]
    fn test_leg_state_unfilled_leg() {
        let mut state = create_test_state();

        state.update_yes(LegStatus::Filled {
            order_id: "order-1".to_string(),
            size: dec!(100),
            price: dec!(0.45),
            fee: dec!(0.01),
        });

        assert_eq!(state.unfilled_leg(), Some(Outcome::No));
    }

    #[test]
    fn test_leg_status_terminal() {
        assert!(!LegStatus::Pending.is_terminal());
        assert!(!LegStatus::Submitted {
            order_id: "1".to_string(),
            submitted_at: Utc::now(),
        }
        .is_terminal());
        assert!(LegStatus::Filled {
            order_id: "1".to_string(),
            size: dec!(100),
            price: dec!(0.5),
            fee: dec!(0.01),
        }
        .is_terminal());
        assert!(LegStatus::Failed {
            reason: "test".to_string(),
            at: Utc::now(),
        }
        .is_terminal());
    }

    #[test]
    fn test_leg_risk_manager_assess_complete() {
        let manager = LegRiskManager::new(LegRiskConfig::default());
        let mut state = create_test_state();

        state.update_yes(LegStatus::Filled {
            order_id: "order-1".to_string(),
            size: dec!(100),
            price: dec!(0.45),
            fee: dec!(0.01),
        });
        state.update_no(LegStatus::Filled {
            order_id: "order-2".to_string(),
            size: dec!(100),
            price: dec!(0.52),
            fee: dec!(0.01),
        });

        let assessment = manager.assess(&state, 300, dec!(0.45), dec!(0.52));
        assert!(matches!(assessment.action, LegRiskAction::Complete));
    }

    #[test]
    fn test_leg_risk_manager_assess_wait() {
        let manager = LegRiskManager::new(LegRiskConfig::default());
        let state = create_test_state();

        // Neither leg filled - should wait
        let assessment = manager.assess(&state, 300, dec!(0.45), dec!(0.52));
        assert!(matches!(assessment.action, LegRiskAction::Wait { .. }));
    }

    #[test]
    fn test_leg_risk_manager_assess_chase() {
        let manager = LegRiskManager::new(LegRiskConfig::default());
        let mut state = create_test_state();

        // YES filled, NO not filled
        state.update_yes(LegStatus::Filled {
            order_id: "order-1".to_string(),
            size: dec!(100),
            price: dec!(0.45),
            fee: dec!(0.01),
        });
        state.update_no(LegStatus::Submitted {
            order_id: "order-2".to_string(),
            submitted_at: Utc::now(),
        });

        let assessment = manager.assess(&state, 300, dec!(0.45), dec!(0.52));
        assert!(matches!(assessment.action, LegRiskAction::AggressiveChase { .. }));

        if let LegRiskAction::AggressiveChase { outcome, .. } = assessment.action {
            assert_eq!(outcome, Outcome::No);
        }
    }

    #[test]
    fn test_leg_risk_manager_assess_emergency_close_exposure() {
        let mut config = LegRiskConfig::default();
        config.emergency_close_threshold = dec!(40); // Low threshold for test

        let manager = LegRiskManager::new(config);
        let mut state = create_test_state();

        // YES filled at 0.45 for 100 shares = $45 exposure
        state.update_yes(LegStatus::Filled {
            order_id: "order-1".to_string(),
            size: dec!(100),
            price: dec!(0.45),
            fee: dec!(0.01),
        });

        let assessment = manager.assess(&state, 300, dec!(0.45), dec!(0.52));
        assert!(matches!(
            assessment.action,
            LegRiskAction::EmergencyClose {
                reason: CloseReason::ExposureExceeded,
                ..
            }
        ));
    }

    #[test]
    fn test_leg_risk_manager_assess_emergency_close_failed() {
        let manager = LegRiskManager::new(LegRiskConfig::default());
        let mut state = create_test_state();

        state.update_yes(LegStatus::Filled {
            order_id: "order-1".to_string(),
            size: dec!(100),
            price: dec!(0.45),
            fee: dec!(0.01),
        });
        state.update_no(LegStatus::Failed {
            reason: "Order rejected".to_string(),
            at: Utc::now(),
        });

        let assessment = manager.assess(&state, 300, dec!(0.45), dec!(0.52));
        assert!(matches!(
            assessment.action,
            LegRiskAction::EmergencyClose {
                reason: CloseReason::LegFailed,
                ..
            }
        ));
    }

    #[test]
    fn test_leg_risk_manager_assess_window_closing() {
        let manager = LegRiskManager::new(LegRiskConfig::default());
        let mut state = create_test_state();

        state.update_yes(LegStatus::Filled {
            order_id: "order-1".to_string(),
            size: dec!(100),
            price: dec!(0.45),
            fee: dec!(0.01),
        });

        // Window closed
        let assessment = manager.assess(&state, 0, dec!(0.45), dec!(0.52));
        assert!(matches!(
            assessment.action,
            LegRiskAction::EmergencyClose {
                reason: CloseReason::WindowClosing,
                ..
            }
        ));
    }

    #[test]
    fn test_chase_reason_time_pressure() {
        let mut config = LegRiskConfig::default();
        config.time_pressure_secs = 120;

        let manager = LegRiskManager::new(config);
        let mut state = create_test_state();

        state.update_yes(LegStatus::Filled {
            order_id: "order-1".to_string(),
            size: dec!(100),
            price: dec!(0.45),
            fee: dec!(0.01),
        });
        state.update_no(LegStatus::Submitted {
            order_id: "order-2".to_string(),
            submitted_at: Utc::now(),
        });

        // Under time pressure threshold
        let assessment = manager.assess(&state, 60, dec!(0.45), dec!(0.52));

        if let LegRiskAction::AggressiveChase { reason, .. } = assessment.action {
            assert_eq!(reason, ChaseReason::TimePressure);
        } else {
            panic!("Expected AggressiveChase action");
        }
    }

    #[test]
    fn test_create_emergency_close_order() {
        let manager = LegRiskManager::new(LegRiskConfig::default());

        let order = manager.create_emergency_close_order(
            "req-1".to_string(),
            "event-1".to_string(),
            "token-1".to_string(),
            Outcome::Yes,
            dec!(100),
            dec!(0.40),
        );

        assert_eq!(order.outcome, Outcome::Yes);
        assert_eq!(order.side, poly_common::types::Side::Sell);
        assert_eq!(order.size, dec!(100));
        assert_eq!(order.price, Some(dec!(0.40)));
        assert_eq!(order.order_type, OrderType::Ioc);
    }

    #[test]
    fn test_create_chase_order() {
        let manager = LegRiskManager::new(LegRiskConfig::default());

        let order = manager.create_chase_order(
            "req-1".to_string(),
            "event-1".to_string(),
            "token-1".to_string(),
            Outcome::No,
            dec!(100),
            dec!(0.54),
        );

        assert_eq!(order.outcome, Outcome::No);
        assert_eq!(order.side, poly_common::types::Side::Buy);
        assert_eq!(order.size, dec!(100));
        assert_eq!(order.price, Some(dec!(0.54)));
        assert_eq!(order.order_type, OrderType::Ioc);
    }

    #[test]
    fn test_inventory_leg_risk() {
        let manager = LegRiskManager::new(LegRiskConfig::default());
        let mut inventory = Inventory::new("event-1".to_string());

        // Balanced - no leg risk
        inventory.yes_shares = dec!(100);
        inventory.no_shares = dec!(100);
        assert!(!manager.inventory_has_leg_risk(&inventory));

        // Imbalanced - has leg risk
        inventory.yes_shares = dec!(100);
        inventory.no_shares = dec!(20);
        assert!(manager.inventory_has_leg_risk(&inventory));
    }

    #[test]
    fn test_inventory_exposure() {
        let manager = LegRiskManager::new(LegRiskConfig::default());
        let mut inventory = Inventory::new("event-1".to_string());

        // 100 YES at $45, 50 NO at $26
        inventory.yes_shares = dec!(100);
        inventory.yes_cost_basis = dec!(45);
        inventory.no_shares = dec!(50);
        inventory.no_cost_basis = dec!(26);

        // Unmatched: 50 YES at avg cost of $0.45/share = $22.50 exposure
        let exposure = manager.inventory_exposure(&inventory);
        assert_eq!(exposure, dec!(22.5));
    }

    #[test]
    fn test_should_emergency_close_inventory() {
        let mut config = LegRiskConfig::default();
        config.emergency_close_threshold = dec!(20);

        let manager = LegRiskManager::new(config);
        let mut inventory = Inventory::new("event-1".to_string());

        inventory.yes_shares = dec!(100);
        inventory.yes_cost_basis = dec!(45);
        inventory.no_shares = dec!(50);
        inventory.no_cost_basis = dec!(26);

        // Exposure = $22.50, threshold = $20
        assert!(manager.should_emergency_close_inventory(&inventory));
    }

    #[test]
    fn test_leg_risk_config_from_risk_config() {
        let risk_config = RiskConfig {
            emergency_close_threshold: dec!(150),
            ..Default::default()
        };

        let leg_config = LegRiskConfig::from_risk_config(&risk_config);
        assert_eq!(leg_config.emergency_close_threshold, dec!(150));
    }

    #[test]
    fn test_leg_state_potential_loss() {
        let mut state = create_test_state();

        // YES filled at 0.45
        state.update_yes(LegStatus::Filled {
            order_id: "order-1".to_string(),
            size: dec!(100),
            price: dec!(0.45),
            fee: dec!(0.01),
        });

        // Current YES price dropped to 0.40
        // Cost = 100 * 0.45 = 45
        // Market value = 100 * 0.40 = 40
        // Loss = 45 - 40 = 5
        let loss = state.potential_loss(dec!(0.40), dec!(0.55));
        assert_eq!(loss, dec!(5));
    }

    #[test]
    fn test_leg_state_total_filled_size() {
        let mut state = create_test_state();

        state.update_yes(LegStatus::Filled {
            order_id: "order-1".to_string(),
            size: dec!(100),
            price: dec!(0.45),
            fee: dec!(0.01),
        });

        assert_eq!(state.total_filled_size(), dec!(100));

        state.update_no(LegStatus::PartialFill {
            order_id: "order-2".to_string(),
            filled_size: dec!(50),
            avg_price: dec!(0.52),
        });

        assert_eq!(state.total_filled_size(), dec!(150));
    }

    #[test]
    fn test_leg_status_filled_size() {
        assert_eq!(LegStatus::Pending.filled_size(), Decimal::ZERO);

        assert_eq!(
            LegStatus::Filled {
                order_id: "1".to_string(),
                size: dec!(100),
                price: dec!(0.5),
                fee: dec!(0.01),
            }
            .filled_size(),
            dec!(100)
        );

        assert_eq!(
            LegStatus::PartialFill {
                order_id: "1".to_string(),
                filled_size: dec!(75),
                avg_price: dec!(0.5),
            }
            .filled_size(),
            dec!(75)
        );

        assert_eq!(
            LegStatus::Cancelled {
                order_id: "1".to_string(),
                filled_size: dec!(25),
            }
            .filled_size(),
            dec!(25)
        );
    }

    #[test]
    fn test_leg_risk_assessment_serialization() {
        let assessment = LegRiskAssessment {
            action: LegRiskAction::AggressiveChase {
                outcome: Outcome::No,
                target_size: dec!(100),
                max_price: dec!(0.54),
                reason: ChaseReason::TimePressure,
            },
            exposure: dec!(45),
            potential_loss: dec!(5),
            elapsed_ms: 1500,
            seconds_remaining: 60,
            assessed_at: Utc::now(),
        };

        let json = serde_json::to_string(&assessment).unwrap();
        assert!(json.contains("AggressiveChase"));
        assert!(json.contains("TimePressure"));
    }
}
