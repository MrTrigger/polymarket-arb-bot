//! Live executor for real order submission via Polymarket CLOB API.
//!
//! This module implements real trading against Polymarket's CLOB exchange.
//! It handles:
//!
//! - EIP-712 order signing for authentication
//! - Order submission via REST API
//! - Order cancellation
//! - Fill tracking via polling
//! - Shadow bid firing on primary fills
//! - Inventory updates
//!
//! ## Security
//!
//! Private keys are never stored in config files. They must be provided
//! via environment variables (POLY_PRIVATE_KEY).
//!
//! ## Performance
//!
//! Shadow bids fire within 2ms of primary fill detection by using
//! pre-hashed order data from the ShadowManager.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use ethers_core::types::{Address, Signature, H256, U256};
use reqwest::Client;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use sha3::{Digest, Keccak256};
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use poly_common::types::{Outcome, Side};

use super::shadow::{ShadowManager, ShadowOrder, SharedShadowManager};
use super::{
    Executor, ExecutorError, OrderCancellation, OrderFill, OrderRequest,
    OrderResult, OrderType, PartialOrderFill, PendingOrder,
};
use crate::config::{ExecutionConfig, ShadowConfig, WalletConfig};

/// Polymarket CLOB API endpoints.
pub mod endpoints {
    /// Production CLOB REST API.
    pub const CLOB_REST: &str = "https://clob.polymarket.com";
    /// Production CLOB WebSocket.
    pub const CLOB_WS: &str = "wss://ws-subscriptions-clob.polymarket.com/ws/user";
    /// Polygon mainnet chain ID.
    pub const CHAIN_ID: u64 = 137;
    /// CTF Exchange contract address (Polygon mainnet).
    pub const CTF_EXCHANGE: &str = "0x4bFb41d5B3570DeFd03C39a9A4D8dE6Bd8B8982E";
}

/// Configuration for the live executor.
#[derive(Debug, Clone)]
pub struct LiveExecutorConfig {
    /// Polymarket CLOB API endpoint.
    pub api_endpoint: String,
    /// Maximum order timeout in milliseconds.
    pub order_timeout_ms: u64,
    /// Whether to enable shadow bids.
    pub enable_shadow: bool,
    /// Price chase step size.
    pub chase_step_size: Decimal,
    /// Fill poll interval in milliseconds.
    pub fill_poll_interval_ms: u64,
    /// Maximum fill poll attempts before timeout.
    pub max_poll_attempts: u32,
    /// Fee rate for orders (basis points).
    pub fee_rate_bps: u32,
}

impl Default for LiveExecutorConfig {
    fn default() -> Self {
        Self {
            api_endpoint: endpoints::CLOB_REST.to_string(),
            order_timeout_ms: 30_000,
            enable_shadow: true,
            chase_step_size: Decimal::new(1, 2), // 0.01
            fill_poll_interval_ms: 100,
            max_poll_attempts: 300, // 30 seconds at 100ms intervals
            fee_rate_bps: 0, // Polymarket has 0% maker fees
        }
    }
}

impl LiveExecutorConfig {
    /// Create config from ExecutionConfig.
    pub fn from_execution_config(exec_config: &ExecutionConfig) -> Self {
        Self {
            order_timeout_ms: exec_config.order_timeout_ms,
            chase_step_size: exec_config.chase_step_size,
            ..Default::default()
        }
    }
}

/// Wallet for signing orders.
#[derive(Clone)]
pub struct Wallet {
    /// Address derived from private key.
    address: Address,
    /// Private key bytes (32 bytes).
    private_key: [u8; 32],
    /// API key for CLOB authentication.
    api_key: String,
    /// API secret for CLOB authentication.
    api_secret: String,
    /// API passphrase for CLOB authentication.
    api_passphrase: String,
}

impl std::fmt::Debug for Wallet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Wallet")
            .field("address", &self.address)
            .field("private_key", &"[REDACTED]")
            .field("api_key", &"[REDACTED]")
            .finish()
    }
}

impl Wallet {
    /// Create a new wallet from private key hex string.
    pub fn from_private_key(private_key_hex: &str, api_key: String, api_secret: String, api_passphrase: String) -> Result<Self, ExecutorError> {
        // Remove 0x prefix if present
        let key_str = private_key_hex.strip_prefix("0x").unwrap_or(private_key_hex);

        let key_bytes = hex::decode(key_str)
            .map_err(|e| ExecutorError::Internal(format!("Invalid private key hex: {}", e)))?;

        if key_bytes.len() != 32 {
            return Err(ExecutorError::Internal(format!(
                "Private key must be 32 bytes, got {}",
                key_bytes.len()
            )));
        }

        let mut private_key = [0u8; 32];
        private_key.copy_from_slice(&key_bytes);

        // Derive address from private key
        let address = Self::derive_address(&private_key)?;

        Ok(Self {
            address,
            private_key,
            api_key,
            api_secret,
            api_passphrase,
        })
    }

    /// Derive Ethereum address from private key.
    fn derive_address(private_key: &[u8; 32]) -> Result<Address, ExecutorError> {
        use ethers_core::k256::ecdsa::SigningKey;
        use ethers_core::k256::elliptic_curve::sec1::ToEncodedPoint;
        use ethers_core::k256::PublicKey;

        let signing_key = SigningKey::from_bytes(private_key.into())
            .map_err(|e| ExecutorError::Internal(format!("Invalid private key: {}", e)))?;

        // Convert verifying key to public key to use ToEncodedPoint
        let verifying_key = signing_key.verifying_key();
        let public_key = PublicKey::from(verifying_key);
        let public_key_point = public_key.to_encoded_point(false);
        let public_key_bytes = public_key_point.as_bytes();

        // Skip the first byte (0x04 prefix for uncompressed public key)
        let mut hasher = Keccak256::new();
        hasher.update(&public_key_bytes[1..]);
        let hash = hasher.finalize();

        // Address is the last 20 bytes of the keccak256 hash
        let address_bytes: [u8; 20] = hash[12..32].try_into()
            .map_err(|_| ExecutorError::Internal("Failed to derive address".to_string()))?;

        Ok(Address::from(address_bytes))
    }

