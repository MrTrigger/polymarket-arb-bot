import { useState } from "react";
import { Link } from "react-router-dom";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Button } from "@/components/ui/button";
import { useDashboardState } from "@/hooks";
import {
  formatUsd,
  formatTimeRemaining,
  parseDecimal,
  type ActiveMarket,
  type Position,
} from "@/lib/types";
import { TrendingUp, BarChart3, Circle, Clock, CheckCircle } from "lucide-react";

/** Market status based on window timing. */
type MarketStatus = "live" | "upcoming" | "ended";

/** Window duration in seconds (15 minutes). */
const WINDOW_DURATION_SECS = 900;

/**
 * Determine market status from seconds remaining.
 * - upcoming: window hasn't started yet (seconds_remaining > window duration)
 * - live: window is active (0 < seconds_remaining <= window duration)
 * - ended: window has closed (seconds_remaining <= 0)
 */
function getMarketStatus(secondsRemaining: number): MarketStatus {
  if (secondsRemaining > WINDOW_DURATION_SECS) {
    return "upcoming";
  }
  if (secondsRemaining > 0) {
    return "live";
  }
  return "ended";
}

/**
 * Get status badge color classes.
 */
function getStatusColor(status: MarketStatus): string {
  switch (status) {
    case "live":
      return "bg-green-500/20 text-green-400 border-green-500/30";
    case "upcoming":
      return "bg-blue-500/20 text-blue-400 border-blue-500/30";
    case "ended":
      return "bg-gray-500/20 text-gray-400 border-gray-500/30";
  }
}

/**
 * Get row background color based on status.
 */
function getRowBgColor(status: MarketStatus, hasArbOpportunity: boolean): string {
  if (hasArbOpportunity) {
    return "bg-yellow-500/5 hover:bg-yellow-500/10";
  }
  switch (status) {
    case "live":
      return "bg-green-500/5 hover:bg-green-500/10";
    case "upcoming":
      return "hover:bg-muted/50";
    case "ended":
      return "bg-muted/30 hover:bg-muted/50 opacity-60";
  }
}

/**
 * Get status icon component.
 */
function StatusIcon({ status }: { status: MarketStatus }) {
  switch (status) {
    case "live":
      return <Circle className="h-3 w-3 fill-green-500 text-green-500 animate-pulse" />;
    case "upcoming":
      return <Clock className="h-3 w-3 text-blue-400" />;
    case "ended":
      return <CheckCircle className="h-3 w-3 text-gray-400" />;
  }
}

/** Filter option type. */
type FilterOption = "all" | "live" | "upcoming" | "ended";

/**
 * MarketsGrid component displays a table of active markets.
 *
 * Features:
 * - Color-coded rows by market status (live/upcoming/ended)
 * - Filter buttons to show specific market types
 * - PnL column showing realized P&L per market
 * - Clickable rows navigate to /market/:eventId
 * - Rows with arb opportunities are highlighted
 */
export function MarketsGrid() {
  const { markets, positions } = useDashboardState();
  const [filter, setFilter] = useState<FilterOption>("all");

  // Create a map of positions by event_id for quick lookup
  const positionsByEventId = new Map<string, Position>(
    positions.map((p) => [p.event_id, p])
  );

  // Count markets by status
  const statusCounts = markets.reduce(
    (acc, market) => {
      const status = getMarketStatus(market.seconds_remaining);
      acc[status]++;
      return acc;
    },
    { live: 0, upcoming: 0, ended: 0 } as Record<MarketStatus, number>
  );

  // Filter markets based on selected filter
  const filteredMarkets = markets.filter((market) => {
    if (filter === "all") return true;
    return getMarketStatus(market.seconds_remaining) === filter;
  });

  if (markets.length === 0) {
    return (
      <Card>
        <CardHeader>
          <CardTitle className="flex items-center gap-2">
            <BarChart3 className="h-5 w-5" />
            Active Markets
          </CardTitle>
        </CardHeader>
        <CardContent>
          <div className="flex h-32 items-center justify-center text-muted-foreground">
            No active markets
          </div>
        </CardContent>
      </Card>
    );
  }

  return (
    <Card>
      <CardHeader className="pb-3">
        <div className="flex items-center justify-between">
          <CardTitle className="flex items-center gap-2">
            <BarChart3 className="h-5 w-5" />
            Active Markets
            <span className="ml-2 rounded-full bg-muted px-2 py-0.5 text-sm font-normal text-muted-foreground">
              {filteredMarkets.length}
            </span>
          </CardTitle>
          {/* Status filter buttons */}
          <div className="flex gap-1">
            <Button
              variant={filter === "all" ? "secondary" : "ghost"}
              size="sm"
              onClick={() => setFilter("all")}
              className="h-7 text-xs"
            >
              All ({markets.length})
            </Button>
            <Button
              variant={filter === "live" ? "secondary" : "ghost"}
              size="sm"
              onClick={() => setFilter("live")}
              className="h-7 text-xs"
            >
              <Circle className="mr-1 h-2 w-2 fill-green-500 text-green-500" />
              Live ({statusCounts.live})
            </Button>
            <Button
              variant={filter === "upcoming" ? "secondary" : "ghost"}
              size="sm"
              onClick={() => setFilter("upcoming")}
              className="h-7 text-xs"
            >
              <Clock className="mr-1 h-3 w-3 text-blue-400" />
              Upcoming ({statusCounts.upcoming})
            </Button>
            <Button
              variant={filter === "ended" ? "secondary" : "ghost"}
              size="sm"
              onClick={() => setFilter("ended")}
              className="h-7 text-xs"
            >
              <CheckCircle className="mr-1 h-3 w-3 text-gray-400" />
              Ended ({statusCounts.ended})
            </Button>
          </div>
        </div>
      </CardHeader>
      <CardContent className="p-0">
        <Table>
          <TableHeader>
            <TableRow>
              <TableHead className="w-[80px]">Status</TableHead>
              <TableHead>Asset</TableHead>
              <TableHead>Strike</TableHead>
              <TableHead>Time Left</TableHead>
              <TableHead>Spot</TableHead>
              <TableHead>YES Bid/Ask</TableHead>
              <TableHead>NO Bid/Ask</TableHead>
              <TableHead>Arb Spread</TableHead>
              <TableHead>Position</TableHead>
              <TableHead>P&L</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {filteredMarkets.map((market) => (
              <MarketRow
                key={market.event_id}
                market={market}
                position={positionsByEventId.get(market.event_id)}
              />
            ))}
          </TableBody>
        </Table>
      </CardContent>
    </Card>
  );
}

