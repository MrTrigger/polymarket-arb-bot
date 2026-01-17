//! Live executor using official Polymarket SDK.
//!
//! This module wraps the polymarket-client-sdk for order submission,
//! letting the SDK handle EIP-712 signing, fee rates, and proxy wallets.

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;

use alloy::signers::local::{LocalSigner, PrivateKeySigner};
use alloy::signers::Signer as AlloySigner;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use polymarket_client_sdk::clob::{Client as ClobClient, Config as ClobConfig};
use polymarket_client_sdk::clob::types::{OrderType as SdkOrderType, Side as SdkSide, SignatureType, request::BalanceAllowanceRequest};
use polymarket_client_sdk::{derive_proxy_wallet, POLYGON};
use rust_decimal::Decimal;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use poly_common::types::Side;

use super::allowance::{AllowanceConfig, AllowanceManager, SharedAllowanceManager};
use super::live::LiveExecutorConfig;
use super::shadow::{ShadowManager, SharedShadowManager};
use super::{
    Executor, ExecutorError, OrderCancellation, OrderFill, OrderRequest,
    OrderResult, OrderType, PartialOrderFill, PendingOrder,
};
use crate::config::{ShadowConfig, WalletConfig};

/// Signer type alias for cleaner code (k256-based signer).
type Signer = PrivateKeySigner;

/// Live executor using official Polymarket SDK.
///
/// This executor delegates all order operations to the polymarket-client-sdk,
/// which handles EIP-712 signing, fee rates, proxy wallets, and authentication.
pub struct LiveSdkExecutor {
    /// Private key signer (with chain_id set for Polygon).
    signer: Signer,
    /// Private key hex for allowance manager.
    private_key_hex: String,
    /// Shadow bid manager.
    shadow_manager: Option<SharedShadowManager>,
    /// Allowance manager for automatic replenishment.
    allowance_manager: Option<SharedAllowanceManager>,
    /// Handle to the allowance monitoring task.
    allowance_task: Option<tokio::task::JoinHandle<()>>,
    /// Current USDC balance.
    balance: Arc<RwLock<Decimal>>,
    /// Order timeout in milliseconds.
    #[allow(dead_code)]
    order_timeout_ms: u64,
    /// Tracked orders.
    orders: Arc<RwLock<HashMap<String, TrackedOrder>>>,
    /// Request ID to order ID mapping.
    request_to_order: Arc<RwLock<HashMap<String, String>>>,
    /// Pending orders list.
    pending: Arc<RwLock<Vec<PendingOrder>>>,
    /// Wallet address (proxy wallet for info).
    wallet_address: String,
}

/// Internal order state for tracking.
#[derive(Debug, Clone)]
struct TrackedOrder {
    request: OrderRequest,
    order_id: String,
    status: TrackedOrderStatus,
    filled_size: Decimal,
    avg_fill_price: Decimal,
    #[allow(dead_code)]
    total_fee: Decimal,
    #[allow(dead_code)]
    created_at: DateTime<Utc>,
    #[allow(dead_code)]
    updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq)]
enum TrackedOrderStatus {
    Pending,
    #[allow(dead_code)]
    PartiallyFilled,
    #[allow(dead_code)]
    Filled,
    #[allow(dead_code)]
    Cancelled,
    #[allow(dead_code)]
    Expired,
}

