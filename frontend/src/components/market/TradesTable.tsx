import { useMemo } from "react";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Badge } from "@/components/ui/badge";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import {
  TrendingUp,
  TrendingDown,
  ArrowUpRight,
  ArrowDownRight,
  Clock,
  Activity,
} from "lucide-react";
import type { Trade } from "@/lib/types";
import { parseDecimal, formatUsd } from "@/lib/types";

interface TradesTableProps {
  /** Trades for this market. */
  trades: Trade[];
  /** Maximum number of trades to display (default: 50 for performance). */
  maxDisplay?: number;
}

/**
 * Configuration for trade side display.
 */
interface SideConfig {
  label: string;
  color: string;
  bgColor: string;
  icon: typeof ArrowUpRight;
}

const SIDE_CONFIG: Record<string, SideConfig> = {
  BUY: {
    label: "BUY",
    color: "text-green-500",
    bgColor: "bg-green-500/10",
    icon: ArrowUpRight,
  },
  SELL: {
    label: "SELL",
    color: "text-red-500",
    bgColor: "bg-red-500/10",
    icon: ArrowDownRight,
  },
};

/**
 * Configuration for outcome display.
 */
interface OutcomeConfig {
  label: string;
  color: string;
}

const OUTCOME_CONFIG: Record<string, OutcomeConfig> = {
  yes: {
    label: "YES",
    color: "text-green-400",
  },
  no: {
    label: "NO",
    color: "text-red-400",
  },
};

/**
 * Configuration for trade status display.
 */
interface StatusConfig {
  label: string;
  color: string;
  bgColor: string;
}

const STATUS_CONFIG: Record<string, StatusConfig> = {
  FILLED: {
    label: "Filled",
    color: "text-green-500",
    bgColor: "bg-green-500/10",
  },
  PARTIAL: {
    label: "Partial",
    color: "text-yellow-500",
    bgColor: "bg-yellow-500/10",
  },
  CANCELLED: {
    label: "Cancelled",
    color: "text-zinc-500",
    bgColor: "bg-zinc-500/10",
  },
  FAILED: {
    label: "Failed",
    color: "text-red-500",
    bgColor: "bg-red-500/10",
  },
};

/**
 * Format timestamp to HH:MM:SS.mmm format.
 */
function formatTime(iso: string): string {
  const date = new Date(iso);
  const hours = date.getHours().toString().padStart(2, "0");
  const minutes = date.getMinutes().toString().padStart(2, "0");
  const seconds = date.getSeconds().toString().padStart(2, "0");
  const ms = date.getMilliseconds().toString().padStart(3, "0");
  return `${hours}:${minutes}:${seconds}.${ms}`;
}

/**
 * Calculate simple P&L estimate for a trade.
 * BUY: Profit if price < 0.50 (yes) or > 0.50 (no)
 * SELL: Profit if price > 0.50 (yes) or < 0.50 (no)
 */
function estimateTradePnl(trade: Trade): number {
  const fillPrice = parseDecimal(trade.fill_price);
  const fillSize = parseDecimal(trade.fill_size);
  const fees = parseDecimal(trade.fees);

  // For arb trades, the "edge" is the arb margin minus fees
  const arbMargin = parseDecimal(trade.arb_margin_at_fill);

  if (arbMargin > 0) {
    // Arb trade: profit is (margin * size) - fees
    return arbMargin * fillSize - fees;
  }

  // Non-arb trade: estimate based on fair value assumption (0.50)
  const fairValue = 0.5;
  const edge = trade.side === "BUY" ? fairValue - fillPrice : fillPrice - fairValue;
  return edge * fillSize - fees;
}

/**
 * Single trade row component.
 */
