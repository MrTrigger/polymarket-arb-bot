//! Set token allowances for Polymarket trading.
//!
//! Run with:
//! ```sh
//! # Set 500 USDC allowance:
//! cargo run -p poly-bot --example set_allowances -- 500
//!
//! # Set unlimited allowance:
//! cargo run -p poly-bot --example set_allowances -- unlimited
//! ```
//!
//! Loads private key from config/bot.toml automatically.

use std::env;
use std::str::FromStr;
use std::time::Duration;

use alloy::primitives::U256;
use alloy::providers::ProviderBuilder;
use alloy::signers::Signer;
use alloy::signers::local::LocalSigner;
use alloy::sol;
use polymarket_client_sdk::types::{Address, address};
use polymarket_client_sdk::{POLYGON, contract_config};

use poly_bot::config::BotConfig;

const RPC_URL: &str = "https://polygon-rpc.com";
const USDC_ADDRESS: Address = address!("0x2791Bca1f2de4661ED88A30C99A7a9449Aa84174");
const USDC_DECIMALS: u8 = 6; // USDC has 6 decimals

/// Retry configuration for rate-limited public RPC
const MAX_RETRIES: u32 = 6;
const RETRY_BASE_DELAY_MS: u64 = 12000;

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

fn parse_allowance_amount(arg: &str) -> U256 {
    if arg.to_lowercase() == "unlimited" {
        U256::MAX
    } else {
        // Parse as USDC amount (e.g., "500" = 500 USDC = 500_000_000 units)
        let amount: u64 = arg.parse().expect("Invalid amount - use a number or 'unlimited'");
        U256::from(amount) * U256::from(10u64.pow(USDC_DECIMALS as u32))
    }
}

fn format_usdc(amount: U256) -> String {
    if amount == U256::MAX {
        "unlimited".to_string()
    } else {
        let divisor = U256::from(10u64.pow(USDC_DECIMALS as u32));
        let usdc = amount / divisor;
        format!("{} USDC", usdc)
    }
}