    /// Get the wallet address.
    pub fn address(&self) -> Address {
        self.address
    }

    /// Sign a message hash with the private key.
    pub fn sign_hash(&self, hash: H256) -> Result<Signature, ExecutorError> {
        use ethers_core::k256::ecdsa::{SigningKey, signature::hazmat::PrehashSigner};

        let signing_key = SigningKey::from_bytes((&self.private_key).into())
            .map_err(|e| ExecutorError::Internal(format!("Invalid signing key: {}", e)))?;

        let (sig, recovery_id): (ethers_core::k256::ecdsa::Signature, _) = signing_key
            .sign_prehash(hash.as_bytes())
            .map_err(|e| ExecutorError::Internal(format!("Signing failed: {}", e)))?;

        let r = U256::from_big_endian(&sig.r().to_bytes());
        let s = U256::from_big_endian(&sig.s().to_bytes());
        let v = recovery_id.to_byte() as u64 + 27;

        Ok(Signature { r, s, v })
    }
}

/// Internal order state for tracking.
#[derive(Debug, Clone)]
struct TrackedOrder {
    /// Original request.
    request: OrderRequest,
    /// Assigned order ID from exchange.
    order_id: String,
    /// Current status.
    status: TrackedOrderStatus,
    /// Filled size so far.
    filled_size: Decimal,
    /// Average fill price.
    avg_fill_price: Decimal,
    /// Total fees paid.
    total_fee: Decimal,
    /// Creation time.
    created_at: DateTime<Utc>,
    /// Last update time.
    updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TrackedOrderStatus {
    Pending,
    PartiallyFilled,
    Filled,
    Cancelled,
}

/// API response types for Polymarket CLOB.
#[allow(dead_code)]
mod api {
    use super::*;

    /// Order creation request body.
    #[derive(Debug, Serialize)]
    pub struct CreateOrderRequest {
        pub order: SignedOrder,
    }

    /// Signed order for submission.
    #[derive(Debug, Serialize)]
    pub struct SignedOrder {
        pub salt: String,
        pub maker: String,
        pub signer: String,
        pub taker: String,
        pub token_id: String,
        pub maker_amount: String,
        pub taker_amount: String,
        pub expiration: String,
        pub nonce: String,
        pub fee_rate_bps: String,
        pub side: String,
        pub signature_type: u8,
        pub signature: String,
    }

    /// Order creation response.
    #[derive(Debug, Deserialize)]
    pub struct CreateOrderResponse {
        pub success: bool,
        #[serde(rename = "orderID")]
        pub order_id: Option<String>,
        pub error_msg: Option<String>,
        pub status: Option<String>,
    }

    /// Order status response.
    #[derive(Debug, Deserialize)]
    pub struct OrderStatusResponse {
        pub id: String,
        pub status: String,
        pub size_matched: Option<String>,
        pub price: Option<String>,
        pub associate_trades: Option<Vec<TradeInfo>>,
    }

    /// Trade information from order status.
    #[derive(Debug, Deserialize)]
    pub struct TradeInfo {
        pub id: String,
        pub price: String,
        pub size: String,
        pub fee: Option<String>,
        pub timestamp: Option<String>,
    }

    /// Cancel order request.
    #[derive(Debug, Serialize)]
    pub struct CancelOrderRequest {
        #[serde(rename = "orderID")]
        pub order_id: String,
    }

    /// Cancel order response.
    #[derive(Debug, Deserialize)]
    pub struct CancelOrderResponse {
        pub success: bool,
        pub error_msg: Option<String>,
        pub canceled: Option<String>,
    }

    /// API error response.
    #[derive(Debug, Deserialize)]
    pub struct ApiError {
        pub error: Option<String>,
        pub message: Option<String>,
    }

    /// Balance and allowance request parameters.
    #[derive(Debug, Serialize)]
    pub struct BalanceAllowanceRequest {
        pub asset_type: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub token_id: Option<String>,
    }

