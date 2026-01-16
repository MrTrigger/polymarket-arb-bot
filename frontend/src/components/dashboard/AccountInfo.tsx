import { useMemo } from "react";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { useDashboardState } from "@/hooks";
import { parseDecimal, formatUsd } from "@/lib/types";
import {
  Wallet,
  PiggyBank,
  TrendingUp,
  TrendingDown,
  BarChart3,
} from "lucide-react";

/**
 * AccountInfo panel component.
 *
 * Displays account-level statistics including:
 * - Total account balance (real exchange balance)
 * - Allocated balance (configured trading capital)
 * - Positions value (sum of all cost basis)
 * - Realized P&L (from settled trades)
 * - Unrealized P&L (mark-to-market estimate)
 */
export function AccountInfo() {
  const { metrics, positions, markets } = useDashboardState();

  // Calculate account stats from positions and metrics
  const accountStats = useMemo(() => {
    // Get balances from metrics (set by backend)
    const allocatedBalance = metrics ? parseDecimal(metrics.allocated_balance) : 0;
    const currentBalance = metrics ? parseDecimal(metrics.current_balance) : 0;
    const realizedPnl = metrics ? parseDecimal(metrics.pnl_usdc) : 0;

    if (!positions || positions.length === 0) {
      return {
        allocatedBalance,
        currentBalance,
        realizedPnl,
        unrealizedPnl: 0,
        totalCostBasis: 0,
        positionCount: 0,
      };
    }

    let unrealizedPnl = 0;
    let totalCostBasis = 0;

    for (const position of positions) {
      const yesCost = parseDecimal(position.yes_cost_basis);
      const noCost = parseDecimal(position.no_cost_basis);
      const yesShares = parseDecimal(position.yes_shares);
      const noShares = parseDecimal(position.no_shares);

      totalCostBasis += yesCost + noCost;

      // Find market to get spot/strike for unrealized P&L calculation
      const market = markets?.find((m) => m.event_id === position.event_id);
      if (market) {
        const spotPrice = parseDecimal(market.spot_price);
        const strikePrice = parseDecimal(market.strike_price);

        // Binary outcome: YES wins if spot > strike
        if (strikePrice > 0) {
          const yesWins = spotPrice > strikePrice;
          const settlementValue = yesWins ? yesShares : noShares;
          const positionCost = yesCost + noCost;
          unrealizedPnl += settlementValue - positionCost;
        }
      }
    }

    return {
      allocatedBalance,
      currentBalance,
      realizedPnl,
      unrealizedPnl,
      totalCostBasis,
      positionCount: positions.length,
    };
  }, [positions, markets, metrics]);

  return (
    <Card>
      <CardHeader className="pb-3">
        <CardTitle className="flex items-center gap-2 text-sm font-medium">
          <Wallet className="h-4 w-4" />
          Account Summary
        </CardTitle>
      </CardHeader>
      <CardContent>
        <div className="space-y-4">
          {/* Balance section */}
          <div className="grid grid-cols-2 gap-3">
            {/* Account Balance */}
            <div className="space-y-1">
              <div className="flex items-center gap-1 text-xs text-muted-foreground">
                <Wallet className="h-3 w-3" />
                Account Balance
              </div>
              <div className="text-lg font-semibold">
                {formatUsd(accountStats.currentBalance.toString())}
              </div>
            </div>

            {/* Allocated to Bot */}
            <div className="space-y-1">
              <div className="flex items-center gap-1 text-xs text-muted-foreground">
                <BarChart3 className="h-3 w-3" />
                Allocated to Bot
              </div>
              <div className="text-lg font-semibold text-blue-500">
                {formatUsd(accountStats.allocatedBalance.toString())}
              </div>
            </div>
          </div>

          {/* Positions & P&L section */}
          <div className="border-t pt-3">
            <div className="grid grid-cols-3 gap-3">
              {/* Positions Value */}
              <div className="space-y-1">
                <div className="flex items-center gap-1 text-xs text-muted-foreground">
                  <PiggyBank className="h-3 w-3" />
                  In Positions
                </div>
                <div className="text-base font-semibold">
                  {formatUsd(accountStats.totalCostBasis.toString())}
                </div>
                <div className="text-xs text-muted-foreground">
                  {accountStats.positionCount} pos
                </div>
              </div>

              {/* Realized P&L */}
              <div className="space-y-1">
                <div className="flex items-center gap-1 text-xs text-muted-foreground">
                  {accountStats.realizedPnl >= 0 ? (
                    <TrendingUp className="h-3 w-3 text-green-500" />
                  ) : (
                    <TrendingDown className="h-3 w-3 text-red-500" />
                  )}
                  Realized
                </div>
                <div className={`text-base font-semibold ${accountStats.realizedPnl >= 0 ? "text-green-500" : "text-red-500"}`}>
                  {accountStats.realizedPnl >= 0 ? "+" : ""}
                  {formatUsd(accountStats.realizedPnl.toString())}
                </div>
              </div>

              {/* Unrealized P&L */}
              <div className="space-y-1">
                <div className="flex items-center gap-1 text-xs text-muted-foreground">
                  {accountStats.unrealizedPnl >= 0 ? (
                    <TrendingUp className="h-3 w-3 text-green-500" />
                  ) : (
                    <TrendingDown className="h-3 w-3 text-red-500" />
                  )}
                  Unrealized
                </div>
                <div className={`text-base font-semibold ${accountStats.unrealizedPnl >= 0 ? "text-green-500" : "text-red-500"}`}>
                  {accountStats.unrealizedPnl >= 0 ? "+" : ""}
                  {formatUsd(accountStats.unrealizedPnl.toString())}
                </div>
              </div>
            </div>
          </div>
        </div>
      </CardContent>
    </Card>
  );
}

/**
 * Skeleton loader for AccountInfo when data is loading.
 */
export function AccountInfoSkeleton() {
  return (
    <Card>
      <CardHeader className="pb-3">
        <div className="flex items-center gap-2">
          <div className="h-4 w-4 animate-pulse rounded bg-muted" />
          <div className="h-4 w-32 animate-pulse rounded bg-muted" />
        </div>
      </CardHeader>
      <CardContent>
        <div className="space-y-4">
          <div className="grid grid-cols-2 gap-3">
            {Array.from({ length: 2 }).map((_, i) => (
              <div key={i} className="space-y-1">
                <div className="h-3 w-20 animate-pulse rounded bg-muted" />
                <div className="h-6 w-24 animate-pulse rounded bg-muted" />
              </div>
            ))}
          </div>
          <div className="border-t pt-3">
            <div className="grid grid-cols-3 gap-3">
              {Array.from({ length: 3 }).map((_, i) => (
                <div key={i} className="space-y-1">
                  <div className="h-3 w-16 animate-pulse rounded bg-muted" />
                  <div className="h-5 w-20 animate-pulse rounded bg-muted" />
                </div>
              ))}
            </div>
          </div>
        </div>
      </CardContent>
    </Card>
  );
}

export default AccountInfo;
