//! Automatic allowance management for Polymarket trading.
//!
//! This module monitors on-chain allowances and replenishes them when they
//! fall below a threshold, ensuring uninterrupted trading.
//!
//! ## Risk Integration
//!
//! The allowance manager respects the bot's risk state - it will NOT replenish
//! allowances if:
//! - Trading is disabled
//! - Circuit breaker has tripped (e.g., max daily loss hit)
//!
//! This prevents the bot from setting up allowances for trading that shouldn't happen.

use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use alloy::primitives::U256;
use alloy::providers::ProviderBuilder;
use alloy::signers::Signer;
use alloy::signers::local::LocalSigner;
use alloy::sol;
use polymarket_client_sdk::types::{Address, address};
use polymarket_client_sdk::{POLYGON, contract_config};
use rust_decimal::Decimal;
use tokio::sync::RwLock;
use tokio::time::interval;
use tracing::{debug, info, warn, error};

use crate::state::GlobalState;

/// USDC.e contract on Polygon.
const USDC_ADDRESS: Address = address!("0x2791Bca1f2de4661ED88A30C99A7a9449Aa84174");
const USDC_DECIMALS: u8 = 6;

/// RPC endpoint for Polygon.
const RPC_URL: &str = "https://polygon-rpc.com";

/// Replenish threshold: when allowance drops below this % of target, replenish.
const REPLENISH_THRESHOLD_PCT: u64 = 20;

/// Check interval in seconds.
const CHECK_INTERVAL_SECS: u64 = 60;

/// Maximum retries for RPC calls.
const MAX_RETRIES: u32 = 6;

/// Base delay between retries in milliseconds (public RPC needs longer delays).
const RETRY_BASE_DELAY_MS: u64 = 12000;

/// Timeout for sending a transaction to the RPC.
const TX_SEND_TIMEOUT_SECS: u64 = 30;

/// Timeout for waiting for transaction confirmation.
const TX_CONFIRM_TIMEOUT_SECS: u64 = 120;

sol! {
    #[sol(rpc)]
    interface IERC20 {
        function approve(address spender, uint256 value) external returns (bool);
        function allowance(address owner, address spender) external view returns (uint256);
    }

    #[sol(rpc)]
    interface IERC1155 {
        function setApprovalForAll(address operator, bool approved) external;
        function isApprovedForAll(address account, address operator) external view returns (bool);
    }
}

/// Contract target for allowance management.
#[derive(Debug, Clone)]
pub struct AllowanceTarget {
    pub name: &'static str,
    pub address: Address,
}

/// Current allowance state.
#[derive(Debug, Clone)]
pub struct AllowanceState {
    pub usdc_allowance: U256,
    pub ctf_approved: bool,
}

/// Allowance manager configuration.
#[derive(Debug, Clone)]
pub struct AllowanceConfig {
    /// Target USDC allowance in USDC units (e.g., 500 = 500 USDC).
    pub target_usdc: Decimal,
    /// Replenish when allowance drops below this percentage of target.
    pub replenish_threshold_pct: u64,
    /// How often to check allowances (seconds).
    pub check_interval_secs: u64,
}

impl Default for AllowanceConfig {
    fn default() -> Self {
        Self {
            target_usdc: Decimal::new(500, 0), // 500 USDC
            replenish_threshold_pct: REPLENISH_THRESHOLD_PCT,
            check_interval_secs: CHECK_INTERVAL_SECS,
        }
    }
}

impl AllowanceConfig {
    pub fn new(target_usdc: Decimal) -> Self {
        Self {
            target_usdc,
            ..Default::default()
        }
    }

    /// Convert target USDC to U256 with proper decimals.
    pub fn target_as_u256(&self) -> U256 {
        let amount = self.target_usdc.to_string().parse::<u64>().unwrap_or(500);
        U256::from(amount) * U256::from(10u64.pow(USDC_DECIMALS as u32))
    }

    /// Calculate replenish threshold in U256.
    pub fn threshold_as_u256(&self) -> U256 {
        self.target_as_u256() * U256::from(self.replenish_threshold_pct) / U256::from(100u64)
    }
}

/// Allowance manager handles automatic allowance replenishment.
pub struct AllowanceManager {
    /// Private key for signing approve transactions.
    private_key: String,
    /// Configuration.
    config: AllowanceConfig,
    /// Targets that need allowances.
    targets: Vec<AllowanceTarget>,
    /// Last known allowance states.
    states: Arc<RwLock<Vec<(AllowanceTarget, AllowanceState)>>>,
    /// Whether manager is running.
    running: Arc<RwLock<bool>>,
    /// Global state for checking if trading is allowed.
    global_state: Option<Arc<GlobalState>>,
}

