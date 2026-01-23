//! Auto-claim module for redeeming resolved market positions.
//!
//! This module automatically redeems winning positions when markets resolve,
//! converting outcome tokens back to USDC so the bot can continue trading.
//!
//! # How It Works
//!
//! 1. Queries the Polymarket API for redeemable positions
//! 2. Calls ProxyWalletFactory.proxy() to execute redemption through proxy wallet
//! 3. The proxy wallet calls CTF contract's `redeemPositions`
//! 4. Balance is updated automatically after redemption
//!
//! # Architecture Note
//!
//! Polymarket uses proxy wallets (EIP-1167 minimal proxies) to hold user positions.
//! The EOA cannot directly call CTF.redeemPositions because the tokens are held
//! by the proxy wallet. Instead, we call the ProxyWalletFactory.proxy() function
//! which forwards calls to the user's proxy wallet.
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
use std::time::Duration;

use alloy::primitives::{Address, Bytes, B256, U256};
use alloy::providers::ProviderBuilder;
use alloy::signers::local::LocalSigner;
use alloy::sol;
use rust_decimal::Decimal;
use thiserror::Error;
use tokio::time::sleep;
use tracing::{debug, error, info, warn};

use polymarket_client_sdk::data::Client as DataClient;
use polymarket_client_sdk::data::types::request::PositionsRequest;
use polymarket_client_sdk::data::types::response::Position;
use polymarket_client_sdk::{derive_proxy_wallet, POLYGON};

/// USDC contract address on Polygon mainnet.
const USDC_ADDRESS: &str = "0x2791Bca1f2de4661ED88A30C99A7a9449Aa84174";

/// Conditional Tokens Framework contract address on Polygon mainnet.
const CTF_ADDRESS: &str = "0x4D97DCd97eC945f40cF65F87097ACe5EA0476045";

/// Polygon RPC endpoint.
const POLYGON_RPC: &str = "https://polygon-rpc.com";