    /// Balance and allowance response from Polymarket API.
    #[derive(Debug, Deserialize)]
    pub struct BalanceAllowanceResponse {
        pub balance: String,
        pub allowance: String,
    }
}

/// Live executor for real order submission.
///
/// Submits orders to Polymarket's CLOB exchange with EIP-712 signatures.
/// Tracks pending orders and fires shadow bids on fills.
pub struct LiveExecutor {
    /// Configuration.
    config: LiveExecutorConfig,
    /// HTTP client for API calls.
    client: Client,
    /// Wallet for signing.
    wallet: Wallet,
    /// Tracked orders by order ID.
    orders: Arc<RwLock<HashMap<String, TrackedOrder>>>,
    /// Request ID to order ID mapping.
    request_to_order: Arc<RwLock<HashMap<String, String>>>,
    /// Shadow bid manager.
    shadow_manager: Option<SharedShadowManager>,
    /// Available balance (tracked locally).
    balance: Arc<RwLock<Decimal>>,
    /// Next nonce for orders.
    next_nonce: std::sync::atomic::AtomicU64,
    /// Domain separator for EIP-712 signing.
    domain_separator: H256,
    /// Order type hash for EIP-712 signing.
    order_type_hash: H256,
}

impl LiveExecutor {
    /// Create a new live executor.
    ///
    /// # Arguments
    ///
    /// * `config` - Executor configuration
    /// * `wallet_config` - Wallet credentials (must have private_key, api_key, api_secret, api_passphrase)
    /// * `shadow_config` - Optional shadow bid configuration
    /// * `initial_balance` - Starting USDC balance
    ///
    /// # Errors
    ///
    /// Returns an error if wallet credentials are missing or invalid.
    pub fn new(
        config: LiveExecutorConfig,
        wallet_config: &WalletConfig,
        shadow_config: Option<&ShadowConfig>,
        initial_balance: Decimal,
    ) -> Result<Self, ExecutorError> {
        // Validate wallet credentials
        let private_key = wallet_config.private_key.as_ref()
            .ok_or_else(|| ExecutorError::Internal("POLY_PRIVATE_KEY not set".to_string()))?;
        let api_key = wallet_config.api_key.as_ref()
            .ok_or_else(|| ExecutorError::Internal("POLY_API_KEY not set".to_string()))?;
        let api_secret = wallet_config.api_secret.as_ref()
            .ok_or_else(|| ExecutorError::Internal("POLY_API_SECRET not set".to_string()))?;
        let api_passphrase = wallet_config.api_passphrase.as_ref()
            .ok_or_else(|| ExecutorError::Internal("POLY_API_PASSPHRASE not set".to_string()))?;

        let wallet = Wallet::from_private_key(
            private_key,
            api_key.clone(),
            api_secret.clone(),
            api_passphrase.clone(),
        )?;

        info!(
            address = %wallet.address(),
            "Initialized LiveExecutor"
        );

        // Create shadow manager if enabled
        let shadow_manager = if config.enable_shadow {
            shadow_config.map(|cfg| {
                Arc::new(ShadowManager::new(cfg.clone(), format!("{:?}", wallet.address())))
            })
        } else {
            None
        };

        // Compute domain separator and type hash
        let domain_separator = Self::compute_domain_separator();
        let order_type_hash = Self::compute_order_type_hash();

        // Build HTTP client with timeout
        let client = Client::builder()
            .timeout(Duration::from_millis(config.order_timeout_ms))
            .build()
            .map_err(|e| ExecutorError::Internal(format!("Failed to build HTTP client: {}", e)))?;

        Ok(Self {
            config,
            client,
            wallet,
            orders: Arc::new(RwLock::new(HashMap::new())),
            request_to_order: Arc::new(RwLock::new(HashMap::new())),
            shadow_manager,
            balance: Arc::new(RwLock::new(initial_balance)),
            next_nonce: std::sync::atomic::AtomicU64::new(1),
            domain_separator,
            order_type_hash,
        })
    }

    /// Compute EIP-712 domain separator for Polymarket CTF Exchange.
    fn compute_domain_separator() -> H256 {
        // Domain separator = keccak256(abi.encode(
        //   keccak256("EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)"),
        //   keccak256("Polymarket CTF Exchange"),
        //   keccak256("1"),
        //   137, // Polygon chainId
        //   CTF_EXCHANGE_ADDRESS
        // ))

        let mut hasher = Keccak256::new();
        hasher.update(b"EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)");
        let domain_type_hash = hasher.finalize();

        let mut hasher = Keccak256::new();
        hasher.update(b"Polymarket CTF Exchange");
        let name_hash = hasher.finalize();

        let mut hasher = Keccak256::new();
        hasher.update(b"1");
        let version_hash = hasher.finalize();

        let chain_id = endpoints::CHAIN_ID;
        let exchange_address = endpoints::CTF_EXCHANGE
            .parse::<Address>()
            .unwrap_or_default();

        // ABI encode and hash
        let mut encoded = Vec::with_capacity(160);
        encoded.extend_from_slice(&domain_type_hash);
        encoded.extend_from_slice(&name_hash);
        encoded.extend_from_slice(&version_hash);
        encoded.extend_from_slice(&[0u8; 24]); // Pad chain_id to 32 bytes
        encoded.extend_from_slice(&chain_id.to_be_bytes());
        encoded.extend_from_slice(&[0u8; 12]); // Pad address to 32 bytes
        encoded.extend_from_slice(exchange_address.as_bytes());

        let mut hasher = Keccak256::new();
        hasher.update(&encoded);
        H256::from_slice(&hasher.finalize())
    }

    /// Compute order type hash for EIP-712 signing.
    fn compute_order_type_hash() -> H256 {
        // Order(uint256 salt,address maker,address signer,address taker,uint256 tokenId,
        //       uint256 makerAmount,uint256 takerAmount,uint256 expiration,uint256 nonce,
        //       uint256 feeRateBps,uint8 side,uint8 signatureType)
        let mut hasher = Keccak256::new();
        hasher.update(b"Order(uint256 salt,address maker,address signer,address taker,uint256 tokenId,uint256 makerAmount,uint256 takerAmount,uint256 expiration,uint256 nonce,uint256 feeRateBps,uint8 side,uint8 signatureType)");
        H256::from_slice(&hasher.finalize())
    }