impl LiveSdkExecutor {
    /// Create a new SDK-based live executor.
    ///
    /// # Arguments
    /// * `config` - Executor configuration
    /// * `wallet_config` - Wallet private key configuration
    /// * `shadow_config` - Optional shadow bid configuration
    /// * `initial_balance` - Initial balance (for display, actual balance fetched from chain)
    /// * `available_balance` - Target allowance amount for auto-replenishment
    /// * `global_state` - Optional global state for risk-aware allowance management
    pub async fn new(
        config: LiveExecutorConfig,
        wallet_config: &WalletConfig,
        shadow_config: Option<&ShadowConfig>,
        initial_balance: Decimal,
        available_balance: Decimal,
        global_state: Option<Arc<crate::state::GlobalState>>,
    ) -> Result<Self, ExecutorError> {
        // Get private key from config
        let private_key_hex = wallet_config.private_key.as_ref()
            .ok_or_else(|| ExecutorError::Internal("POLY_PRIVATE_KEY not set".to_string()))?
            .clone();

        // Create alloy signer with chain_id for Polygon
        let signer: Signer = LocalSigner::from_str(&private_key_hex)
            .map_err(|e| ExecutorError::Internal(format!("Invalid private key: {}", e)))?
            .with_chain_id(Some(POLYGON));

        // Get EOA address
        let eoa_address = signer.address();
        let eoa_address_str = format!("{:?}", eoa_address);

        // Derive proxy wallet address using CREATE2
        let proxy_address = derive_proxy_wallet(eoa_address, POLYGON)
            .ok_or_else(|| ExecutorError::Internal("Failed to derive proxy wallet".to_string()))?;
        let proxy_address_str = format!("{:?}", proxy_address);

        // Create SDK client and authenticate to verify credentials work
        let sdk_config = ClobConfig::builder()
            .use_server_time(true)
            .build();

        // Use SignatureType::Proxy for Magic/Google OAuth accounts (funds in proxy wallet)
        let _client = ClobClient::new("https://clob.polymarket.com", sdk_config)
            .map_err(|e| ExecutorError::Internal(format!("Failed to create CLOB client: {}", e)))?
            .authentication_builder(&signer)
            .signature_type(SignatureType::Proxy)
            .authenticate()
            .await
            .map_err(|e| ExecutorError::Internal(format!("Failed to authenticate: {}", e)))?;

        info!(eoa = %eoa_address_str, proxy = %proxy_address_str, "SDK client authenticated successfully");

        // Create shadow manager if enabled (config.enable_shadow controls it)
        let shadow_manager = if config.enable_shadow {
            shadow_config.map(|cfg| {
                Arc::new(ShadowManager::new(cfg.clone(), proxy_address_str.clone()))
            })
        } else {
            None
        };

        // Create allowance manager for auto-replenishment
        // Pass global_state so it respects risk limits (won't replenish if circuit breaker tripped)
        let allowance_config = AllowanceConfig::new(available_balance);
        let allowance_manager: SharedAllowanceManager = Arc::new(
            AllowanceManager::new(private_key_hex.clone(), allowance_config, global_state)
        );

        // Ensure allowances are set, then start background monitoring
        // This blocks until initial allowances are verified/set
        info!(
            target_allowance = %available_balance,
            "Checking and setting allowances before trading..."
        );
        let allowance_task = Some(allowance_manager.clone().ensure_and_start_monitoring().await);

        Ok(Self {
            signer,
            private_key_hex,
            shadow_manager,
            allowance_manager: Some(allowance_manager),
            allowance_task,
            balance: Arc::new(RwLock::new(initial_balance)),
            order_timeout_ms: config.order_timeout_ms,
            orders: Arc::new(RwLock::new(HashMap::new())),
            request_to_order: Arc::new(RwLock::new(HashMap::new())),
            pending: Arc::new(RwLock::new(Vec::new())),
            wallet_address: proxy_address_str,
        })
    }

    /// Create an authenticated client for a single operation.
    ///
    /// Returns the authenticated client. Each operation creates a fresh client
    /// since the SDK handles connection pooling internally.
    async fn create_authenticated_client(&self) -> Result<ClobClient<polymarket_client_sdk::auth::state::Authenticated<polymarket_client_sdk::auth::Normal>>, ExecutorError> {
        let config = ClobConfig::builder()
            .use_server_time(true)
            .build();

        // Use SignatureType::Proxy for Magic/Google OAuth accounts (funds in proxy wallet)
        ClobClient::new("https://clob.polymarket.com", config)
            .map_err(|e| ExecutorError::Internal(format!("Failed to create CLOB client: {}", e)))?
            .authentication_builder(&self.signer)
            .signature_type(SignatureType::Proxy)
            .authenticate()
            .await
            .map_err(|e| ExecutorError::Internal(format!("Failed to authenticate: {}", e)))
    }

    /// Get current balance.
    #[allow(dead_code)]
    pub async fn balance(&self) -> Decimal {
        *self.balance.read().await
    }