/**
 * Single market row with clickable navigation.
 */
interface MarketRowProps {
  market: ActiveMarket;
  position?: Position;
}

function MarketRow({ market, position }: MarketRowProps) {
  const arbSpread = parseDecimal(market.arb_spread);
  const status = getMarketStatus(market.seconds_remaining);
  const hasPosition =
    position &&
    (parseDecimal(position.yes_shares) > 0 ||
      parseDecimal(position.no_shares) > 0);
  const realizedPnl = position ? parseDecimal(position.realized_pnl) : 0;

  return (
    <TableRow
      className={`cursor-pointer ${getRowBgColor(status, market.has_arb_opportunity)}`}
    >
      {/* Status badge */}
      <TableCell>
        <Link to={`/market/${market.event_id}`}>
          <div className={`inline-flex items-center gap-1 rounded-full border px-2 py-0.5 text-xs ${getStatusColor(status)}`}>
            <StatusIcon status={status} />
            <span className="capitalize">{status}</span>
          </div>
        </Link>
      </TableCell>
      <TableCell>
        <Link
          to={`/market/${market.event_id}`}
          className="flex items-center gap-2 font-medium"
        >
          {market.has_arb_opportunity && (
            <TrendingUp className="h-4 w-4 text-yellow-500" />
          )}
          {market.asset}
        </Link>
      </TableCell>
      <TableCell>
        <Link to={`/market/${market.event_id}`}>
          {parseDecimal(market.strike_price) > 0 ? formatUsd(market.strike_price) : "—"}
        </Link>
      </TableCell>
      <TableCell>
        <Link
          to={`/market/${market.event_id}`}
          className={
            status === "live" && market.seconds_remaining < 60
              ? "text-red-500 font-medium"
              : status === "ended"
              ? "text-muted-foreground"
              : ""
          }
        >
          {formatTimeRemaining(market.seconds_remaining)}
        </Link>
      </TableCell>
      <TableCell>
        <Link to={`/market/${market.event_id}`}>
          {formatUsd(market.spot_price)}
        </Link>
      </TableCell>
      <TableCell>
        <Link to={`/market/${market.event_id}`}>
          <OrderBookCell book={market.yes_book} />
        </Link>
      </TableCell>
      <TableCell>
        <Link to={`/market/${market.event_id}`}>
          <OrderBookCell book={market.no_book} />
        </Link>
      </TableCell>
      <TableCell>
        <Link
          to={`/market/${market.event_id}`}
          className={`font-mono ${getArbSpreadColor(arbSpread, market.has_arb_opportunity)}`}
        >
          {formatArbSpread(arbSpread)}
        </Link>
      </TableCell>
      <TableCell>
        <Link to={`/market/${market.event_id}`}>
          {hasPosition ? (
            <PositionCell position={position} />
          ) : (
            <span className="text-muted-foreground">—</span>
          )}
        </Link>
      </TableCell>
      {/* P&L column */}
      <TableCell>
        <Link to={`/market/${market.event_id}`}>
          {hasPosition && realizedPnl !== 0 ? (
            <span className={`font-mono text-sm ${realizedPnl >= 0 ? "text-green-500" : "text-red-500"}`}>
              {realizedPnl >= 0 ? "+" : ""}{formatUsd(position!.realized_pnl)}
            </span>
          ) : (
            <span className="text-muted-foreground">—</span>
          )}
        </Link>
      </TableCell>
    </TableRow>
  );
}