    /// Create a signed order for submission.
    fn create_signed_order(&self, request: &OrderRequest) -> Result<api::SignedOrder, ExecutorError> {
        let price = request.price.ok_or_else(|| {
            ExecutorError::InvalidOrder("Limit price required for live orders".to_string())
        })?;

        // Generate salt (random nonce)
        let salt = self.next_nonce.fetch_add(1, std::sync::atomic::Ordering::SeqCst);

        // Calculate amounts in base units (USDC has 6 decimals, CTF tokens have 6 decimals)
        // For a buy order: makerAmount = USDC cost, takerAmount = token amount
        // For a sell order: makerAmount = token amount, takerAmount = USDC proceeds
        let (maker_amount, taker_amount) = match request.side {
            Side::Buy => {
                let usdc_amount = (price * request.size * Decimal::new(1_000_000, 0))
                    .round()
                    .to_string()
                    .parse::<u128>()
                    .unwrap_or(0);
                let token_amount = (request.size * Decimal::new(1_000_000, 0))
                    .round()
                    .to_string()
                    .parse::<u128>()
                    .unwrap_or(0);
                (usdc_amount, token_amount)
            }
            Side::Sell => {
                let token_amount = (request.size * Decimal::new(1_000_000, 0))
                    .round()
                    .to_string()
                    .parse::<u128>()
                    .unwrap_or(0);
                let usdc_amount = (price * request.size * Decimal::new(1_000_000, 0))
                    .round()
                    .to_string()
                    .parse::<u128>()
                    .unwrap_or(0);
                (token_amount, usdc_amount)
            }
        };

        // Expiration: order timeout from now
        let expiration = (Utc::now() + chrono::Duration::milliseconds(self.config.order_timeout_ms as i64))
            .timestamp() as u64;

        // Nonce for replay protection
        let nonce = salt;

        // Side: 0 = buy, 1 = sell
        let side = match request.side {
            Side::Buy => 0u8,
            Side::Sell => 1u8,
        };

        // Build struct hash
        let struct_hash = self.compute_struct_hash(
            salt,
            &self.wallet.address(),
            &request.token_id,
            maker_amount,
            taker_amount,
            expiration,
            nonce,
            self.config.fee_rate_bps as u64,
            side,
        );

        // Compute final signing hash: keccak256(0x1901 || domainSeparator || structHash)
        let mut hasher = Keccak256::new();
        hasher.update([0x19, 0x01]);
        hasher.update(self.domain_separator.as_bytes());
        hasher.update(struct_hash.as_bytes());
        let signing_hash = H256::from_slice(&hasher.finalize());

        // Sign the hash
        let signature = self.wallet.sign_hash(signing_hash)?;

        // Format signature as hex
        let sig_bytes = signature_to_bytes(&signature);
        let signature_hex = format!("0x{}", hex::encode(sig_bytes));

        Ok(api::SignedOrder {
            salt: salt.to_string(),
            maker: format!("{:?}", self.wallet.address()),
            signer: format!("{:?}", self.wallet.address()),
            taker: "0x0000000000000000000000000000000000000000".to_string(),
            token_id: request.token_id.clone(),
            maker_amount: maker_amount.to_string(),
            taker_amount: taker_amount.to_string(),
            expiration: expiration.to_string(),
            nonce: nonce.to_string(),
            fee_rate_bps: self.config.fee_rate_bps.to_string(),
            side: side.to_string(),
            signature_type: 0, // EOA signature
            signature: signature_hex,
        })
    }

    /// Compute struct hash for order.
    #[allow(clippy::too_many_arguments)]
    fn compute_struct_hash(
        &self,
        salt: u64,
        maker: &Address,
        token_id: &str,
        maker_amount: u128,
        taker_amount: u128,
        expiration: u64,
        nonce: u64,
        fee_rate_bps: u64,
        side: u8,
    ) -> H256 {
        // Parse token_id as U256
        let token_id_u256 = U256::from_dec_str(token_id).unwrap_or_default();

        // ABI encode the struct
        let mut encoded = Vec::with_capacity(384);
        encoded.extend_from_slice(self.order_type_hash.as_bytes());

        // salt (32 bytes)
        let mut salt_bytes = [0u8; 32];
        salt_bytes[24..].copy_from_slice(&salt.to_be_bytes());
        encoded.extend_from_slice(&salt_bytes);

        // maker (20 bytes padded to 32)
        let mut maker_bytes = [0u8; 32];
        maker_bytes[12..].copy_from_slice(maker.as_bytes());
        encoded.extend_from_slice(&maker_bytes);

        // signer (same as maker)
        encoded.extend_from_slice(&maker_bytes);

        // taker (zero address)
        encoded.extend_from_slice(&[0u8; 32]);

        // tokenId (32 bytes)
        let mut token_bytes = [0u8; 32];
        token_id_u256.to_big_endian(&mut token_bytes);
        encoded.extend_from_slice(&token_bytes);

        // makerAmount (32 bytes)
        let mut amount_bytes = [0u8; 32];
        amount_bytes[16..].copy_from_slice(&maker_amount.to_be_bytes());
        encoded.extend_from_slice(&amount_bytes);

        // takerAmount (32 bytes)
        let mut amount_bytes = [0u8; 32];
        amount_bytes[16..].copy_from_slice(&taker_amount.to_be_bytes());
        encoded.extend_from_slice(&amount_bytes);

        // expiration (32 bytes)
        let mut exp_bytes = [0u8; 32];
        exp_bytes[24..].copy_from_slice(&expiration.to_be_bytes());
        encoded.extend_from_slice(&exp_bytes);

        // nonce (32 bytes)
        let mut nonce_bytes = [0u8; 32];
        nonce_bytes[24..].copy_from_slice(&nonce.to_be_bytes());
        encoded.extend_from_slice(&nonce_bytes);

        // feeRateBps (32 bytes)
        let mut fee_bytes = [0u8; 32];
        fee_bytes[24..].copy_from_slice(&fee_rate_bps.to_be_bytes());
        encoded.extend_from_slice(&fee_bytes);

        // side (32 bytes - uint8 padded)
        let mut side_bytes = [0u8; 32];
        side_bytes[31] = side;
        encoded.extend_from_slice(&side_bytes);

        // signatureType (32 bytes - uint8 padded)
        let mut sig_type_bytes = [0u8; 32];
        sig_type_bytes[31] = 0; // EOA
        encoded.extend_from_slice(&sig_type_bytes);

        let mut hasher = Keccak256::new();
        hasher.update(&encoded);
        H256::from_slice(&hasher.finalize())
    }

