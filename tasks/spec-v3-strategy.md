# Spec: V3 Three-Engine Strategy

## Source
`spec/POLYMARKET_BOT_SPEC_v3_RUST.md`

## Project Context

### Stack
- Backend: Rust (Cargo workspace)
- Async runtime: Tokio
- Financial math: `rust_decimal::Decimal` (never f64)
- Concurrency: DashMap + atomics (lock-free hot path)

### Relevant Existing Code
- `crates/poly-bot/src/strategy/arb.rs`: Arbitrage detection (Engine 1 basis)
- `crates/poly-bot/src/strategy/sizing.rs`: Limit-based position sizing
- `crates/poly-bot/src/strategy/toxic.rs`: Toxic flow detection
- `crates/poly-bot/src/strategy/mod.rs`: StrategyLoop orchestrator
- `crates/poly-bot/src/risk/circuit_breaker.rs`: Execution failure protection
- `crates/poly-bot/src/config.rs`: TOML config with CLI/env overrides
- `crates/poly-bot/src/state.rs`: GlobalState with DashMaps and atomics
- `crates/poly-bot/src/types.rs`: MarketState, OrderBook, Inventory
- `crates/poly-bot/src/executor/`: Executor trait + implementations

### Patterns to Follow
- Error handling: `thiserror` with custom error enums
- Config: TOML structs with `#[serde(default)]`
- Async: `async fn` everywhere, Tokio runtime
- Financial math: Always `Decimal`, never `f64`
- Hot path: No Mutex, use atomics and DashMap
- Observability: `try_send()` to bounded channels (fire-and-forget)

### Utilities to Leverage
- `ArbDetector`: Existing arb detection (wrap as Engine 1)
- `PositionSizer`: Existing limit-based sizing (use in hybrid mode)
- `CircuitBreaker`: Existing execution failure tracking
- `ToxicFlowDetector`: Apply to all engines
- `GlobalState`: Extend with new fields
- `TrackedMarket`: Extend with engine state

---

## Work Breakdown

### Phase 1: Core Types & Signal System
Goal: Add foundational types for the three-engine strategy

#### 1.1 Add Signal enum and detection
- **What**: Implement directional signal based on BTC price vs strike
- **Where**: `crates/poly-bot/src/strategy/signal.rs` (new file)
- **Builds on**: Spec Section 2 signal logic
- **Acceptance**: Unit tests pass for all signal thresholds

##### Subtasks:
- [ ] Create `Signal` enum: `StrongUp`, `LeanUp`, `Neutral`, `LeanDown`, `StrongDown`
- [ ] Implement `Signal::up_ratio()` and `down_ratio()` methods
- [ ] Implement `get_signal(spot_price, strike_price, minutes_remaining)` function
- [ ] Implement `get_thresholds(minutes_remaining)` with time-based values
- [ ] Add unit tests for each time bracket and signal level
- [ ] Export from `strategy/mod.rs`

#### 1.2 Add Confidence types for sizing
- **What**: Types for confidence-based order sizing
- **Where**: `crates/poly-bot/src/strategy/confidence.rs` (new file)
- **Builds on**: Spec Section 5 confidence factors
- **Acceptance**: Confidence calculation matches spec examples

##### Subtasks:
- [ ] Create `ConfidenceFactors` struct (distance, minutes_remaining, signal, book_imbalance, favorable_depth)
- [ ] Create `Confidence` struct (time, distance, signal, book components)
- [ ] Implement `Confidence::total_multiplier()` using geometric mean
- [ ] Add constants for multiplier caps (0.5x - 3.0x)
- [ ] Add unit tests for confidence calculations

#### 1.3 Add Position and trade decision types
- **What**: Types for position tracking and multi-engine decisions
- **Where**: `crates/poly-bot/src/types.rs` (extend existing)
- **Builds on**: Existing `Inventory` type
- **Acceptance**: Types compile and integrate with existing code