/**
 * Order book bid/ask cell.
 */
interface OrderBookCellProps {
  book: ActiveMarket["yes_book"];
}

function OrderBookCell({ book }: OrderBookCellProps) {
  if (!book) {
    return <span className="text-muted-foreground">—</span>;
  }

  const bid = parseDecimal(book.best_bid);
  const ask = parseDecimal(book.best_ask);

  return (
    <div className="text-xs">
      <span className="text-green-500">{(bid * 100).toFixed(1)}¢</span>
      <span className="mx-1 text-muted-foreground">/</span>
      <span className="text-red-500">{(ask * 100).toFixed(1)}¢</span>
    </div>
  );
}

/**
 * Position summary cell.
 */
interface PositionCellProps {
  position: Position;
}

function PositionCell({ position }: PositionCellProps) {
  const yesShares = parseDecimal(position.yes_shares);
  const noShares = parseDecimal(position.no_shares);

  return (
    <div className="text-xs">
      {yesShares > 0 && (
        <span className="mr-2">
          Y: <span className="font-medium">{yesShares.toFixed(1)}</span>
        </span>
      )}
      {noShares > 0 && (
        <span>
          N: <span className="font-medium">{noShares.toFixed(1)}</span>
        </span>
      )}
    </div>
  );
}

/**
 * Format arb spread for display.
 */
function formatArbSpread(spread: number): string {
  // Convert to basis points for display
  const bps = spread * 10000;
  if (Math.abs(bps) < 1) {
    return "0 bps";
  }
  return `${bps >= 0 ? "+" : ""}${bps.toFixed(0)} bps`;
}

/**
 * Get color class for arb spread.
 */
function getArbSpreadColor(spread: number, hasOpportunity: boolean): string {
  if (hasOpportunity) {
    return "text-yellow-500 font-medium";
  }
  if (spread > 0) {
    return "text-green-500";
  }
  if (spread < -0.001) {
    // Negative spread (cost)
    return "text-red-500";
  }
  return "text-muted-foreground";
}

/**
 * Skeleton loader for MarketsGrid.
 */
export function MarketsGridSkeleton() {
  return (
    <Card>
      <CardHeader>
        <CardTitle className="flex items-center gap-2">
          <BarChart3 className="h-5 w-5" />
          Active Markets
        </CardTitle>
      </CardHeader>
      <CardContent className="p-0">
        <Table>
          <TableHeader>
            <TableRow>
              <TableHead>Status</TableHead>
              <TableHead>Asset</TableHead>
              <TableHead>Strike</TableHead>
              <TableHead>Time Left</TableHead>
              <TableHead>Spot</TableHead>
              <TableHead>YES Bid/Ask</TableHead>
              <TableHead>NO Bid/Ask</TableHead>
              <TableHead>Arb Spread</TableHead>
              <TableHead>Position</TableHead>
              <TableHead>P&L</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {Array.from({ length: 5 }).map((_, i) => (
              <TableRow key={i}>
                <TableCell>
                  <div className="h-4 w-16 animate-pulse rounded bg-muted" />
                </TableCell>
                <TableCell>
                  <div className="h-4 w-12 animate-pulse rounded bg-muted" />
                </TableCell>
                <TableCell>
                  <div className="h-4 w-20 animate-pulse rounded bg-muted" />
                </TableCell>
                <TableCell>
                  <div className="h-4 w-14 animate-pulse rounded bg-muted" />
                </TableCell>
                <TableCell>
                  <div className="h-4 w-20 animate-pulse rounded bg-muted" />
                </TableCell>
                <TableCell>
                  <div className="h-4 w-16 animate-pulse rounded bg-muted" />
                </TableCell>
                <TableCell>
                  <div className="h-4 w-16 animate-pulse rounded bg-muted" />
                </TableCell>
                <TableCell>
                  <div className="h-4 w-14 animate-pulse rounded bg-muted" />
                </TableCell>
                <TableCell>
                  <div className="h-4 w-10 animate-pulse rounded bg-muted" />
                </TableCell>
                <TableCell>
                  <div className="h-4 w-14 animate-pulse rounded bg-muted" />
                </TableCell>
              </TableRow>
            ))}
          </TableBody>
        </Table>
      </CardContent>
    </Card>
  );
}

export default MarketsGrid;