    /// Submit order to CLOB API.
    async fn submit_order(&self, signed_order: api::SignedOrder) -> Result<String, ExecutorError> {
        let url = format!("{}/order", self.config.api_endpoint);

        let request_body = api::CreateOrderRequest {
            order: signed_order,
        };

        let response = self.client
            .post(&url)
            .header("POLY-ADDRESS", format!("{:?}", self.wallet.address()))
            .header("POLY-SIGNATURE", &self.wallet.api_secret)
            .header("POLY-TIMESTAMP", Utc::now().timestamp().to_string())
            .header("POLY-API-KEY", &self.wallet.api_key)
            .header("POLY-PASSPHRASE", &self.wallet.api_passphrase)
            .json(&request_body)
            .send()
            .await
            .map_err(|e| ExecutorError::Connection(format!("Failed to submit order: {}", e)))?;

        let status = response.status();
        let body = response.text().await
            .map_err(|e| ExecutorError::Connection(format!("Failed to read response: {}", e)))?;

        if !status.is_success() {
            // Try to parse error
            if let Ok(error) = serde_json::from_str::<api::ApiError>(&body) {
                let msg = error.error.or(error.message).unwrap_or_else(|| body.clone());
                return Err(ExecutorError::Rejected(msg));
            }
            return Err(ExecutorError::Rejected(format!("HTTP {}: {}", status, body)));
        }

        let response: api::CreateOrderResponse = serde_json::from_str(&body)
            .map_err(|e| ExecutorError::Internal(format!("Failed to parse response: {} - {}", e, body)))?;

        if !response.success {
            let msg = response.error_msg.unwrap_or_else(|| "Unknown error".to_string());
            return Err(ExecutorError::Rejected(msg));
        }

        response.order_id.ok_or_else(|| {
            ExecutorError::Internal("No order ID in response".to_string())
        })
    }

    /// Poll order status from API.
    async fn poll_order_status(&self, order_id: &str) -> Result<api::OrderStatusResponse, ExecutorError> {
        let url = format!("{}/order/{}", self.config.api_endpoint, order_id);

        let response = self.client
            .get(&url)
            .header("POLY-ADDRESS", format!("{:?}", self.wallet.address()))
            .header("POLY-API-KEY", &self.wallet.api_key)
            .send()
            .await
            .map_err(|e| ExecutorError::Connection(format!("Failed to poll status: {}", e)))?;

        let status = response.status();
        let body = response.text().await
            .map_err(|e| ExecutorError::Connection(format!("Failed to read response: {}", e)))?;

        if !status.is_success() {
            return Err(ExecutorError::Connection(format!("HTTP {}: {}", status, body)));
        }

        serde_json::from_str(&body)
            .map_err(|e| ExecutorError::Internal(format!("Failed to parse status: {} - {}", e, body)))
    }

    /// Cancel order via API.
    async fn cancel_order_api(&self, order_id: &str) -> Result<api::CancelOrderResponse, ExecutorError> {
        let url = format!("{}/order", self.config.api_endpoint);

        let request_body = api::CancelOrderRequest {
            order_id: order_id.to_string(),
        };

        let response = self.client
            .delete(&url)
            .header("POLY-ADDRESS", format!("{:?}", self.wallet.address()))
            .header("POLY-SIGNATURE", &self.wallet.api_secret)
            .header("POLY-TIMESTAMP", Utc::now().timestamp().to_string())
            .header("POLY-API-KEY", &self.wallet.api_key)
            .header("POLY-PASSPHRASE", &self.wallet.api_passphrase)
            .json(&request_body)
            .send()
            .await
            .map_err(|e| ExecutorError::Connection(format!("Failed to cancel order: {}", e)))?;

        let status = response.status();
        let body = response.text().await
            .map_err(|e| ExecutorError::Connection(format!("Failed to read response: {}", e)))?;

        if !status.is_success() {
            return Err(ExecutorError::Connection(format!("HTTP {}: {}", status, body)));
        }

        serde_json::from_str(&body)
            .map_err(|e| ExecutorError::Internal(format!("Failed to parse cancel response: {} - {}", e, body)))
    }

    /// Fetch USDC balance from Polymarket API.
    ///
    /// Returns the collateral (USDC) balance for the authenticated wallet.
    pub async fn fetch_balance(&self) -> Result<Decimal, ExecutorError> {
        let url = format!("{}/balance-allowance", self.config.api_endpoint);

        let request_body = api::BalanceAllowanceRequest {
            asset_type: "COLLATERAL".to_string(),
            token_id: None,
        };

        let response = self.client
            .get(&url)
            .header("POLY-ADDRESS", format!("{:?}", self.wallet.address()))
            .header("POLY-SIGNATURE", &self.wallet.api_secret)
            .header("POLY-TIMESTAMP", Utc::now().timestamp().to_string())
            .header("POLY-API-KEY", &self.wallet.api_key)
            .header("POLY-PASSPHRASE", &self.wallet.api_passphrase)
            .query(&request_body)
            .send()
            .await
            .map_err(|e| ExecutorError::Connection(format!("Failed to fetch balance: {}", e)))?;

        let status = response.status();
        let body = response.text().await
            .map_err(|e| ExecutorError::Connection(format!("Failed to read balance response: {}", e)))?;

        if !status.is_success() {
            return Err(ExecutorError::Connection(format!("HTTP {}: {}", status, body)));
        }

        let balance_response: api::BalanceAllowanceResponse = serde_json::from_str(&body)
            .map_err(|e| ExecutorError::Internal(format!("Failed to parse balance response: {} - {}", e, body)))?;

        // Parse balance string to Decimal
        let balance: Decimal = balance_response.balance.parse()
            .map_err(|e| ExecutorError::Internal(format!("Failed to parse balance value '{}': {}", balance_response.balance, e)))?;

        // Balance is in wei (6 decimals for USDC), convert to USDC
        let usdc_balance = balance / Decimal::new(1_000_000, 0);

        info!(balance_wei = %balance_response.balance, balance_usdc = %usdc_balance, "Fetched Polymarket balance");

        Ok(usdc_balance)
    }

    /// Fire shadow bid for a filled primary order.
    async fn fire_shadow_on_fill(&self, event_id: &str, outcome: Outcome) {
        if let Some(ref manager) = self.shadow_manager {
            if !manager.is_enabled() {
                return;
            }

            let start = Instant::now();

            match manager.fire(event_id, outcome) {
                Ok(result) => {
                    let latency_us = start.elapsed().as_micros();
                    info!(
                        event_id = %event_id,
                        outcome = ?outcome,
                        latency_us = latency_us,
                        order_id = ?result.order_id,
                        "Shadow bid fired successfully"
                    );

                    // Check if we hit <2ms target
                    if latency_us > 2000 {
                        warn!(
                            latency_us = latency_us,
                            target_us = 2000,
                            "Shadow bid exceeded latency target"
                        );
                    }
                }
                Err(e) => {
                    warn!(
                        event_id = %event_id,
                        outcome = ?outcome,
                        error = %e,
                        "Failed to fire shadow bid"
                    );
                }
            }
        }
    }

