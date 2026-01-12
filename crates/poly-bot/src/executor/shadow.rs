//! Shadow bid manager for fast secondary order submission.
//!
//! Shadow bids are pre-prepared orders that fire immediately when a primary
//! order fills. This module implements:
//!
//! - Pre-hashing of EIP-712 order data at shadow creation time
//! - Fast signing path (<2ms) when primary fill triggers shadow
//! - DashMap storage keyed by (event_id, outcome)
//!
//! ## Performance Target
//!
//! Shadow bid must fire within 2ms of primary fill notification.
//! We achieve this by pre-computing everything except the final signature.

use std::sync::Arc;
use std::time::Instant;

use chrono::{DateTime, Utc};
use dashmap::DashMap;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use poly_common::types::Outcome;

use crate::config::ShadowConfig;

/// Errors that can occur in shadow bid operations.
#[derive(Debug, Error)]
pub enum ShadowError {
    #[error("Shadow order not found for event={event_id}, outcome={outcome:?}")]
    NotFound { event_id: String, outcome: Outcome },

    #[error("Shadow already exists for event={event_id}, outcome={outcome:?}")]
    AlreadyExists { event_id: String, outcome: Outcome },

    #[error("Shadow expired: created={created_ms}ms ago, max={max_ms}ms")]
    Expired { created_ms: u64, max_ms: u64 },

    #[error("Signing failed: {0}")]
    SigningFailed(String),

    #[error("Submission failed: {0}")]
    SubmissionFailed(String),

    #[error("Invalid shadow order: {0}")]
    Invalid(String),
}

/// Pre-computed hash components for EIP-712 signing.
///
/// EIP-712 structured data signing involves:
/// 1. Domain separator (constant per contract/chain)
/// 2. Type hash (constant per order struct)
/// 3. Struct hash (computed from order fields)
/// 4. Final hash = keccak256(0x1901 || domain_separator || struct_hash)
///
/// We pre-compute as much as possible at shadow creation time:
/// - Domain separator: constant, compute once
/// - Type hash: constant, compute once
/// - Partial struct hash: compute from known fields (token, side, maker)
/// - At fire time: only need to update price/size and finalize
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrehashedOrder {
    /// Pre-computed domain separator hash (keccak256 of domain struct).
    /// This is constant for a given contract on a given chain.
    pub domain_separator: [u8; 32],

    /// Pre-computed type hash (keccak256 of type definition string).
    /// This is constant for the Order struct schema.
    pub type_hash: [u8; 32],

    /// Partial struct encoding of known fields.
    /// At fire time, we append price/size fields and hash.
    pub partial_encoding: Vec<u8>,

    /// Token address (for verification).
    pub token_id: String,

    /// Maker address (for verification).
    pub maker: String,

    /// Order side (0 = buy, 1 = sell).
    pub side: u8,

    /// Nonce for replay protection.
    pub nonce: u64,

    /// Expiration timestamp.
    pub expiration: i64,
}

impl PrehashedOrder {
    /// Polymarket CLOB domain separator for mainnet.
    /// Computed as keccak256(abi.encode(
    ///   keccak256("EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)"),
    ///   keccak256("Polymarket CTF Exchange"),
    ///   keccak256("1"),
    ///   137,  // Polygon mainnet
    ///   exchange_address
    /// ))
    ///
    /// For production, this would be computed from actual contract address.
    pub const MAINNET_DOMAIN_SEPARATOR: [u8; 32] = [0u8; 32]; // Placeholder

    /// Order type hash.
    /// keccak256("Order(uint256 salt,address maker,address signer,address taker,uint256 tokenId,uint256 makerAmount,uint256 takerAmount,uint256 expiration,uint256 nonce,uint256 feeRateBps,uint8 side,uint8 signatureType)")
    pub const ORDER_TYPE_HASH: [u8; 32] = [0u8; 32]; // Placeholder