impl AllowanceManager {
    /// Create a new allowance manager.
    ///
    /// # Arguments
    /// * `private_key` - Private key for signing approve transactions
    /// * `config` - Allowance configuration
    /// * `global_state` - Optional global state for checking if trading is allowed
    pub fn new(
        private_key: String,
        config: AllowanceConfig,
        global_state: Option<Arc<GlobalState>>,
    ) -> Self {
        // Get contract addresses from SDK
        let regular_config = contract_config(POLYGON, false).unwrap();
        let neg_risk_config = contract_config(POLYGON, true).unwrap();

        let targets = vec![
            AllowanceTarget {
                name: "CTF Exchange",
                address: regular_config.exchange,
            },
            AllowanceTarget {
                name: "Neg Risk CTF Exchange",
                address: neg_risk_config.exchange,
            },
            AllowanceTarget {
                name: "Neg Risk Adapter",
                address: neg_risk_config.neg_risk_adapter.unwrap(),
            },
        ];

        Self {
            private_key,
            config,
            targets,
            states: Arc::new(RwLock::new(Vec::new())),
            running: Arc::new(RwLock::new(false)),
            global_state,
        }
    }

    /// Create from available_balance config value.
    pub fn from_available_balance(
        private_key: String,
        available_balance: Decimal,
        global_state: Option<Arc<GlobalState>>,
    ) -> Self {
        Self::new(private_key, AllowanceConfig::new(available_balance), global_state)
    }

    /// Check if replenishment is allowed based on risk state.
    ///
    /// Returns false if:
    /// - Trading is disabled
    /// - Circuit breaker has tripped (e.g., max daily loss reached)
    fn can_replenish(&self) -> bool {
        match &self.global_state {
            Some(state) => state.can_trade(),
            None => true, // No state = allow (for standalone use like the example script)
        }
    }

    /// Check current allowances for all targets with retry logic for RPC rate limits.
    pub async fn check_allowances(&self) -> Result<Vec<(AllowanceTarget, AllowanceState)>, String> {
        let signer = LocalSigner::from_str(&self.private_key)
            .map_err(|e| format!("Invalid private key: {}", e))?
            .with_chain_id(Some(POLYGON));

        let provider = ProviderBuilder::new()
            .wallet(signer.clone())
            .connect(RPC_URL)
            .await
            .map_err(|e| format!("Failed to connect to RPC: {}", e))?;

        let owner = signer.address();
        let regular_config = contract_config(POLYGON, false).unwrap();

        let token = IERC20::new(USDC_ADDRESS, provider.clone());
        let ctf = IERC1155::new(regular_config.conditional_tokens, provider.clone());

        let mut results = Vec::new();

        for target in &self.targets {
            // Get USDC allowance with retry logic
            let usdc_allowance = self.call_with_retry(|| async {
                token.allowance(owner, target.address)
                    .call()
                    .await
                    .map_err(|e| e.to_string())
            }, "USDC allowance").await?;

            // Get CTF approval with retry logic
            let ctf_approved = self.call_with_retry(|| async {
                ctf.isApprovedForAll(owner, target.address)
                    .call()
                    .await
                    .map_err(|e| e.to_string())
            }, "CTF approval").await?;

            results.push((
                target.clone(),
                AllowanceState {
                    usdc_allowance,
                    ctf_approved,
                },
            ));
        }

        // Update cached states
        {
            let mut states = self.states.write().await;
            *states = results.clone();
        }

        Ok(results)
    }

    /// Helper to retry RPC calls with exponential backoff on rate limit errors.
    async fn call_with_retry<F, Fut, T>(&self, f: F, desc: &str) -> Result<T, String>
    where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = Result<T, String>>,
    {
        let mut last_error = String::new();
        for attempt in 0..MAX_RETRIES {
            if attempt > 0 {
                let delay_ms = RETRY_BASE_DELAY_MS * 2u64.pow(attempt - 1);
                info!(
                    call = desc,
                    attempt = attempt + 1,
                    delay_ms = delay_ms,
                    "Retrying RPC call after rate limit"
                );
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            }

            match f().await {
                Ok(result) => return Ok(result),
                Err(e) => {
                    // Check if this is a rate limit error
                    if e.contains("-32090") || e.contains("rate limit") || e.contains("Too many requests") {
                        warn!(
                            call = desc,
                            attempt = attempt + 1,
                            error = %e,
                            "RPC rate limit hit, will retry"
                        );
                        last_error = e;
                        continue;
                    }
                    // Not a rate limit error, fail immediately
                    return Err(format!("Failed to get {}: {}", desc, e));
                }
            }
        }
        Err(format!("Failed to get {} after {} retries: {}", desc, MAX_RETRIES, last_error))
    }