    /// Update local balance after a fill.
    async fn update_balance_on_fill(&self, side: Side, size: Decimal, price: Decimal, fee: Decimal) {
        let mut balance = self.balance.write().await;

        match side {
            Side::Buy => {
                // Spent USDC to buy tokens
                let cost = size * price + fee;
                *balance -= cost;
            }
            Side::Sell => {
                // Received USDC from selling tokens
                let proceeds = size * price - fee;
                *balance += proceeds;
            }
        }

        debug!(
            side = ?side,
            size = %size,
            price = %price,
            fee = %fee,
            new_balance = %*balance,
            "Updated balance on fill"
        );
    }

    /// Get the shadow manager for external access.
    pub fn shadow_manager(&self) -> Option<&SharedShadowManager> {
        self.shadow_manager.as_ref()
    }

    /// Create a shadow order for a pending primary order.
    pub fn create_shadow(
        &self,
        event_id: &str,
        token_id: &str,
        outcome: Outcome,
        primary_price: Decimal,
        size: Decimal,
        primary_order_id: Option<String>,
    ) -> Result<ShadowOrder, super::shadow::ShadowError> {
        match &self.shadow_manager {
            Some(manager) => manager.create_shadow(
                event_id.to_string(),
                token_id.to_string(),
                outcome,
                primary_price,
                size,
                primary_order_id,
            ),
            None => Err(super::shadow::ShadowError::Invalid("Shadow manager not configured".to_string())),
        }
    }
}

/// Convert Signature to 65-byte array.
fn signature_to_bytes(sig: &Signature) -> [u8; 65] {
    let mut bytes = [0u8; 65];
    sig.r.to_big_endian(&mut bytes[0..32]);
    sig.s.to_big_endian(&mut bytes[32..64]);
    bytes[64] = sig.v as u8;
    bytes
}

#[async_trait]
impl Executor for LiveExecutor {
    async fn place_order(&mut self, request: OrderRequest) -> Result<OrderResult, ExecutorError> {
        debug!(
            request_id = %request.request_id,
            event_id = %request.event_id,
            token_id = %request.token_id,
            side = ?request.side,
            size = %request.size,
            price = ?request.price,
            "Placing live order"
        );

        // Validate order type
        if request.order_type == OrderType::Market {
            return Err(ExecutorError::InvalidOrder(
                "Market orders not supported in live mode - use limit orders".to_string()
            ));
        }

        // Check balance for buy orders
        if request.side == Side::Buy {
            let balance = self.balance.read().await;
            let cost = request.max_cost();
            if *balance < cost {
                return Err(ExecutorError::InsufficientFunds {
                    available: *balance,
                    required: cost,
                });
            }
        }

        // Create signed order
        let signed_order = self.create_signed_order(&request)?;

        // Submit to exchange
        let order_id = self.submit_order(signed_order).await?;

        info!(
            request_id = %request.request_id,
            order_id = %order_id,
            "Order submitted"
        );

        // Track the order
        let tracked = TrackedOrder {
            request: request.clone(),
            order_id: order_id.clone(),
            status: TrackedOrderStatus::Pending,
            filled_size: Decimal::ZERO,
            avg_fill_price: Decimal::ZERO,
            total_fee: Decimal::ZERO,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };

        {
            let mut orders = self.orders.write().await;
            orders.insert(order_id.clone(), tracked);
        }
        {
            let mut mapping = self.request_to_order.write().await;
            mapping.insert(request.request_id.clone(), order_id.clone());
        }

        // For IOC orders, poll immediately for fill status
        if request.order_type == OrderType::Ioc {
            // Short delay to allow exchange to process
            tokio::time::sleep(Duration::from_millis(50)).await;

            match self.poll_order_status(&order_id).await {
                Ok(status) => {
                    return self.process_order_status(&order_id, &request, status).await;
                }
                Err(e) => {
                    warn!(order_id = %order_id, error = %e, "Failed to poll IOC order status");
                }
            }
        }

        // Return pending for other order types
        Ok(OrderResult::Pending(PendingOrder {
            request_id: request.request_id,
            order_id,
            timestamp: Utc::now(),
        }))
    }

    async fn cancel_order(&mut self, order_id: &str) -> Result<OrderCancellation, ExecutorError> {
        debug!(order_id = %order_id, "Cancelling order");

        // Get tracked order info
        let tracked = {
            let orders = self.orders.read().await;
            orders.get(order_id).cloned()
        };

        let tracked = tracked.ok_or_else(|| {
            ExecutorError::InvalidOrder(format!("Order {} not found", order_id))
        })?;

        // Cancel via API
        let response = self.cancel_order_api(order_id).await?;

        if !response.success {
            return Err(ExecutorError::Rejected(
                response.error_msg.unwrap_or_else(|| "Cancel failed".to_string())
            ));
        }

        // Update tracked state
        {
            let mut orders = self.orders.write().await;
            if let Some(order) = orders.get_mut(order_id) {
                order.status = TrackedOrderStatus::Cancelled;
                order.updated_at = Utc::now();
            }
        }

        info!(
            order_id = %order_id,
            filled = %tracked.filled_size,
            "Order cancelled"
        );

        Ok(OrderCancellation {
            request_id: tracked.request.request_id,
            order_id: order_id.to_string(),
            filled_size: tracked.filled_size,
            unfilled_size: tracked.request.size - tracked.filled_size,
            timestamp: Utc::now(),
        })
    }

