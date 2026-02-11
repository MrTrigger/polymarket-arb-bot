# Polymarket Scalp Strategy

## Overview

Replace the monolithic strategy loop with a **pluggable strategy trait** and implement a new **Scalp Strategy** as the first plugin. The scalp strategy merges an expert's design (drift conviction, fair value model, postOnly maker orders, 4 exit paths) into the existing poly-bot codebase. The current directional binary strategy remains as the default code path, preserving backward compatibility.

## Goals

- **Pluggable architecture**: Define a `TradingStrategy` trait so strategies can be swapped via config (`strategy = "scalp"` or `strategy = "binary"`)
- **Scalp strategy**: Enter on drift conviction signal, manage positions with fair-value-based exits, all entries/exits as `postOnly` limit orders for maker rebates
- **Maker rebate optimization**: Be most active when share prices are near $0.50 (peak rebate zone), adjust min_edge by rebate zone score
- **Backtest-compatible**: All new logic must work with the existing CSV replay + simulated executor + sweep system

## Non-Goals

- Rewriting the data pipeline (WebSocket feeds, orderbook updates, price tracking stay as-is)
- Rewriting the executor layer (SimulatedExecutor and LiveSdkExecutor stay as-is)
- Extracting the existing binary strategy into the trait (existing code path stays monolithic — refactor later if needed)
- Binance perpetual integration (keep using spot + Chainlink — perps deferred)

## Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                     STRATEGY LOOP (modified)                     │
│  Event dispatch, market lifecycle, settlement, data pipeline     │
│  Generic over: D: DataSource, E: Executor                       │
└──────────────────────────┬──────────────────────────────────────┘
                           │ on each tick/event
                           ▼
                   ┌───────────────┐
                   │ Dispatch:     │
                   │ strategy =    │
                   │ "scalp" or    │
                   │ "binary"      │
                   └───┬───────┬───┘
                       │       │
          ┌────────────┘       └────────────┐
          ▼                                 ▼
┌─────────────────────┐       ┌─────────────────────┐
│  Existing Code Path │       │  TradingStrategy     │
│  (binary, default)  │       │  trait dispatch       │
│                     │       │                      │
│  • DirectionalDet.  │       │  ┌────────────────┐  │
│  • ArbDetector      │       │  │ ScalpStrategy  │  │
│  • MakerDetector    │       │  │                │  │
│  • Phase budgets    │       │  │ • Drift conv.  │  │
│  • Kelly sizing     │       │  │ • Fair value   │  │
│  • Early exit       │       │  │ • postOnly     │  │
│                     │       │  │ • 4 exit paths │  │
│  (unchanged)        │       │  │ • Dynamic size │  │
└─────────────────────┘       │  └────────────────┘  │
                              │  (future strategies)  │
                              └───────────────────────┘
```

### TradingStrategy Trait

```rust
trait TradingStrategy: Send {
    fn evaluate(&mut self, ctx: &MarketContext) -> Vec<StrategyAction>;
    fn on_fill(&mut self, ctx: &MarketContext, fill: &OrderFill);
    fn on_cancel(&mut self, ctx: &MarketContext, order_id: &str);
    fn name(&self) -> &str;
}
```

### MarketContext (read-only view passed to strategies)

```rust
struct MarketContext<'a> {
    event_id: &'a str,
    asset: CryptoAsset,
    strike_price: Decimal,
    window_start: DateTime<Utc>,
    window_end: DateTime<Utc>,
    seconds_remaining: i64,
    current_time: DateTime<Utc>,     // Simulated time (backtest safe)

    // Price feeds
    binance_price: Option<Decimal>,
    chainlink_price: Option<Decimal>,
    chainlink_age_secs: f64,

    // Order books (Polymarket)
    yes_book: &'a OrderBook,
    no_book: &'a OrderBook,

    // Position state
    inventory: &'a Inventory,
    has_position: bool,

    // Volatility
    atr: Decimal,
    realized_vol_15m: Decimal,       // From VolEstimator
}
```

### StrategyAction

```rust
enum StrategyAction {
    Hold,
    Enter { side: Side, token_id: String, outcome: Outcome,
            size: Decimal, price: Decimal, order_type: OrderType, reason: String },
    Exit  { token_id: String, outcome: Outcome,
            size: Decimal, price: Decimal, order_type: OrderType, reason: String },
    Cancel { order_id: String },
    CancelAll,
}
```

### ScalpStrategy Internal Flow

```
On each tick:
  │
  ├─ If has active position:
  │   ├─ Path 1: Hold to Resolution — conviction strong AND <2min → Hold
  │   ├─ Path 2: Scalp Out (TP) — captured >threshold% of max gain → Sell (maker)
  │   ├─ Path 3: Soft Stop — held >5min AND <1¢ captured → Sell at break-even
  │   └─ Path 4: Hard Stop — signal reversed OR loss > max → Sell (maker, IOC fallback)
  │
  └─ If no position:
      ├─ drift_conviction = drift / (σ_15m × √time_frac × start_price)
      ├─ Gates: dead zone (<10bps), oracle stale, time, chainlink lag
      ├─ fair_p = Φ(drift / σ_remaining)
      ├─ edge = fair_p - book_price (must be > adjusted_min_edge)
      ├─ Adjust min_edge by rebate zone and oracle stale
      ├─ Limit price: passive (bid+1tick) or aggressive (ask-1tick)
      ├─ Clamp to ceiling: fair_value - adjusted_min_edge
      ├─ Size: base_size × conviction × rebate_bonus
      └─ Return Enter { postOnly GTC }
