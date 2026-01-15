import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { useDashboardState } from "@/hooks";
import { formatUsd, formatPercent, parseDecimal } from "@/lib/types";
import {
  TrendingUp,
  TrendingDown,
  Activity,
  DollarSign,
  Target,
  AlertCircle,
  Zap,
  Crosshair,
} from "lucide-react";

/**
 * MetricsCards component displays key trading metrics in a card grid.
 * Shows P&L (colored green/red), volume, trades executed/failed/skipped,
 * win rate, and exposure. Updates in real-time from WebSocket data.
 */
export function MetricsCards() {
  const { metrics } = useDashboardState();

  if (!metrics) {
    return <MetricsCardsSkeleton />;
  }

  const pnl = parseDecimal(metrics.pnl_usdc);
  const isProfitable = pnl >= 0;

  return (
    <div className="grid grid-cols-1 gap-4 sm:grid-cols-2 lg:grid-cols-4">
      {/* P&L Card */}
      <Card>
        <CardHeader className="flex flex-row items-center justify-between space-y-0 pb-2">
          <CardTitle className="text-sm font-medium">P&L</CardTitle>
          {isProfitable ? (
            <TrendingUp className="h-4 w-4 text-green-500" />
          ) : (
            <TrendingDown className="h-4 w-4 text-red-500" />
          )}
        </CardHeader>
        <CardContent>
          <div
            className={`text-2xl font-bold ${isProfitable ? "text-green-500" : "text-red-500"}`}
          >
            {formatUsd(metrics.pnl_usdc)}
          </div>
          <p className="text-xs text-muted-foreground">
            Total realized profit/loss
          </p>
        </CardContent>
      </Card>

      {/* Volume Card */}
      <Card>
        <CardHeader className="flex flex-row items-center justify-between space-y-0 pb-2">
          <CardTitle className="text-sm font-medium">Volume</CardTitle>
          <DollarSign className="h-4 w-4 text-muted-foreground" />
        </CardHeader>
        <CardContent>
          <div className="text-2xl font-bold">
            {formatUsd(metrics.volume_usdc)}
          </div>
          <p className="text-xs text-muted-foreground">Total trading volume</p>
        </CardContent>
      </Card>

      {/* Trades Card */}
      <Card>
        <CardHeader className="flex flex-row items-center justify-between space-y-0 pb-2">
          <CardTitle className="text-sm font-medium">Trades</CardTitle>
          <Activity className="h-4 w-4 text-muted-foreground" />
        </CardHeader>
        <CardContent>
          <div className="text-2xl font-bold">{metrics.trades_executed}</div>
          <div className="flex gap-3 text-xs text-muted-foreground">
            <span className="text-red-500">{metrics.trades_failed} failed</span>
            <span className="text-yellow-500">
              {metrics.trades_skipped} skipped
            </span>
          </div>
        </CardContent>
      </Card>

      {/* Win Rate Card */}
      <Card>
        <CardHeader className="flex flex-row items-center justify-between space-y-0 pb-2">
          <CardTitle className="text-sm font-medium">Win Rate</CardTitle>
          <Target className="h-4 w-4 text-muted-foreground" />
        </CardHeader>
        <CardContent>
          <div className="text-2xl font-bold">
            {metrics.win_rate != null ? formatPercent(metrics.win_rate) : "—"}
          </div>
          <p className="text-xs text-muted-foreground">
            Percentage of winning trades
          </p>
        </CardContent>
      </Card>

      {/* Opportunities Card */}
      <Card>
        <CardHeader className="flex flex-row items-center justify-between space-y-0 pb-2">
          <CardTitle className="text-sm font-medium">Opportunities</CardTitle>
          <Crosshair className="h-4 w-4 text-muted-foreground" />
        </CardHeader>
        <CardContent>
          <div className="text-2xl font-bold">
            {metrics.opportunities_detected}
          </div>
          <p className="text-xs text-muted-foreground">
            Arbitrage opportunities detected
          </p>
        </CardContent>
      </Card>

      {/* Events Processed Card */}
      <Card>
        <CardHeader className="flex flex-row items-center justify-between space-y-0 pb-2">
          <CardTitle className="text-sm font-medium">Events</CardTitle>
          <Zap className="h-4 w-4 text-muted-foreground" />
        </CardHeader>
        <CardContent>
          <div className="text-2xl font-bold">
            {metrics.events_processed.toLocaleString()}
          </div>
          <p className="text-xs text-muted-foreground">Market events processed</p>
        </CardContent>
      </Card>

      {/* Shadow Orders Card */}
      <Card>
        <CardHeader className="flex flex-row items-center justify-between space-y-0 pb-2">
          <CardTitle className="text-sm font-medium">Shadow Orders</CardTitle>
          <AlertCircle className="h-4 w-4 text-muted-foreground" />
        </CardHeader>
        <CardContent>
          <div className="text-2xl font-bold">{metrics.shadow_orders_fired}</div>
          <p className="text-xs text-muted-foreground">
            {metrics.shadow_orders_filled} filled (
            {metrics.shadow_orders_fired > 0
              ? (
                  (metrics.shadow_orders_filled / metrics.shadow_orders_fired) *
                  100
                ).toFixed(1)
              : "0"}
            %)
          </p>
        </CardContent>
      </Card>

      {/* Execution Rate Card */}
      <Card>
        <CardHeader className="flex flex-row items-center justify-between space-y-0 pb-2">
          <CardTitle className="text-sm font-medium">Execution Rate</CardTitle>
          <Activity className="h-4 w-4 text-muted-foreground" />
        </CardHeader>
        <CardContent>
          <div className="text-2xl font-bold">
            {metrics.opportunities_detected > 0
              ? formatPercent(
                  metrics.trades_executed / metrics.opportunities_detected
                )
              : "—"}
          </div>
          <p className="text-xs text-muted-foreground">
            Trades per opportunity
          </p>
        </CardContent>
      </Card>
    </div>
  );
}

/**
 * Skeleton loader for MetricsCards when data is not yet available.
 */
function MetricsCardsSkeleton() {
  return (
    <div className="grid grid-cols-1 gap-4 sm:grid-cols-2 lg:grid-cols-4">
      {Array.from({ length: 8 }).map((_, i) => (
        <Card key={i}>
          <CardHeader className="flex flex-row items-center justify-between space-y-0 pb-2">
            <div className="h-4 w-16 animate-pulse rounded bg-muted" />
            <div className="h-4 w-4 animate-pulse rounded bg-muted" />
          </CardHeader>
          <CardContent>
            <div className="mb-1 h-8 w-24 animate-pulse rounded bg-muted" />
            <div className="h-3 w-32 animate-pulse rounded bg-muted" />
          </CardContent>
        </Card>
      ))}
    </div>
  );
}

export default MetricsCards;