##### Subtasks:
- [ ] Add `EngineType` enum: `Arbitrage`, `Directional`, `MakerRebates`
- [ ] Add `Position` struct (up_shares, down_shares, up_cost, down_cost)
- [ ] Implement `Position::total_cost()`, `up_ratio()`, `min_side_ratio()`
- [ ] Implement `Position::calculate_pnl(winning_side)`
- [ ] Add `TradeDecision` enum variants for each engine type

---

### Phase 2: Fee & Rewards API Integration
Goal: Fetch fee rates and reward configs from Polymarket API

#### 2.1 Add fee rate client
- **What**: Client to fetch fee rates per token from CLOB API
- **Where**: `crates/poly-bot/src/api/fees.rs` (new file)
- **Builds on**: Polymarket `GET /fee-rate` endpoint
- **Acceptance**: Can fetch and cache fee rate for a token

##### Subtasks:
- [ ] Create `api/` module in poly-bot
- [ ] Add `FeeRateResponse` struct matching API response
- [ ] Implement `fetch_fee_rate(token_id) -> Result<u32>`
- [ ] Add in-memory cache (per market session)
- [ ] Add error handling for API failures
- [ ] Add integration test with real API (feature-gated)

#### 2.2 Add rewards client
- **What**: Client to fetch reward configs and track earnings
- **Where**: `crates/poly-bot/src/api/rewards.rs` (new file)
- **Builds on**: Polymarket rewards endpoints
- **Acceptance**: Can fetch current rewards and user earnings

##### Subtasks:
- [ ] Add `CurrentRewardResponse`, `RewardsConfig` types
- [ ] Add `UserEarningResponse`, `TotalUserEarningResponse` types
- [ ] Implement `fetch_current_rewards() -> Result<Vec<CurrentRewardResponse>>`
- [ ] Implement `fetch_reward_percentages() -> Result<HashMap<String, Decimal>>`
- [ ] Implement `fetch_user_earnings(date) -> Result<TotalUserEarningResponse>`
- [ ] Add periodic earnings tracking task

#### 2.3 Create MarketSession with cached API data
- **What**: Per-market session state with cached fee rate and reward config
- **Where**: `crates/poly-bot/src/types.rs` (extend)
- **Builds on**: Fee and rewards clients
- **Acceptance**: MarketSession initializes with API data

##### Subtasks:
- [ ] Add `MarketSession` struct (market_id, up_token, down_token, strike, fee_rate_bps, reward_config)
- [ ] Implement `MarketSession::new(client, market) -> Result<Self>`
- [ ] Integrate into `TrackedMarket` or `StrategyLoop`
- [ ] Add tests for session initialization

---

### Phase 3: Configurable Sizing System
Goal: Implement confidence-based sizing with configurable modes

#### 3.1 Add TradingConfig with balance scaling
- **What**: Configuration that scales with available balance
- **Where**: `crates/poly-bot/src/config.rs` (extend)
- **Builds on**: Spec Section 5 scaling formula
- **Acceptance**: Config calculates correct market_budget and base_order_size

##### Subtasks:
- [ ] Add `TradingConfig` struct with `available_balance`
- [ ] Add `max_market_allocation` (default 0.20)
- [ ] Add `expected_trades_per_market` (default 200)
- [ ] Add `min_order_size` (default $1.00)
- [ ] Add `max_confidence_multiplier` (default 3.0)
- [ ] Add `min_hedge_ratio` (default 0.20)
- [ ] Implement `market_budget()` and `base_order_size()` methods
- [ ] Add unit tests for scaling at different balance levels

#### 3.2 Implement ConfidenceSizer
- **What**: Confidence-based order sizing from spec
- **Where**: `crates/poly-bot/src/strategy/confidence_sizing.rs` (new file)
- **Builds on**: Spec Section 5 OrderSizer
- **Acceptance**: Sizing matches spec examples

##### Subtasks:
- [ ] Create `ConfidenceSizer` struct with TradingConfig, spent, trades
- [ ] Implement `reset()` for new market
- [ ] Implement `get_order_size(factors) -> OrderSizeResult`
- [ ] Implement `record_trade(size)`
- [ ] Implement confidence calculators: `time_confidence()`, `distance_confidence()`, `signal_confidence()`, `book_confidence()`
- [ ] Add `OrderSizeResult` enum (Trade, BudgetExhausted)
- [ ] Add unit tests matching spec scenarios