    /// Create a new pre-hashed order from known fields.
    ///
    /// This pre-computes as much as possible for fast signing later.
    pub fn new(
        token_id: String,
        maker: String,
        side: u8,
        nonce: u64,
        expiration: i64,
        domain_separator: Option<[u8; 32]>,
    ) -> Self {
        let domain = domain_separator.unwrap_or(Self::MAINNET_DOMAIN_SEPARATOR);

        // Build partial encoding of known fields
        // This would encode: salt, maker, signer, taker, tokenId, nonce, expiration, side
        // Leaving makerAmount, takerAmount to be filled at fire time
        let mut encoding = Vec::with_capacity(320);

        // In real implementation, this would use abi.encode() equivalent
        // For now, we just store the raw field values for later encoding
        encoding.extend_from_slice(&nonce.to_be_bytes());
        encoding.extend_from_slice(maker.as_bytes());
        encoding.extend_from_slice(token_id.as_bytes());
        encoding.push(side);
        encoding.extend_from_slice(&expiration.to_be_bytes());

        Self {
            domain_separator: domain,
            type_hash: Self::ORDER_TYPE_HASH,
            partial_encoding: encoding,
            token_id,
            maker,
            side,
            nonce,
            expiration,
        }
    }

    /// Finalize the struct hash with price and size.
    ///
    /// This completes the pre-computed encoding and returns the
    /// full struct hash ready for signing.
    pub fn finalize_struct_hash(&self, maker_amount: u128, taker_amount: u128) -> [u8; 32] {
        // In real implementation:
        // 1. Complete the abi.encode with maker_amount and taker_amount
        // 2. Prepend type_hash
        // 3. Return keccak256 of the full encoding

        // Placeholder: combine inputs for testing
        let mut data = Vec::with_capacity(self.partial_encoding.len() + 32);
        data.extend_from_slice(&self.partial_encoding);
        data.extend_from_slice(&maker_amount.to_be_bytes());
        data.extend_from_slice(&taker_amount.to_be_bytes());

        // Would use actual keccak256 in production
        let mut hash = [0u8; 32];
        if data.len() >= 32 {
            hash.copy_from_slice(&data[..32]);
        }
        hash
    }

    /// Compute the final EIP-712 hash for signing.
    ///
    /// Returns: keccak256(0x1901 || domain_separator || struct_hash)
    pub fn compute_signing_hash(&self, struct_hash: [u8; 32]) -> [u8; 32] {
        // In real implementation:
        // let mut data = Vec::with_capacity(66);
        // data.extend_from_slice(&[0x19, 0x01]);
        // data.extend_from_slice(&self.domain_separator);
        // data.extend_from_slice(&struct_hash);
        // keccak256(&data)

        // Placeholder: XOR domain and struct for testing
        let mut hash = [0u8; 32];
        for i in 0..32 {
            hash[i] = self.domain_separator[i] ^ struct_hash[i];
        }
        hash
    }
}

/// A shadow order waiting to be fired.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShadowOrder {
    /// Unique shadow order ID.
    pub id: String,

    /// Event ID this shadow is associated with.
    pub event_id: String,

    /// Token ID to trade.
    pub token_id: String,

    /// YES or NO outcome.
    pub outcome: Outcome,

    /// Shadow bid price (adjusted from primary).
    pub price: Decimal,

    /// Shadow bid size.
    pub size: Decimal,

    /// Price offset applied from primary order (basis points).
    pub price_offset_bps: u32,

    /// Creation timestamp.
    pub created_at: DateTime<Utc>,

    /// Maximum time to wait before expiring (milliseconds).
    pub max_wait_ms: u64,

    /// Pre-computed hash data for fast signing.
    pub pre_hash: PrehashedOrder,

    /// Primary order ID that triggers this shadow.
    pub primary_order_id: Option<String>,

    /// Status of the shadow order.
    pub status: ShadowStatus,
}

/// Shadow order status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ShadowStatus {
    /// Waiting for primary fill.
    Pending,
    /// Primary filled, shadow firing.
    Firing,
    /// Shadow order submitted.
    Submitted,
    /// Shadow order filled.
    Filled,
    /// Shadow order failed.
    Failed,
    /// Shadow expired before use.
    Expired,
    /// Shadow cancelled.
    Cancelled,
}

