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

/// Polymarket CLOB API endpoints and contract addresses.
pub mod endpoints {
    /// Production CLOB REST API.
    pub const CLOB_REST: &str = "https://clob.polymarket.com";
    /// Production CLOB WebSocket.
    pub const CLOB_WS: &str = "wss://ws-subscriptions-clob.polymarket.com/ws/user";
    /// Polygon mainnet chain ID.
    pub const CHAIN_ID: u64 = 137;
    /// CTF Exchange contract address (Polygon mainnet).
    pub const CTF_EXCHANGE: &str = "0x4bFb41d5B3570DeFd03C39a9A4D8dE6Bd8B8982E";
    /// Proxy wallet factory address (Polygon mainnet).
    pub const PROXY_FACTORY: &str = "0xaB45c5A4B0c941a2F231C04C3f49182e1A254052";
    /// EIP-1167 minimal proxy init code hash for CREATE2 derivation.
    pub const PROXY_INIT_CODE_HASH: &str = "0xd21df8dc65880a8606f09fe0ce3df9b8869287ab0b058be05aa9e8af6330a00b";
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
            fee_rate_bps: 1000, // 10% maker fee for crypto up/down markets
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
    /// EOA address derived from private key (used for signing).
    eoa_address: Address,
    /// Proxy wallet address (where funds are held, used for API calls).
    proxy_address: Address,
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
            .field("eoa_address", &self.eoa_address)
            .field("proxy_address", &self.proxy_address)
            .field("private_key", &"[REDACTED]")
            .field("api_key", &"[REDACTED]")
            .finish()
    }
}

/// API credentials derived from private key.
#[derive(Debug, Clone, Deserialize)]
pub struct ApiCredentials {
    #[serde(rename = "apiKey")]
    pub api_key: String,
    pub secret: String,
    pub passphrase: String,
}

impl Wallet {
    /// Create a new wallet from private key hex string with provided credentials.
    pub fn from_private_key(private_key_hex: &str, api_key: String, api_secret: String, api_passphrase: String) -> Result<Self, ExecutorError> {
        let (eoa_address, private_key) = Self::parse_private_key(private_key_hex)?;
        let proxy_address = Self::derive_proxy_wallet(eoa_address)?;

        Ok(Self {
            eoa_address,
            proxy_address,
            private_key,
            api_key,
            api_secret,
            api_passphrase,
        })
    }

    /// Create a wallet from private key only (credentials will be derived).
    pub fn from_private_key_only(private_key_hex: &str) -> Result<(Self, [u8; 32]), ExecutorError> {
        let (eoa_address, private_key) = Self::parse_private_key(private_key_hex)?;
        let proxy_address = Self::derive_proxy_wallet(eoa_address)?;

        // Return wallet with empty credentials - they'll be filled after derivation
        let wallet = Self {
            eoa_address,
            proxy_address,
            private_key,
            api_key: String::new(),
            api_secret: String::new(),
            api_passphrase: String::new(),
        };

        Ok((wallet, private_key))
    }

    /// Derive the Polymarket proxy wallet address from an EOA address using CREATE2.
    ///
    /// Polymarket uses EIP-1167 minimal proxy wallets. The proxy address is deterministically
    /// derived from the EOA address using the formula:
    /// proxy = CREATE2(factory, keccak256(eoa_address), PROXY_INIT_CODE_HASH)
    fn derive_proxy_wallet(eoa_address: Address) -> Result<Address, ExecutorError> {
        // Parse factory address
        let factory = endpoints::PROXY_FACTORY
            .parse::<Address>()
            .map_err(|e| ExecutorError::Internal(format!("Invalid proxy factory address: {}", e)))?;

        // Parse init code hash
        let init_code_hash_hex = endpoints::PROXY_INIT_CODE_HASH
            .strip_prefix("0x")
            .unwrap_or(endpoints::PROXY_INIT_CODE_HASH);
        let init_code_hash_bytes = hex::decode(init_code_hash_hex)
            .map_err(|e| ExecutorError::Internal(format!("Invalid init code hash: {}", e)))?;

        // Salt = keccak256(eoa_address) - address is 20 bytes, no padding
        let mut salt_hasher = Keccak256::new();
        salt_hasher.update(eoa_address.as_bytes());
        let salt = salt_hasher.finalize();

        // CREATE2 address = keccak256(0xff ++ factory ++ salt ++ init_code_hash)[12:]
        let mut create2_hasher = Keccak256::new();
        create2_hasher.update(&[0xff]);
        create2_hasher.update(factory.as_bytes());
        create2_hasher.update(&salt);
        create2_hasher.update(&init_code_hash_bytes);
        let create2_hash = create2_hasher.finalize();

        // Address is the last 20 bytes
        let proxy_bytes: [u8; 20] = create2_hash[12..32]
            .try_into()
            .map_err(|_| ExecutorError::Internal("Failed to derive proxy address".to_string()))?;

        Ok(Address::from(proxy_bytes))
    }