#### 3.3 Add configurable sizing mode
- **What**: Config to select sizing strategy (limits, confidence, hybrid)
- **Where**: `crates/poly-bot/src/config.rs` (extend)
- **Builds on**: Existing SizingConfig + new ConfidenceSizer
- **Acceptance**: Can switch between modes via config

##### Subtasks:
- [ ] Add `SizingMode` enum: `Limits`, `Confidence`, `Hybrid`
- [ ] Add `sizing_mode` field to config
- [ ] Create `SizingStrategy` trait abstracting both approaches
- [ ] Implement trait for `PositionSizer` (existing)
- [ ] Implement trait for `ConfidenceSizer` (new)
- [ ] Implement `HybridSizer` that applies confidence then caps with limits
- [ ] Add factory function to create sizer from config

---

### Phase 4: Configurable Risk System
Goal: Add P&L-based risk management alongside circuit breaker

#### 4.1 Implement PnlRiskManager
- **What**: Daily P&L and consecutive loss tracking from spec
- **Where**: `crates/poly-bot/src/risk/pnl.rs` (new file)
- **Builds on**: Spec Section 5 RiskManager
- **Acceptance**: Enforces daily loss limit and consecutive loss stop

##### Subtasks:
- [ ] Create `PnlRiskManager` struct (config, daily_pnl, consecutive_losses, trading_enabled, markets_traded)
- [ ] Implement `check_trade(size, position) -> TradeDecision`
- [ ] Add `TradeDecision` enum: `Approve`, `Reject`, `ReduceSize`, `RebalanceRequired`
- [ ] Implement `record_market_result(pnl)`
- [ ] Implement `new_day()` reset
- [ ] Add daily loss limit check (10% of capital)
- [ ] Add consecutive loss check (3 losses -> disable)
- [ ] Add hedge ratio check (min 20% each side)
- [ ] Add unit tests for each risk rule

#### 4.2 Add configurable risk mode
- **What**: Config to select risk strategy (circuit_breaker, daily_pnl, both)
- **Where**: `crates/poly-bot/src/config.rs` (extend)
- **Builds on**: Existing CircuitBreaker + new PnlRiskManager
- **Acceptance**: Can stack risk layers via config

##### Subtasks:
- [ ] Add `RiskMode` enum: `CircuitBreaker`, `DailyPnl`, `Both`
- [ ] Add `risk_mode` field to RiskConfig
- [ ] Create `RiskChecker` trait abstracting both approaches
- [ ] Implement `CompositeRiskChecker` that runs both if mode is `Both`
- [ ] Integrate with StrategyLoop to check before trades
- [ ] Add tests for each mode

---

### Phase 5: Engine 2 - Directional Trading
Goal: Implement directional betting based on spot vs strike

#### 5.1 Implement DirectionalDetector
- **What**: Detect directional opportunities using signal system
- **Where**: `crates/poly-bot/src/strategy/directional.rs` (new file)
- **Builds on**: Signal system from Phase 1
- **Acceptance**: Generates correct UP/DOWN ratios per signal

##### Subtasks:
- [ ] Create `DirectionalDetector` struct with config
- [ ] Create `DirectionalOpportunity` struct (signal, up_ratio, down_ratio, confidence)
- [ ] Implement `detect(market_state, spot_price, minutes_remaining) -> Option<DirectionalOpportunity>`
- [ ] Implement allocation calculation based on signal ratios
- [ ] Add skip reasons: `Neutral`, `InsufficientTime`, `NoQuotes`
- [ ] Add unit tests for each signal level

#### 5.2 Implement directional execution
- **What**: Execute directional trades with proper UP/DOWN split
- **Where**: `crates/poly-bot/src/strategy/mod.rs` (extend StrategyLoop)
- **Builds on**: Existing executor infrastructure
- **Acceptance**: Places orders with correct ratios