function TradeRow({ trade }: { trade: Trade }) {
  const sideConfig = SIDE_CONFIG[trade.side] || SIDE_CONFIG.BUY;
  const outcomeConfig = OUTCOME_CONFIG[trade.outcome] || OUTCOME_CONFIG.yes;
  const statusConfig = STATUS_CONFIG[trade.status] || STATUS_CONFIG.FILLED;
  const SideIcon = sideConfig.icon;

  const pnl = useMemo(() => estimateTradePnl(trade), [trade]);
  const isProfitable = pnl >= 0;

  const fillPrice = parseDecimal(trade.fill_price);
  const fillSize = parseDecimal(trade.fill_size);

  return (
    <TableRow
      className={`${
        isProfitable
          ? "hover:bg-green-500/5"
          : "hover:bg-red-500/5"
      } transition-colors`}
    >
      {/* Time */}
      <TableCell className="font-mono text-xs text-muted-foreground">
        <div className="flex items-center gap-1">
          <Clock className="h-3 w-3" />
          {formatTime(trade.fill_time)}
        </div>
      </TableCell>

      {/* Side */}
      <TableCell>
        <div className={`flex items-center gap-1 ${sideConfig.color}`}>
          <SideIcon className="h-3 w-3" />
          <span className="font-medium">{sideConfig.label}</span>
        </div>
      </TableCell>

      {/* Outcome (Action) */}
      <TableCell>
        <Badge
          variant="secondary"
          className={`${outcomeConfig.color} bg-transparent`}
        >
          {outcomeConfig.label}
        </Badge>
      </TableCell>

      {/* Price (in cents) */}
      <TableCell className="text-right font-mono">
        <span className="text-sm">
          {(fillPrice * 100).toFixed(1)}c
        </span>
      </TableCell>

      {/* Size */}
      <TableCell className="text-right font-mono">
        <span className="text-sm">{fillSize.toFixed(2)}</span>
      </TableCell>

      {/* P&L */}
      <TableCell className="text-right">
        <div
          className={`flex items-center justify-end gap-1 font-mono ${
            isProfitable ? "text-green-500" : "text-red-500"
          }`}
        >
          {isProfitable ? (
            <TrendingUp className="h-3 w-3" />
          ) : (
            <TrendingDown className="h-3 w-3" />
          )}
          <span>
            {isProfitable ? "+" : ""}
            {formatUsd(pnl.toFixed(4))}
          </span>
        </div>
      </TableCell>

      {/* Status (optional - shown as small badge) */}
      <TableCell>
        {trade.status !== "FILLED" && (
          <Badge
            variant="secondary"
            className={`text-xs ${statusConfig.bgColor} ${statusConfig.color}`}
          >
            {statusConfig.label}
          </Badge>
        )}
      </TableCell>
    </TableRow>
  );
}

/**
 * TradesTable component displays trades for a specific market.
 *
 * Features:
 * - Columns: time, side, outcome, price, size, P&L
 * - Sorted by time descending (most recent first)
 * - Color-coded by profit/loss
 * - Shows status for non-filled trades
 * - Capped at maxDisplay for performance (virtual scrolling not needed for ~50 items)
 */