    /// Fetch actual balance from Polymarket.
    ///
    /// This calls the SDK's balance_allowance endpoint after refreshing the
    /// on-chain state via update_balance_allowance.
    pub async fn fetch_balance(&self) -> Result<Decimal, ExecutorError> {
        let client = self.create_authenticated_client().await?;

        // First, refresh the balance/allowance cache for proxy wallets
        let request = BalanceAllowanceRequest::default();
        if let Err(e) = client.update_balance_allowance(request.clone()).await {
            warn!(error = %e, "Failed to update balance/allowance cache (non-fatal)");
        }

        // Now query the balance
        match client.balance_allowance(request).await {
            Ok(response) => {
                info!(
                    balance = %response.balance,
                    allowances = ?response.allowances,
                    "Fetched balance from Polymarket"
                );
                // Update local balance cache
                let mut balance = self.balance.write().await;
                *balance = response.balance;
                Ok(response.balance)
            }
            Err(e) => {
                warn!(error = %e, "Failed to fetch balance from Polymarket");
                // Return cached balance as fallback
                Ok(*self.balance.read().await)
            }
        }
    }

    /// Get wallet address.
    pub fn wallet_address(&self) -> &str {
        &self.wallet_address
    }

    /// Convert our Side to SDK Side.
    fn to_sdk_side(side: Side) -> SdkSide {
        match side {
            Side::Buy => SdkSide::Buy,
            Side::Sell => SdkSide::Sell,
        }
    }
}

#[async_trait]
impl Executor for LiveSdkExecutor {
    async fn place_order(&mut self, request: OrderRequest) -> Result<OrderResult, ExecutorError> {
        debug!(
            request_id = %request.request_id,
            event_id = %request.event_id,
            token_id = %request.token_id,
            side = ?request.side,
            size = %request.size,
            price = ?request.price,
            "Placing order via SDK"
        );

        // Validate order type
        if request.order_type == OrderType::Market {
            return Err(ExecutorError::InvalidOrder(
                "Market orders not supported - use limit orders".to_string()
            ));
        }

        // Skip local balance check - let the server validate
        // The balance_allowance API may not reflect the actual trading balance
        // for proxy wallets. The server will reject if truly insufficient.

        let price = request.price.ok_or_else(|| {
            ExecutorError::InvalidOrder("Limit price required".to_string())
        })?;

        // Round price to 0.01 tick grid (Polymarket requires 2 decimal places max)
        let price = price.round_dp(2);

        // Round size to whole shares and enforce minimum of 5
        let size = request.size.round_dp(0);
        if size < Decimal::from(5) {
            return Err(ExecutorError::InvalidOrder(
                format!("Size {} is below minimum of 5 shares", size)
            ));
        }

        // Parse token_id as U256
        let token_id = alloy::primitives::U256::from_str(&request.token_id)
            .map_err(|e| ExecutorError::InvalidOrder(format!("Invalid token_id: {}", e)))?;

        // Create authenticated client
        let client = self.create_authenticated_client().await?;

        // Build limit order using SDK with rounded price/size
        // POST_ONLY ensures we're a maker - order rejected if it would fill immediately
        let order = client
            .limit_order()
            .token_id(token_id)
            .order_type(SdkOrderType::GTC)
            .price(price)
            .size(size)
            .side(Self::to_sdk_side(request.side))
            .post_only(true)
            .build()
            .await
            .map_err(|e| ExecutorError::Internal(format!("Failed to build order: {}", e)))?;

        // Sign and submit
        let signed_order = client
            .sign(&self.signer, order)
            .await
            .map_err(|e| ExecutorError::Internal(format!("Failed to sign order: {}", e)))?;

        let response = client
            .post_order(signed_order)
            .await
            .map_err(|e| ExecutorError::Rejected(format!("Order rejected: {}", e)))?;

        let order_id = response.order_id;
        info!(request_id = %request.request_id, order_id = %order_id, "Order submitted via SDK");

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

        let pending_order = PendingOrder {
            request_id: request.request_id.clone(),
            order_id: order_id.clone(),
            timestamp: Utc::now(),
        };

        {
            let mut pending = self.pending.write().await;
            pending.push(pending_order.clone());
        }

        Ok(OrderResult::Pending(pending_order))
    }