    /// Set API credentials after derivation.
    pub fn set_credentials(&mut self, creds: ApiCredentials) {
        self.api_key = creds.api_key;
        self.api_secret = creds.secret;
        self.api_passphrase = creds.passphrase;
    }

    /// Parse private key hex string into address and key bytes.
    fn parse_private_key(private_key_hex: &str) -> Result<(Address, [u8; 32]), ExecutorError> {
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

        Ok((address, private_key))
    }

    /// Derive API credentials from Polymarket using EIP-712 signed message.
    pub async fn derive_api_credentials(
        address: Address,
        private_key: &[u8; 32],
    ) -> Result<ApiCredentials, ExecutorError> {
        use std::time::{SystemTime, UNIX_EPOCH};

        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            .to_string();
        let nonce = "0";
        let message = "This message attests that I control the given wallet";

        // Compute EIP-712 struct hash for ClobAuth
        let auth_hash = Self::compute_clob_auth_hash(address, &timestamp, nonce, message);

        // Compute domain separator for ClobAuthDomain
        let domain_separator = Self::compute_auth_domain_separator();

        // Final EIP-712 hash: keccak256("\x19\x01" || domainSeparator || structHash)
        let mut final_msg = Vec::with_capacity(66);
        final_msg.extend_from_slice(b"\x19\x01");
        final_msg.extend_from_slice(domain_separator.as_bytes());
        final_msg.extend_from_slice(auth_hash.as_bytes());
        let final_hash = H256::from_slice(&Keccak256::digest(&final_msg));

        // Sign the hash
        let signature = Self::sign_hash_with_key(private_key, final_hash)?;
        let mut r_bytes = [0u8; 32];
        let mut s_bytes = [0u8; 32];
        signature.r.to_big_endian(&mut r_bytes);
        signature.s.to_big_endian(&mut s_bytes);
        let sig_hex = format!(
            "0x{}{}{}",
            hex::encode(r_bytes),
            hex::encode(s_bytes),
            hex::encode([signature.v as u8])
        );

        // Call derive-api-key endpoint
        let client = Client::new();
        let resp = client
            .get(format!("{}/auth/derive-api-key", endpoints::CLOB_REST))
            .header("POLY_ADDRESS", format!("{:?}", address))
            .header("POLY_SIGNATURE", &sig_hex)
            .header("POLY_TIMESTAMP", &timestamp)
            .header("POLY_NONCE", nonce)
            .send()
            .await
            .map_err(|e| ExecutorError::Internal(format!("Failed to derive API key: {}", e)))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(ExecutorError::Internal(format!(
                "Failed to derive API key: HTTP {} - {}",
                status, body
            )));
        }

        let creds: ApiCredentials = resp
            .json()
            .await
            .map_err(|e| ExecutorError::Internal(format!("Failed to parse API credentials: {}", e)))?;

