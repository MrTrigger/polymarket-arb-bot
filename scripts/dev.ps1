# Development startup script for Polymarket Trading Dashboard
# Starts the bot in paper mode and the frontend dev server concurrently
#
# Usage: .\scripts\dev.ps1
#
# Prerequisites:
#   - Rust toolchain installed
#   - Bun installed
#   - ClickHouse accessible (configured in .env)
#   - Environment variables set in .env

$ErrorActionPreference = "Stop"

Write-Host "========================================" -ForegroundColor Cyan
Write-Host "Polymarket Trading Dashboard - DEV MODE" -ForegroundColor Cyan
Write-Host "========================================" -ForegroundColor Cyan
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
Write-Host "[1/4] Checking prerequisites..." -ForegroundColor Yellow

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
        Write-Host "         Dashboard will work but historical data won't be stored" -ForegroundColor Yellow
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

# Build the bot
Write-Host "[2/4] Building poly-bot..." -ForegroundColor Yellow
cargo build -p poly-bot --release
if ($LASTEXITCODE -ne 0) {
    Write-Host "ERROR: Failed to build poly-bot" -ForegroundColor Red
    exit 1
}
Write-Host "Build complete" -ForegroundColor Green
Write-Host ""

# Install frontend dependencies
Write-Host "[3/4] Installing frontend dependencies..." -ForegroundColor Yellow
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

# Start services
Write-Host "[4/4] Starting services..." -ForegroundColor Yellow
Write-Host ""
Write-Host "  Frontend dev server: http://localhost:5173" -ForegroundColor Cyan
Write-Host "  Bot WebSocket:       ws://localhost:3001" -ForegroundColor Cyan
Write-Host "  Bot REST API:        http://localhost:3002" -ForegroundColor Cyan
Write-Host ""
Write-Host "Press Ctrl+C to stop all services" -ForegroundColor Yellow
Write-Host ""

# Start frontend dev server in background
$frontendJob = Start-Job -ScriptBlock {
    Set-Location $using:PWD
    Push-Location frontend
    bun run dev
}

# Give frontend a moment to start
Start-Sleep -Seconds 2

# Start the bot (foreground)
try {
    cargo run -p poly-bot --release -- --mode paper
} finally {
    # Clean up frontend job on exit
    Write-Host ""
    Write-Host "Stopping frontend server..." -ForegroundColor Yellow
    Stop-Job -Job $frontendJob -ErrorAction SilentlyContinue
    Remove-Job -Job $frontendJob -ErrorAction SilentlyContinue
    Write-Host "Shutdown complete" -ForegroundColor Green
}
