import { useMemo } from "react";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Badge } from "@/components/ui/badge";
import {
  Scale,
  TrendingUp,
  TrendingDown,
  AlertTriangle,
  ShieldAlert,
  CircleDot,
} from "lucide-react";
import type { Position, ActiveMarket } from "@/lib/types";
import { parseDecimal, formatUsd, formatPercent } from "@/lib/types";

interface PositionPanelProps {
  /** Position data for this market, null if no position. */
  position: Position | null;
  /** Market data for calculating unrealized P&L (optional). */
  market?: ActiveMarket | null;
}

/**
 * Configuration for inventory state display.
 */
interface InventoryStateConfig {
  label: string;
  color: string;
  bgColor: string;
  borderColor: string;
  icon: typeof Scale;
  description: string;
}

const INVENTORY_STATE_CONFIG: Record<string, InventoryStateConfig> = {
  balanced: {
    label: "Balanced",
    color: "text-green-500",
    bgColor: "bg-green-500/10",
    borderColor: "border-green-500/30",
    icon: Scale,
    description: "YES and NO holdings are balanced",
  },
  skewed: {
    label: "Skewed",
    color: "text-yellow-500",
    bgColor: "bg-yellow-500/10",
    borderColor: "border-yellow-500/30",
    icon: TrendingUp,
    description: "Position is moderately imbalanced",
  },
  exposed: {
    label: "Exposed",
    color: "text-orange-500",
    bgColor: "bg-orange-500/10",
    borderColor: "border-orange-500/30",
    icon: AlertTriangle,
    description: "Significant directional exposure",
  },
  crisis: {
    label: "Crisis",
    color: "text-red-500",
    bgColor: "bg-red-500/10",
    borderColor: "border-red-500/30",
    icon: ShieldAlert,
    description: "Critical imbalance - intervention needed",
  },
};

/**
 * PositionPanel component displays the current position in a market.
 *
 * Features:
 * - YES/NO shares and cost basis display
 * - Imbalance ratio with visual indicator
 * - Inventory state badge with color coding
 * - Realized P&L (colored green/red)
 * - Unrealized P&L calculation (when market data available)
 * - Total exposure display
 * - Visual indicators for different inventory states
 */
