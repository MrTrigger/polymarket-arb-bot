# Polymarket Arbitrage Bot

HFT-optimized trading bot for crypto binary options on Polymarket's 15-minute and 1-hour up/down markets (BTC, ETH, SOL, XRP).

## Overview

This bot exploits mispricings when YES + NO shares sum to less than $1.00, and uses directional signals based on spot price distance from strike to allocate capital asymmetrically.

### Backtest Results

On 7 days of historical data (BTC only):
- **+22.92% return** on single-day test
- Parameter sweep across 168 combinations to optimize allocation ratios and confidence thresholds

## Architecture

Two separate binaries in a Cargo workspace:

| Binary | Purpose |
|--------|---------|
| `poly-collect` | Data collection: historical (API) or live (WebSocket). Outputs to CSV or ClickHouse. |
| `poly-bot` | Trading strategy execution with multiple modes (live, paper, shadow, backtest) |

Shared types and utilities live in `poly-common` crate.

### Design Principles

- **Lock-free hot path**: DashMap and atomics for shared state, no Mutex on trading path
- **Zero-copy parsing**: Minimize allocations in WebSocket message handling
- **Fire-and-forget observability**: <10ns overhead on hot path via bounded channels

## Prerequisites

- Rust 1.75+ (install via [rustup](https://rustup.rs/))
- ClickHouse database for data storage
- Polymarket API credentials (for live trading)

## Build

```bash
# Build all crates
cargo build --release

# Build single crate
cargo build -p poly-bot --release

# Run tests
cargo test

# Lint
cargo clippy -- -D warnings
```

## Configuration

### Environment Variables

Sensitive values should be set via environment variables:

```bash
# Polymarket credentials
export POLY_PRIVATE_KEY="your-wallet-private-key"
export POLY_API_KEY="your-api-key"
export POLY_API_SECRET="your-api-secret"
export POLY_API_PASSPHRASE="your-passphrase"

# ClickHouse
export CLICKHOUSE_URL="http://localhost:8123"
export CLICKHOUSE_USER="default"
export CLICKHOUSE_PASSWORD="your-password"

# Alerts (optional)
export ALERT_WEBHOOK_URL="https://your-webhook-url"
```

### Config Files

- `config/bot.toml` - Main bot configuration (mode, risk, dashboard, engines)
- `config/strategy.toml` - Strategy parameters (phases, signals, allocation, sweep)

## Tools

### 1. Data Collection (`poly-collect`)

Collects data from Binance (spot prices) and Polymarket (orderbooks, price history). Supports both historical and live modes, with output to CSV or ClickHouse.

```bash
# Historical mode (default) - fetch past data to CSV
cargo run -p poly-collect --release

# With custom config
cargo run -p poly-collect --release -- --config config/collect.toml

# Override assets
cargo run -p poly-collect --release -- --assets BTC,ETH
```

Configure in `config/collect.toml`:

```toml
[collection]
mode = "history"    # "history" or "live"
days = 7            # Collect last N days

[output]
mode = "csv"        # "csv", "clickhouse", or "both"
csv_dir = "data"    # Output directory
```

**Run this first** to build historical data for backtesting.

### 2. Trading Bot (`poly-bot`)

The main trading bot with multiple operating modes.

#### Modes

| Mode | Description |
|------|-------------|
| `live` | Real trading with real money |
| `paper` | Real data, simulated execution |
| `shadow` | Real data, log-only (no execution) |
| `backtest` | Historical data replay |

#### Running the Bot

```bash
# Paper trading (recommended for testing)
cargo run -p poly-bot --release -- --mode paper

# Live trading (requires API credentials)
cargo run -p poly-bot --release -- --mode live

# Shadow mode (observe only)
cargo run -p poly-bot --release -- --mode shadow

# Backtest mode (from CSV data)
cargo run -p poly-bot --release -- --mode backtest --assets BTC --start 2026-01-10 --end 2026-01-10

# Backtest with parameter sweep
cargo run -p poly-bot --release -- --mode backtest --assets BTC --start 2026-01-08 --end 2026-01-15 --sweep
```

#### Data Sources for Backtesting

The backtest can use either:
- **CSV files**: Place in `data/<date_range>_<window>/` (e.g., `data/2026-01-08_2026-01-15_15min/`)
  - `binance_spot_prices.csv` - Spot price history
  - `polymarket_market_windows.csv` - Market window metadata
  - `polymarket_price_history.csv` - YES/NO token price history
- **ClickHouse**: Historical data from live collection

#### Command Line Options

```bash
poly-bot [OPTIONS]

Options:
  -m, --mode <MODE>           Trading mode: live, paper, shadow, backtest
  -c, --config <FILE>         Config file path (default: config/bot.toml)
  -w, --window <WINDOW>       Market window duration: 15min or 1h (default: 1h)
  --clickhouse-url <URL>      ClickHouse HTTP URL (overrides config)
  --assets <ASSETS>           Comma-separated assets (overrides config)
  --start <DATE>              Backtest start date (YYYY-MM-DD)
  --end <DATE>                Backtest end date (YYYY-MM-DD)
  --speed <SPEED>             Backtest speed (0 = max, 1.0 = real-time)
  --sweep                     Enable parameter sweep mode for backtest
```

## Parameter Sweep

Optimize strategy parameters by running backtests across multiple configurations.

### Setup

1. **Collect historical data** first using `poly-collect`

2. **Configure sweep parameters** in `config/strategy.toml`:

```toml
[sweep]
# Always swept (used by both modes)
strong_ratios = [0.75, 0.78, 0.80, 0.82, 0.85, 0.88, 0.90]
lean_ratios = [0.55, 0.60, 0.65, 0.70]

# Mode 1: Edge-based (phase thresholds ignored when edge_factor > 0)
edge_factors = [0.05, 0.10, 0.15, 0.20, 0.25, 0.30]

# Mode 2: Phase-based (use when edge_factor = 0)
# Uncomment these and comment out edge_factors above to test phase mode
# early_thresholds = [0.75, 0.80, 0.85]
# final_thresholds = [0.35, 0.40, 0.45]
```

**Note:** Edge-based and phase-based modes are mutually exclusive. Comment out whichever you're not testing.

3. **Enable sweep** in `config/bot.toml`:

```toml
[backtest]
start_date = "2025-01-01"
end_date = "2025-01-14"
sweep_enabled = true
```

4. **Run the sweep**:

```bash
# Run sweep with CLI flag (ignores bot.toml sweep_enabled setting)
cargo run -p poly-bot --release -- --mode backtest --sweep

# Or with specific date range and assets
cargo run -p poly-bot --release -- --mode backtest --assets BTC --start 2026-01-08 --end 2026-01-15 --sweep
```

Results are logged showing P&L and return for each parameter combination. The sweep tests all combinations of:
- `strong_ratios` × `lean_ratios` × `edge_factors` (or `thresholds`)

Example output:
```
Sweep results (168 runs):
  Run 1: P&L=$2292.23, Return=22.92%, Params={"strong_ratio": 0.82, "lean_ratio": 0.65, "edge_factor": 0.10}
  ...
Best run: P&L=$3500.00, Params={"strong_ratio": 0.85, ...}
```

## Dashboard

When running in paper or live mode, the bot exposes:

- **WebSocket server** (port 3001): Real-time state broadcast
- **REST API** (port 3002): Historical queries

Configure in `config/bot.toml`:

```toml
[dashboard]
enabled = true
websocket_port = 3001
api_port = 3002
broadcast_interval_ms = 500
```

## Strategy Overview

### Engines

The bot runs multiple trading engines in priority order:

1. **Directional Engine**: Signal-based asymmetric allocations based on spot price distance from strike
2. **Arbitrage Engine**: Buy YES+NO when combined cost < $1.00 (risk-free profit)
3. **Maker Engine**: Passive limit orders for rebates (disabled by default)

### Phase-Based Trading

Markets are divided into phases based on time remaining:

| Phase | Time Remaining | Confidence Threshold |
|-------|---------------|---------------------|
| Early | >10 min | 0.80 (very selective) |
| Build | 5-10 min | 0.60 (selective) |
| Core | 2-5 min | 0.50 (active) |
| Final | <2 min | 0.40 (most active) |

### Edge-Based Mode (Alternative)

When `max_edge_factor > 0`, uses dynamic confidence requirement:
```
required_confidence = favorable_price + (edge_factor * time_remaining / window_duration)
```

Set `max_edge_factor = 0` to use phase-based thresholds (slightly better in backtests: 18.2% vs 18.0% ROI).

## Data Storage

### CSV Files (Recommended for Backtesting)

Generated by `poly-collect` in `data/<date_range>_<window>/`:
- `binance_spot_prices.csv` - Spot price history (asset, price, timestamp)
- `polymarket_market_windows.csv` - Market metadata (event_id, tokens, strike, times)
- `polymarket_price_history.csv` - YES/NO token prices over time

### ClickHouse (Optional, for Live Trading)

Tables for real-time data storage:
- `orderbook_snapshots` - Full orderbook snapshots
- `orderbook_deltas` - Incremental orderbook updates
- `spot_prices` - Binance spot prices
- `market_windows` - Market window metadata
- `decisions` - Trading decisions for analysis
- `pnl_snapshots` - P&L snapshots for equity curve

## Project Structure

```
crates/
├── poly-common/          # Shared types, ClickHouse client
├── poly-collect/         # Data collection binary
│   └── src/
│       ├── binance.rs      # Binance WebSocket client
│       ├── clob.rs         # Polymarket CLOB client
│       ├── history.rs      # Historical data fetcher
│       ├── csv_writer.rs   # CSV output
│       └── data_writer.rs  # Output abstraction
└── poly-bot/             # Trading bot binary
    └── src/
        ├── config.rs       # Configuration loading
        ├── state.rs        # Global state (DashMaps, atomics)
        ├── data_source/    # Live WebSocket, CSV replay
        ├── executor/       # Executor trait + implementations
        │   ├── position_manager.rs  # Common position/risk management
        │   ├── simulated.rs         # Paper trading & backtesting
        │   └── live.rs              # Real API execution
        ├── strategy/       # Trading strategy logic
        ├── risk/           # Risk management
        ├── observability/  # Decision capture, anomaly detection
        ├── dashboard/      # WebSocket + REST API servers
        └── mode/           # Live, paper, shadow, backtest modes

config/
├── bot.toml              # Main bot configuration
├── collect.toml          # Data collection configuration
└── strategy.toml         # Strategy parameters and sweep config

data/                     # CSV data files (generated by poly-collect)
└── <date_range>_<window>/
    ├── binance_spot_prices.csv
    ├── polymarket_market_windows.csv
    └── polymarket_price_history.csv
```

## Executor Architecture

The bot uses a **common code** architecture for position and risk management:

```
┌─────────────────────────────────────────────────────────────────┐
│                 POSITION MANAGER (Common Code)                   │
│  • check_limits() - enforce exposure limits                      │
│  • record_fill() - track positions                               │
│  • settle_market() - calculate PnL                               │
└─────────────────────────────────────────────────────────────────┘
                              │
              ┌───────────────┴───────────────┐
              ▼                               ▼
┌─────────────────────────┐     ┌─────────────────────────┐
│   SimulatedExecutor     │     │     LiveExecutor        │
│   (paper/backtest)      │     │   (real trading)        │
│   • Simulated fills     │     │   • Polymarket API      │
│   • Balance tracking    │     │   • EIP-712 signing     │
└─────────────────────────┘     └─────────────────────────┘
```

Both executors share the same `PositionManager` for:
- **Risk limits**: `max_market_exposure`, `max_total_exposure`
- **Position tracking**: YES/NO shares, cost basis per market
- **Settlement**: PnL calculation when markets expire

This ensures identical risk behavior between backtesting and live trading.

## Development

### Running Tests

```bash
# All tests
cargo test

# Specific crate
cargo test -p poly-bot

# Specific test
cargo test strategy::arb
```

### Logging

Control log level via config or environment:

```bash
# Via environment
RUST_LOG=info,poly_bot::strategy=debug cargo run -p poly-bot --release -- --mode paper

# Via config (bot.toml)
[general]
log_level = "debug"
```

## License

MIT
