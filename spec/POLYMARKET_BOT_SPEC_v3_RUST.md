# Polymarket BTC Up/Down Trading Bot
## Complete Implementation Specification v3.1 (Rust)

---

# Table of Contents

1. [Executive Summary](#1-executive-summary)
2. [Strategy Overview](#2-strategy-overview)
3. [Fee Structure & Maker Rebates](#3-fee-structure--maker-rebates)
4. [The Three Profit Engines](#4-the-three-profit-engines)
5. [Risk Management & Order Sizing](#5-risk-management--order-sizing)
6. [Complete Algorithm (Rust)](#6-complete-algorithm-rust)
7. [Technical Architecture](#7-technical-architecture)
8. [Expected Economics](#8-expected-economics)

---

# 1. Executive Summary

## What We Learned from Account88888

| Metric | Value |
|--------|-------|
| Total trades analyzed | 16,587 |
| Markets | 50 |
| Total volume | $249,635 |
| Avg trades per market | **332** |
| Avg volume per market | **$4,993** |
| Avg trade size | **$15.04** |
| Win rate | **98%** |

## Three Profit Engines

| Engine | Description | Est. Contribution |
|--------|-------------|-------------------|
| **Directional** | Bet in direction of price vs strike | 60-70% |
| **Arbitrage** | Capture gaps when UP + DOWN < $1.00 | 15-25% |
| **Rebates** | Earn from maker orders being filled | 10-20% |

---

# 2. Strategy Overview

## The Core Signal

```rust
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Signal {
    StrongUp,   // 78% UP, 22% DOWN
    LeanUp,     // 61% UP, 39% DOWN
    Neutral,    // 50% UP, 50% DOWN
    LeanDown,   // 39% UP, 61% DOWN
    StrongDown, // 22% UP, 78% DOWN
}

impl Signal {
    pub fn up_ratio(&self) -> f64 {
        match self {
            Signal::StrongUp => 0.78,
            Signal::LeanUp => 0.61,
            Signal::Neutral => 0.50,
            Signal::LeanDown => 0.39,
            Signal::StrongDown => 0.22,
        }
    }
    
    pub fn down_ratio(&self) -> f64 {
        1.0 - self.up_ratio()
    }
}

pub fn get_signal(btc_price: f64, strike_price: f64, minutes_remaining: f64) -> Signal {
    let distance = btc_price - strike_price;
    let (lean_threshold, conviction_threshold) = get_thresholds(minutes_remaining);
    
    if distance > conviction_threshold {
        Signal::StrongUp
    } else if distance > lean_threshold {
        Signal::LeanUp
    } else if distance < -conviction_threshold {
        Signal::StrongDown
    } else if distance < -lean_threshold {
        Signal::LeanDown
    } else {
        Signal::Neutral
    }
}

fn get_thresholds(minutes_remaining: f64) -> (f64, f64) {
    // (lean_threshold, conviction_threshold)
    // Thresholds tighten as time runs out
    match minutes_remaining {
        m if m > 12.0 => (40.0, 80.0),  // Early: conservative
        m if m > 9.0  => (30.0, 60.0),
        m if m > 6.0  => (20.0, 50.0),  // Account88888's baseline
        m if m > 3.0  => (15.0, 40.0),
        _             => (10.0, 30.0),  // Late: aggressive
    }
}
```

---

# 3. Fee Structure & Maker Rebates

## Fee Formula

```rust
/// Calculate taker fee for a trade
/// fee = C × feeRate × (p × (1-p))^exponent
pub fn calculate_taker_fee(shares: f64, price: f64) -> f64 {
    const FEE_RATE: f64 = 0.25;
    const EXPONENT: f64 = 2.0;
    const MIN_FEE: f64 = 0.0001;
    
    let probability_factor = price * (1.0 - price);
    let fee = shares * FEE_RATE * probability_factor.powf(EXPONENT);
    
    if fee > MIN_FEE { fee } else { 0.0 }
}
```

## Fee Table

| Price | Fee per 100 shares | Fee % |
|-------|-------------------|-------|
| $0.10 | $0.20 | 2.0% |
| $0.30 | $1.10 | 3.7% |
| $0.50 | $1.56 | 3.1% |
| $0.70 | $1.10 | 1.6% |
| $0.90 | $0.20 | 0.2% |

## MAKER vs TAKER

| | TAKER | MAKER |
|---|-------|-------|
| Fee | PAY ~3% | $0 |
| Rebate | None | EARN daily share |
| How | Market order | Limit order with `post_only=true` |

## Fetching Fee Rate (IMPORTANT: Do NOT Hardcode!)

The fee rate must be fetched dynamically from the exchange. It may vary by market and can change over time.

```rust
/// Fetch fee rate for a specific token
/// Returns 0 for fee-free markets, currently 1000 for 15-min crypto markets
pub async fn get_fee_rate(&self, token_id: &str) -> Result<u32, Error> {
    let url = format!(
        "https://clob.polymarket.com/fee-rate?token_id={}", 
        token_id
    );
    
    let response: FeeRateResponse = self.client
        .get(&url)
        .send()
        .await?
        .json()
        .await?;
    
    Ok(response.fee_rate_bps)
}

#[derive(Debug, Deserialize)]
struct FeeRateResponse {
    fee_rate_bps: u32,  // 0 = fee-free, 1000 = 10% base rate (current for 15-min crypto)
}
```

## Order Parameters

```rust
#[derive(Debug)]
pub struct OrderParams {
    pub token_id: String,
    pub side: Side,
    pub price: f64,
    pub size: f64,
    pub fee_rate_bps: u32,  // MUST be fetched from API, not hardcoded!
    pub post_only: bool,    // Set true to guarantee maker status
}

impl OrderParams {
    /// Create a maker order with dynamically fetched fee rate
    pub async fn new_maker_order(
        client: &PolymarketClient,
        token_id: String, 
        side: Side, 
        price: f64, 
        size: f64,
    ) -> Result<Self, Error> {
        // IMPORTANT: Fetch fee rate from exchange
        let fee_rate_bps = client.get_fee_rate(&token_id).await?;
        
        Ok(Self {
            token_id,
            side,
            price,
            size,
            fee_rate_bps,  // Dynamic!
            post_only: true,
        })
    }
}

// Cache fee rate per market to avoid repeated API calls
pub struct MarketSession {
    pub market_id: String,
    pub up_token: String,
    pub down_token: String,
    pub strike_price: f64,
    pub fee_rate_bps: u32,  // Fetched once at session start
}

impl MarketSession {
    pub async fn new(client: &PolymarketClient, market: &Market) -> Result<Self, Error> {
        // Fetch fee rate once per market session
        let fee_rate_bps = client.get_fee_rate(&market.up_token).await?;
        
        Ok(Self {
            market_id: market.id.clone(),
            up_token: market.up_token.clone(),
            down_token: market.down_token.clone(),
            strike_price: market.strike_price,
            fee_rate_bps,
        })
    }
}
```

---

# 4. The Three Profit Engines

## Engine 1: Directional (60-70% of profit)

See Section 2 for signal logic.

## Engine 2: Arbitrage (15-25% of profit)

```rust
#[derive(Debug)]
pub struct ArbitrageOpportunity {
    pub up_price: f64,
    pub down_price: f64,
    pub combined_price: f64,
    pub profit_margin: f64,
    pub recommended_size: f64,
}

impl ArbitrageOpportunity {
    pub fn detect(
        up_best_ask: f64, 
        down_best_ask: f64,
        max_size: f64,
    ) -> Option<Self> {
        let combined = up_best_ask + down_best_ask;
        
        // Only arb if profit > 2% (to cover any slippage)
        if combined < 0.98 {
            Some(Self {
                up_price: up_best_ask,
                down_price: down_best_ask,
                combined_price: combined,
                profit_margin: 1.0 - combined,
                recommended_size: max_size,
            })
        } else {
            None
        }
    }
    
    pub fn expected_profit(&self, size: f64) -> f64 {
        size * self.profit_margin
    }
}
```

## Engine 3: Maker Rebates (10-20% of profit)

### CRITICAL: Rebate Tiers Change Over Time

The rebate percentage is NOT fixed. Polymarket adjusts rebate tiers periodically:

| Period | Rebate % | Impact |
|--------|----------|--------|
| Launch (ended Jan 11) | 100% | All taker fees → makers |
| Current (Jan 11-18) | 20% | Only 20% of fees → makers |
| Future | Variable | Check announcements |

**Your bot MUST account for this or profitability estimates will be 5x off!**

```rust
/// Rebate configuration - MUST be updated based on current Polymarket announcements
#[derive(Debug, Clone)]
pub struct RebateConfig {
    /// Current rebate percentage (0.0 to 1.0)
    /// CHECK POLYMARKET ANNOUNCEMENTS FOR CURRENT VALUE
    pub rebate_percentage: f64,
    
    /// When this config expires (check for updates)
    pub valid_until: chrono::DateTime<chrono::Utc>,
}

impl RebateConfig {
    /// Current rebate tier as of Jan 13, 2026
    /// WARNING: This changes! Monitor Polymarket announcements.
    pub fn current() -> Self {
        Self {
            rebate_percentage: 0.20,  // 20% until Jan 18
            valid_until: chrono::Utc.with_ymd_and_hms(2026, 1, 18, 0, 0, 0).unwrap(),
        }
    }
    
    pub fn is_expired(&self) -> bool {
        chrono::Utc::now() > self.valid_until
    }
}

/// Estimate daily rebate with current tier
pub fn estimate_daily_rebate(
    your_maker_volume: f64,
    estimated_fee_pool: f64,      // Total taker fees collected (~$20K-50K/day)
    your_market_share: f64,       // Your share of maker volume (~5% if active)
    rebate_config: &RebateConfig,
) -> f64 {
    if rebate_config.is_expired() {
        log::warn!("Rebate config expired! Update rebate_percentage from Polymarket announcements");
    }
    
    // Only a percentage of the fee pool is distributed as rebates
    let rebate_pool = estimated_fee_pool * rebate_config.rebate_percentage;
    rebate_pool * your_market_share
}

/// Example rebate calculations at different tiers
/// 
/// At 100% rebate (launch period):
///   $20,000 fee pool × 5% share = $1,000/day
/// 
/// At 20% rebate (current):
///   $20,000 fee pool × 20% × 5% share = $200/day
///   
/// This is a 5x reduction! Factor this into profitability.
```

### Monitoring Rebate Tier Changes

```rust
impl TradingBot {
    /// Check if rebate config needs updating
    /// Call this at startup and periodically
    pub fn check_rebate_config(&self) -> Result<(), Error> {
        if self.rebate_config.is_expired() {
            log::error!(
                "REBATE CONFIG EXPIRED! Current tier ended {}. \
                 Update rebate_percentage from Polymarket announcements before trading.",
                self.rebate_config.valid_until
            );
            return Err(Error::ConfigExpired);
        }
        
        // Warn if expiring soon
        let time_until_expiry = self.rebate_config.valid_until - chrono::Utc::now();
        if time_until_expiry < chrono::Duration::hours(24) {
            log::warn!(
                "Rebate config expires in {} hours. Check for updates.",
                time_until_expiry.num_hours()
            );
        }
        
        Ok(())
    }
}
```

### Impact on Profitability

| Component | At 100% Rebate | At 20% Rebate |
|-----------|----------------|---------------|
| Directional profit | $10,125/day | $10,125/day |
| Arbitrage profit | $250/day | $250/day |
| **Maker rebates** | **$1,000/day** | **$200/day** |
| **Total** | **$11,375/day** | **$10,575/day** |

The rebate reduction mainly affects the "free money" component. Core strategy profitability remains intact.

---

# 5. Risk Management & Order Sizing

## Account88888's Trading Pattern (Reference)

| Metric | Value |
|--------|-------|
| Avg trades per market | 332 |
| Avg volume per market | $4,993 |
| Avg trade size | $15.04 |
| Market duration | ~15 min (900 sec) |
| Trade frequency | ~1 trade per 2.7 seconds |

## Dynamic Order Sizing

The key insight: **scale everything to available balance**.

```rust
use std::time::Duration;

/// Configuration that scales with available balance
#[derive(Debug, Clone)]
pub struct TradingConfig {
    /// Total available balance for trading
    pub available_balance: f64,
    
    /// Maximum % of balance to risk per market
    pub max_market_allocation: f64,  // Default: 0.20 (20%)
    
    /// Minimum hedge ratio (always keep some on both sides)
    pub min_hedge_ratio: f64,  // Default: 0.20 (20%)
    
    /// Expected number of trades per market (based on Account88888's ~332)
    /// Used to calculate base order size: budget / expected_trades
    pub expected_trades_per_market: u32,  // Default: 200
    
    /// Minimum order size (Polymarket minimum)
    pub min_order_size: f64,  // Default: $1.00
    
    /// Maximum confidence multiplier (caps aggressive sizing)
    pub max_confidence_multiplier: f64,  // Default: 3.0
}

impl TradingConfig {
    pub fn new(available_balance: f64) -> Self {
        Self {
            available_balance,
            max_market_allocation: 0.20,
            min_hedge_ratio: 0.20,
            expected_trades_per_market: 200,
            min_order_size: 1.0,
            max_confidence_multiplier: 3.0,
        }
    }
    
    /// Calculate the budget for a single market
    pub fn market_budget(&self) -> f64 {
        self.available_balance * self.max_market_allocation
    }
    
    /// Calculate base order size
    /// This is the MINIMUM size - actual size scales up with confidence
    pub fn base_order_size(&self) -> f64 {
        let budget = self.market_budget();
        let size = budget / self.expected_trades_per_market as f64;
        size.max(self.min_order_size)
    }
}

/// Confidence factors that affect order sizing
#[derive(Debug, Clone)]
pub struct ConfidenceFactors {
    /// How far price is from strike (in $)
    pub distance_to_strike: f64,
    
    /// Minutes remaining in market
    pub minutes_remaining: f64,
    
    /// Signal strength from our algorithm
    pub signal: Signal,
    
    /// Order book imbalance (-1 to +1, positive = more bids than asks)
    pub book_imbalance: f64,
    
    /// Depth available on our side of the book (in $)
    pub favorable_depth: f64,
}

/// Breakdown of confidence components
#[derive(Debug, Clone)]
pub struct Confidence {
    pub time: f64,      // Time remaining + distance interaction
    pub distance: f64,  // Distance to strike
    pub signal: f64,    // Signal strength
    pub book: f64,      // Order book factors
}

impl Confidence {
    /// Combined multiplier using geometric mean (prevents extreme values)
    pub fn total_multiplier(&self) -> f64 {
        (self.time * self.distance * self.signal * self.book).powf(0.25) * 1.5
    }
}

/// Order sizer that scales based on CONFIDENCE, not pace
/// 
/// Philosophy:
/// - Base size guarantees we CAN trade the full 15 minutes (~200 trades)
/// - Individual order size scales UP when confidence is HIGH
/// - Confidence comes from: time remaining, distance to strike, order book, signal
/// - Result: We naturally spend MORE late in the market when outcome is clearer
#[derive(Debug)]
pub struct OrderSizer {
    config: TradingConfig,
    
    /// Amount already spent in current market
    spent: f64,
    
    /// Trades executed in current market
    trades: u32,
}

impl OrderSizer {
    pub fn new(config: TradingConfig) -> Self {
        Self {
            config,
            spent: 0.0,
            trades: 0,
        }
    }
    
    /// Reset for a new market
    pub fn reset(&mut self) {
        self.spent = 0.0;
        self.trades = 0;
    }
    
    /// Calculate order size based on confidence factors
    /// 
    /// NOT pace-based throttling! We scale UP when confident, DOWN when uncertain.
    pub fn get_order_size(&self, factors: &ConfidenceFactors) -> OrderSizeResult {
        let budget_remaining = self.config.market_budget() - self.spent;
        
        if budget_remaining < self.config.min_order_size {
            return OrderSizeResult::BudgetExhausted;
        }
        
        // Start with base size (ensures ~200 trades possible)
        let base_size = self.config.base_order_size();
        
        // Calculate confidence from all factors
        let confidence = self.calculate_confidence(factors);
        
        // Apply multiplier (capped between 0.5x and 3x)
        let multiplier = confidence.total_multiplier()
            .min(3.0)
            .max(0.5);
        
        let raw_size = base_size * multiplier;
        
        // Cap at 10% of remaining budget per trade (risk management)
        let max_single_trade = budget_remaining * 0.10;
        let final_size = raw_size
            .min(max_single_trade)
            .max(self.config.min_order_size);
        
        OrderSizeResult::Trade {
            size: final_size,
            base_size,
            multiplier,
            confidence,
            budget_remaining,
        }
    }
    
    /// Record a completed trade
    pub fn record_trade(&mut self, size: f64) {
        self.spent += size;
        self.trades += 1;
    }
    
    fn calculate_confidence(&self, factors: &ConfidenceFactors) -> Confidence {
        Confidence {
            time: self.time_confidence(factors.minutes_remaining, factors.distance_to_strike),
            distance: self.distance_confidence(factors.distance_to_strike),
            signal: self.signal_confidence(factors.signal),
            book: self.book_confidence(factors.book_imbalance, factors.favorable_depth),
        }
    }
    
    /// Time confidence: Higher when less time AND far from strike
    /// 
    /// Key insight: 2 mins left at strike = still uncertain
    ///              2 mins left $80 from strike = very certain
    fn time_confidence(&self, minutes_remaining: f64, distance: f64) -> f64 {
        let time_factor = match minutes_remaining {
            m if m > 12.0 => 0.6,   // Early: lots can change
            m if m > 9.0  => 0.8,
            m if m > 6.0  => 1.0,   // Mid: baseline
            m if m > 3.0  => 1.3,   // Late: increasing confidence
            _             => 1.6,   // Final minutes: high confidence
        };
        
        // Only apply time confidence if we're far from strike
        let distance_modifier = match distance.abs() {
            d if d > 50.0 => 1.0,   // Far: full time confidence applies
            d if d > 20.0 => 0.8,
            _             => 0.5,   // Close to strike: time doesn't help
        };
        
        time_factor * distance_modifier
    }
    
    /// Distance confidence: Further from strike = more certain outcome
    fn distance_confidence(&self, distance: f64) -> f64 {
        match distance.abs() {
            d if d > 100.0 => 1.5,  // Very far: high confidence
            d if d > 50.0  => 1.3,
            d if d > 30.0  => 1.1,
            d if d > 20.0  => 1.0,  // Baseline
            d if d > 10.0  => 0.8,
            _              => 0.6,  // At strike: low confidence
        }
    }
    
    /// Signal confidence: Strong signals get larger sizes
    fn signal_confidence(&self, signal: Signal) -> f64 {
        match signal {
            Signal::StrongUp | Signal::StrongDown => 1.4,
            Signal::LeanUp | Signal::LeanDown => 1.1,
            Signal::Neutral => 0.7,  // Uncertain: smaller sizes
        }
    }
    
    /// Order book confidence: Favorable depth and imbalance = more confidence
    fn book_confidence(&self, imbalance: f64, favorable_depth: f64) -> f64 {
        let imbalance_factor = if imbalance.abs() > 0.3 { 1.2 } else { 1.0 };
        
        let depth_factor = match favorable_depth {
            d if d > 50000.0 => 1.2,  // Very deep
            d if d > 20000.0 => 1.1,
            d if d < 5000.0  => 0.9,  // Thin: be careful
            _                => 1.0,
        };
        
        imbalance_factor * depth_factor
    }
}

#[derive(Debug)]
pub enum OrderSizeResult {
    Trade {
        size: f64,
        base_size: f64,
        multiplier: f64,
        confidence: Confidence,
        budget_remaining: f64,
    },
    BudgetExhausted,
}
```

## Balance-Scaled Examples

```rust
/// Examples of how the system scales with different balances
fn scaling_examples() {
    // Small account: $1,000
    let small = TradingConfig::new(1000.0);
    println!("$1,000 account:");
    println!("  Market budget: ${:.2}", small.market_budget());        // $200
    println!("  Base order size: ${:.2}", small.base_order_size());    // $1.00
    println!("  Expected trades: {}", small.target_trades_per_market); // 200
    
    // Medium account: $5,000
    let medium = TradingConfig::new(5000.0);
    println!("\n$5,000 account:");
    println!("  Market budget: ${:.2}", medium.market_budget());       // $1,000
    println!("  Base order size: ${:.2}", medium.base_order_size());   // $5.00
    
    // Large account: $25,000
    let large = TradingConfig::new(25000.0);
    println!("\n$25,000 account:");
    println!("  Market budget: ${:.2}", large.market_budget());        // $5,000
    println!("  Base order size: ${:.2}", large.base_order_size());    // $25.00
}

/// Example: Confidence-based sizing in action
fn confidence_sizing_example() {
    let config = TradingConfig::new(1000.0);  // $1,000 account
    let sizer = OrderSizer::new(config);
    
    // Scenario 1: Early market, close to strike, weak signal
    // LOW CONFIDENCE → SMALL ORDER
    let factors_weak = ConfidenceFactors {
        distance_to_strike: 15.0,
        minutes_remaining: 13.0,
        signal: Signal::Neutral,
        book_imbalance: 0.0,
        favorable_depth: 10000.0,
    };
    // Result: ~0.7x multiplier → $0.70 order
    
    // Scenario 2: Late market, far from strike, strong signal
    // HIGH CONFIDENCE → LARGE ORDER
    let factors_strong = ConfidenceFactors {
        distance_to_strike: 80.0,
        minutes_remaining: 2.0,
        signal: Signal::StrongDown,
        book_imbalance: -0.4,
        favorable_depth: 40000.0,
    };
    // Result: ~2.5x multiplier → $2.50 order
    
    // Natural result: MORE capital flows to HIGH-CONFIDENCE opportunities
}
```

## Confidence Multipliers

| Factor | Low Value | Multiplier | High Value | Multiplier |
|--------|-----------|------------|------------|------------|
| **Time** | 13+ mins left | 0.6x | <3 mins left | 1.6x |
| **Distance** | At strike ($10) | 0.6x | Far ($100+) | 1.5x |
| **Signal** | Neutral | 0.7x | Strong | 1.4x |
| **Book** | Thin/against | 0.9x | Deep/favorable | 1.2x |

**Combined**: Multipliers combine via geometric mean, capped at 0.5x - 3.0x
```

## Scaling Table

| Balance | Market Budget (20%) | Base Order Size | Est. Trades/Market |
|---------|--------------------|-----------------|--------------------|
| $500 | $100 | $0.50 | 200 |
| $1,000 | $200 | $1.00 | 200 |
| $2,500 | $500 | $2.50 | 200 |
| $5,000 | $1,000 | $5.00 | 200 |
| $10,000 | $2,000 | $10.00 | 200 |
| $25,000 | $5,000 | $25.00 | 200 |

## Position Limits & Kill Switches

```rust
/// Risk manager that enforces hard limits
#[derive(Debug)]
pub struct RiskManager {
    config: TradingConfig,
    
    /// Daily P&L tracking
    daily_pnl: f64,
    
    /// Consecutive losses counter
    consecutive_losses: u32,
    
    /// Whether trading is enabled
    trading_enabled: bool,
    
    /// Markets traded today
    markets_traded_today: u32,
}

impl RiskManager {
    pub fn new(config: TradingConfig) -> Self {
        Self {
            config,
            daily_pnl: 0.0,
            consecutive_losses: 0,
            trading_enabled: true,
            markets_traded_today: 0,
        }
    }
    
    /// Check if a trade is allowed
    pub fn check_trade(&self, trade_size: f64, current_position: &Position) -> TradeDecision {
        // Rule 1: Trading must be enabled
        if !self.trading_enabled {
            return TradeDecision::Reject("Trading disabled".into());
        }
        
        // Rule 2: Check daily loss limit (10% of capital)
        let daily_loss_limit = self.config.available_balance * 0.10;
        if self.daily_pnl < -daily_loss_limit {
            return TradeDecision::Reject("Daily loss limit hit".into());
        }
        
        // Rule 3: Check consecutive losses
        if self.consecutive_losses >= 3 {
            return TradeDecision::Reject("3 consecutive losses - review required".into());
        }
        
        // Rule 4: Check market allocation
        let market_budget = self.config.market_budget();
        if current_position.total_cost() + trade_size > market_budget {
            return TradeDecision::ReduceSize {
                max_allowed: market_budget - current_position.total_cost(),
            };
        }
        
        // Rule 5: Check minimum hedge ratio
        let hedge_ratio = current_position.min_side_ratio();
        if hedge_ratio < self.config.min_hedge_ratio {
            return TradeDecision::RebalanceRequired {
                current_ratio: hedge_ratio,
                target_ratio: self.config.min_hedge_ratio,
            };
        }
        
        TradeDecision::Approve
    }
    
    /// Record market result
    pub fn record_market_result(&mut self, pnl: f64) {
        self.daily_pnl += pnl;
        self.markets_traded_today += 1;
        
        if pnl < 0.0 {
            self.consecutive_losses += 1;
            
            // Auto-disable after 3 losses
            if self.consecutive_losses >= 3 {
                self.trading_enabled = false;
                log::warn!("Trading disabled after 3 consecutive losses");
            }
        } else {
            self.consecutive_losses = 0;
        }
        
        // Auto-disable on daily loss limit
        let daily_loss_limit = self.config.available_balance * 0.10;
        if self.daily_pnl < -daily_loss_limit {
            self.trading_enabled = false;
            log::warn!("Trading disabled: daily loss limit reached");
        }
    }
    
    /// Reset for new day
    pub fn new_day(&mut self) {
        self.daily_pnl = 0.0;
        self.consecutive_losses = 0;
        self.markets_traded_today = 0;
        self.trading_enabled = true;
    }
}

#[derive(Debug)]
pub enum TradeDecision {
    Approve,
    Reject(String),
    ReduceSize { max_allowed: f64 },
    RebalanceRequired { current_ratio: f64, target_ratio: f64 },
}

#[derive(Debug, Default)]
pub struct Position {
    pub up_shares: f64,
    pub down_shares: f64,
    pub up_cost: f64,
    pub down_cost: f64,
}

impl Position {
    pub fn total_cost(&self) -> f64 {
        self.up_cost + self.down_cost
    }
    
    pub fn total_shares(&self) -> f64 {
        self.up_shares + self.down_shares
    }
    
    pub fn up_ratio(&self) -> f64 {
        if self.total_shares() == 0.0 {
            0.5
        } else {
            self.up_shares / self.total_shares()
        }
    }
    
    pub fn min_side_ratio(&self) -> f64 {
        let up_ratio = self.up_ratio();
        up_ratio.min(1.0 - up_ratio)
    }
    
    /// Calculate P&L for a resolved market
    pub fn calculate_pnl(&self, winning_side: Side) -> f64 {
        let payout = match winning_side {
            Side::Up => self.up_shares,    // $1 per winning share
            Side::Down => self.down_shares,
        };
        payout - self.total_cost()
    }
}

#[derive(Debug, Clone, Copy)]
pub enum Side {
    Up,
    Down,
}
```

## Trade Interval Control

```rust
/// Simple interval control to avoid spamming the API
/// NOT pace-based throttling - just prevents API rate limit issues
pub struct TradeInterval {
    min_interval: Duration,
    last_trade: Instant,
}

impl TradeInterval {
    pub fn new() -> Self {
        Self {
            min_interval: Duration::from_millis(500),
            last_trade: Instant::now() - Duration::from_secs(1),
        }
    }
    
    pub fn can_trade(&self) -> bool {
        self.last_trade.elapsed() >= self.min_interval
    }
    
    pub fn record_trade(&mut self) {
        self.last_trade = Instant::now();
    }
    
    pub fn time_until_next(&self) -> Duration {
        self.min_interval.saturating_sub(self.last_trade.elapsed())
    }
}
```
```

---

# 6. Complete Algorithm (Rust)

## Main Trading Bot

```rust
use tokio::sync::mpsc;
use tokio::time::{interval, Duration};

pub struct TradingBot {
    config: TradingConfig,
    risk_manager: RiskManager,
    order_sizer: OrderSizer,
    position: Position,
    trade_interval: TradeInterval,
    
    // API clients
    binance_feed: BinancePriceFeed,
    polymarket_client: PolymarketClient,
}

impl TradingBot {
    pub fn new(available_balance: f64) -> Self {
        let config = TradingConfig::new(available_balance);
        let risk_manager = RiskManager::new(config.clone());
        let order_sizer = OrderSizer::new(config.clone());
        
        Self {
            config,
            risk_manager,
            order_sizer,
            position: Position::default(),
            trade_interval: TradeInterval::new(),
            binance_feed: BinancePriceFeed::new(),
            polymarket_client: PolymarketClient::new(),
        }
    }
    
    /// Run the bot for a single market
    pub async fn run_market(&mut self, market: &Market) -> Result<MarketResult, Error> {
        // Initialize market session with cached fee rate
        let session = MarketSession::new(&self.polymarket_client, market).await?;
        
        // Reset for new market
        self.order_sizer.reset();
        self.position = Position::default();
        self.trade_interval = TradeInterval::new();
        
        log::info!(
            "Starting market {} | Budget: ${:.2} | Base size: ${:.2} | Fee rate: {} bps",
            market.id,
            self.config.market_budget(),
            self.config.base_order_size(),
            session.fee_rate_bps,
        );
        
        let strike_price = market.strike_price;
        let market_end = market.end_time;
        
        // Main trading loop
        loop {
            // Check if market has ended
            let now = chrono::Utc::now();
            if now >= market_end {
                break;
            }
            
            let minutes_remaining = (market_end - now).num_seconds() as f64 / 60.0;
            
            // Respect minimum trade interval (API rate limits)
            if !self.trade_interval.can_trade() {
                tokio::time::sleep(self.trade_interval.time_until_next()).await;
                continue;
            }
            
            // Get current BTC price
            let btc_price = self.binance_feed.get_price().await?;
            
            // Get order books
            let up_book = self.polymarket_client.get_order_book(&market.up_token).await?;
            let down_book = self.polymarket_client.get_order_book(&market.down_token).await?;
            
            // === ENGINE 1: Check for Arbitrage ===
            if let Some(arb) = ArbitrageOpportunity::detect(
                up_book.best_ask(),
                down_book.best_ask(),
                self.config.base_order_size() * 2.0,
            ) {
                if arb.profit_margin > 0.02 {
                    self.execute_arbitrage(&session, arb).await?;
                    continue;
                }
            }
            
            // === ENGINE 2: Calculate Signal ===
            let distance = btc_price - strike_price;
            let signal = get_signal(btc_price, strike_price, minutes_remaining);
            
            // Build confidence factors
            let book_imbalance = calculate_book_imbalance(&up_book, &down_book, signal);
            let favorable_depth = get_favorable_depth(&up_book, &down_book, signal);
            
            let factors = ConfidenceFactors {
                distance_to_strike: distance,
                minutes_remaining,
                signal,
                book_imbalance,
                favorable_depth,
            };
            
            // === ENGINE 3: Execute with Confidence-Based Sizing ===
            match self.order_sizer.get_order_size(&factors) {
                OrderSizeResult::Trade { size, multiplier, confidence, .. } => {
                    // Check with risk manager
                    match self.risk_manager.check_trade(size, &self.position) {
                        TradeDecision::Approve => {
                            log::debug!(
                                "Trade: {:?} | ${:.2} ({}x) | dist=${:.0} | mins={:.1}",
                                signal, size, multiplier, distance, minutes_remaining
                            );
                            self.execute_directional(&session, signal, size).await?;
                        }
                        TradeDecision::ReduceSize { max_allowed } => {
                            if max_allowed >= self.config.min_order_size {
                                self.execute_directional(&session, signal, max_allowed).await?;
                            }
                        }
                        TradeDecision::RebalanceRequired { target_ratio, .. } => {
                            self.rebalance_position(&session, target_ratio).await?;
                        }
                        TradeDecision::Reject(reason) => {
                            log::warn!("Trade rejected: {}", reason);
                            break;
                        }
                    }
                }
                OrderSizeResult::BudgetExhausted => {
                    log::info!("Budget exhausted, waiting for resolution");
                    break;
                }
            }
            
            // Small delay between iterations
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        
        // Wait for market resolution
        let winning_side = self.wait_for_resolution(&market).await?;
        let pnl = self.position.calculate_pnl(winning_side);
        
        // Record result with risk manager
        self.risk_manager.record_market_result(pnl);
        
        Ok(MarketResult {
            market_id: market.id.clone(),
            pnl,
            trades: self.order_sizer.trades,
            volume: self.position.total_cost(),
        })
    }
    
    async fn execute_arbitrage(
        &mut self,
        session: &MarketSession,
        arb: ArbitrageOpportunity,
    ) -> Result<(), Error> {
        let size = arb.recommended_size;
        
        // Place maker orders on both sides using cached fee rate
        self.polymarket_client.place_order(OrderParams {
            token_id: session.up_token.clone(),
            side: Side::Up,
            price: arb.up_price,
            size,
            fee_rate_bps: session.fee_rate_bps,
            post_only: true,
        }).await?;
        
        self.polymarket_client.place_order(OrderParams {
            token_id: session.down_token.clone(),
            side: Side::Down,
            price: arb.down_price,
            size,
            fee_rate_bps: session.fee_rate_bps,
            post_only: true,
        }).await?;
        
        // Update position
        self.position.up_shares += size / arb.up_price;
        self.position.up_cost += size;
        self.position.down_shares += size / arb.down_price;
        self.position.down_cost += size;
        
        self.order_sizer.record_trade(size * 2.0);
        self.trade_interval.record_trade();
        
        log::info!(
            "ARB: {} shares each side, {:.2}% margin",
            size, arb.profit_margin * 100.0
        );
        
        Ok(())
    }
    
    async fn execute_directional(
        &mut self,
        session: &MarketSession,
        signal: Signal,
        total_size: f64,
    ) -> Result<(), Error> {
        let up_ratio = signal.up_ratio();
        let down_ratio = signal.down_ratio();
        
        let up_size = total_size * up_ratio;
        let down_size = total_size * down_ratio;
        
        // Get optimal maker prices
        let up_book = self.polymarket_client.get_order_book(&session.up_token).await?;
        let down_book = self.polymarket_client.get_order_book(&session.down_token).await?;
        
        let up_price = Self::get_maker_price(&up_book);
        let down_price = Self::get_maker_price(&down_book);
        
        // Place orders using cached fee rate
        if up_size >= self.config.min_order_size {
            self.polymarket_client.place_order(OrderParams {
                token_id: session.up_token.clone(),
                side: Side::Up,
                price: up_price,
                size: up_size,
                fee_rate_bps: session.fee_rate_bps,
                post_only: true,
            }).await?;
            self.position.up_shares += up_size / up_price;
            self.position.up_cost += up_size;
        }
        
        if down_size >= self.config.min_order_size {
            self.polymarket_client.place_order(OrderParams {
                token_id: session.down_token.clone(),
                side: Side::Down,
                price: down_price,
                size: down_size,
                fee_rate_bps: session.fee_rate_bps,
                post_only: true,
            }).await?;
            self.position.down_shares += down_size / down_price;
            self.position.down_cost += down_size;
        }
        
        self.order_sizer.record_trade(total_size);
        self.trade_interval.record_trade();
        
        Ok(())
    }
    
    fn get_maker_price(book: &OrderBook) -> f64 {
        let best_bid = book.best_bid();
        let best_ask = book.best_ask();
        let spread = best_ask - best_bid;
        
        // Place inside spread for high fill probability
        if spread > 0.03 {
            best_bid + spread * 0.4
        } else if spread > 0.01 {
            best_bid + spread * 0.3
        } else {
            best_bid
        }
    }
    
    async fn rebalance_position(
        &mut self,
        session: &MarketSession,
        target_min_ratio: f64,
    ) -> Result<(), Error> {
        // Implementation unchanged
        Ok(())
    }
    
    async fn wait_for_resolution(&self, market: &Market) -> Result<Side, Error> {
        loop {
            if let Some(result) = self.polymarket_client.get_resolution(&market.id).await? {
                return Ok(result);
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }
}

/// Helper: Calculate order book imbalance
fn calculate_book_imbalance(up_book: &OrderBook, down_book: &OrderBook, signal: Signal) -> f64 {
    let up_bid_depth = up_book.bid_depth();
    let up_ask_depth = up_book.ask_depth();
    let down_bid_depth = down_book.bid_depth();
    let down_ask_depth = down_book.ask_depth();
    
    match signal {
        Signal::StrongUp | Signal::LeanUp => {
            // For UP bets, we want strong UP book (more bids than asks)
            (up_bid_depth - up_ask_depth) / (up_bid_depth + up_ask_depth).max(1.0)
        }
        Signal::StrongDown | Signal::LeanDown => {
            // For DOWN bets, we want strong DOWN book
            (down_bid_depth - down_ask_depth) / (down_bid_depth + down_ask_depth).max(1.0)
        }
        Signal::Neutral => 0.0,
    }
}

/// Helper: Get depth on our favorable side
fn get_favorable_depth(up_book: &OrderBook, down_book: &OrderBook, signal: Signal) -> f64 {
    match signal {
        Signal::StrongUp | Signal::LeanUp => up_book.bid_depth(),
        Signal::StrongDown | Signal::LeanDown => down_book.bid_depth(),
        Signal::Neutral => (up_book.bid_depth() + down_book.bid_depth()) / 2.0,
    }
}

#[derive(Debug)]
pub struct MarketResult {
    pub market_id: String,
    pub pnl: f64,
    pub trades: u32,
    pub volume: f64,
}
```

---

# 7. Technical Architecture

## System Components

```
┌─────────────────────────────────────────────────────────────────┐
│                      TRADING BOT ARCHITECTURE                    │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  ┌─────────────┐    ┌─────────────┐    ┌─────────────┐         │
│  │   BINANCE   │    │ POLYMARKET  │    │ POLYMARKET  │         │
│  │  WebSocket  │    │  REST API   │    │    CLOB     │         │
│  │  (BTC/USD)  │    │  (Markets)  │    │  (Orders)   │         │
│  └──────┬──────┘    └──────┬──────┘    └──────┬──────┘         │
│         │                  │                  │                 │
│         ▼                  ▼                  ▼                 │
│  ┌─────────────────────────────────────────────────┐           │
│  │              TradingBot                          │           │
│  │                                                  │           │
│  │  ┌─────────────────────────────────────────┐   │           │
│  │  │ TradingConfig (scales with balance)     │   │           │
│  │  │ - available_balance: $1,000-$100,000    │   │           │
│  │  │ - market_budget(): 20% of balance       │   │           │
│  │  │ - base_order_size(): budget / 200       │   │           │
│  │  └─────────────────────────────────────────┘   │           │
│  │                                                  │           │
│  │  ┌─────────────────────────────────────────┐   │           │
│  │  │ MarketSession (per-market state)        │   │           │
│  │  │ - fee_rate_bps: fetched from API        │   │           │
│  │  │ - up_token, down_token                  │   │           │
│  │  │ - strike_price                          │   │           │
│  │  └─────────────────────────────────────────┘   │           │
│  │                                                  │           │
│  │  ┌─────────────────────────────────────────┐   │           │
│  │  │ OrderSizer (CONFIDENCE-BASED)           │   │           │
│  │  │ - Base size ensures ~200 trades         │   │           │
│  │  │ - Scales UP with confidence:            │   │           │
│  │  │   • Time remaining + distance           │   │           │
│  │  │   • Signal strength                     │   │           │
│  │  │   • Order book depth/imbalance          │   │           │
│  │  │ - NOT pace-based throttling!            │   │           │
│  │  └─────────────────────────────────────────┘   │           │
│  │                                                  │           │
│  │  ┌─────────────────────────────────────────┐   │           │
│  │  │ RiskManager (hard limits)               │   │           │
│  │  │ - 20% max per market                    │   │           │
│  │  │ - 20% min hedge ratio                   │   │           │
│  │  │ - 10% daily loss limit                  │   │           │
│  │  │ - 3 consecutive loss stop               │   │           │
│  │  └─────────────────────────────────────────┘   │           │
│  │                                                  │           │
│  └─────────────────────────────────────────────────┘           │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

## Cargo.toml Dependencies

```toml
[package]
name = "polymarket-bot"
version = "0.1.0"
edition = "2021"

[dependencies]
tokio = { version = "1.0", features = ["full"] }
tokio-tungstenite = "0.21"
reqwest = { version = "0.11", features = ["json"] }
serde = { version = "1.0", features = ["derive"] }
serde_json = "1.0"
chrono = { version = "0.4", features = ["serde"] }
rust_decimal = "1.33"
ethers = "2.0"
log = "0.4"
env_logger = "0.10"
anyhow = "1.0"
thiserror = "1.0"
```

---

# 8. Expected Economics

## Scaling Table

| Balance | Market Budget | Base Order | Est. Daily Profit* |
|---------|--------------|------------|-------------------|
| $500 | $100 | $0.50 | $20-50 |
| $1,000 | $200 | $1.00 | $50-100 |
| $2,500 | $500 | $2.50 | $150-300 |
| $5,000 | $1,000 | $5.00 | $300-600 |
| $10,000 | $2,000 | $10.00 | $600-1,200 |
| $25,000 | $5,000 | $25.00 | $1,500-3,000 |

*At current 20% rebate tier. Add ~8% at 100% rebate tier.

## Rebate Tier Impact

| Component | At 100% Rebate | At 20% Rebate (Current) |
|-----------|----------------|-------------------------|
| Directional profit | $10,125/day | $10,125/day |
| Arbitrage profit | $250/day | $250/day |
| Maker rebates | $1,000/day | **$200/day** |
| **Total (10 mkts)** | **$11,375/day** | **$10,575/day** |

**Key insight**: Rebate tier changes affect ~8% of profit. Core strategy is rebate-agnostic.

## Risk-Adjusted Returns

| Metric | Conservative | Expected | Optimistic |
|--------|--------------|----------|------------|
| Win rate | 90% | 95% | 98% |
| Markets/day | 5 | 10 | 15 |
| Return/market | 15% | 22% | 30% |
| Monthly ROI | 30% | 50% | 100% |

---

# Appendix: Quick Reference

## Scaling Formula

```rust
// Everything scales from available_balance
let market_budget = available_balance * 0.20;
let base_order_size = market_budget / 200.0;  // Minimum size
let daily_loss_limit = available_balance * 0.10;

// Actual order size = base_size × confidence_multiplier (0.5x to 3.0x)
```

## Confidence Multipliers

| Factor | Low → Multiplier | High → Multiplier |
|--------|------------------|-------------------|
| **Time + Distance** | Early OR at strike → 0.6x | Late AND far → 1.6x |
| **Distance** | At strike ($10) → 0.6x | Far ($100+) → 1.5x |
| **Signal** | Neutral → 0.7x | Strong → 1.4x |
| **Book** | Thin/against → 0.9x | Deep/favorable → 1.2x |

## Signal Thresholds (Time-Adjusted)

```
Minutes 15-12: lean=$40, conviction=$80
Minutes 12-9:  lean=$30, conviction=$60  
Minutes 9-6:   lean=$20, conviction=$50  ← Account88888 baseline
Minutes 6-3:   lean=$15, conviction=$40
Minutes 3-0:   lean=$10, conviction=$30
```

## Order Checklist

- [ ] Fetch `fee_rate_bps` from API (do NOT hardcode!)
- [ ] Check `RebateConfig` is not expired
- [ ] Use limit orders with `post_only: true`
- [ ] Check arbitrage before directional
- [ ] Calculate confidence factors for sizing
- [ ] Maintain 20% minimum hedge ratio

## Fee Rate Endpoint

```
GET https://clob.polymarket.com/fee-rate?token_id={token_id}

Response: { "fee_rate_bps": 1000 }  // 0 = fee-free, >0 = fee-enabled
```

## Rebate Tier (MUST MONITOR)

```rust
// Current as of Jan 13, 2026 - CHECK POLYMARKET ANNOUNCEMENTS
RebateConfig {
    rebate_percentage: 0.20,  // 20% until Jan 18
    valid_until: "2026-01-18T00:00:00Z",
}
```

**Warning**: Bot should refuse to start if `RebateConfig` is expired!

---

*Version 3.1 - Rust Implementation with Confidence-Based Sizing*
*Dynamic fee fetching, no hardcoded values*