export function PositionPanel({ position, market }: PositionPanelProps) {
  // Calculate unrealized P&L based on current market prices
  const unrealizedPnl = useMemo(() => {
    if (!position || !market) return null;

    const yesShares = parseDecimal(position.yes_shares);
    const noShares = parseDecimal(position.no_shares);
    const yesCost = parseDecimal(position.yes_cost_basis);
    const noCost = parseDecimal(position.no_cost_basis);

    // Get current best bids to estimate liquidation value
    const yesBid = market.yes_book ? parseDecimal(market.yes_book.best_bid) : 0;
    const noBid = market.no_book ? parseDecimal(market.no_book.best_bid) : 0;

    // Unrealized = (current value) - (cost basis)
    const yesValue = yesShares * yesBid;
    const noValue = noShares * noBid;
    const totalValue = yesValue + noValue;
    const totalCost = yesCost + noCost;

    return totalValue - totalCost;
  }, [position, market]);

  // Calculate imbalance visualization
  const imbalanceViz = useMemo(() => {
    if (!position) return null;

    const ratio = parseDecimal(position.imbalance_ratio);
    // Convert to percentage bar width (0.5 = centered, 0 = all NO, 1 = all YES)
    const yesWeight = ratio;
    const noWeight = 1 - ratio;

    return {
      yesPercent: Math.round(yesWeight * 100),
      noPercent: Math.round(noWeight * 100),
      ratio,
    };
  }, [position]);

  // Get inventory state config
  const stateConfig = position
    ? INVENTORY_STATE_CONFIG[position.inventory_state] ||
      INVENTORY_STATE_CONFIG.balanced
    : null;

  if (!position) {
    return (
      <Card className="h-full">
        <CardHeader className="flex flex-row items-center justify-between space-y-0 pb-2">
          <CardTitle className="text-sm font-medium">Position</CardTitle>
        </CardHeader>
        <CardContent>
          <div className="flex h-40 flex-col items-center justify-center text-muted-foreground">
            <CircleDot className="mb-2 h-8 w-8 opacity-50" />
            <p className="text-sm">No position in this market</p>
          </div>
        </CardContent>
      </Card>
    );
  }

  const realizedPnl = parseDecimal(position.realized_pnl);
  const yesShares = parseDecimal(position.yes_shares);
  const noShares = parseDecimal(position.no_shares);

  return (
    <Card className="h-full">
      <CardHeader className="flex flex-row items-center justify-between space-y-0 pb-2">
        <CardTitle className="text-sm font-medium">Position</CardTitle>
        {stateConfig && (
          <Badge
            variant="secondary"
            className={`${stateConfig.bgColor} ${stateConfig.color}`}
          >
            <stateConfig.icon className="mr-1 h-3 w-3" />
            {stateConfig.label}
          </Badge>
        )}
      </CardHeader>
      <CardContent className="space-y-4">
        {/* Shares display */}
        <div className="grid grid-cols-2 gap-4">
          <div className="rounded-lg border border-border bg-muted/20 p-3">
            <div className="mb-1 text-xs text-muted-foreground">YES Shares</div>
            <div className="font-mono text-lg font-semibold text-green-500">
              {yesShares.toFixed(2)}
            </div>
            <div className="text-xs text-muted-foreground">
              Cost: {formatUsd(position.yes_cost_basis)}
            </div>
          </div>
          <div className="rounded-lg border border-border bg-muted/20 p-3">
            <div className="mb-1 text-xs text-muted-foreground">NO Shares</div>
            <div className="font-mono text-lg font-semibold text-red-500">
              {noShares.toFixed(2)}
            </div>
            <div className="text-xs text-muted-foreground">
              Cost: {formatUsd(position.no_cost_basis)}
            </div>
          </div>
        </div>

        {/* Imbalance visualization */}
        {imbalanceViz && (
          <div className="space-y-2">
            <div className="flex items-center justify-between text-xs text-muted-foreground">
              <span>Imbalance Ratio</span>
              <span className="font-mono">{formatPercent(imbalanceViz.ratio)}</span>
            </div>
            <div className="flex h-2 overflow-hidden rounded-full bg-muted">
              <div
                className="bg-green-500 transition-all"
                style={{ width: `${imbalanceViz.yesPercent}%` }}
              />
              <div
                className="bg-red-500 transition-all"
                style={{ width: `${imbalanceViz.noPercent}%` }}
              />
            </div>
            <div className="flex justify-between text-xs">
              <span className="text-green-500">YES {imbalanceViz.yesPercent}%</span>
              <span className="text-red-500">NO {imbalanceViz.noPercent}%</span>
            </div>
          </div>
        )}

        {/* P&L display */}
        <div
          className={`rounded-lg border p-3 ${
            stateConfig ? stateConfig.borderColor : "border-border"
          } ${stateConfig ? stateConfig.bgColor : "bg-muted/20"}`}
        >
          <div className="grid grid-cols-2 gap-4">
            {/* Realized P&L */}
            <div>
              <div className="mb-1 flex items-center gap-1 text-xs text-muted-foreground">
                {realizedPnl >= 0 ? (
                  <TrendingUp className="h-3 w-3 text-green-500" />
                ) : (
                  <TrendingDown className="h-3 w-3 text-red-500" />
                )}
                <span>Realized P&L</span>
              </div>
              <div
                className={`font-mono font-semibold ${
                  realizedPnl >= 0 ? "text-green-500" : "text-red-500"
                }`}
              >
                {realizedPnl >= 0 ? "+" : ""}
                {formatUsd(position.realized_pnl)}
              </div>
            </div>

            {/* Unrealized P&L (if market data available) */}
            {unrealizedPnl !== null && (
              <div>
                <div className="mb-1 flex items-center gap-1 text-xs text-muted-foreground">
                  {unrealizedPnl >= 0 ? (
                    <TrendingUp className="h-3 w-3 text-green-500" />
                  ) : (
                    <TrendingDown className="h-3 w-3 text-red-500" />
                  )}
                  <span>Unrealized P&L</span>
                </div>
                <div
                  className={`font-mono font-semibold ${
                    unrealizedPnl >= 0 ? "text-green-500" : "text-red-500"
                  }`}
                >
                  {unrealizedPnl >= 0 ? "+" : ""}
                  {formatUsd(unrealizedPnl.toString())}
                </div>
              </div>
            )}
          </div>
        </div>

        {/* Exposure display */}
        <div className="flex items-center justify-between rounded-lg border border-border bg-muted/20 p-3">
          <div className="text-sm text-muted-foreground">Total Exposure</div>
          <div className="font-mono font-semibold">
            {formatUsd(position.total_exposure)}
          </div>
        </div>

        {/* Inventory state description */}
        {stateConfig && (
          <div className="text-xs text-muted-foreground">
            <p>{stateConfig.description}</p>
          </div>
        )}
      </CardContent>
    </Card>
  );
}