    async fn cancel_order(&mut self, request_id: &str) -> Result<OrderCancellation, ExecutorError> {
        let order_id = {
            let mapping = self.request_to_order.read().await;
            mapping.get(request_id).cloned()
        };

        let order_id = order_id.ok_or_else(|| {
            ExecutorError::Internal(format!("Order not found for request: {}", request_id))
        })?;

        let client = self.create_authenticated_client().await?;
        client
            .cancel_order(&order_id)
            .await
            .map_err(|e| ExecutorError::Internal(format!("Failed to cancel order: {}", e)))?;

        // Remove from pending
        {
            let mut pending = self.pending.write().await;
            pending.retain(|p| p.order_id != order_id);
        }

        Ok(OrderCancellation {
            request_id: request_id.to_string(),
            order_id,
            filled_size: Decimal::ZERO,
            unfilled_size: Decimal::ZERO, // TODO: Get from response
            timestamp: Utc::now(),
        })
    }

    async fn order_status(&self, order_id: &str) -> Option<OrderResult> {
        let orders = self.orders.read().await;
        orders.get(order_id).map(|tracked| {
            match tracked.status {
                TrackedOrderStatus::Pending => OrderResult::Pending(PendingOrder {
                    request_id: tracked.request.request_id.clone(),
                    order_id: tracked.order_id.clone(),
                    timestamp: Utc::now(),
                }),
                TrackedOrderStatus::Filled => OrderResult::Filled(OrderFill {
                    request_id: tracked.request.request_id.clone(),
                    order_id: tracked.order_id.clone(),
                    size: tracked.filled_size,
                    price: tracked.avg_fill_price,
                    fee: tracked.total_fee,
                    timestamp: Utc::now(),
                }),
                TrackedOrderStatus::PartiallyFilled => OrderResult::PartialFill(PartialOrderFill {
                    request_id: tracked.request.request_id.clone(),
                    order_id: tracked.order_id.clone(),
                    requested_size: tracked.request.size,
                    filled_size: tracked.filled_size,
                    avg_price: tracked.avg_fill_price,
                    fee: tracked.total_fee,
                    timestamp: Utc::now(),
                }),
                TrackedOrderStatus::Cancelled | TrackedOrderStatus::Expired => {
                    OrderResult::Cancelled(OrderCancellation {
                        request_id: tracked.request.request_id.clone(),
                        order_id: tracked.order_id.clone(),
                        filled_size: tracked.filled_size,
                        unfilled_size: tracked.request.size - tracked.filled_size,
                        timestamp: Utc::now(),
                    })
                }
            }
        })
    }

    fn pending_orders(&self) -> Vec<PendingOrder> {
        // This is synchronous, so we can't await. Return empty for now.
        Vec::new()
    }

    fn available_balance(&self) -> Decimal {
        // Synchronous access - would need restructuring
        Decimal::ZERO
    }

    async fn shutdown(&mut self) {
        // Stop allowance manager first
        if let Some(ref manager) = self.allowance_manager {
            info!("Stopping allowance manager");
            manager.stop().await;
        }
        if let Some(task) = self.allowance_task.take() {
            task.abort();
        }

        // Cancel all pending orders
        let order_ids: Vec<String> = {
            let orders = self.orders.read().await;
            orders.iter()
                .filter(|(_, o)| o.status == TrackedOrderStatus::Pending)
                .map(|(id, _)| id.clone())
                .collect()
        };

        if order_ids.is_empty() {
            return;
        }

        if let Ok(client) = self.create_authenticated_client().await {
            for order_id in order_ids {
                if let Err(e) = client.cancel_order(&order_id).await {
                    warn!(order_id = %order_id, error = %e, "Failed to cancel order on shutdown");
                }
            }
        }
    }

}

impl LiveSdkExecutor {
    /// Get the allowance manager for manual control.
    pub fn allowance_manager(&self) -> Option<&SharedAllowanceManager> {
        self.allowance_manager.as_ref()
    }
}