// Define the ProxyWallet contract interface (called DIRECTLY on the user's proxy wallet)
sol! {
    /// Call type for proxy calls (0 = Call, 1 = DelegateCall)
    #[derive(Debug)]
    enum CallType {
        Call,
        DelegateCall,
    }

    /// ProxyCall struct for the proxy() function
    #[derive(Debug)]
    struct ProxyCall {
        address to;
        CallType typeCode;
        bytes data;
        uint256 value;
    }

    /// ProxyWallet contract interface - call DIRECTLY on the proxy wallet address
    /// The proxy wallet is owned by the EOA and can execute calls on its behalf
    #[sol(rpc)]
    contract ProxyWallet {
        function proxy(ProxyCall[] memory calls) public payable returns (bytes[] memory returnValues);
    }

    /// CTF contract interface (just the redeemPositions function)
    #[sol(rpc)]
    contract ConditionalTokens {
        function redeemPositions(
            address collateralToken,
            bytes32 parentCollectionId,
            bytes32 conditionId,
            uint256[] calldata indexSets
        ) external;
    }
}

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
    ///
    /// This works by calling ProxyWalletFactory.proxy() which forwards the
    /// redeemPositions call to our proxy wallet. The proxy wallet then calls
    /// the CTF contract, so msg.sender for CTF is the proxy wallet (which
    /// holds the tokens), not the EOA.
    async fn redeem_position(&self, condition_id: B256, size: Decimal) -> Result<RedeemResult, AutoClaimError> {
        use alloy::sol_types::SolCall;

        // Create provider with wallet for this redemption
        let key_str = self.private_key.strip_prefix("0x").unwrap_or(&self.private_key);
        let signer = LocalSigner::from_str(key_str)
            .map_err(|e| AutoClaimError::InvalidKey(e.to_string()))?;

        let provider = ProviderBuilder::new()
            .wallet(signer)
            .connect(POLYGON_RPC)
            .await
            .map_err(|e| AutoClaimError::Connection(e.to_string()))?;

        let condition_id_str = format!("0x{}", hex::encode(condition_id));
        info!(
            condition_id = %condition_id_str,
            size = %size,
            proxy_wallet = %self.proxy_wallet,
            "Attempting to redeem position via proxy factory"
        );

        // Parse CTF contract address
        let ctf_address: Address = CTF_ADDRESS
            .parse()
            .map_err(|e| AutoClaimError::ClientInit(format!("Invalid CTF address: {}", e)))?;

        // Encode the redeemPositions call for CTF contract
        // For binary markets: indexSets = [1, 2] (YES and NO outcomes)
        let redeem_call = ConditionalTokens::redeemPositionsCall {
            collateralToken: self.usdc_address,
            parentCollectionId: B256::ZERO, // No parent collection for top-level positions
            conditionId: condition_id,
            indexSets: vec![U256::from(1), U256::from(2)], // Binary market: YES=1, NO=2
        };
        let redeem_calldata = redeem_call.abi_encode();

        // Create ProxyCall struct
        let proxy_call = ProxyCall {
            to: ctf_address,
            typeCode: CallType::Call,
            data: Bytes::from(redeem_calldata),
            value: U256::ZERO,
        };

        // Call the proxy wallet DIRECTLY (not through the factory)
        // The proxy wallet is owned by the EOA and allows owner to execute arbitrary calls
        let proxy_wallet_contract = ProxyWallet::new(self.proxy_wallet, &provider);

        // Call proxy() with our redemption call
        // This executes the redeemPositions call from the proxy wallet (which holds the tokens)
        let tx_builder = proxy_wallet_contract.proxy(vec![proxy_call]);

        let pending_tx = tx_builder
            .send()
            .await
            .map_err(|e| AutoClaimError::RedeemFailed(format!("Failed to send proxy tx: {}", e)))?;

        let tx_hash = *pending_tx.tx_hash();
        let tx_hash_str = format!("0x{}", hex::encode(tx_hash));

        info!(
            condition_id = %condition_id_str,
            tx_hash = %tx_hash_str,
            "Proxy transaction submitted, waiting for confirmation..."
        );

        // Wait for transaction to be mined
        let receipt = pending_tx
            .get_receipt()
            .await
            .map_err(|e| AutoClaimError::RedeemFailed(format!("Failed to get receipt: {}", e)))?;

        let block_number = receipt.block_number.unwrap_or(0);

        // Check if transaction succeeded
        if !receipt.status() {
            return Err(AutoClaimError::RedeemFailed(format!(
                "Transaction reverted in block {}",
                block_number
            )));
        }

        info!(
            condition_id = %condition_id_str,
            tx_hash = %tx_hash_str,
            block = block_number,
            "Successfully redeemed position via proxy"
        );

        Ok(RedeemResult {
            condition_id: condition_id_str,
            transaction_hash: tx_hash_str,
            block_number,
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

        let estimated_time_secs = if positions.len() > 1 {
            (positions.len() - 1) as u64 * 12 // 12s delay between each
        } else {
            0
        };
        info!(
            count = positions.len(),
            estimated_time_secs,
            "Found redeemable positions (processing with rate limit delays)"
        );

        let mut results = Vec::new();

        // Delay between redemptions to avoid RPC rate limiting
        // Polygon RPC returns "retry in 10s" on rate limit, so we use 12s to be safe
        const REDEMPTION_DELAY: Duration = Duration::from_secs(12);
        const RATE_LIMIT_DELAY: Duration = Duration::from_secs(15);

        for (i, position) in positions.iter().enumerate() {
            // Add delay before each redemption (except the first one)
            if i > 0 {
                debug!("Waiting {}s before next redemption to avoid rate limiting", REDEMPTION_DELAY.as_secs());
                sleep(REDEMPTION_DELAY).await;
            }

            match self.redeem_position(position.condition_id, position.size).await {
                Ok(result) => {
                    results.push(result);
                }
                Err(AutoClaimError::RedeemFailed(msg)) => {
                    // Check if it's a rate limit error
                    if msg.contains("Too many requests") || msg.contains("rate limit") {
                        warn!(
                            condition_id = %format!("0x{}", hex::encode(position.condition_id)),
                            "Rate limited, waiting {}s before continuing",
                            RATE_LIMIT_DELAY.as_secs()
                        );
                        sleep(RATE_LIMIT_DELAY).await;
                    } else {
                        // Log but continue with other positions
                        warn!(
                            condition_id = %format!("0x{}", hex::encode(position.condition_id)),
                            error = %msg,
                            "Failed to redeem position"
                        );
                    }
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