/**
 * Skeleton loader for PositionPanel when data is loading.
 */
export function PositionPanelSkeleton() {
  return (
    <Card className="h-full">
      <CardHeader className="flex flex-row items-center justify-between space-y-0 pb-2">
        <div className="h-4 w-20 animate-pulse rounded bg-muted" />
        <div className="h-5 w-24 animate-pulse rounded-full bg-muted" />
      </CardHeader>
      <CardContent className="space-y-4">
        {/* Shares skeleton */}
        <div className="grid grid-cols-2 gap-4">
          <div className="rounded-lg border border-border bg-muted/20 p-3">
            <div className="mb-1 h-3 w-16 animate-pulse rounded bg-muted" />
            <div className="mb-2 h-7 w-20 animate-pulse rounded bg-muted" />
            <div className="h-3 w-24 animate-pulse rounded bg-muted" />
          </div>
          <div className="rounded-lg border border-border bg-muted/20 p-3">
            <div className="mb-1 h-3 w-16 animate-pulse rounded bg-muted" />
            <div className="mb-2 h-7 w-20 animate-pulse rounded bg-muted" />
            <div className="h-3 w-24 animate-pulse rounded bg-muted" />
          </div>
        </div>

        {/* Imbalance skeleton */}
        <div className="space-y-2">
          <div className="flex items-center justify-between">
            <div className="h-3 w-24 animate-pulse rounded bg-muted" />
            <div className="h-3 w-12 animate-pulse rounded bg-muted" />
          </div>
          <div className="h-2 w-full animate-pulse rounded-full bg-muted" />
          <div className="flex justify-between">
            <div className="h-3 w-16 animate-pulse rounded bg-muted" />
            <div className="h-3 w-16 animate-pulse rounded bg-muted" />
          </div>
        </div>

        {/* P&L skeleton */}
        <div className="rounded-lg border border-border p-3">
          <div className="grid grid-cols-2 gap-4">
            <div>
              <div className="mb-1 h-3 w-20 animate-pulse rounded bg-muted" />
              <div className="h-5 w-24 animate-pulse rounded bg-muted" />
            </div>
            <div>
              <div className="mb-1 h-3 w-24 animate-pulse rounded bg-muted" />
              <div className="h-5 w-24 animate-pulse rounded bg-muted" />
            </div>
          </div>
        </div>

        {/* Exposure skeleton */}
        <div className="flex items-center justify-between rounded-lg border border-border bg-muted/20 p-3">
          <div className="h-4 w-24 animate-pulse rounded bg-muted" />
          <div className="h-5 w-20 animate-pulse rounded bg-muted" />
        </div>
      </CardContent>
    </Card>
  );
}

export default PositionPanel;
