#!/usr/bin/env bash
# Production startup script for Polymarket Trading Dashboard
# Builds the frontend and starts the bot with static file serving
#
# Usage: ./scripts/start.sh [--mode <paper|live|shadow>] [--skip-build]
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

# Default values
MODE="paper"
SKIP_BUILD=false

# Parse arguments
while [[ $# -gt 0 ]]; do
    case $1 in
        --mode|-m)
            MODE="$2"
            shift 2
            ;;
        --skip-build)
            SKIP_BUILD=true
            shift
            ;;
        *)
            echo "Unknown option: $1"
            echo "Usage: $0 [--mode <paper|live|shadow>] [--skip-build]"
            exit 1
            ;;
    esac
done

# Validate mode
if [[ ! "$MODE" =~ ^(paper|live|shadow)$ ]]; then
    echo "Invalid mode: $MODE. Must be paper, live, or shadow."
    exit 1
fi

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
CYAN='\033[0;36m'
NC='\033[0m' # No Color

echo "============================================"
echo "Polymarket Trading Dashboard - PRODUCTION"
echo "============================================"
echo ""
echo -e "${CYAN}Mode: $MODE${NC}"
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

# Check prerequisites
echo ""
echo -e "${YELLOW}[1/5] Checking prerequisites...${NC}"

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
        echo -e "${YELLOW}         Historical data and session persistence will not work${NC}"
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

if [ "$SKIP_BUILD" = false ]; then
    # Build the bot
    echo -e "${YELLOW}[2/5] Building poly-bot (release)...${NC}"
    cargo build -p poly-bot --release
    echo -e "${GREEN}Bot build complete${NC}"
    echo ""

    # Install frontend dependencies
    echo -e "${YELLOW}[3/5] Installing frontend dependencies...${NC}"
    cd frontend
    bun install
    cd ..
    echo -e "${GREEN}Dependencies installed${NC}"
    echo ""

    # Build frontend
    echo -e "${YELLOW}[4/5] Building frontend for production...${NC}"
    cd frontend
    bun run build
    cd ..
    echo -e "${GREEN}Frontend build complete${NC}"
    echo ""
else
    echo -e "${YELLOW}[2-4/5] Skipping builds (--skip-build)${NC}"
    echo ""
fi

# Start bot with dashboard
echo -e "${YELLOW}[5/5] Starting bot with dashboard...${NC}"
echo ""
echo -e "  ${CYAN}Dashboard:    http://localhost:3002${NC}"
echo -e "  ${CYAN}WebSocket:    ws://localhost:3001${NC}"
echo -e "  ${CYAN}REST API:     http://localhost:3002/api/*${NC}"
echo ""
echo -e "${YELLOW}Press Ctrl+C to stop${NC}"
echo ""

# Set environment variable to enable static file serving
export POLY_DASHBOARD_STATIC_DIR="frontend/dist"

# Start the bot
cargo run -p poly-bot --release -- --mode "$MODE"