impl ShadowOrder {
    /// Check if this shadow has expired.
    pub fn is_expired(&self) -> bool {
        let elapsed_ms = Utc::now()
            .signed_duration_since(self.created_at)
            .num_milliseconds() as u64;
        elapsed_ms > self.max_wait_ms
    }

    /// Milliseconds since creation.
    pub fn age_ms(&self) -> u64 {
        Utc::now()
            .signed_duration_since(self.created_at)
            .num_milliseconds()
            .max(0) as u64
    }

    /// Check if shadow is in a terminal state.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self.status,
            ShadowStatus::Filled
                | ShadowStatus::Failed
                | ShadowStatus::Expired
                | ShadowStatus::Cancelled
        )
    }
}

/// Result of firing a shadow order.
#[derive(Debug, Clone)]
pub struct ShadowFireResult {
    /// Shadow order ID.
    pub shadow_id: String,

    /// Assigned order ID (if submitted).
    pub order_id: Option<String>,

    /// Whether the fire was successful.
    pub success: bool,

    /// Time from fire() call to order submission (microseconds).
    pub latency_us: u64,

    /// Error message if failed.
    pub error: Option<String>,

    /// Timestamp of fire attempt.
    pub timestamp: DateTime<Utc>,
}

/// Shadow bid manager for tracking and firing shadow orders.
///
/// Shadows are stored in a DashMap keyed by (event_id, outcome) for
/// lock-free concurrent access.
pub struct ShadowManager {
    /// Configuration.
    config: ShadowConfig,

    /// Active shadow orders by (event_id, outcome).
    shadows: DashMap<(String, Outcome), ShadowOrder>,

    /// Maker address for orders.
    maker_address: String,

    /// Domain separator for EIP-712 signing.
    domain_separator: [u8; 32],

    /// Next nonce for orders.
    next_nonce: std::sync::atomic::AtomicU64,
}

impl ShadowManager {
    /// Create a new shadow manager.
    pub fn new(config: ShadowConfig, maker_address: String) -> Self {
        Self {
            config,
            shadows: DashMap::new(),
            maker_address,
            domain_separator: PrehashedOrder::MAINNET_DOMAIN_SEPARATOR,
            next_nonce: std::sync::atomic::AtomicU64::new(1),
        }
    }

    /// Create with custom domain separator (for testing or alternate chains).
    pub fn with_domain_separator(
        config: ShadowConfig,
        maker_address: String,
        domain_separator: [u8; 32],
    ) -> Self {
        Self {
            config,
            shadows: DashMap::new(),
            maker_address,
            domain_separator,
            next_nonce: std::sync::atomic::AtomicU64::new(1),
        }
    }

    /// Check if shadow bidding is enabled.
    pub fn is_enabled(&self) -> bool {
        self.config.enabled
    }

    /// Create and store a shadow order for a primary order.
    ///
    /// The shadow will fire automatically when the primary order fills.
    pub fn create_shadow(
        &self,
        event_id: String,
        token_id: String,
        outcome: Outcome,
        primary_price: Decimal,
        size: Decimal,
        primary_order_id: Option<String>,
    ) -> Result<ShadowOrder, ShadowError> {
        if !self.config.enabled {
            return Err(ShadowError::Invalid("Shadow bidding disabled".to_string()));
        }

        let key = (event_id.clone(), outcome);

        // Check if shadow already exists
        if self.shadows.contains_key(&key) {
            return Err(ShadowError::AlreadyExists {
                event_id,
                outcome,
            });
        }

        // Calculate shadow price with offset
        let offset_decimal = Decimal::new(self.config.price_offset_bps as i64, 4);
        let shadow_price = primary_price + offset_decimal;

        // Generate unique ID and nonce
        let nonce = self
            .next_nonce
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let id = format!("shadow-{}-{:?}-{}", event_id, outcome, nonce);

        // Pre-compute hash for fast signing
        let expiration = (Utc::now() + chrono::Duration::milliseconds(self.config.max_wait_ms as i64))
            .timestamp();

        let pre_hash = PrehashedOrder::new(
            token_id.clone(),
            self.maker_address.clone(),
            match outcome {
                Outcome::Yes => 0,
                Outcome::No => 1,
            },
            nonce,
            expiration,
            Some(self.domain_separator),
        );

        let shadow = ShadowOrder {
            id,
            event_id: event_id.clone(),
            token_id,
            outcome,
            price: shadow_price,
            size,
            price_offset_bps: self.config.price_offset_bps,
            created_at: Utc::now(),
            max_wait_ms: self.config.max_wait_ms,
            pre_hash,
            primary_order_id,
            status: ShadowStatus::Pending,
        };

        self.shadows.insert(key, shadow.clone());

        Ok(shadow)
    }

