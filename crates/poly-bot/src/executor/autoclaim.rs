//! Auto-claim module for redeeming resolved market positions.
//!
//! This module automatically redeems winning positions when markets resolve,
//! converting outcome tokens back to USDC so the bot can continue trading.
//!
//! # How It Works
//!
//! 1. Tracks positions we hold via `TrackedPosition`
//! 2. Periodically checks if tracked markets have resolved
//! 3. Calls CTF contract's `redeemPositions` for resolved markets
//! 4. Updates balance after successful redemption
//!
//! # Usage
//!
//! ```ignore
//! let autoclaim = AutoClaimManager::new(private_key).await?;
//! autoclaim.track_position(condition_id, token_id, outcome, size);
//!
//! // Call periodically (e.g., every minute)
//! let redeemed = autoclaim.claim_resolved().await?;
//! ```

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;

use alloy::primitives::{Address, B256};
use alloy::providers::ProviderBuilder;
use alloy::signers::local::LocalSigner;
use chrono::{DateTime, Utc};
use parking_lot::RwLock;
use rust_decimal::Decimal;
use thiserror::Error;
use tracing::{debug, error, info, warn};

use poly_common::types::Outcome;
use polymarket_client_sdk::ctf::types::RedeemPositionsRequest;

/// USDC contract address on Polygon mainnet.
const USDC_ADDRESS: &str = "0x2791Bca1f2de4661ED88A30C99A7a9449Aa84174";

/// Polygon mainnet chain ID.
const POLYGON_CHAIN_ID: u64 = 137;

/// Polygon RPC endpoint.
const POLYGON_RPC: &str = "https://polygon-rpc.com";

/// Errors that can occur during auto-claim operations.
#[derive(Debug, Error)]
pub enum AutoClaimError {
    #[error("Failed to connect to RPC: {0}")]
    Connection(String),

    #[error("Failed to create CTF client: {0}")]
    ClientInit(String),

    #[error("Invalid private key: {0}")]
    InvalidKey(String),

    #[error("Redeem transaction failed: {0}")]
    RedeemFailed(String),

    #[error("Invalid condition ID: {0}")]
    InvalidConditionId(String),

    #[error("Market not resolved yet")]
    NotResolved,
}

/// A position tracked for potential redemption.
#[derive(Debug, Clone)]
pub struct TrackedPosition {
    /// Condition ID (market identifier for CTF contract).
    pub condition_id: String,
    /// Token ID for the position.
    pub token_id: String,
    /// Outcome (YES/NO).
    pub outcome: Outcome,
    /// Size held.
    pub size: Decimal,
    /// When the position was opened.
    pub opened_at: DateTime<Utc>,
    /// When the market expires (for scheduling claim attempts).
    pub expires_at: Option<DateTime<Utc>>,
    /// Whether we've attempted to redeem this position.
    pub redeem_attempted: bool,
}

/// Result of a redemption attempt.
#[derive(Debug, Clone)]
pub struct RedeemResult {
    /// Condition ID that was redeemed.
    pub condition_id: String,
    /// Transaction hash.
    pub transaction_hash: String,
    /// Block number where redemption was confirmed.
    pub block_number: u64,
    /// Amount of USDC received (estimated).
    pub amount_usdc: Decimal,
}

/// Auto-claim manager for redeeming resolved positions.
///
/// This struct manages position tracking and redemption. It owns all resources
/// needed to interact with the CTF contract.
pub struct AutoClaimManager {
    /// Private key for signing transactions.
    private_key: String,
    /// USDC token address.
    usdc_address: Address,
    /// Tracked positions by condition_id.
    positions: Arc<RwLock<HashMap<String, TrackedPosition>>>,
    /// Successfully redeemed condition IDs.
    redeemed: Arc<RwLock<Vec<String>>>,
}

impl AutoClaimManager {
    /// Create a new AutoClaimManager.
    ///
    /// # Arguments
    ///
    /// * `private_key` - Private key for signing transactions
    ///
    /// # Errors
    ///
    /// Returns error if the private key is invalid.
    pub fn new(private_key: String) -> Result<Self, AutoClaimError> {
        // Validate the private key
        let key_str = private_key.strip_prefix("0x").unwrap_or(&private_key);
        let _ = LocalSigner::from_str(key_str)
            .map_err(|e| AutoClaimError::InvalidKey(e.to_string()))?;

        let usdc_address = USDC_ADDRESS
            .parse::<Address>()
            .map_err(|e| AutoClaimError::ClientInit(format!("Invalid USDC address: {}", e)))?;

        Ok(Self {
            private_key,
            usdc_address,
            positions: Arc::new(RwLock::new(HashMap::new())),
            redeemed: Arc::new(RwLock::new(Vec::new())),
        })
    }