    /// Replenish allowances for all targets that are below threshold.
    ///
    /// Returns Ok(true) if any allowances were replenished, Ok(false) if none needed,
    /// or Err if replenishment was blocked by risk limits or failed.
    ///
    /// Note: This respects risk limits. Use `replenish_if_needed_force` for initial setup.
    pub async fn replenish_if_needed(&self) -> Result<bool, String> {
        // Check risk state before replenishing
        if !self.can_replenish() {
            debug!("Allowance replenishment skipped: trading disabled or circuit breaker tripped");
            return Ok(false);
        }

        self.replenish_if_needed_inner(false).await
    }

    /// Force replenish allowances, bypassing risk checks.
    ///
    /// Use this for initial setup before trading starts, when GlobalState.can_trade()
    /// may return false but we still need to set up allowances.
    pub async fn replenish_if_needed_force(&self) -> Result<bool, String> {
        self.replenish_if_needed_inner(true).await
    }

    /// Internal implementation of allowance replenishment.
    ///
    /// # Arguments
    /// * `force` - If true, bypass risk checks (used for initial setup)
    async fn replenish_if_needed_inner(&self, force: bool) -> Result<bool, String> {
        let states = self.check_allowances().await?;
        let threshold = self.config.threshold_as_u256();
        let target = self.config.target_as_u256();

        let mut replenished = false;

        for (target_info, state) in &states {
            let needs_usdc = state.usdc_allowance < threshold;
            let needs_ctf = !state.ctf_approved;

            if needs_usdc || needs_ctf {
                // Double-check risk state before each approval (in case it changed)
                // Skip this check in force mode (initial setup)
                if !force && !self.can_replenish() {
                    warn!("Allowance replenishment aborted mid-way: risk limits triggered");
                    return Ok(replenished);
                }

                info!(
                    target = target_info.name,
                    usdc_allowance = %state.usdc_allowance,
                    ctf_approved = state.ctf_approved,
                    threshold = %threshold,
                    "Replenishing allowances"
                );

                self.approve_target(target_info, target, needs_usdc, needs_ctf).await?;
                replenished = true;
            }
        }

        Ok(replenished)
    }