    /// Get a shadow order by event and outcome.
    pub fn get_shadow(&self, event_id: &str, outcome: Outcome) -> Option<ShadowOrder> {
        self.shadows
            .get(&(event_id.to_string(), outcome))
            .map(|r| r.value().clone())
    }

    /// Fire a shadow order immediately.
    ///
    /// This is called when the primary order fills. It must complete
    /// in under 2ms to capture the opportunity.
    ///
    /// Returns the fire result with timing information.
    pub fn fire(&self, event_id: &str, outcome: Outcome) -> Result<ShadowFireResult, ShadowError> {
        let start = Instant::now();
        let key = (event_id.to_string(), outcome);

        // Get and validate shadow
        let mut shadow = self
            .shadows
            .get_mut(&key)
            .ok_or_else(|| ShadowError::NotFound {
                event_id: event_id.to_string(),
                outcome,
            })?;

        // Check expiration
        if shadow.is_expired() {
            shadow.status = ShadowStatus::Expired;
            return Err(ShadowError::Expired {
                created_ms: shadow.age_ms(),
                max_ms: shadow.max_wait_ms,
            });
        }

        // Mark as firing
        shadow.status = ShadowStatus::Firing;

        // Compute final hash with actual price/size
        // Convert Decimal to u128 (in smallest units, e.g., USDC cents)
        let maker_amount = (shadow.size * Decimal::new(1_000_000, 0))
            .try_into()
            .unwrap_or(0u128);
        let taker_amount = (shadow.price * shadow.size * Decimal::new(1_000_000, 0))
            .try_into()
            .unwrap_or(0u128);

        let struct_hash = shadow.pre_hash.finalize_struct_hash(maker_amount, taker_amount);
        let _signing_hash = shadow.pre_hash.compute_signing_hash(struct_hash);

        // In production, we would now:
        // 1. Sign the hash with the private key
        // 2. Submit the order to the exchange
        // For now, we simulate success

        let latency_us = start.elapsed().as_micros() as u64;

        // Generate mock order ID
        let order_id = format!("order-{}", shadow.id);

        shadow.status = ShadowStatus::Submitted;
        drop(shadow); // Release lock before returning

        Ok(ShadowFireResult {
            shadow_id: event_id.to_string(),
            order_id: Some(order_id),
            success: true,
            latency_us,
            error: None,
            timestamp: Utc::now(),
        })
    }

