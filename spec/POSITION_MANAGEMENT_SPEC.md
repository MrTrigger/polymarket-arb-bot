# Position Management System Specification

## Overview

This document specifies the position management system for the Polymarket BTC Up/Down 15-minute trading bot. The system controls **when**, **how much**, and **in what ratio** to place positions throughout a market's lifecycle.

### Core Problem

Without position management, a naive bot would:
- Trade on every signal (200+ trades per market)
- Exhaust budget in the first few minutes
- Miss high-conviction opportunities late in the market
- Take excessive risk on uncertain early signals

### Solution Philosophy

**Intelligent capital allocation based on opportunity quality, not pace-based throttling.**

Key principles:
1. **Reserve capital for later phases** when signals are clearer
2. **Gate trades by confidence threshold** - stricter early, looser late
3. **Scale position size by confidence** - bigger when more certain
4. **Enforce position limits** - never go all-in on one side

---

## Market Phases

A 15-minute market is divided into four phases, each with its own budget allocation and confidence threshold.

| Phase | Time Remaining | Budget % | Min Confidence | Rationale |
|-------|----------------|----------|----------------|-----------|
| **Early** | 15 → 10 min | 15% | 0.80 | High uncertainty, only exceptional signals |
| **Build** | 10 → 5 min | 25% | 0.60 | Starting to see direction, selective trading |
| **Core** | 5 → 2 min | 30% | 0.50 | Main trading phase, signals clearer |
| **Final** | 2 → 0 min | 30% | 0.40 | Highest conviction, outcome nearly certain |

### Phase Budget Example ($1,000 total budget)

```
Early:  $150 (only used if exceptional signal)
Build:  $250 (selective position building)
Core:   $300 (main trading activity)
Final:  $300 (high-conviction plays)
```

### Why This Distribution?

- **Early phase gets least** because BTC can easily reverse in 10+ minutes
- **Final phase gets most** because with 2 minutes left and BTC $50+ from strike, outcome is ~95% certain
- **Budget is reserved, not throttled** - if no good signals in a phase, capital carries forward implicitly (by not being spent)

---

## Confidence Calculation

Confidence is a score from 0.0 to 1.0 indicating how certain we are of the market outcome.

### Components

**1. Time Factor** - Confidence increases as time runs out

| Minutes Remaining | Time Confidence |
|-------------------|-----------------|
| > 12 min | 0.40 |
| 9-12 min | 0.50 |
| 6-9 min | 0.60 |
| 3-6 min | 0.80 |
| 0-3 min | 1.00 |

**2. Distance Factor** - Further from strike = more certain

| Distance from Strike | Distance Confidence |
|---------------------|---------------------|
| > $100 | 1.00 |
| $50-100 | 0.85 |
| $30-50 | 0.70 |
| $20-30 | 0.55 |
| $10-20 | 0.40 |
| < $10 | 0.20 |

### Combined Confidence Formula

```python
combined = sqrt(time_confidence * distance_confidence)

# Boost when BOTH factors are strong
if time_confidence > 0.7 and distance_confidence > 0.7:
    combined = min(1.0, combined * 1.2)
```

### Confidence Examples

| Scenario | Time Conf | Dist Conf | Combined | Action |
|----------|-----------|-----------|----------|--------|
| 14 min left, $25 from strike | 0.40 | 0.55 | 0.47 | SKIP (< 0.80 early threshold) |
| 8 min left, $40 from strike | 0.60 | 0.70 | 0.65 | TRADE (≥ 0.60 build threshold) |
| 3 min left, $15 from strike | 1.00 | 0.40 | 0.63 | TRADE but small size |
| 1 min left, $80 from strike | 1.00 | 0.85 | 1.00+ | TRADE maximum size |

### Key Insight

**Time confidence alone is not enough.** Being 1 minute from expiry with BTC right at strike is NOT high confidence. The combination of time AND distance determines certainty.

---

## Signal Generation

Signals determine the **direction** of our position allocation.

### Time-Adjusted Thresholds

| Minutes Remaining | Lean Threshold | Conviction Threshold |
|-------------------|----------------|----------------------|
| > 12 min | ±$30 | ±$60 |
| 9-12 min | ±$25 | ±$50 |
| 6-9 min | ±$15 | ±$40 |
| 3-6 min | ±$12 | ±$35 |
| 0-3 min | ±$8 | ±$25 |

### Signal Types

| Signal | Condition | Up/Down Allocation |
|--------|-----------|-------------------|
| **STRONG_UP** | distance > conviction | 78% / 22% |
| **LEAN_UP** | distance > lean | 61% / 39% |
| **NEUTRAL** | \|distance\| < lean | 50% / 50% |
| **LEAN_DOWN** | distance < -lean | 39% / 61% |
| **STRONG_DOWN** | distance < -conviction | 22% / 78% |