    /// Approve a specific target with retry logic for RPC rate limits.
    async fn approve_target(
        &self,
        target: &AllowanceTarget,
        usdc_amount: U256,
        approve_usdc: bool,
        approve_ctf: bool,
    ) -> Result<(), String> {
        let signer = LocalSigner::from_str(&self.private_key)
            .map_err(|e| format!("Invalid private key: {}", e))?
            .with_chain_id(Some(POLYGON));

        let provider = ProviderBuilder::new()
            .wallet(signer.clone())
            .connect(RPC_URL)
            .await
            .map_err(|e| format!("Failed to connect to RPC: {}", e))?;

        let regular_config = contract_config(POLYGON, false).unwrap();

        if approve_usdc {
            let token = IERC20::new(USDC_ADDRESS, provider.clone());

            // Retry with exponential backoff for rate limit errors
            let mut last_error = None;
            for attempt in 0..MAX_RETRIES {
                if attempt > 0 {
                    let delay_ms = RETRY_BASE_DELAY_MS * 2u64.pow(attempt - 1);
                    info!(
                        target = target.name,
                        attempt = attempt + 1,
                        delay_ms = delay_ms,
                        "Retrying USDC approve after rate limit"
                    );
                    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                }

                // Send transaction with timeout
                let send_result = tokio::time::timeout(
                    Duration::from_secs(TX_SEND_TIMEOUT_SECS),
                    token.approve(target.address, usdc_amount).send(),
                )
                .await;

                match send_result {
                    Ok(Ok(tx)) => {
                        // Wait for confirmation with timeout
                        let confirm_result = tokio::time::timeout(
                            Duration::from_secs(TX_CONFIRM_TIMEOUT_SECS),
                            tx.watch(),
                        )
                        .await;

                        match confirm_result {
                            Ok(Ok(receipt)) => {
                                info!(
                                    target = target.name,
                                    amount = %usdc_amount,
                                    tx_hash = ?receipt,
                                    "USDC allowance set"
                                );
                                last_error = None;
                                break;
                            }
                            Ok(Err(e)) => {
                                last_error = Some(format!("Failed to confirm USDC approve tx: {}", e));
                            }
                            Err(_) => {
                                last_error = Some(format!(
                                    "USDC approve confirmation timed out after {}s",
                                    TX_CONFIRM_TIMEOUT_SECS
                                ));
                            }
                        }
                    }
                    Ok(Err(e)) => {
                        let err_str = e.to_string();
                        // Check if this is a rate limit error
                        if err_str.contains("-32090") || err_str.contains("rate limit") || err_str.contains("Too many requests") {
                            warn!(
                                target = target.name,
                                attempt = attempt + 1,
                                error = %e,
                                "RPC rate limit hit, will retry"
                            );
                            last_error = Some(format!("RPC rate limit: {}", e));
                            continue;
                        }
                        // Not a rate limit error, fail immediately
                        return Err(format!("Failed to send USDC approve tx: {}", e));
                    }
                    Err(_) => {
                        last_error = Some(format!(
                            "USDC approve send timed out after {}s",
                            TX_SEND_TIMEOUT_SECS
                        ));
                    }
                }
            }

            if let Some(err) = last_error {
                return Err(err);
            }
        }

        if approve_ctf {
            let ctf = IERC1155::new(regular_config.conditional_tokens, provider.clone());

            // Retry with exponential backoff for rate limit errors
            let mut last_error = None;
            for attempt in 0..MAX_RETRIES {
                if attempt > 0 {
                    let delay_ms = RETRY_BASE_DELAY_MS * 2u64.pow(attempt - 1);
                    info!(
                        target = target.name,
                        attempt = attempt + 1,
                        delay_ms = delay_ms,
                        "Retrying CTF approve after rate limit"
                    );
                    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                }

                // Send transaction with timeout
                let send_result = tokio::time::timeout(
                    Duration::from_secs(TX_SEND_TIMEOUT_SECS),
                    ctf.setApprovalForAll(target.address, true).send(),
                )
                .await;

                match send_result {
                    Ok(Ok(tx)) => {
                        // Wait for confirmation with timeout
                        let confirm_result = tokio::time::timeout(
                            Duration::from_secs(TX_CONFIRM_TIMEOUT_SECS),
                            tx.watch(),
                        )
                        .await;

                        match confirm_result {
                            Ok(Ok(receipt)) => {
                                info!(
                                    target = target.name,
                                    tx_hash = ?receipt,
                                    "CTF approval set"
                                );
                                last_error = None;
                                break;
                            }
                            Ok(Err(e)) => {
                                last_error = Some(format!("Failed to confirm CTF approve tx: {}", e));
                            }
                            Err(_) => {
                                last_error = Some(format!(
                                    "CTF approve confirmation timed out after {}s",
                                    TX_CONFIRM_TIMEOUT_SECS
                                ));
                            }
                        }
                    }
                    Ok(Err(e)) => {
                        let err_str = e.to_string();
                        // Check if this is a rate limit error
                        if err_str.contains("-32090") || err_str.contains("rate limit") || err_str.contains("Too many requests") {
                            warn!(
                                target = target.name,
                                attempt = attempt + 1,
                                error = %e,
                                "RPC rate limit hit, will retry"
                            );
                            last_error = Some(format!("RPC rate limit: {}", e));
                            continue;
                        }
                        // Not a rate limit error, fail immediately
                        return Err(format!("Failed to send CTF approve tx: {}", e));
                    }
                    Err(_) => {
                        last_error = Some(format!(
                            "CTF approve send timed out after {}s",
                            TX_SEND_TIMEOUT_SECS
                        ));
                    }
                }
            }

            if let Some(err) = last_error {
                return Err(err);
            }
        }

        Ok(())
    }