```

## User Stories

### US-001: Define TradingStrategy trait types
Define the pluggable strategy interface and dispatch point in the strategy loop.

### US-002: VolEstimator
Rolling realized volatility (σ_15m) from Binance price feed, with regime detection.

### US-003: Fair value model + oracle risk
Normal CDF probability model, edge calculation, rebate zone scoring, oracle stale multiplier.

### US-004: ScalpConfig
Configuration struct, TOML parsing, defaults, and sweep parameter wiring.

### US-005: ScalpStrategy signal + entry
Drift conviction signal engine with postOnly maker order placement.

### US-006: ScalpStrategy exit paths
Four exit paths: hold to resolution, scalp out, soft stop, hard stop.

### US-007: ScalpStrategy sizing + rebate zone
Dynamic conviction-based sizing with rebate zone min_edge adjustment.

### US-008: Backtest sweep integration
Wire scalp parameters into apply_parameter() and config TOML.

### US-009: postOnly simulation in backtest
SimulatedExecutor rejects GTC orders that would cross the spread.

### US-010: Integration test
End-to-end backtest on small dataset, verify trades execute and P&L tracking.

## Technical Considerations

### Existing Code to Reuse
- `DataSource` trait + CSV replay — unchanged
- `Executor` trait + `SimulatedExecutor` — supports Side::Sell, OrderType::Gtc, maker rebates
- `postOnly` — already in live_sdk.rs:377, chase.rs handles PostOnlyRejected
- `ATR tracker` (strategy/atr.rs) — foundation for VolEstimator
- `OrderBook` methods (best_bid, best_ask, cost_to_buy, proceeds_to_sell)
- `Inventory` — yes_shares, no_shares, cost_basis
- Trades log, decision log — strategies provide reason strings
- Sweep system (mode/backtest.rs) — add parameter names to apply_parameter()

### Key Files
| File | Action |
|------|--------|
| `strategy/traits.rs` | NEW — TradingStrategy trait, MarketContext, StrategyAction |
| `strategy/scalp.rs` | NEW — ScalpStrategy implementation |
| `strategy/vol.rs` | NEW — VolEstimator |
| `strategy/fair_value.rs` | NEW — Normal CDF, edge, rebate zone, oracle stale |
| `strategy/mod.rs` | MODIFY — Dispatch to trait when strategy="scalp" |
| `config.rs` | MODIFY — ScalpConfig, strategy selector, sweep params |
| `mode/backtest.rs` | MODIFY — apply_parameter() for scalp params |
| `executor/simulated.rs` | MODIFY — postOnly simulation |
| `config/15min.toml` | MODIFY — [strategy.scalp] section |

## Config Reference

```toml
[general]
strategy = "scalp"   # "scalp" or "binary" (default)

[strategy.scalp]
min_conviction = 0.5
high_conviction = 1.5
drift_dead_zone_bps = 10
oracle_heartbeat_secs = 60
oracle_stale_warning_secs = 120
oracle_stale_danger_secs = 180
min_edge_cents = 2.0
post_only = true
stale_order_timeout_secs = 30
no_new_orders_before_end_secs = 60
rebate_zone_tighten = 0.75
rebate_zone_widen = 1.50
base_size_usd = 25.0
max_size_usd = 200.0
max_position_usd = 500.0
scalp_exit_ratio_early = 0.40
scalp_exit_ratio_mid = 0.55
scalp_exit_ratio_late = 0.70
max_loss_per_trade_usd = 50.0
soft_stop_hold_secs = 300
soft_stop_min_edge_cents = 1.0
hard_stop_spot_move_pct = 0.5
cooldown_secs = 10
max_round_trips = 5
force_exit_secs = 30
vol_lookback_secs = 3600
vol_sampling_interval_secs = 60
low_vol_threshold = 0.003
high_vol_threshold = 0.008
extreme_vol_threshold = 0.015
```
