# Startup Scripts

Scripts to run the Polymarket Trading Bot with the React Dashboard.

## Prerequisites

1. **Rust toolchain** - Install from https://rustup.rs
2. **Bun** - Install from https://bun.sh
3. **ClickHouse** - Accessible at URL configured in `.env`
4. **Environment variables** - Set in `.env`:
   - `POLY_PRIVATE_KEY` - Wallet private key
   - `POLY_API_KEY` - Polymarket API key
   - `POLY_API_SECRET` - Polymarket API secret
   - `POLY_API_PASSPHRASE` - Polymarket API passphrase
   - `CLICKHOUSE_URL` - ClickHouse hostname
   - `CLICKHOUSE_HTTP_PORT` - ClickHouse HTTP port (default 8123)

The scripts automatically load `.env` from the project root.

## Development Mode

Runs the frontend dev server (with hot reload) alongside the bot.

```powershell
# Windows (PowerShell)
.\scripts\dev.ps1
```

```bash
# Linux/Mac
chmod +x scripts/dev.sh
./scripts/dev.sh
```

**URLs:**
- Frontend: http://localhost:5173 (Vite dev server with hot reload)
- WebSocket: ws://localhost:3001
- REST API: http://localhost:3002/api/*

## Production Mode

Builds the frontend and serves static files from the bot.

```powershell
# Windows (PowerShell)
.\scripts\start.ps1                    # Default: paper mode
.\scripts\start.ps1 -Mode live         # Live trading
.\scripts\start.ps1 -Mode shadow       # Shadow mode (log only)
.\scripts\start.ps1 -SkipBuild         # Skip rebuilding
```

```bash
# Linux/Mac
chmod +x scripts/start.sh
./scripts/start.sh                     # Default: paper mode
./scripts/start.sh --mode live         # Live trading
./scripts/start.sh --mode shadow       # Shadow mode
./scripts/start.sh --skip-build        # Skip rebuilding
```

**URLs:**
- Dashboard: http://localhost:3002 (static files served by bot)
- WebSocket: ws://localhost:3001
- REST API: http://localhost:3002/api/*

## Configuration

Dashboard settings are in `config/bot.toml`:

```toml
[dashboard]
enabled = true
websocket_port = 3001
api_port = 3002
broadcast_interval_ms = 500
pnl_snapshot_interval_secs = 10
# static_dir = "frontend/dist"  # Uncomment for production
```

The `POLY_DASHBOARD_STATIC_DIR` environment variable overrides the config file setting.

## Troubleshooting

### ClickHouse not running
The dashboard will work without ClickHouse, but:
- Session data won't persist
- Historical queries will fail
- Equity curve won't have historical data

Start ClickHouse:
```bash
# Docker
docker run -d --name clickhouse -p 8123:8123 -p 9000:9000 clickhouse/clickhouse-server

# Or use ClickHouse Cloud
```

### Frontend not connecting
1. Check WebSocket port (default 3001) is not blocked
2. Verify bot is running with dashboard enabled
3. Check browser console for errors

### Build failures
```bash
# Clean and rebuild
cargo clean
cd frontend && rm -rf node_modules && bun install && cd ..
```