export function TradesTable({ trades, maxDisplay = 50 }: TradesTableProps) {
  // Sort trades by fill_time descending (most recent first)
  const sortedTrades = useMemo(() => {
    return [...trades]
      .sort((a, b) => new Date(b.fill_time).getTime() - new Date(a.fill_time).getTime())
      .slice(0, maxDisplay);
  }, [trades, maxDisplay]);

  // Calculate summary stats
  const stats = useMemo(() => {
    if (sortedTrades.length === 0) return null;

    let totalPnl = 0;
    let wins = 0;
    let losses = 0;

    for (const trade of sortedTrades) {
      const pnl = estimateTradePnl(trade);
      totalPnl += pnl;
      if (pnl >= 0) wins++;
      else losses++;
    }

    return {
      count: sortedTrades.length,
      totalPnl,
      wins,
      losses,
      winRate: sortedTrades.length > 0 ? (wins / sortedTrades.length) * 100 : 0,
    };
  }, [sortedTrades]);

  if (trades.length === 0) {
    return (
      <Card className="h-full">
        <CardHeader className="flex flex-row items-center justify-between space-y-0 pb-2">
          <CardTitle className="text-sm font-medium">Recent Trades</CardTitle>
        </CardHeader>
        <CardContent>
          <div className="flex h-40 flex-col items-center justify-center text-muted-foreground">
            <Activity className="mb-2 h-8 w-8 opacity-50" />
            <p className="text-sm">No trades in this market</p>
          </div>
        </CardContent>
      </Card>
    );
  }

  return (
    <Card className="h-full">
      <CardHeader className="flex flex-row items-center justify-between space-y-0 pb-2">
        <CardTitle className="text-sm font-medium">Recent Trades</CardTitle>
        {stats && (
          <div className="flex items-center gap-2">
            <Badge
              variant="secondary"
              className={`${
                stats.totalPnl >= 0 ? "bg-green-500/10 text-green-500" : "bg-red-500/10 text-red-500"
              }`}
            >
              {stats.totalPnl >= 0 ? "+" : ""}
              {formatUsd(stats.totalPnl.toFixed(2))}
            </Badge>
            <Badge variant="secondary" className="text-muted-foreground">
              {stats.count} trades
            </Badge>
            <Badge
              variant="secondary"
              className={`${
                stats.winRate >= 50
                  ? "bg-green-500/10 text-green-500"
                  : "bg-red-500/10 text-red-500"
              }`}
            >
              {stats.winRate.toFixed(0)}% win
            </Badge>
          </div>
        )}
      </CardHeader>
      <CardContent>
        <div className="max-h-80 overflow-y-auto">
          <Table>
            <TableHeader className="sticky top-0 bg-card">
              <TableRow>
                <TableHead className="w-28">Time</TableHead>
                <TableHead className="w-16">Side</TableHead>
                <TableHead className="w-16">Action</TableHead>
                <TableHead className="w-16 text-right">Price</TableHead>
                <TableHead className="w-16 text-right">Size</TableHead>
                <TableHead className="w-24 text-right">P&L</TableHead>
                <TableHead className="w-16"></TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {sortedTrades.map((trade) => (
                <TradeRow key={trade.trade_id} trade={trade} />
              ))}
            </TableBody>
          </Table>
        </div>
        {trades.length > maxDisplay && (
          <div className="mt-2 text-center text-xs text-muted-foreground">
            Showing {maxDisplay} of {trades.length} trades
          </div>
        )}
      </CardContent>
    </Card>
  );
}

/**
 * Skeleton loader for TradesTable when data is loading.
 */
export function TradesTableSkeleton() {
  return (
    <Card className="h-full">
      <CardHeader className="flex flex-row items-center justify-between space-y-0 pb-2">
        <div className="h-4 w-24 animate-pulse rounded bg-muted" />
        <div className="flex items-center gap-2">
          <div className="h-5 w-16 animate-pulse rounded-full bg-muted" />
          <div className="h-5 w-20 animate-pulse rounded-full bg-muted" />
          <div className="h-5 w-16 animate-pulse rounded-full bg-muted" />
        </div>
      </CardHeader>
      <CardContent>
        <div className="space-y-2">
          {/* Header skeleton */}
          <div className="flex items-center gap-4 border-b border-border pb-2">
            <div className="h-3 w-20 animate-pulse rounded bg-muted" />
            <div className="h-3 w-12 animate-pulse rounded bg-muted" />
            <div className="h-3 w-12 animate-pulse rounded bg-muted" />
            <div className="ml-auto h-3 w-12 animate-pulse rounded bg-muted" />
            <div className="h-3 w-12 animate-pulse rounded bg-muted" />
            <div className="h-3 w-16 animate-pulse rounded bg-muted" />
          </div>
          {/* Row skeletons */}
          {Array.from({ length: 5 }).map((_, i) => (
            <div key={i} className="flex items-center gap-4 py-2">
              <div className="h-4 w-24 animate-pulse rounded bg-muted" />
              <div className="h-4 w-12 animate-pulse rounded bg-muted" />
              <div className="h-5 w-10 animate-pulse rounded-full bg-muted" />
              <div className="ml-auto h-4 w-10 animate-pulse rounded bg-muted" />
              <div className="h-4 w-12 animate-pulse rounded bg-muted" />
              <div className="h-4 w-16 animate-pulse rounded bg-muted" />
            </div>
          ))}
        </div>
      </CardContent>
    </Card>
  );
}

export default TradesTable;
