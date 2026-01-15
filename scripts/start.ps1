# Production startup script for Polymarket Trading Dashboard
# Builds the frontend and starts the bot with static file serving
#
# Usage: .\scripts\start.ps1 [-Mode <paper|live|shadow>] [-SkipBuild]
#
# Prerequisites:
#   - Rust toolchain installed
#   - Bun installed
#   - ClickHouse accessible (configured in .env)
#   - Environment variables set in .env

param(
    [ValidateSet("paper", "live", "shadow")]
    [string]$Mode = "paper",

    [switch]$SkipBuild
)

$ErrorActionPreference = "Stop"

Write-Host "============================================" -ForegroundColor Cyan
Write-Host "Polymarket Trading Dashboard - PRODUCTION" -ForegroundColor Cyan
Write-Host "============================================" -ForegroundColor Cyan
Write-Host ""
Write-Host "Mode: $Mode" -ForegroundColor Cyan
Write-Host ""

# Load .env file
if (Test-Path ".env") {
    Get-Content ".env" | ForEach-Object {
        if ($_ -match "^\s*([^#][^=]+)=(.*)$") {
            $name = $matches[1].Trim()
            $value = $matches[2].Trim()
            [Environment]::SetEnvironmentVariable($name, $value, "Process")
        }
    }
    Write-Host "Loaded .env file" -ForegroundColor Green
} else {
    Write-Host "WARNING: .env file not found" -ForegroundColor Yellow
}

# Check prerequisites
Write-Host ""
Write-Host "[1/5] Checking prerequisites..." -ForegroundColor Yellow

if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    Write-Host "ERROR: Rust/Cargo not found. Install from https://rustup.rs" -ForegroundColor Red
    exit 1
}

if (-not (Get-Command bun -ErrorAction SilentlyContinue)) {
    Write-Host "ERROR: Bun not found. Install from https://bun.sh" -ForegroundColor Red
    exit 1
}

# Check ClickHouse using environment variables
$clickhouseUrl = $env:CLICKHOUSE_URL
$clickhousePort = if ($env:CLICKHOUSE_HTTP_PORT) { $env:CLICKHOUSE_HTTP_PORT } else { "8123" }
$clickhouseEndpoint = "http://${clickhouseUrl}:${clickhousePort}/ping"

if ($clickhouseUrl) {
    try {
        $response = Invoke-WebRequest -Uri $clickhouseEndpoint -TimeoutSec 5 -ErrorAction SilentlyContinue
        if ($response.StatusCode -eq 200) {
            Write-Host "ClickHouse ($clickhouseUrl`:$clickhousePort): OK" -ForegroundColor Green
        }
    } catch {
        Write-Host "WARNING: ClickHouse not responding at $clickhouseUrl`:$clickhousePort" -ForegroundColor Yellow
        Write-Host "         Historical data and session persistence will not work" -ForegroundColor Yellow
    }
} else {
    Write-Host "WARNING: CLICKHOUSE_URL not set in .env" -ForegroundColor Yellow
}

Write-Host "Prerequisites OK" -ForegroundColor Green
Write-Host ""

# Kill any existing poly-bot process to avoid build lock
$existingProcess = Get-Process -Name "poly-bot" -ErrorAction SilentlyContinue
if ($existingProcess) {
    Write-Host "Stopping existing poly-bot process..." -ForegroundColor Yellow
    $existingProcess | Stop-Process -Force
    Start-Sleep -Seconds 1
}

if (-not $SkipBuild) {
    # Build the bot
    Write-Host "[2/5] Building poly-bot (release)..." -ForegroundColor Yellow
    cargo build -p poly-bot --release
    if ($LASTEXITCODE -ne 0) {
        Write-Host "ERROR: Failed to build poly-bot" -ForegroundColor Red
        exit 1
    }
    Write-Host "Bot build complete" -ForegroundColor Green
    Write-Host ""

    # Install frontend dependencies
    Write-Host "[3/5] Installing frontend dependencies..." -ForegroundColor Yellow
    Push-Location frontend
    bun install
    if ($LASTEXITCODE -ne 0) {
        Pop-Location
        Write-Host "ERROR: Failed to install frontend dependencies" -ForegroundColor Red
        exit 1
    }
    Pop-Location
    Write-Host "Dependencies installed" -ForegroundColor Green
    Write-Host ""

    # Build frontend
    Write-Host "[4/5] Building frontend for production..." -ForegroundColor Yellow
    Push-Location frontend
    bun run build
    if ($LASTEXITCODE -ne 0) {
        Pop-Location
        Write-Host "ERROR: Failed to build frontend" -ForegroundColor Red
        exit 1
    }
    Pop-Location
    Write-Host "Frontend build complete" -ForegroundColor Green
    Write-Host ""
} else {
    Write-Host "[2-4/5] Skipping builds (--SkipBuild)" -ForegroundColor Yellow
    Write-Host ""
}

# Update config to serve static files
Write-Host "[5/5] Starting bot with dashboard..." -ForegroundColor Yellow
Write-Host ""
Write-Host "  Dashboard:    http://localhost:3002" -ForegroundColor Cyan
Write-Host "  WebSocket:    ws://localhost:3001" -ForegroundColor Cyan
Write-Host "  REST API:     http://localhost:3002/api/*" -ForegroundColor Cyan
Write-Host ""
Write-Host "Press Ctrl+C to stop" -ForegroundColor Yellow
Write-Host ""

# Set environment variable to enable static file serving
$env:POLY_DASHBOARD_STATIC_DIR = "frontend/dist"

# Start the bot
cargo run -p poly-bot --release -- --mode $Mode