##### Subtasks:
- [ ] Add `execute_directional()` method to StrategyLoop
- [ ] Calculate UP and DOWN sizes from signal ratios
- [ ] Calculate maker prices (inside spread)
- [ ] Place orders with `post_only: true`
- [ ] Update position tracking
- [ ] Record trades with sizer
- [ ] Add integration test

---

### Phase 6: Engine 3 - Maker Rebates (Optional)
Goal: Implement passive maker order strategy

#### 6.1 Implement MakerDetector
- **What**: Detect maker rebate opportunities
- **Where**: `crates/poly-bot/src/strategy/maker.rs` (new file)
- **Builds on**: Order book state and reward config
- **Acceptance**: Places competitive maker orders

##### Subtasks:
- [ ] Create `MakerDetector` struct with config
- [ ] Create `MakerOpportunity` struct (token_id, side, price, size, expected_rebate)
- [ ] Implement spread analysis to find optimal maker price
- [ ] Implement `detect(market_state, reward_config) -> Option<MakerOpportunity>`
- [ ] Consider `rewards_min_size` and `rewards_max_spread` from API
- [ ] Add unit tests

#### 6.2 Implement maker order management
- **What**: Place and manage passive maker orders
- **Where**: `crates/poly-bot/src/strategy/mod.rs` (extend)
- **Builds on**: Executor with `post_only` support
- **Acceptance**: Maintains maker orders, cancels when needed

##### Subtasks:
- [ ] Add `execute_maker()` method
- [ ] Track active maker orders per market
- [ ] Implement order refresh logic (cancel stale, place new)
- [ ] Handle partial fills
- [ ] Add integration test

---

### Phase 7: Multi-Engine Orchestration
Goal: Coordinate all three engines with priority and conflict resolution

#### 7.1 Add engine configuration
- **What**: Config to enable/disable each engine with priority
- **Where**: `crates/poly-bot/src/config.rs` (extend)
- **Builds on**: Existing config patterns
- **Acceptance**: Can configure which engines run

##### Subtasks:
- [ ] Add `EnginesConfig` struct
- [ ] Add `ArbitrageEngineConfig` (enabled, min_margin_bps)
- [ ] Add `DirectionalEngineConfig` (enabled, signal_thresholds, allocation_ratios)
- [ ] Add `MakerEngineConfig` (enabled, spread_inside_bps)
- [ ] Add `priority: Vec<EngineType>` for conflict resolution
- [ ] Add TOML serialization tests

#### 7.2 Implement DecisionAggregator
- **What**: Aggregate and prioritize decisions from multiple engines
- **Where**: `crates/poly-bot/src/strategy/aggregator.rs` (new file)
- **Builds on**: Individual engine detectors
- **Acceptance**: Picks best decision based on priority

##### Subtasks:
- [ ] Create `EngineDecision` wrapper (engine_type, opportunity, sizing)
- [ ] Create `DecisionAggregator` struct
- [ ] Implement `aggregate(decisions: Vec<EngineDecision>) -> Option<EngineDecision>`
- [ ] Apply priority ordering from config
- [ ] Handle conflicts (e.g., arb overrides directional)
- [ ] Add unit tests for priority scenarios

#### 7.3 Integrate engines into StrategyLoop
- **What**: Run all enabled engines and execute winning decision
- **Where**: `crates/poly-bot/src/strategy/mod.rs` (refactor)
- **Builds on**: All previous phases
- **Acceptance**: Multi-engine strategy runs correctly

##### Subtasks:
- [ ] Add engine instances to StrategyLoop (arb, directional, maker)
- [ ] Add aggregator instance
- [ ] Refactor `check_opportunities()` to run all enabled engines
- [ ] Route execution to correct handler based on engine type
- [ ] Tag observability decisions with engine source
- [ ] Add integration test with multiple engines

---

### Phase 8: Trade Interval & Execution Polish
Goal: Add API rate limiting and execution refinements

#### 8.1 Implement TradeInterval
- **What**: Minimum interval between trades to avoid API rate limits
- **Where**: `crates/poly-bot/src/executor/interval.rs` (new file)
- **Builds on**: Spec Section 5 TradeInterval
- **Acceptance**: Enforces minimum 500ms between trades