    /// Fire a shadow with async order submission.
    ///
    /// This variant accepts a submission function for real order placement.
    pub async fn fire_with_submitter<F, Fut>(
        &self,
        event_id: &str,
        outcome: Outcome,
        submitter: F,
    ) -> Result<ShadowFireResult, ShadowError>
    where
        F: FnOnce(ShadowOrder, [u8; 32]) -> Fut,
        Fut: std::future::Future<Output = Result<String, String>>,
    {
        let start = Instant::now();
        let key = (event_id.to_string(), outcome);

        // Get shadow order
        let shadow_clone;
        {
            let mut shadow = self
                .shadows
                .get_mut(&key)
                .ok_or_else(|| ShadowError::NotFound {
                    event_id: event_id.to_string(),
                    outcome,
                })?;

            if shadow.is_expired() {
                shadow.status = ShadowStatus::Expired;
                return Err(ShadowError::Expired {
                    created_ms: shadow.age_ms(),
                    max_ms: shadow.max_wait_ms,
                });
            }

            shadow.status = ShadowStatus::Firing;
            shadow_clone = shadow.clone();
        }

        // Compute signing hash
        let maker_amount = (shadow_clone.size * Decimal::new(1_000_000, 0))
            .try_into()
            .unwrap_or(0u128);
        let taker_amount = (shadow_clone.price * shadow_clone.size * Decimal::new(1_000_000, 0))
            .try_into()
            .unwrap_or(0u128);

        let struct_hash = shadow_clone
            .pre_hash
            .finalize_struct_hash(maker_amount, taker_amount);
        let signing_hash = shadow_clone.pre_hash.compute_signing_hash(struct_hash);

        // Submit order
        match submitter(shadow_clone.clone(), signing_hash).await {
            Ok(order_id) => {
                if let Some(mut shadow) = self.shadows.get_mut(&key) {
                    shadow.status = ShadowStatus::Submitted;
                }

                Ok(ShadowFireResult {
                    shadow_id: shadow_clone.id,
                    order_id: Some(order_id),
                    success: true,
                    latency_us: start.elapsed().as_micros() as u64,
                    error: None,
                    timestamp: Utc::now(),
                })
            }
            Err(e) => {
                if let Some(mut shadow) = self.shadows.get_mut(&key) {
                    shadow.status = ShadowStatus::Failed;
                }

                Err(ShadowError::SubmissionFailed(e))
            }
        }
    }

    /// Update shadow status to filled.
    pub fn mark_filled(&self, event_id: &str, outcome: Outcome) {
        let key = (event_id.to_string(), outcome);
        if let Some(mut shadow) = self.shadows.get_mut(&key) {
            shadow.status = ShadowStatus::Filled;
        }
    }

    /// Update shadow status to failed.
    pub fn mark_failed(&self, event_id: &str, outcome: Outcome) {
        let key = (event_id.to_string(), outcome);
        if let Some(mut shadow) = self.shadows.get_mut(&key) {
            shadow.status = ShadowStatus::Failed;
        }
    }

    /// Cancel a pending shadow order.
    pub fn cancel(&self, event_id: &str, outcome: Outcome) -> bool {
        let key = (event_id.to_string(), outcome);
        if let Some(mut shadow) = self.shadows.get_mut(&key)
            && shadow.status == ShadowStatus::Pending
        {
            shadow.status = ShadowStatus::Cancelled;
            return true;
        }
        false
    }

    /// Remove a shadow order from tracking.
    pub fn remove(&self, event_id: &str, outcome: Outcome) -> Option<ShadowOrder> {
        let key = (event_id.to_string(), outcome);
        self.shadows.remove(&key).map(|(_, v)| v)
    }

    /// Get all pending shadows.
    pub fn pending_shadows(&self) -> Vec<ShadowOrder> {
        self.shadows
            .iter()
            .filter(|r| r.value().status == ShadowStatus::Pending)
            .map(|r| r.value().clone())
            .collect()
    }

    /// Clean up expired shadows.
    ///
    /// Returns the number of shadows removed.
    pub fn cleanup_expired(&self) -> usize {
        let mut removed = 0;
        let keys_to_remove: Vec<_> = self
            .shadows
            .iter()
            .filter(|r| r.value().is_expired() || r.value().is_terminal())
            .map(|r| r.key().clone())
            .collect();

        for key in keys_to_remove {
            if self.shadows.remove(&key).is_some() {
                removed += 1;
            }
        }
        removed
    }

    /// Get count of active shadows.
    pub fn active_count(&self) -> usize {
        self.shadows
            .iter()
            .filter(|r| !r.value().is_terminal())
            .count()
    }

    /// Get total shadow count.
    pub fn total_count(&self) -> usize {
        self.shadows.len()
    }
}

impl std::fmt::Debug for ShadowManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShadowManager")
            .field("enabled", &self.config.enabled)
            .field("active_shadows", &self.active_count())
            .field("total_shadows", &self.total_count())
            .finish()
    }
}