### Signal Examples (at 5 minutes remaining)

| BTC vs Strike | Signal | Allocation |
|---------------|--------|------------|
| +$50 | STRONG_UP | 78% UP / 22% DOWN |
| +$25 | LEAN_UP | 61% UP / 39% DOWN |
| +$10 | NEUTRAL | 50% / 50% |
| -$30 | LEAN_DOWN | 39% UP / 61% DOWN |
| -$60 | STRONG_DOWN | 22% UP / 78% DOWN |

---

## Trade Sizing

### Base Size Calculation

```python
phase_budget_remaining = phase_allocation - phase_spent
base_size = phase_budget_remaining / 15  # Target ~15 trades per phase
```

### Confidence Multiplier

```python
size_multiplier = 0.5 + (confidence * 1.5)
# Range: 0.5x (low confidence) to 2.0x (high confidence)
```

### Final Size with Limits

```python
trade_size = base_size * size_multiplier
trade_size = min(trade_size, phase_budget_remaining)      # Don't exceed phase budget
trade_size = min(trade_size, total_remaining * 0.15)      # Max 15% of remaining
trade_size = max(trade_size, min_order_size)              # Minimum $1
```

### Sizing Example

```
Phase: Core (budget $300, spent $100, remaining $200)
Confidence: 0.85

base_size = $200 / 15 = $13.33
multiplier = 0.5 + (0.85 * 1.5) = 1.775
trade_size = $13.33 * 1.775 = $23.66
```

---

## Position Limits

### Hard Limits

| Limit | Value | Purpose |
|-------|-------|---------|
| Max single-side exposure | 80% | Never more than 80% on UP or DOWN |
| Min hedge ratio | 20% | Always keep 20% on minority side |
| Max per-trade | 15% of remaining | No single trade dominates |

### Exposure Adjustment

If placing a trade would exceed position limits, the allocation is adjusted:

```python
if current_up_exposure > 0.80 and signal favors UP:
    # Reduce UP allocation to maintain limits
    adjustment = current_up_exposure - 0.60
    new_up_ratio = max(0.30, base_up_ratio - adjustment)
```

### Exception: Strong Conviction Override

```python
if signal is STRONG and confidence > 0.80:
    # Allow pushing to limits for high-conviction plays
    allow_up_to_80_percent_exposure()
```

---

## Trade Decision Flow

```
┌─────────────────────────────────────────┐
│           NEW DATA POINT                │
│   (BTC price, minutes remaining)        │
└────────────────┬────────────────────────┘
                 │
                 ▼
┌─────────────────────────────────────────┐
│      1. GET CURRENT PHASE               │
│   Determine: budget_%, min_confidence   │
└────────────────┬────────────────────────┘
                 │
                 ▼
┌─────────────────────────────────────────┐
│      2. CALCULATE CONFIDENCE            │
│   confidence = f(time, distance)        │
└────────────────┬────────────────────────┘
                 │
                 ▼
┌─────────────────────────────────────────┐
│      3. CHECK CONFIDENCE THRESHOLD      │
│   confidence >= phase.min_confidence?   │
└────────────────┬────────────────────────┘
                 │
          ┌──────┴──────┐
          │             │
         NO            YES
          │             │
          ▼             ▼
       SKIP     ┌───────────────────────┐
                │  4. CHECK BUDGETS     │
                │  Phase + Total        │
                └───────────┬───────────┘
                            │
                     ┌──────┴──────┐
                     │             │
                 EXHAUSTED      AVAILABLE
                     │             │
                     ▼             ▼
                  SKIP     ┌───────────────────────┐
                           │  5. GET SIGNAL        │
                           │  Calculate direction  │
                           └───────────┬───────────┘
                                       │
                                       ▼
                           ┌───────────────────────┐
                           │  6. SIZE TRADE        │
                           │  base * confidence    │
                           └───────────┬───────────┘
                                       │
                                       ▼
                           ┌───────────────────────┐
                           │  7. CHECK LIMITS      │
                           │  Adjust if needed     │
                           └───────────┬───────────┘
                                       │
                                       ▼
                           ┌───────────────────────┐
                           │  8. EXECUTE TRADE     │
                           │  Place orders         │
                           └───────────────────────┘
```

---

## Expected Behavior

### Typical Market Simulation