    /// Ensure allowances are set, then start background monitoring.
    ///
    /// This method:
    /// 1. Performs an initial allowance check/replenish (awaited, blocking)
    /// 2. Starts the background monitoring task
    ///
    /// Use this instead of `start_monitoring` to ensure allowances are set
    /// before the bot starts trading.
    ///
    /// Note: Uses force mode for initial setup, bypassing can_trade() check since
    /// trading isn't enabled yet at startup.
    pub async fn ensure_and_start_monitoring(self: Arc<Self>) -> tokio::task::JoinHandle<()> {
        // Do initial check synchronously (blocking) so caller knows allowances are set
        // Use force mode since trading may not be enabled yet at startup
        match self.replenish_if_needed_force().await {
            Ok(replenished) => {
                if replenished {
                    info!("Initial allowance setup completed");
                } else {
                    info!("Allowances already sufficient, no replenishment needed");
                }
            }
            Err(e) => {
                error!(error = %e, "Initial allowance setup failed - trading may fail");
            }
        }

        // Now start background monitoring
        self.start_monitoring()
    }

    /// Start background monitoring task.
    ///
    /// Returns a handle that can be used to stop the task.
    ///
    /// Note: Use `ensure_and_start_monitoring` if you need allowances to be
    /// set before continuing (e.g., before running a test trade).
    pub fn start_monitoring(self: Arc<Self>) -> tokio::task::JoinHandle<()> {
        let manager = self.clone();

        tokio::spawn(async move {
            {
                let mut running = manager.running.write().await;
                *running = true;
            }

            let mut check_interval = interval(Duration::from_secs(manager.config.check_interval_secs));

            loop {
                check_interval.tick().await;

                let running = *manager.running.read().await;
                if !running {
                    info!("Allowance manager stopping");
                    break;
                }

                match manager.replenish_if_needed().await {
                    Ok(replenished) => {
                        if replenished {
                            info!("Allowance replenishment completed");
                        } else {
                            debug!("Allowance check: all allowances sufficient");
                        }
                    }
                    Err(e) => {
                        warn!(error = %e, "Allowance check failed, will retry");
                    }
                }
            }
        })
    }

    /// Stop the monitoring task.
    pub async fn stop(&self) {
        let mut running = self.running.write().await;
        *running = false;
    }

    /// Get cached allowance states.
    pub async fn cached_states(&self) -> Vec<(AllowanceTarget, AllowanceState)> {
        self.states.read().await.clone()
    }

    /// Format allowance for display.
    pub fn format_usdc(amount: U256) -> String {
        if amount == U256::MAX {
            "unlimited".to_string()
        } else {
            let divisor = U256::from(10u64.pow(USDC_DECIMALS as u32));
            let usdc = amount / divisor;
            format!("{} USDC", usdc)
        }
    }
}

/// Shared allowance manager type.
pub type SharedAllowanceManager = Arc<AllowanceManager>;

/// Create a shared allowance manager.
///
/// # Arguments
/// * `private_key` - Private key for signing approve transactions
/// * `available_balance` - Target allowance amount
/// * `global_state` - Optional global state for checking risk limits
pub fn create_allowance_manager(
    private_key: String,
    available_balance: Decimal,
    global_state: Option<Arc<GlobalState>>,
) -> SharedAllowanceManager {
    Arc::new(AllowanceManager::from_available_balance(private_key, available_balance, global_state))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_allowance_config_default() {
        let config = AllowanceConfig::default();
        assert_eq!(config.target_usdc, Decimal::new(500, 0));
        assert_eq!(config.replenish_threshold_pct, 20);
        assert_eq!(config.check_interval_secs, 60);
    }

    #[test]
    fn test_allowance_config_target_as_u256() {
        let config = AllowanceConfig::new(Decimal::new(1000, 0)); // 1000 USDC
        let expected = U256::from(1000u64) * U256::from(1_000_000u64); // 1000 * 10^6
        assert_eq!(config.target_as_u256(), expected);
    }

    #[test]
    fn test_allowance_config_threshold() {
        let config = AllowanceConfig::new(Decimal::new(1000, 0)); // 1000 USDC
        let target = config.target_as_u256();
        let threshold = config.threshold_as_u256();
        // 20% of 1000 USDC = 200 USDC
        assert_eq!(threshold, target * U256::from(20u64) / U256::from(100u64));
    }

    #[test]
    fn test_format_usdc() {
        // 500 USDC = 500_000_000 units
        let amount = U256::from(500_000_000u64);
        assert_eq!(AllowanceManager::format_usdc(amount), "500 USDC");

        // Unlimited
        assert_eq!(AllowanceManager::format_usdc(U256::MAX), "unlimited");
    }
}