    /// Track a new position for potential redemption.
    ///
    /// Call this when the bot opens a position so we can redeem it later.
    pub fn track_position(
        &self,
        condition_id: impl Into<String>,
        token_id: impl Into<String>,
        outcome: Outcome,
        size: Decimal,
        expires_at: Option<DateTime<Utc>>,
    ) {
        let condition_id = condition_id.into();
        let token_id = token_id.into();

        let position = TrackedPosition {
            condition_id: condition_id.clone(),
            token_id,
            outcome,
            size,
            opened_at: Utc::now(),
            expires_at,
            redeem_attempted: false,
        };

        let mut positions = self.positions.write();

        // If we already have a position for this condition, update the size
        if let Some(existing) = positions.get_mut(&condition_id) {
            existing.size += size;
            debug!(
                condition_id = %condition_id,
                new_size = %existing.size,
                "Updated tracked position size"
            );
        } else {
            positions.insert(condition_id.clone(), position);
            debug!(
                condition_id = %condition_id,
                size = %size,
                outcome = ?outcome,
                "Tracking new position for auto-claim"
            );
        }
    }

    /// Remove a tracked position (e.g., when sold before resolution).
    pub fn remove_position(&self, condition_id: &str, size: Decimal) {
        let mut positions = self.positions.write();

        if let Some(position) = positions.get_mut(condition_id) {
            position.size -= size;

            if position.size <= Decimal::ZERO {
                positions.remove(condition_id);
                debug!(condition_id = %condition_id, "Removed tracked position (fully sold)");
            } else {
                debug!(
                    condition_id = %condition_id,
                    remaining = %position.size,
                    "Reduced tracked position size"
                );
            }
        }
    }

    /// Get all tracked positions.
    pub fn tracked_positions(&self) -> Vec<TrackedPosition> {
        self.positions.read().values().cloned().collect()
    }

    /// Get count of tracked positions.
    pub fn position_count(&self) -> usize {
        self.positions.read().len()
    }

    /// Attempt to redeem a specific position.
    ///
    /// Returns Ok(RedeemResult) if successful, or error if market not resolved
    /// or transaction fails.
    pub async fn redeem_position(&self, condition_id: &str) -> Result<RedeemResult, AutoClaimError> {
        use polymarket_client_sdk::ctf::Client as CtfClient;

        // Parse condition_id as B256
        let condition_id_hex = condition_id.strip_prefix("0x").unwrap_or(condition_id);
        let condition_bytes = hex::decode(condition_id_hex)
            .map_err(|e| AutoClaimError::InvalidConditionId(e.to_string()))?;

        if condition_bytes.len() != 32 {
            return Err(AutoClaimError::InvalidConditionId(format!(
                "Condition ID must be 32 bytes, got {}",
                condition_bytes.len()
            )));
        }

        let condition_b256 = B256::from_slice(&condition_bytes);

        // Create provider with wallet for this redemption
        let key_str = self.private_key.strip_prefix("0x").unwrap_or(&self.private_key);
        let signer = LocalSigner::from_str(key_str)
            .map_err(|e| AutoClaimError::InvalidKey(e.to_string()))?;

        let provider = ProviderBuilder::new()
            .wallet(signer)
            .connect(POLYGON_RPC)
            .await
            .map_err(|e| AutoClaimError::Connection(e.to_string()))?;

        let ctf_client = CtfClient::new(provider, POLYGON_CHAIN_ID)
            .map_err(|e| AutoClaimError::ClientInit(e.to_string()))?;

        // Create redeem request for binary market
        let redeem_req = RedeemPositionsRequest::for_binary_market(
            self.usdc_address,
            condition_b256,
        );

        info!(condition_id = %condition_id, "Attempting to redeem position");

        // Execute redemption
        let result = ctf_client
            .redeem_positions(&redeem_req)
            .await
            .map_err(|e| AutoClaimError::RedeemFailed(e.to_string()))?;

        let tx_hash = format!("0x{}", hex::encode(result.transaction_hash));

        info!(
            condition_id = %condition_id,
            tx_hash = %tx_hash,
            block = result.block_number,
            "Successfully redeemed position"
        );

        // Mark as redeemed
        {
            let mut positions = self.positions.write();
            positions.remove(condition_id);
        }
        {
            let mut redeemed = self.redeemed.write();
            redeemed.push(condition_id.to_string());
        }

        Ok(RedeemResult {
            condition_id: condition_id.to_string(),
            transaction_hash: tx_hash,
            block_number: result.block_number,
            amount_usdc: Decimal::ZERO, // TODO: Parse from events
        })
    }