##### Subtasks:
- [ ] Create `TradeInterval` struct (min_interval, last_trade)
- [ ] Implement `can_trade() -> bool`
- [ ] Implement `record_trade()`
- [ ] Implement `time_until_next() -> Duration`
- [ ] Integrate into StrategyLoop main loop
- [ ] Add unit tests

#### 8.2 Add maker price calculation
- **What**: Calculate optimal maker price inside spread
- **Where**: `crates/poly-bot/src/strategy/pricing.rs` (new file)
- **Builds on**: Spec Section 6 get_maker_price logic
- **Acceptance**: Places orders inside spread for fill probability

##### Subtasks:
- [ ] Implement `get_maker_price(book) -> Decimal`
- [ ] Handle different spread sizes (>3%, 1-3%, <1%)
- [ ] Add unit tests for various spread scenarios

---

### Phase 9: Testing & Validation
Goal: Comprehensive tests for the new strategy

#### 9.1 Unit tests for all new modules
- **What**: Unit tests for each new component
- **Where**: `crates/poly-bot/src/**/*.rs` (inline tests)
- **Builds on**: Existing test patterns
- **Acceptance**: All unit tests pass

##### Subtasks:
- [ ] Signal detection tests (all time brackets)
- [ ] Confidence calculation tests (spec examples)
- [ ] TradingConfig scaling tests
- [ ] ConfidenceSizer tests (budget exhaustion, multipliers)
- [ ] PnlRiskManager tests (all rules)
- [ ] DirectionalDetector tests
- [ ] DecisionAggregator priority tests

#### 9.2 Integration tests
- **What**: End-to-end tests for multi-engine strategy
- **Where**: `crates/poly-bot/tests/` (new files)
- **Builds on**: Existing integration test patterns
- **Acceptance**: Strategy runs correctly in backtest mode

##### Subtasks:
- [ ] Add `test_directional_strategy.rs`
- [ ] Add `test_multi_engine.rs`
- [ ] Add `test_confidence_sizing.rs`
- [ ] Add `test_pnl_risk.rs`
- [ ] Verify backtest produces expected results

#### 9.3 Backtest validation
- **What**: Validate strategy against historical data
- **Where**: `crates/poly-bot/` (backtest mode)
- **Builds on**: Existing backtest infrastructure
- **Acceptance**: P&L matches expected from spec economics

##### Subtasks:
- [ ] Run backtest with directional engine enabled
- [ ] Compare results to spec Section 8 expected economics
- [ ] Tune thresholds if needed
- [ ] Document findings in `tasks/progress.txt`

---

## Integration Points

- **Backward compatible**: Default config runs only arbitrage engine (existing behavior)
- **Existing state**: GlobalState extended, not replaced
- **Existing risk**: CircuitBreaker preserved, PnlRiskManager added alongside
- **Existing sizing**: PositionSizer preserved, used in hybrid mode
- **Observability**: Decisions tagged with `EngineType` for analysis

## Testing Strategy

Based on spec and project conventions:
- **Unit tests**: Inline in each module, test pure functions
- **Integration tests**: In `tests/` directory, test component interactions
- **Backtest validation**: Run against historical data, compare to spec expectations

## Open Questions

- None - all clarifications resolved

---

## Summary

| Phase | Tasks | New Files |
|-------|-------|-----------|
| 1. Core Types | 3 | `signal.rs`, `confidence.rs` |
| 2. API Integration | 3 | `api/fees.rs`, `api/rewards.rs` |
| 3. Sizing System | 3 | `confidence_sizing.rs` |
| 4. Risk System | 2 | `risk/pnl.rs` |
| 5. Directional Engine | 2 | `directional.rs` |
| 6. Maker Engine | 2 | `maker.rs` |
| 7. Orchestration | 3 | `aggregator.rs` |
| 8. Execution Polish | 2 | `interval.rs`, `pricing.rs` |
| 9. Testing | 3 | test files |

**Total: 23 tasks across 9 phases**
