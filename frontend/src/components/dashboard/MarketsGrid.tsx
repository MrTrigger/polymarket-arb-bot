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
import { useDashboardState } from "@/hooks";
import {
  formatUsd,
  formatTimeRemaining,
  parseDecimal,
  type ActiveMarket,
  type Position,
} from "@/lib/types";
import { TrendingUp, BarChart3 } from "lucide-react";

/**
 * MarketsGrid component displays a table of active markets.
 *
 * Columns: Asset, Strike, Time Remaining, Spot Price, YES Bid/Ask, NO Bid/Ask,
 * Arb Spread, Position.
 *
 * Features:
 * - Clickable rows navigate to /market/:eventId
 * - Rows with arb opportunities are highlighted
 * - Shows position info if present
 * - Real-time updates from WebSocket
 */
export function MarketsGrid() {
  const { markets, positions } = useDashboardState();

  // Create a map of positions by event_id for quick lookup
  const positionsByEventId = new Map<string, Position>(
    positions.map((p) => [p.event_id, p])
  );

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
      <CardHeader>
        <CardTitle className="flex items-center gap-2">
          <BarChart3 className="h-5 w-5" />
          Active Markets
          <span className="ml-2 rounded-full bg-muted px-2 py-0.5 text-sm font-normal text-muted-foreground">
            {markets.length}
          </span>
        </CardTitle>
      </CardHeader>
      <CardContent className="p-0">
        <Table>
          <TableHeader>
            <TableRow>
              <TableHead>Asset</TableHead>
              <TableHead>Strike</TableHead>
              <TableHead>Time Left</TableHead>
              <TableHead>Spot</TableHead>
              <TableHead>YES Bid/Ask</TableHead>
              <TableHead>NO Bid/Ask</TableHead>
              <TableHead>Arb Spread</TableHead>
              <TableHead>Position</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {markets.map((market) => (
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
  const hasPosition =
    position &&
    (parseDecimal(position.yes_shares) > 0 ||
      parseDecimal(position.no_shares) > 0);

  return (
    <TableRow
      className={`cursor-pointer ${market.has_arb_opportunity ? "bg-yellow-500/5 hover:bg-yellow-500/10" : ""}`}
    >
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
          {formatUsd(market.strike_price)}
        </Link>
      </TableCell>
      <TableCell>
        <Link
          to={`/market/${market.event_id}`}
          className={
            market.seconds_remaining < 60 ? "text-red-500 font-medium" : ""
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
  const realizedPnl = parseDecimal(position.realized_pnl);

  return (
    <div className="text-xs">
      <div>
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
      {realizedPnl !== 0 && (
        <div className={realizedPnl >= 0 ? "text-green-500" : "text-red-500"}>
          {realizedPnl >= 0 ? "+" : ""}
          {formatUsd(position.realized_pnl)}
        </div>
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
              <TableHead>Asset</TableHead>
              <TableHead>Strike</TableHead>
              <TableHead>Time Left</TableHead>
              <TableHead>Spot</TableHead>
              <TableHead>YES Bid/Ask</TableHead>
              <TableHead>NO Bid/Ask</TableHead>
              <TableHead>Arb Spread</TableHead>
              <TableHead>Position</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {Array.from({ length: 5 }).map((_, i) => (
              <TableRow key={i}>
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
              </TableRow>
            ))}
          </TableBody>
        </Table>
      </CardContent>
    </Card>
  );
}

export default MarketsGrid;