    /// Attempt to claim all positions that may have resolved.
    ///
    /// This is safe to call periodically - it will skip positions that
    /// haven't resolved yet (the CTF contract will revert, which we catch).
    ///
    /// Returns list of successfully redeemed positions.
    pub async fn claim_all_resolved(&self) -> Vec<RedeemResult> {
        let positions: Vec<TrackedPosition> = {
            self.positions.read().values().cloned().collect()
        };

        if positions.is_empty() {
            return Vec::new();
        }

        debug!(count = positions.len(), "Checking positions for redemption");

        let mut results = Vec::new();

        for position in positions {
            // Skip if already attempted recently
            if position.redeem_attempted {
                continue;
            }

            // Only try to redeem if market should have expired
            if let Some(expires_at) = position.expires_at
                && Utc::now() < expires_at
            {
                debug!(
                    condition_id = %position.condition_id,
                    expires_at = %expires_at,
                    "Market not expired yet, skipping"
                );
                continue;
            }

            match self.redeem_position(&position.condition_id).await {
                Ok(result) => {
                    results.push(result);
                }
                Err(AutoClaimError::RedeemFailed(msg)) => {
                    // This is expected for unresolved markets
                    if msg.contains("not resolved") || msg.contains("execution reverted") {
                        debug!(
                            condition_id = %position.condition_id,
                            "Market not resolved yet"
                        );
                    } else {
                        warn!(
                            condition_id = %position.condition_id,
                            error = %msg,
                            "Failed to redeem position"
                        );
                    }

                    // Mark as attempted to avoid spam
                    let mut positions = self.positions.write();
                    if let Some(p) = positions.get_mut(&position.condition_id) {
                        p.redeem_attempted = true;
                    }
                }
                Err(e) => {
                    error!(
                        condition_id = %position.condition_id,
                        error = %e,
                        "Error attempting redemption"
                    );
                }
            }
        }

        if !results.is_empty() {
            info!(
                count = results.len(),
                "Successfully redeemed positions"
            );
        }

        results
    }

    /// Reset the "attempted" flag on all positions.
    ///
    /// Call this periodically (e.g., every hour) to retry failed redemptions.
    pub fn reset_attempted_flags(&self) {
        let mut positions = self.positions.write();
        for position in positions.values_mut() {
            position.redeem_attempted = false;
        }
        debug!(count = positions.len(), "Reset redemption attempt flags");
    }

    /// Get list of condition IDs that have been successfully redeemed.
    pub fn redeemed_conditions(&self) -> Vec<String> {
        self.redeemed.read().clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn test_track_position() {
        // Use a valid test private key format
        let manager = AutoClaimManager::new(
            "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80".to_string()
        ).unwrap();

        manager.track_position("0x1234", "token_yes", Outcome::Yes, dec!(100), None);

        assert_eq!(manager.position_count(), 1);
        let positions = manager.tracked_positions();
        assert_eq!(positions[0].size, dec!(100));
    }

    #[test]
    fn test_condition_id_parsing() {
        let condition_id = "0x1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef";
        let condition_id_hex = condition_id.strip_prefix("0x").unwrap();
        let condition_bytes = hex::decode(condition_id_hex).unwrap();

        assert_eq!(condition_bytes.len(), 32);

        let _condition_b256 = B256::from_slice(&condition_bytes);
    }

    #[test]
    fn test_update_position_size() {
        let manager = AutoClaimManager::new(
            "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80".to_string()
        ).unwrap();

        // Add initial position
        manager.track_position("0x1234", "token_yes", Outcome::Yes, dec!(100), None);

        // Add more to same position
        manager.track_position("0x1234", "token_yes", Outcome::Yes, dec!(50), None);

        let positions = manager.tracked_positions();
        assert_eq!(positions[0].size, dec!(150));
    }

    #[test]
    fn test_remove_position() {
        let manager = AutoClaimManager::new(
            "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80".to_string()
        ).unwrap();

        manager.track_position("0x1234", "token_yes", Outcome::Yes, dec!(100), None);
        manager.remove_position("0x1234", dec!(30));

        let positions = manager.tracked_positions();
        assert_eq!(positions[0].size, dec!(70));

        // Remove the rest
        manager.remove_position("0x1234", dec!(70));
        assert_eq!(manager.position_count(), 0);
    }
}
