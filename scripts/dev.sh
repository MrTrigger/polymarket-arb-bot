#!/usr/bin/env bash
# Development startup script for Polymarket Trading Dashboard
# Starts the bot in paper mode and the frontend dev server concurrently
#
# Usage: ./scripts/dev.sh
#
# Prerequisites:
#   - Rust toolchain installed
#   - Bun installed
#   - ClickHouse accessible (configured in .env)
#   - Environment variables set in .env

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"

cd "$PROJECT_DIR"

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
CYAN='\033[0;36m'
NC='\033[0m' # No Color

echo "========================================"
echo "Polymarket Trading Dashboard - DEV MODE"
echo "========================================"
echo ""

# Load .env file
if [ -f ".env" ]; then
    set -a
    source .env
    set +a
    echo -e "${GREEN}Loaded .env file${NC}"
else
    echo -e "${YELLOW}WARNING: .env file not found${NC}"
fi

# Cleanup function
cleanup() {
    echo ""
    echo -e "${YELLOW}Stopping services...${NC}"
    if [ -n "$FRONTEND_PID" ]; then
        kill $FRONTEND_PID 2>/dev/null || true
    fi
    echo -e "${GREEN}Shutdown complete${NC}"
    exit 0
}

trap cleanup SIGINT SIGTERM

# Check prerequisites
echo ""
echo -e "${YELLOW}[1/4] Checking prerequisites...${NC}"

if ! command -v cargo &> /dev/null; then
    echo -e "${RED}ERROR: Rust/Cargo not found. Install from https://rustup.rs${NC}"
    exit 1
fi

if ! command -v bun &> /dev/null; then
    echo -e "${RED}ERROR: Bun not found. Install from https://bun.sh${NC}"
    exit 1
fi

# Check ClickHouse using environment variables
CLICKHOUSE_PORT="${CLICKHOUSE_HTTP_PORT:-8123}"
if [ -n "$CLICKHOUSE_URL" ]; then
    if curl -s --connect-timeout 5 "http://${CLICKHOUSE_URL}:${CLICKHOUSE_PORT}/ping" > /dev/null 2>&1; then
        echo -e "${GREEN}ClickHouse (${CLICKHOUSE_URL}:${CLICKHOUSE_PORT}): OK${NC}"
    else
        echo -e "${YELLOW}WARNING: ClickHouse not responding at ${CLICKHOUSE_URL}:${CLICKHOUSE_PORT}${NC}"
        echo -e "${YELLOW}         Dashboard will work but historical data won't be stored${NC}"
    fi
else
    echo -e "${YELLOW}WARNING: CLICKHOUSE_URL not set in .env${NC}"
fi

echo -e "${GREEN}Prerequisites OK${NC}"
echo ""

# Kill any existing poly-bot process
if pgrep -x "poly-bot" > /dev/null 2>&1; then
    echo -e "${YELLOW}Stopping existing poly-bot process...${NC}"
    pkill -x "poly-bot" || true
    sleep 1
fi

# Build the bot
echo -e "${YELLOW}[2/4] Building poly-bot...${NC}"
cargo build -p poly-bot --release
echo -e "${GREEN}Build complete${NC}"
echo ""

# Install frontend dependencies
echo -e "${YELLOW}[3/4] Installing frontend dependencies...${NC}"
cd frontend
bun install
cd ..
echo -e "${GREEN}Dependencies installed${NC}"
echo ""

# Start services
echo -e "${YELLOW}[4/4] Starting services...${NC}"
echo ""
echo -e "  ${CYAN}Frontend dev server: http://localhost:5173${NC}"
echo -e "  ${CYAN}Bot WebSocket:       ws://localhost:3001${NC}"
echo -e "  ${CYAN}Bot REST API:        http://localhost:3002${NC}"
echo ""
echo -e "${YELLOW}Press Ctrl+C to stop all services${NC}"
echo ""

# Start frontend dev server in background
cd frontend
bun run dev &
FRONTEND_PID=$!
cd ..

# Give frontend a moment to start
sleep 2

# Start the bot (foreground)
cargo run -p poly-bot --release -- --mode paper

# Cleanup on normal exit
cleanup