        info!("Successfully derived API credentials");
        Ok(creds)
    }

    /// Compute the EIP-712 domain separator for ClobAuthDomain.
    fn compute_auth_domain_separator() -> H256 {
        // EIP-712 domain: keccak256("EIP712Domain(string name,string version,uint256 chainId)")
        let domain_type_hash = Keccak256::digest(
            b"EIP712Domain(string name,string version,uint256 chainId)"
        );
        let name_hash = Keccak256::digest(b"ClobAuthDomain");
        let version_hash = Keccak256::digest(b"1");

        let mut encoded = Vec::with_capacity(128);
        encoded.extend_from_slice(&domain_type_hash);
        encoded.extend_from_slice(&name_hash);
        encoded.extend_from_slice(&version_hash);
        // chainId = 137
        let mut chain_id_bytes = [0u8; 32];
        chain_id_bytes[31] = 137u8;
        encoded.extend_from_slice(&chain_id_bytes);

        H256::from_slice(&Keccak256::digest(&encoded))
    }

    /// Compute the EIP-712 struct hash for ClobAuth.
    fn compute_clob_auth_hash(address: Address, timestamp: &str, nonce: &str, message: &str) -> H256 {
        // Type hash: keccak256("ClobAuth(address address,string timestamp,uint256 nonce,string message)")
        let type_hash = Keccak256::digest(
            b"ClobAuth(address address,string timestamp,uint256 nonce,string message)"
        );

        let timestamp_hash = Keccak256::digest(timestamp.as_bytes());
        let nonce_value: u64 = nonce.parse().unwrap_or(0);
        let message_hash = Keccak256::digest(message.as_bytes());

        let mut encoded = Vec::with_capacity(160);
        encoded.extend_from_slice(&type_hash);
        // Address is padded to 32 bytes
        let mut addr_bytes = [0u8; 32];
        addr_bytes[12..].copy_from_slice(address.as_bytes());
        encoded.extend_from_slice(&addr_bytes);
        encoded.extend_from_slice(&timestamp_hash);
        // Nonce as uint256
        let mut nonce_bytes = [0u8; 32];
        nonce_bytes[24..].copy_from_slice(&nonce_value.to_be_bytes());
        encoded.extend_from_slice(&nonce_bytes);
        encoded.extend_from_slice(&message_hash);

        H256::from_slice(&Keccak256::digest(&encoded))
    }

    /// Sign a hash with the given private key.
    fn sign_hash_with_key(private_key: &[u8; 32], hash: H256) -> Result<Signature, ExecutorError> {
        use ethers_core::k256::ecdsa::{SigningKey, signature::hazmat::PrehashSigner};

        let signing_key = SigningKey::from_bytes(private_key.into())
            .map_err(|e| ExecutorError::Internal(format!("Invalid signing key: {}", e)))?;

        let (sig, recovery_id): (ethers_core::k256::ecdsa::Signature, _) = signing_key
            .sign_prehash(hash.as_bytes())
            .map_err(|e| ExecutorError::Internal(format!("Signing failed: {}", e)))?;

        let r = U256::from_big_endian(&sig.r().to_bytes());
        let s = U256::from_big_endian(&sig.s().to_bytes());
        let v = recovery_id.to_byte() as u64 + 27;

        Ok(Signature { r, s, v })
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

    /// Get the proxy wallet address (where funds are held, used for API calls).
    pub fn address(&self) -> Address {
        self.proxy_address
    }

    /// Get the EOA address (used for signing).
    pub fn eoa_address(&self) -> Address {
        self.eoa_address
    }

    /// Get the proxy wallet address explicitly.
    pub fn proxy_address(&self) -> Address {
        self.proxy_address
    }

    /// Create L2 HMAC signature for API requests.
    ///
    /// The signature is computed as: HMAC-SHA256(base64_decode(secret), message)
    /// where message = timestamp + method + path + body
    pub fn sign_l2_request(
        &self,
        timestamp: &str,
        method: &str,
        path: &str,
        body: Option<&str>,
    ) -> Result<String, ExecutorError> {
        use base64::{engine::general_purpose::URL_SAFE, Engine};
        use hmac::{Hmac, Mac};
        use sha2::Sha256;

        type HmacSha256 = Hmac<Sha256>;

        // Decode the base64 secret
        let secret_bytes = URL_SAFE
            .decode(&self.api_secret)
            .map_err(|e| ExecutorError::Internal(format!("Failed to decode API secret: {}", e)))?;

        // Construct message: timestamp + method + path + body
        let mut message = format!("{}{}{}", timestamp, method, path);
        if let Some(b) = body {
            // Replace single quotes with double quotes for compatibility
            message.push_str(&b.replace('\'', "\""));
        }

        // Create HMAC-SHA256
        let mut mac = HmacSha256::new_from_slice(&secret_bytes)
            .map_err(|e| ExecutorError::Internal(format!("Failed to create HMAC: {}", e)))?;
        mac.update(message.as_bytes());
        let result = mac.finalize();

        // Return base64-encoded signature
        Ok(URL_SAFE.encode(result.into_bytes()))
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
    #[serde(rename_all = "camelCase")]
    pub struct CreateOrderRequest {
        pub order: SignedOrder,
        pub owner: String,
        pub order_type: String,
    }

    /// Signed order for submission.
    #[derive(Debug, Serialize)]
    #[serde(rename_all = "camelCase")]
    pub struct SignedOrder {
        pub salt: u64,
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
    /// * `wallet_config` - Wallet credentials (requires private_key; api credentials will be derived if not provided)
    /// * `shadow_config` - Optional shadow bid configuration
    /// * `initial_balance` - Starting USDC balance
    ///
    /// # Errors
    ///
    /// Returns an error if wallet credentials are missing or invalid.
    pub async fn new(
        config: LiveExecutorConfig,
        wallet_config: &WalletConfig,
        shadow_config: Option<&ShadowConfig>,
        initial_balance: Decimal,
    ) -> Result<Self, ExecutorError> {
        // Validate private key is present
        let private_key_hex = wallet_config.private_key.as_ref()
            .ok_or_else(|| ExecutorError::Internal("POLY_PRIVATE_KEY not set".to_string()))?;

        // Check if API credentials are provided or need to be derived
        let wallet = if wallet_config.api_key.is_some()
            && wallet_config.api_secret.is_some()
            && wallet_config.api_passphrase.is_some()
        {
            // Use provided credentials
            Wallet::from_private_key(
                private_key_hex,
                wallet_config.api_key.clone().unwrap(),
                wallet_config.api_secret.clone().unwrap(),
                wallet_config.api_passphrase.clone().unwrap(),
            )?
        } else {
            // Derive credentials from private key
            info!("Deriving API credentials from private key...");
            let (mut wallet, pk_bytes) = Wallet::from_private_key_only(private_key_hex)?;
            let creds = Wallet::derive_api_credentials(wallet.eoa_address(), &pk_bytes).await?;
            wallet.set_credentials(creds);
            wallet
        };

        info!(
            eoa = %wallet.eoa_address(),
            proxy = %wallet.proxy_address(),
            "Initialized LiveExecutor - proxy wallet is where funds should be"
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

        // Expiration: 0 for GTC orders (non-zero only for GTD orders which we don't use)
        let expiration = 0u64;

        // Nonce for replay protection
        let nonce = salt;

        // Side: 0 = buy, 1 = sell
        let side = match request.side {
            Side::Buy => 0u8,
            Side::Sell => 1u8,
        };

        // Build struct hash (maker is the proxy wallet, signer is the EOA)
        let struct_hash = self.compute_struct_hash(
            salt,
            &self.wallet.proxy_address(),
            &self.wallet.eoa_address(),
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
            salt,
            maker: format!("{:?}", self.wallet.proxy_address()),
            signer: format!("{:?}", self.wallet.eoa_address()),
            taker: "0x0000000000000000000000000000000000000000".to_string(),
            token_id: request.token_id.clone(),
            maker_amount: maker_amount.to_string(),
            taker_amount: taker_amount.to_string(),
            expiration: expiration.to_string(),
            nonce: nonce.to_string(),
            fee_rate_bps: self.config.fee_rate_bps.to_string(),
            side: side.to_string(),
            signature_type: 1, // POLY_PROXY (EOA that owns a proxy wallet)
            signature: signature_hex,
        })
    }

    /// Compute struct hash for order.
    #[allow(clippy::too_many_arguments)]
    fn compute_struct_hash(
        &self,
        salt: u64,
        maker: &Address,
        signer: &Address,
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

        // maker (20 bytes padded to 32) - proxy wallet
        let mut maker_bytes = [0u8; 32];
        maker_bytes[12..].copy_from_slice(maker.as_bytes());
        encoded.extend_from_slice(&maker_bytes);

        // signer (20 bytes padded to 32) - EOA that signs
        let mut signer_bytes = [0u8; 32];
        signer_bytes[12..].copy_from_slice(signer.as_bytes());
        encoded.extend_from_slice(&signer_bytes);

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
        sig_type_bytes[31] = 1; // POLY_PROXY (EOA that owns a proxy wallet)
        encoded.extend_from_slice(&sig_type_bytes);

        let mut hasher = Keccak256::new();
        hasher.update(&encoded);
        H256::from_slice(&hasher.finalize())
    }

    /// Submit order to CLOB API.
    async fn submit_order(&self, signed_order: api::SignedOrder, order_type: &str) -> Result<String, ExecutorError> {
        let url = format!("{}/order", self.config.api_endpoint);

        let request_body = api::CreateOrderRequest {
            order: signed_order,
            owner: self.wallet.api_key.clone(),
            order_type: order_type.to_string(),
        };

        // Serialize body for HMAC signing
        let body_json = serde_json::to_string(&request_body)
            .map_err(|e| ExecutorError::Internal(format!("Failed to serialize order: {}", e)))?;

        // Debug: log the order payload
        tracing::info!(payload = %body_json, "Submitting order");

        // Create L2 HMAC signature
        let timestamp = Utc::now().timestamp().to_string();
        let path = "/order";
        let signature = self.wallet.sign_l2_request(&timestamp, "POST", path, Some(&body_json))?;

        let response = self.client
            .post(&url)
            .header("POLY_ADDRESS", format!("{:?}", self.wallet.eoa_address()))
            .header("POLY_SIGNATURE", signature)
            .header("POLY_TIMESTAMP", &timestamp)
            .header("POLY_API_KEY", &self.wallet.api_key)
            .header("POLY_PASSPHRASE", &self.wallet.api_passphrase)
            .header("Content-Type", "application/json")
            .body(body_json)
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
        let path = format!("/order/{}", order_id);
        let url = format!("{}{}", self.config.api_endpoint, path);
        let timestamp = Utc::now().timestamp().to_string();

        // Create HMAC signature for L2 auth
        let signature = self.wallet.sign_l2_request(&timestamp, "GET", &path, None)?;

        let response = self.client
            .get(&url)
            .header("POLY_ADDRESS", format!("{:?}", self.wallet.eoa_address()))
            .header("POLY_SIGNATURE", signature)
            .header("POLY_TIMESTAMP", &timestamp)
            .header("POLY_API_KEY", &self.wallet.api_key)
            .header("POLY_PASSPHRASE", &self.wallet.api_passphrase)
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
        let path = "/order";
        let url = format!("{}{}", self.config.api_endpoint, path);

        let request_body = api::CancelOrderRequest {
            order_id: order_id.to_string(),
        };

        // Serialize body for HMAC signing
        let body_json = serde_json::to_string(&request_body)
            .map_err(|e| ExecutorError::Internal(format!("Failed to serialize cancel request: {}", e)))?;

        // Create L2 HMAC signature
        let timestamp = Utc::now().timestamp().to_string();
        let signature = self.wallet.sign_l2_request(&timestamp, "DELETE", path, Some(&body_json))?;

        let response = self.client
            .delete(&url)
            .header("POLY_ADDRESS", format!("{:?}", self.wallet.eoa_address()))
            .header("POLY_SIGNATURE", signature)
            .header("POLY_TIMESTAMP", &timestamp)
            .header("POLY_API_KEY", &self.wallet.api_key)
            .header("POLY_PASSPHRASE", &self.wallet.api_passphrase)
            .header("Content-Type", "application/json")
            .body(body_json)
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
        let path = "/balance-allowance";
        let url = format!("{}{}", self.config.api_endpoint, path);
        let timestamp = Utc::now().timestamp().to_string();

        let request_body = api::BalanceAllowanceRequest {
            asset_type: "COLLATERAL".to_string(),
            token_id: None,
        };

        // For GET requests with query params, include them in the path for signing
        let query_string = serde_urlencoded::to_string(&request_body)
            .map_err(|e| ExecutorError::Internal(format!("Failed to encode query: {}", e)))?;
        let full_path = format!("{}?{}", path, query_string);

        // Create HMAC signature for L2 auth
        let signature = self.wallet.sign_l2_request(&timestamp, "GET", &full_path, None)?;

        let response = self.client
            .get(&url)
            .header("POLY_ADDRESS", format!("{:?}", self.wallet.eoa_address()))
            .header("POLY_SIGNATURE", &signature)
            .header("POLY_TIMESTAMP", &timestamp)
            .header("POLY_API_KEY", &self.wallet.api_key)
            .header("POLY_PASSPHRASE", &self.wallet.api_passphrase)
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

        // Map order type to API format
        let api_order_type = match request.order_type {
            OrderType::Gtc | OrderType::Limit => "GTC",
            OrderType::Ioc => "FOK", // Fill-Or-Kill
            OrderType::Market => unreachable!(), // Validated above
        };

        // Submit to exchange
        let order_id = self.submit_order(signed_order, api_order_type).await?;

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
        )
        .await;

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
