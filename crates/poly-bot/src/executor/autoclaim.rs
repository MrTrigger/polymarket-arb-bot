//! Auto-claim module for redeeming resolved market positions.
//!
//! This module automatically redeems winning positions when markets resolve,
//! converting outcome tokens back to USDC so the bot can continue trading.
//!
//! # How It Works
//!
//! 1. Queries the Polymarket API for redeemable positions
//! 2. Calls CTF contract's `redeemPositions` for each resolved position
//! 3. Balance is updated automatically after redemption
//!
//! # Usage
//!
//! ```ignore
//! let autoclaim = AutoClaimManager::new(private_key)?;
//!
//! // Call periodically (e.g., every minute)
//! let redeemed = autoclaim.claim_all_redeemable().await?;
//! ```

use std::str::FromStr;

use alloy::primitives::{Address, B256};
use alloy::providers::ProviderBuilder;
use alloy::signers::local::LocalSigner;
use rust_decimal::Decimal;
use thiserror::Error;
use tracing::{debug, error, info, warn};

use polymarket_client_sdk::ctf::types::RedeemPositionsRequest;
use polymarket_client_sdk::data::Client as DataClient;
use polymarket_client_sdk::data::types::request::PositionsRequest;
use polymarket_client_sdk::data::types::response::Position;
use polymarket_client_sdk::{derive_proxy_wallet, POLYGON};

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

    #[error("Failed to create client: {0}")]
    ClientInit(String),

    #[error("Invalid private key: {0}")]
    InvalidKey(String),

    #[error("Redeem transaction failed: {0}")]
    RedeemFailed(String),

    #[error("Failed to query positions: {0}")]
    QueryFailed(String),

    #[error("Invalid condition ID: {0}")]
    InvalidConditionId(String),
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
    /// Size that was redeemed.
    pub size: Decimal,
}

/// Auto-claim manager for redeeming resolved positions.
///
/// Queries the Polymarket API for redeemable positions and redeems them
/// via the CTF contract. No manual position tracking needed.
pub struct AutoClaimManager {
    /// Private key for signing transactions.
    private_key: String,
    /// Proxy wallet address (derived from private key).
    proxy_wallet: Address,
    /// USDC token address.
    usdc_address: Address,
    /// Data client for querying positions.
    data_client: DataClient,
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
        // Validate and parse the private key
        let key_str = private_key.strip_prefix("0x").unwrap_or(&private_key);
        let signer = LocalSigner::from_str(key_str)
            .map_err(|e| AutoClaimError::InvalidKey(e.to_string()))?;

        // Derive the proxy wallet address
        let eoa_address = signer.address();
        let proxy_wallet = derive_proxy_wallet(eoa_address, POLYGON)
            .ok_or_else(|| AutoClaimError::ClientInit("Failed to derive proxy wallet".to_string()))?;

        let usdc_address = USDC_ADDRESS
            .parse::<Address>()
            .map_err(|e| AutoClaimError::ClientInit(format!("Invalid USDC address: {}", e)))?;

        // Create data client for querying positions (use default API host)
        let data_client = DataClient::new("https://data-api.polymarket.com")
            .map_err(|e| AutoClaimError::ClientInit(format!("Failed to create data client: {}", e)))?;

        info!(
            eoa = %eoa_address,
            proxy_wallet = %proxy_wallet,
            "AutoClaimManager initialized"
        );

        Ok(Self {
            private_key,
            proxy_wallet,
            usdc_address,
            data_client,
        })
    }

    /// Query the API for all redeemable positions.
    async fn get_redeemable_positions(&self) -> Result<Vec<Position>, AutoClaimError> {
        let request = PositionsRequest::builder()
            .user(self.proxy_wallet)
            .redeemable(true)
            .build();

        self.data_client
            .positions(&request)
            .await
            .map_err(|e| AutoClaimError::QueryFailed(e.to_string()))
    }

    /// Redeem a single position by condition ID.
    async fn redeem_position(&self, condition_id: B256, size: Decimal) -> Result<RedeemResult, AutoClaimError> {
        use polymarket_client_sdk::ctf::Client as CtfClient;

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
            condition_id,
        );

        let condition_id_str = format!("0x{}", hex::encode(condition_id));
        info!(condition_id = %condition_id_str, size = %size, "Attempting to redeem position");

        // Execute redemption
        let result = ctf_client
            .redeem_positions(&redeem_req)
            .await
            .map_err(|e| AutoClaimError::RedeemFailed(e.to_string()))?;

        let tx_hash = format!("0x{}", hex::encode(result.transaction_hash));

        info!(
            condition_id = %condition_id_str,
            tx_hash = %tx_hash,
            block = result.block_number,
            "Successfully redeemed position"
        );

        Ok(RedeemResult {
            condition_id: condition_id_str,
            transaction_hash: tx_hash,
            block_number: result.block_number,
            size,
        })
    }

    /// Claim all redeemable positions.
    ///
    /// Queries the API for positions that can be redeemed and redeems each one.
    /// Safe to call periodically - only redeems positions that are actually redeemable.
    ///
    /// Returns list of successfully redeemed positions.
    pub async fn claim_all_redeemable(&self) -> Vec<RedeemResult> {
        // Query API for redeemable positions
        let positions = match self.get_redeemable_positions().await {
            Ok(p) => p,
            Err(e) => {
                warn!(error = %e, "Failed to query redeemable positions");
                return Vec::new();
            }
        };

        if positions.is_empty() {
            debug!("No redeemable positions found");
            return Vec::new();
        }

        info!(count = positions.len(), "Found redeemable positions");

        let mut results = Vec::new();

        for position in positions {
            match self.redeem_position(position.condition_id, position.size).await {
                Ok(result) => {
                    results.push(result);
                }
                Err(AutoClaimError::RedeemFailed(msg)) => {
                    // Log but continue with other positions
                    warn!(
                        condition_id = %format!("0x{}", hex::encode(position.condition_id)),
                        error = %msg,
                        "Failed to redeem position"
                    );
                }
                Err(e) => {
                    error!(
                        condition_id = %format!("0x{}", hex::encode(position.condition_id)),
                        error = %e,
                        "Error attempting redemption"
                    );
                }
            }
        }

        if !results.is_empty() {
            let total_size: Decimal = results.iter().map(|r| r.size).sum();
            info!(
                count = results.len(),
                total_size = %total_size,
                "Successfully redeemed positions"
            );
        }

        results
    }

    /// Get the proxy wallet address being used.
    pub fn proxy_wallet(&self) -> Address {
        self.proxy_wallet
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_manager_creation() {
        // Use a valid test private key format
        let manager = AutoClaimManager::new(
            "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80".to_string()
        );

        assert!(manager.is_ok());
        let manager = manager.unwrap();

        // Proxy wallet should be derived
        assert_ne!(manager.proxy_wallet(), Address::ZERO);
    }

    #[test]
    fn test_invalid_private_key() {
        let result = AutoClaimManager::new("invalid_key".to_string());
        assert!(result.is_err());
    }

    #[test]
    fn test_condition_id_formatting() {
        let condition_id = B256::from_slice(&[0x12; 32]);
        let formatted = format!("0x{}", hex::encode(condition_id));

        assert!(formatted.starts_with("0x"));
        assert_eq!(formatted.len(), 66); // 0x + 64 hex chars
    }
}