fn is_rate_limit_error(err: &str) -> bool {
    err.contains("-32090") || err.contains("rate limit") || err.contains("Too many requests")
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = env::args().collect();

    if args.len() < 2 {
        eprintln!("Usage: set_allowances <amount>");
        eprintln!("  amount: USDC amount (e.g., 500) or 'unlimited'");
        eprintln!();
        eprintln!("Example:");
        eprintln!("  cargo run -p poly-bot --example set_allowances -- 500");
        eprintln!("  cargo run -p poly-bot --example set_allowances -- unlimited");
        std::process::exit(1);
    }

    let allowance_amount = parse_allowance_amount(&args[1]);
    println!("Setting allowance to: {}", format_usdc(allowance_amount));

    // Load private key from config file + env var override
    let mut bot_config = BotConfig::from_file("config/bot.toml")?;
    bot_config.apply_env_overrides();
    let private_key = bot_config.wallet.private_key
        .ok_or_else(|| anyhow::anyhow!("No private key - set POLY_PRIVATE_KEY env var"))?;

    let signer = LocalSigner::from_str(&private_key)?.with_chain_id(Some(POLYGON));

    let provider = ProviderBuilder::new()
        .wallet(signer.clone())
        .connect(RPC_URL)
        .await?;

    let owner = signer.address();
    println!("Wallet (EOA): {:?}", owner);

    let config = contract_config(POLYGON, false).unwrap();
    let neg_risk_config = contract_config(POLYGON, true).unwrap();

    let targets: Vec<(&str, Address)> = vec![
        ("CTF Exchange", config.exchange),
        ("Neg Risk CTF Exchange", neg_risk_config.exchange),
        ("Neg Risk Adapter", neg_risk_config.neg_risk_adapter.unwrap()),
    ];

    let token = IERC20::new(USDC_ADDRESS, provider.clone());
    let ctf = IERC1155::new(config.conditional_tokens, provider.clone());

    println!("\n=== Current Allowances ===");
    for (name, target) in &targets {
        // Retry logic for reading allowances
        let mut allowance = U256::ZERO;
        let mut approved = false;

        for attempt in 0..MAX_RETRIES {
            if attempt > 0 {
                let delay = RETRY_BASE_DELAY_MS * 2u64.pow(attempt - 1);
                println!("  Rate limited, retrying in {}s...", delay / 1000);
                tokio::time::sleep(Duration::from_millis(delay)).await;
            }

            match token.allowance(owner, *target).call().await {
                Ok(a) => {
                    allowance = a;
                    break;
                }
                Err(e) if is_rate_limit_error(&e.to_string()) => continue,
                Err(e) => return Err(e.into()),
            }
        }

        for attempt in 0..MAX_RETRIES {
            if attempt > 0 {
                let delay = RETRY_BASE_DELAY_MS * 2u64.pow(attempt - 1);
                println!("  Rate limited, retrying in {}s...", delay / 1000);
                tokio::time::sleep(Duration::from_millis(delay)).await;
            }

            match ctf.isApprovedForAll(owner, *target).call().await {
                Ok(a) => {
                    approved = a;
                    break;
                }
                Err(e) if is_rate_limit_error(&e.to_string()) => continue,
                Err(e) => return Err(e.into()),
            }
        }

        println!("{}: USDC={}, CTF={}", name, format_usdc(allowance), approved);
    }

    println!("\n=== Setting Approvals (with retry for rate limits) ===");
    for (name, target) in &targets {
        println!("Approving {}...", name);

        // Approve USDC with retry
        for attempt in 0..MAX_RETRIES {
            if attempt > 0 {
                let delay = RETRY_BASE_DELAY_MS * 2u64.pow(attempt - 1);
                println!("  Rate limited, retrying in {}s...", delay / 1000);
                tokio::time::sleep(Duration::from_millis(delay)).await;
            }

            match token.approve(*target, allowance_amount).send().await {
                Ok(tx) => {
                    match tx.watch().await {
                        Ok(receipt) => {
                            println!("  USDC approved: {:?}", receipt);
                            break;
                        }
                        Err(e) if is_rate_limit_error(&e.to_string()) => continue,
                        Err(e) => return Err(e.into()),
                    }
                }
                Err(e) if is_rate_limit_error(&e.to_string()) => continue,
                Err(e) => return Err(e.into()),
            }
        }

        // Wait between operations to avoid rate limits
        println!("  Waiting 15s before next approval...");
        tokio::time::sleep(Duration::from_secs(15)).await;

        // Approve CTF with retry
        for attempt in 0..MAX_RETRIES {
            if attempt > 0 {
                let delay = RETRY_BASE_DELAY_MS * 2u64.pow(attempt - 1);
                println!("  Rate limited, retrying in {}s...", delay / 1000);
                tokio::time::sleep(Duration::from_millis(delay)).await;
            }

            match ctf.setApprovalForAll(*target, true).send().await {
                Ok(tx) => {
                    match tx.watch().await {
                        Ok(receipt) => {
                            println!("  CTF approved: {:?}", receipt);
                            break;
                        }
                        Err(e) if is_rate_limit_error(&e.to_string()) => continue,
                        Err(e) => return Err(e.into()),
                    }
                }
                Err(e) if is_rate_limit_error(&e.to_string()) => continue,
                Err(e) => return Err(e.into()),
            }
        }

        // Wait between targets
        println!("  Waiting 15s before next target...");
        tokio::time::sleep(Duration::from_secs(15)).await;
    }

    println!("\n=== Verifying ===");
    for (name, target) in &targets {
        let mut allowance = U256::ZERO;
        let mut approved = false;

        for attempt in 0..MAX_RETRIES {
            if attempt > 0 {
                let delay = RETRY_BASE_DELAY_MS * 2u64.pow(attempt - 1);
                tokio::time::sleep(Duration::from_millis(delay)).await;
            }

            match token.allowance(owner, *target).call().await {
                Ok(a) => {
                    allowance = a;
                    break;
                }
                Err(e) if is_rate_limit_error(&e.to_string()) => continue,
                Err(e) => return Err(e.into()),
            }
        }

        for attempt in 0..MAX_RETRIES {
            if attempt > 0 {
                let delay = RETRY_BASE_DELAY_MS * 2u64.pow(attempt - 1);
                tokio::time::sleep(Duration::from_millis(delay)).await;
            }

            match ctf.isApprovedForAll(owner, *target).call().await {
                Ok(a) => {
                    approved = a;
                    break;
                }
                Err(e) if is_rate_limit_error(&e.to_string()) => continue,
                Err(e) => return Err(e.into()),
            }
        }

        println!("{}: USDC={}, CTF={}", name, format_usdc(allowance), approved);
    }

    println!("\nDone! You can now trade via API.");
    Ok(())
}