    async fn order_status(&self, order_id: &str) -> Option<OrderResult> {
        // Try local cache first
        let tracked = {
            let orders = self.orders.read().await;
            orders.get(order_id).cloned()
        };

        let tracked = tracked?;

        // For terminal states, return cached result
        match tracked.status {
            TrackedOrderStatus::Filled => {
                return Some(OrderResult::Filled(OrderFill {
                    request_id: tracked.request.request_id,
                    order_id: tracked.order_id,
                    size: tracked.filled_size,
                    price: tracked.avg_fill_price,
                    fee: tracked.total_fee,
                    timestamp: tracked.updated_at,
                }));
            }
            TrackedOrderStatus::Cancelled => {
                return Some(OrderResult::Cancelled(OrderCancellation {
                    request_id: tracked.request.request_id,
                    order_id: tracked.order_id,
                    filled_size: tracked.filled_size,
                    unfilled_size: tracked.request.size - tracked.filled_size,
                    timestamp: tracked.updated_at,
                }));
            }
            _ => {}
        }

        // Poll exchange for current status
        match self.poll_order_status(order_id).await {
            Ok(status) => {
                self.update_tracked_order(order_id, &status).await;

                match status.status.as_str() {
                    "matched" | "filled" => {
                        let filled_size = status.size_matched
                            .and_then(|s| s.parse::<Decimal>().ok())
                            .unwrap_or(tracked.request.size);
                        let price = status.price
                            .and_then(|p| p.parse::<Decimal>().ok())
                            .unwrap_or(tracked.avg_fill_price);

                        Some(OrderResult::Filled(OrderFill {
                            request_id: tracked.request.request_id,
                            order_id: order_id.to_string(),
                            size: filled_size,
                            price,
                            fee: Decimal::ZERO, // Maker fees are 0
                            timestamp: Utc::now(),
                        }))
                    }
                    "live" | "pending" => {
                        let filled = status.size_matched
                            .and_then(|s| s.parse::<Decimal>().ok())
                            .unwrap_or(Decimal::ZERO);

                        if filled > Decimal::ZERO {
                            Some(OrderResult::PartialFill(PartialOrderFill {
                                request_id: tracked.request.request_id,
                                order_id: order_id.to_string(),
                                requested_size: tracked.request.size,
                                filled_size: filled,
                                avg_price: status.price
                                    .and_then(|p| p.parse().ok())
                                    .unwrap_or(Decimal::ZERO),
                                fee: Decimal::ZERO,
                                timestamp: Utc::now(),
                            }))
                        } else {
                            Some(OrderResult::Pending(PendingOrder {
                                request_id: tracked.request.request_id,
                                order_id: order_id.to_string(),
                                timestamp: Utc::now(),
                            }))
                        }
                    }
                    "cancelled" => {
                        let filled = status.size_matched
                            .and_then(|s| s.parse::<Decimal>().ok())
                            .unwrap_or(Decimal::ZERO);

                        Some(OrderResult::Cancelled(OrderCancellation {
                            request_id: tracked.request.request_id,
                            order_id: order_id.to_string(),
                            filled_size: filled,
                            unfilled_size: tracked.request.size - filled,
                            timestamp: Utc::now(),
                        }))
                    }
                    _ => Some(OrderResult::Pending(PendingOrder {
                        request_id: tracked.request.request_id,
                        order_id: order_id.to_string(),
                        timestamp: Utc::now(),
                    })),
                }
            }
            Err(e) => {
                warn!(order_id = %order_id, error = %e, "Failed to poll order status");
                None
            }
        }
    }

    fn pending_orders(&self) -> Vec<PendingOrder> {
        // Use try_read to avoid blocking
        let orders = match self.orders.try_read() {
            Ok(guard) => guard,
            Err(_) => return Vec::new(),
        };

        orders
            .values()
            .filter(|o| matches!(o.status, TrackedOrderStatus::Pending | TrackedOrderStatus::PartiallyFilled))
            .map(|o| PendingOrder {
                request_id: o.request.request_id.clone(),
                order_id: o.order_id.clone(),
                timestamp: o.created_at,
            })
            .collect()
    }

    fn available_balance(&self) -> Decimal {
        // Use try_read to avoid blocking
        match self.balance.try_read() {
            Ok(guard) => *guard,
            Err(_) => Decimal::ZERO,
        }
    }