```
Budget: $1,000 | Strike: $95,000 | Winner: UP (BTC ends +$80)

[14.0m] early  | Dist: +$15 | Conf: 0.35 | SKIP (< 0.80 threshold)
[12.0m] early  | Dist: +$22 | Conf: 0.42 | SKIP (< 0.80 threshold)
[10.0m] build  | Dist: +$35 | Conf: 0.58 | SKIP (< 0.60 threshold)
[ 8.5m] build  | Dist: +$48 | Conf: 0.68 | TRADE $22 (78/22)
[ 7.0m] build  | Dist: +$52 | Conf: 0.72 | TRADE $24 (78/22)
[ 5.5m] build  | Dist: +$45 | Conf: 0.69 | TRADE $20 (78/22)
[ 5.0m] core   | Dist: +$55 | Conf: 0.82 | TRADE $35 (78/22)
[ 4.0m] core   | Dist: +$62 | Conf: 0.88 | TRADE $38 (78/22)
[ 3.0m] core   | Dist: +$58 | Conf: 0.91 | TRADE $32 (78/22)
[ 2.0m] final  | Dist: +$70 | Conf: 0.98 | TRADE $45 (78/22)
[ 1.0m] final  | Dist: +$75 | Conf: 1.00 | TRADE $48 (78/22)
[ 0.5m] final  | Dist: +$80 | Conf: 1.00 | TRADE $42 (78/22)

RESULT:
  Trades: 10
  Invested: $306 (30.6% of budget)
  Exposure: 78% UP / 22% DOWN
  P&L: +$172 (56% ROI)
```

### Key Observations

1. **No trades in first 5 minutes** - confidence never hit 0.80 threshold
2. **Build phase selective** - only traded when confidence ≥ 0.60
3. **Core/Final most active** - this is where signals are clearest
4. **Only used 30% of budget** - conservative, capital preserved
5. **Consistent 78/22 allocation** - strong signal maintained

---

## Comparison: Before vs After

| Metric | Without PM | With PM |
|--------|------------|---------|
| Trades per market | 200+ | 10-20 |
| Budget used | 90%+ | 30-50% |
| Early phase activity | High | Minimal |
| Late phase reserves | None | 30% budget |
| Risk control | None | Phase limits + exposure caps |

---

## Implementation Notes

### Rust Structure

```rust
pub struct PositionManager {
    config: PositionConfig,
    position: Position,
    phase_spent: HashMap<Phase, f64>,
    trade_count: u32,
}

pub struct PositionConfig {
    pub total_budget: f64,
    pub min_order_size: f64,           // $1.00
    pub max_single_side_exposure: f64, // 0.80
    pub min_hedge_ratio: f64,          // 0.20
    pub phases: Vec<PhaseConfig>,
}

pub struct PhaseConfig {
    pub name: String,
    pub start_minute: f64,
    pub end_minute: f64,
    pub budget_allocation: f64,
    pub min_confidence: f64,
}

impl PositionManager {
    pub fn should_trade(&self, distance: f64, mins_remaining: f64) -> TradeDecision;
    pub fn calculate_size(&self, distance: f64, mins_remaining: f64) -> f64;
    pub fn get_allocation(&self, signal: Signal) -> (f64, f64);
    pub fn execute(&mut self, trade: Trade) -> Result<(), Error>;
}
```

### Integration with Main Loop

```rust
loop {
    let btc_price = binance.get_price().await?;
    let distance = btc_price - strike_price;
    let mins_remaining = calculate_time_remaining();
    
    // Position manager decides everything
    match position_manager.should_trade(distance, mins_remaining) {
        TradeDecision::Skip(reason) => {
            log::debug!("Skipping: {}", reason);
            continue;
        }
        TradeDecision::Trade => {
            let size = position_manager.calculate_size(distance, mins_remaining);
            let signal = get_signal(distance, mins_remaining);
            let (up_ratio, down_ratio) = position_manager.get_allocation(signal);
            
            // Execute orders...
            position_manager.execute(trade)?;
        }
    }
    
    tokio::time::sleep(Duration::from_millis(500)).await;
}
```

---

## Tuning Parameters

These parameters can be adjusted based on backtesting results:

| Parameter | Default | Range | Impact |
|-----------|---------|-------|--------|
| Early budget % | 15% | 10-20% | More = aggressive early |
| Early min confidence | 0.80 | 0.70-0.90 | Lower = more early trades |
| Final budget % | 30% | 25-40% | More = aggressive late |
| Final min confidence | 0.40 | 0.30-0.50 | Lower = more final trades |
| Max exposure | 80% | 70-85% | Higher = more directional |
| Trades per phase | 15 | 10-25 | More = smaller sizes |

---

## Summary

The position management system ensures:

1. ✅ **Capital is preserved** for high-conviction moments
2. ✅ **Early uncertainty** is respected (minimal early trading)
3. ✅ **Late conviction** is capitalized (active final phase)
4. ✅ **Position limits** prevent catastrophic losses
5. ✅ **Trade sizing** scales with confidence
6. ✅ **Direction** follows time-adjusted signals

This is **intelligent allocation**, not throttling. Capital flows to the best opportunities.