/// Shared shadow manager wrapped in Arc for concurrent access.
pub type SharedShadowManager = Arc<ShadowManager>;

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;
    use std::time::Duration;

    fn test_config() -> ShadowConfig {
        ShadowConfig {
            enabled: true,
            price_offset_bps: 50,
            max_wait_ms: 2000,
        }
    }

    #[test]
    fn test_shadow_manager_creation() {
        let manager = ShadowManager::new(test_config(), "0xmaker".to_string());
        assert!(manager.is_enabled());
        assert_eq!(manager.total_count(), 0);
    }

    #[test]
    fn test_create_shadow() {
        let manager = ShadowManager::new(test_config(), "0xmaker".to_string());

        let shadow = manager
            .create_shadow(
                "event-1".to_string(),
                "token-1".to_string(),
                Outcome::Yes,
                dec!(0.45),
                dec!(100),
                Some("primary-1".to_string()),
            )
            .unwrap();

        assert_eq!(shadow.event_id, "event-1");
        assert_eq!(shadow.outcome, Outcome::Yes);
        assert_eq!(shadow.price, dec!(0.455)); // 0.45 + 0.005 (50 bps)
        assert_eq!(shadow.size, dec!(100));
        assert_eq!(shadow.status, ShadowStatus::Pending);
    }

    #[test]
    fn test_create_shadow_duplicate() {
        let manager = ShadowManager::new(test_config(), "0xmaker".to_string());

        manager
            .create_shadow(
                "event-1".to_string(),
                "token-1".to_string(),
                Outcome::Yes,
                dec!(0.45),
                dec!(100),
                None,
            )
            .unwrap();

        // Try to create another shadow for same event/outcome
        let result = manager.create_shadow(
            "event-1".to_string(),
            "token-1".to_string(),
            Outcome::Yes,
            dec!(0.50),
            dec!(50),
            None,
        );

        assert!(matches!(result, Err(ShadowError::AlreadyExists { .. })));
    }

    #[test]
    fn test_create_shadow_different_outcomes() {
        let manager = ShadowManager::new(test_config(), "0xmaker".to_string());

        // Can create shadows for different outcomes
        manager
            .create_shadow(
                "event-1".to_string(),
                "token-yes".to_string(),
                Outcome::Yes,
                dec!(0.45),
                dec!(100),
                None,
            )
            .unwrap();

        manager
            .create_shadow(
                "event-1".to_string(),
                "token-no".to_string(),
                Outcome::No,
                dec!(0.55),
                dec!(100),
                None,
            )
            .unwrap();

        assert_eq!(manager.total_count(), 2);
    }

    #[test]
    fn test_get_shadow() {
        let manager = ShadowManager::new(test_config(), "0xmaker".to_string());

        manager
            .create_shadow(
                "event-1".to_string(),
                "token-1".to_string(),
                Outcome::Yes,
                dec!(0.45),
                dec!(100),
                None,
            )
            .unwrap();

        let shadow = manager.get_shadow("event-1", Outcome::Yes).unwrap();
        assert_eq!(shadow.event_id, "event-1");

        assert!(manager.get_shadow("event-1", Outcome::No).is_none());
        assert!(manager.get_shadow("event-2", Outcome::Yes).is_none());
    }

    #[test]
    fn test_fire_shadow() {
        let manager = ShadowManager::new(test_config(), "0xmaker".to_string());

        manager
            .create_shadow(
                "event-1".to_string(),
                "token-1".to_string(),
                Outcome::Yes,
                dec!(0.45),
                dec!(100),
                None,
            )
            .unwrap();

        let result = manager.fire("event-1", Outcome::Yes).unwrap();

        assert!(result.success);
        assert!(result.order_id.is_some());
        // Check latency is reasonable (should be under 2ms = 2000us)
        assert!(result.latency_us < 2000);

        // Shadow should now be submitted
        let shadow = manager.get_shadow("event-1", Outcome::Yes).unwrap();
        assert_eq!(shadow.status, ShadowStatus::Submitted);
    }

    #[test]
    fn test_fire_not_found() {
        let manager = ShadowManager::new(test_config(), "0xmaker".to_string());

        let result = manager.fire("nonexistent", Outcome::Yes);
        assert!(matches!(result, Err(ShadowError::NotFound { .. })));
    }

    #[test]
    fn test_fire_expired() {
        let mut config = test_config();
        config.max_wait_ms = 1; // 1ms timeout

        let manager = ShadowManager::new(config, "0xmaker".to_string());

        manager
            .create_shadow(
                "event-1".to_string(),
                "token-1".to_string(),
                Outcome::Yes,
                dec!(0.45),
                dec!(100),
                None,
            )
            .unwrap();

        // Wait for expiration
        std::thread::sleep(Duration::from_millis(5));

        let result = manager.fire("event-1", Outcome::Yes);
        assert!(matches!(result, Err(ShadowError::Expired { .. })));
    }

    #[test]
    fn test_cancel_shadow() {
        let manager = ShadowManager::new(test_config(), "0xmaker".to_string());

        manager
            .create_shadow(
                "event-1".to_string(),
                "token-1".to_string(),
                Outcome::Yes,
                dec!(0.45),
                dec!(100),
                None,
            )
            .unwrap();

        assert!(manager.cancel("event-1", Outcome::Yes));

        let shadow = manager.get_shadow("event-1", Outcome::Yes).unwrap();
        assert_eq!(shadow.status, ShadowStatus::Cancelled);

        // Can't cancel again
        assert!(!manager.cancel("event-1", Outcome::Yes));
    }

    #[test]
    fn test_mark_filled() {
        let manager = ShadowManager::new(test_config(), "0xmaker".to_string());

        manager
            .create_shadow(
                "event-1".to_string(),
                "token-1".to_string(),
                Outcome::Yes,
                dec!(0.45),
                dec!(100),
                None,
            )
            .unwrap();

        manager.fire("event-1", Outcome::Yes).unwrap();
        manager.mark_filled("event-1", Outcome::Yes);

        let shadow = manager.get_shadow("event-1", Outcome::Yes).unwrap();
        assert_eq!(shadow.status, ShadowStatus::Filled);
    }

    #[test]
    fn test_remove_shadow() {
        let manager = ShadowManager::new(test_config(), "0xmaker".to_string());

        manager
            .create_shadow(
                "event-1".to_string(),
                "token-1".to_string(),
                Outcome::Yes,
                dec!(0.45),
                dec!(100),
                None,
            )
            .unwrap();

        let removed = manager.remove("event-1", Outcome::Yes);
        assert!(removed.is_some());
        assert!(manager.get_shadow("event-1", Outcome::Yes).is_none());
    }

    #[test]
    fn test_pending_shadows() {
        let manager = ShadowManager::new(test_config(), "0xmaker".to_string());

        manager
            .create_shadow(
                "event-1".to_string(),
                "token-1".to_string(),
                Outcome::Yes,
                dec!(0.45),
                dec!(100),
                None,
            )
            .unwrap();

        manager
            .create_shadow(
                "event-2".to_string(),
                "token-2".to_string(),
                Outcome::No,
                dec!(0.55),
                dec!(50),
                None,
            )
            .unwrap();

        // Fire one
        manager.fire("event-1", Outcome::Yes).unwrap();

        let pending = manager.pending_shadows();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].event_id, "event-2");
    }

    #[test]
    fn test_cleanup_expired() {
        let mut config = test_config();
        config.max_wait_ms = 1;

        let manager = ShadowManager::new(config, "0xmaker".to_string());

        manager
            .create_shadow(
                "event-1".to_string(),
                "token-1".to_string(),
                Outcome::Yes,
                dec!(0.45),
                dec!(100),
                None,
            )
            .unwrap();

        std::thread::sleep(Duration::from_millis(5));

        let removed = manager.cleanup_expired();
        assert_eq!(removed, 1);
        assert_eq!(manager.total_count(), 0);
    }

    #[test]
    fn test_active_count() {
        let manager = ShadowManager::new(test_config(), "0xmaker".to_string());

        manager
            .create_shadow(
                "event-1".to_string(),
                "token-1".to_string(),
                Outcome::Yes,
                dec!(0.45),
                dec!(100),
                None,
            )
            .unwrap();

        manager
            .create_shadow(
                "event-2".to_string(),
                "token-2".to_string(),
                Outcome::No,
                dec!(0.55),
                dec!(50),
                None,
            )
            .unwrap();

        assert_eq!(manager.active_count(), 2);

        manager.fire("event-1", Outcome::Yes).unwrap();
        manager.mark_filled("event-1", Outcome::Yes);

        // One filled (terminal), one pending (active)
        assert_eq!(manager.active_count(), 1);
    }

    #[test]
    fn test_shadow_order_age() {
        let manager = ShadowManager::new(test_config(), "0xmaker".to_string());

        manager
            .create_shadow(
                "event-1".to_string(),
                "token-1".to_string(),
                Outcome::Yes,
                dec!(0.45),
                dec!(100),
                None,
            )
            .unwrap();

        std::thread::sleep(Duration::from_millis(10));

        let shadow = manager.get_shadow("event-1", Outcome::Yes).unwrap();
        assert!(shadow.age_ms() >= 10);
    }

    #[test]
    fn test_prehashed_order() {
        let pre_hash = PrehashedOrder::new(
            "token-1".to_string(),
            "0xmaker".to_string(),
            0, // buy
            12345,
            Utc::now().timestamp() + 3600,
            None,
        );

        assert_eq!(pre_hash.token_id, "token-1");
        assert_eq!(pre_hash.maker, "0xmaker");
        assert_eq!(pre_hash.side, 0);
        assert_eq!(pre_hash.nonce, 12345);

        // Test finalize
        let struct_hash = pre_hash.finalize_struct_hash(1_000_000, 450_000);
        assert_ne!(struct_hash, [0u8; 32]);

        // Test signing hash
        let signing_hash = pre_hash.compute_signing_hash(struct_hash);
        assert_ne!(signing_hash, [0u8; 32]);
    }

    #[test]
    fn test_shadow_disabled() {
        let mut config = test_config();
        config.enabled = false;

        let manager = ShadowManager::new(config, "0xmaker".to_string());
        assert!(!manager.is_enabled());

        let result = manager.create_shadow(
            "event-1".to_string(),
            "token-1".to_string(),
            Outcome::Yes,
            dec!(0.45),
            dec!(100),
            None,
        );

        assert!(matches!(result, Err(ShadowError::Invalid(_))));
    }

    #[tokio::test]
    async fn test_fire_with_submitter() {
        let manager = ShadowManager::new(test_config(), "0xmaker".to_string());

        manager
            .create_shadow(
                "event-1".to_string(),
                "token-1".to_string(),
                Outcome::Yes,
                dec!(0.45),
                dec!(100),
                None,
            )
            .unwrap();

        let result = manager
            .fire_with_submitter("event-1", Outcome::Yes, |_shadow, _hash| async {
                Ok("order-123".to_string())
            })
            .await
            .unwrap();

        assert!(result.success);
        assert_eq!(result.order_id, Some("order-123".to_string()));
    }

    #[tokio::test]
    async fn test_fire_with_submitter_failure() {
        let manager = ShadowManager::new(test_config(), "0xmaker".to_string());

        manager
            .create_shadow(
                "event-1".to_string(),
                "token-1".to_string(),
                Outcome::Yes,
                dec!(0.45),
                dec!(100),
                None,
            )
            .unwrap();

        let result = manager
            .fire_with_submitter("event-1", Outcome::Yes, |_shadow, _hash| async {
                Err("Network error".to_string())
            })
            .await;

        assert!(matches!(result, Err(ShadowError::SubmissionFailed(_))));

        let shadow = manager.get_shadow("event-1", Outcome::Yes).unwrap();
        assert_eq!(shadow.status, ShadowStatus::Failed);
    }
}