    async fn shutdown(&mut self) {
        info!("Shutting down LiveExecutor");

        // Cancel all pending orders
        let pending_ids: Vec<String> = {
            let orders = self.orders.read().await;
            orders
                .iter()
                .filter(|(_, o)| matches!(o.status, TrackedOrderStatus::Pending | TrackedOrderStatus::PartiallyFilled))
                .map(|(id, _)| id.clone())
                .collect()
        };

        for order_id in pending_ids {
            match self.cancel_order(&order_id).await {
                Ok(_) => info!(order_id = %order_id, "Cancelled pending order on shutdown"),
                Err(e) => warn!(order_id = %order_id, error = %e, "Failed to cancel order on shutdown"),
            }
        }

        // Clean up shadow manager
        if let Some(ref manager) = self.shadow_manager {
            let cleaned = manager.cleanup_expired();
            if cleaned > 0 {
                info!(count = cleaned, "Cleaned up expired shadows on shutdown");
            }
        }
    }
}

impl LiveExecutor {
    /// Process order status response and return appropriate OrderResult.
    async fn process_order_status(
        &self,
        order_id: &str,
        request: &OrderRequest,
        status: api::OrderStatusResponse,
    ) -> Result<OrderResult, ExecutorError> {
        let filled_size = status.size_matched.as_ref()
            .and_then(|s| s.parse::<Decimal>().ok())
            .unwrap_or(Decimal::ZERO);
        let fill_price = status.price.as_ref()
            .and_then(|p| p.parse::<Decimal>().ok())
            .unwrap_or_else(|| request.price.unwrap_or(Decimal::ZERO));

        // Update balance if filled
        if filled_size > Decimal::ZERO {
            self.update_balance_on_fill(request.side, filled_size, fill_price, Decimal::ZERO).await;

            // Fire shadow if this was a primary fill
            self.fire_shadow_on_fill(&request.event_id, request.outcome).await;
        }

        match status.status.as_str() {
            "matched" | "filled" => {
                self.update_tracked_order(order_id, &status).await;

                Ok(OrderResult::Filled(OrderFill {
                    request_id: request.request_id.clone(),
                    order_id: order_id.to_string(),
                    size: filled_size,
                    price: fill_price,
                    fee: Decimal::ZERO,
                    timestamp: Utc::now(),
                }))
            }
            "live" | "pending" if filled_size > Decimal::ZERO => {
                self.update_tracked_order(order_id, &status).await;

                Ok(OrderResult::PartialFill(PartialOrderFill {
                    request_id: request.request_id.clone(),
                    order_id: order_id.to_string(),
                    requested_size: request.size,
                    filled_size,
                    avg_price: fill_price,
                    fee: Decimal::ZERO,
                    timestamp: Utc::now(),
                }))
            }
            "cancelled" => {
                self.update_tracked_order(order_id, &status).await;

                if filled_size > Decimal::ZERO {
                    Ok(OrderResult::PartialFill(PartialOrderFill {
                        request_id: request.request_id.clone(),
                        order_id: order_id.to_string(),
                        requested_size: request.size,
                        filled_size,
                        avg_price: fill_price,
                        fee: Decimal::ZERO,
                        timestamp: Utc::now(),
                    }))
                } else {
                    Ok(OrderResult::Cancelled(OrderCancellation {
                        request_id: request.request_id.clone(),
                        order_id: order_id.to_string(),
                        filled_size: Decimal::ZERO,
                        unfilled_size: request.size,
                        timestamp: Utc::now(),
                    }))
                }
            }
            _ => {
                Ok(OrderResult::Pending(PendingOrder {
                    request_id: request.request_id.clone(),
                    order_id: order_id.to_string(),
                    timestamp: Utc::now(),
                }))
            }
        }
    }

    /// Update tracked order from API status response.
    async fn update_tracked_order(&self, order_id: &str, status: &api::OrderStatusResponse) {
        let mut orders = self.orders.write().await;

        if let Some(order) = orders.get_mut(order_id) {
            // Update filled size
            if let Some(ref size_str) = status.size_matched
                && let Ok(size) = size_str.parse::<Decimal>()
            {
                order.filled_size = size;
            }

            // Update price
            if let Some(ref price_str) = status.price
                && let Ok(price) = price_str.parse::<Decimal>()
            {
                order.avg_fill_price = price;
            }

            // Update status
            order.status = match status.status.as_str() {
                "matched" | "filled" => TrackedOrderStatus::Filled,
                "cancelled" => TrackedOrderStatus::Cancelled,
                "live" | "pending" if order.filled_size > Decimal::ZERO => TrackedOrderStatus::PartiallyFilled,
                _ => TrackedOrderStatus::Pending,
            };

            order.updated_at = Utc::now();
        }
    }
}

impl std::fmt::Debug for LiveExecutor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiveExecutor")
            .field("api_endpoint", &self.config.api_endpoint)
            .field("wallet", &self.wallet)
            .field("shadow_enabled", &self.shadow_manager.is_some())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn test_live_executor_config_default() {
        let config = LiveExecutorConfig::default();
        assert_eq!(config.api_endpoint, endpoints::CLOB_REST);
        assert_eq!(config.order_timeout_ms, 30_000);
        assert!(config.enable_shadow);
        assert_eq!(config.fee_rate_bps, 0);
    }

    #[test]
    fn test_domain_separator_computation() {
        let separator = LiveExecutor::compute_domain_separator();
        // Should produce a non-zero hash
        assert_ne!(separator, H256::zero());
    }

    #[test]
    fn test_order_type_hash_computation() {
        let type_hash = LiveExecutor::compute_order_type_hash();
        // Should produce a non-zero hash
        assert_ne!(type_hash, H256::zero());
    }

    #[test]
    fn test_signature_to_bytes() {
        let sig = Signature {
            r: U256::from(1),
            s: U256::from(2),
            v: 27,
        };

        let bytes = signature_to_bytes(&sig);
        assert_eq!(bytes.len(), 65);
        assert_eq!(bytes[64], 27);
    }

    #[test]
    fn test_wallet_address_derivation() {
        // Test with a known test private key
        // This is a well-known test key, DO NOT use in production
        let test_key = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";

        let wallet = Wallet::from_private_key(
            test_key,
            "test_key".to_string(),
            "test_secret".to_string(),
            "test_pass".to_string(),
        );

        // Should succeed with valid key
        assert!(wallet.is_ok());

        let wallet = wallet.unwrap();
        // The address for this test key is known
        assert_ne!(wallet.address(), Address::zero());
    }

    #[test]
    fn test_wallet_invalid_key() {
        let result = Wallet::from_private_key(
            "invalid_hex",
            "key".to_string(),
            "secret".to_string(),
            "pass".to_string(),
        );

        assert!(result.is_err());
    }

    #[test]
    fn test_wallet_short_key() {
        let result = Wallet::from_private_key(
            "0x1234", // Too short
            "key".to_string(),
            "secret".to_string(),
            "pass".to_string(),
        );

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_live_executor_requires_credentials() {
        let config = LiveExecutorConfig::default();
        let wallet_config = WalletConfig::default(); // No credentials

        let result = LiveExecutor::new(
            config,
            &wallet_config,
            None,
            dec!(1000),
        );

        assert!(result.is_err());
    }

    #[test]
    fn test_tracked_order_status() {
        let status = TrackedOrderStatus::Pending;
        assert!(matches!(status, TrackedOrderStatus::Pending));

        let status = TrackedOrderStatus::Filled;
        assert!(matches!(status, TrackedOrderStatus::Filled));
    }

    // Integration tests would require a test network or mocked API
    // They are skipped here to avoid external dependencies
}
