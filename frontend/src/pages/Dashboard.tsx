import { useDashboardState } from "@/hooks";
import { formatUsd } from "@/lib/types";
import { MetricsCards } from "@/components/dashboard";

/**
 * Main dashboard page showing trading metrics, markets, and logs.
 * Components will be added in subsequent tasks.
 */
export function Dashboard() {
  const {
    initialized,
    markets,
    tradingEnabled,
    circuitBreakerTripped,
    arbOpportunities,
  } = useDashboardState();

  if (!initialized) {
    return (
      <div className="flex h-[calc(100vh-73px)] items-center justify-center">
        <div className="text-center">
          <div className="mb-4 text-lg text-muted-foreground">
            Waiting for data...
          </div>
          <div className="text-sm text-muted-foreground">
            Connect to the bot to see live data
          </div>
        </div>
      </div>
    );
  }

  return (
    <div className="p-6">
      {/* Metrics Cards */}
      <div className="mb-6">
        <MetricsCards />
      </div>

      {/* Status indicators */}
      <div className="mb-6 flex gap-4">
        <div
          className={`rounded-full px-3 py-1 text-sm ${tradingEnabled ? "bg-green-500/10 text-green-500" : "bg-red-500/10 text-red-500"}`}
        >
          Trading: {tradingEnabled ? "Enabled" : "Disabled"}
        </div>
        {circuitBreakerTripped && (
          <div className="rounded-full bg-red-500/10 px-3 py-1 text-sm text-red-500">
            Circuit Breaker Tripped
          </div>
        )}
        {arbOpportunities.length > 0 && (
          <div className="rounded-full bg-yellow-500/10 px-3 py-1 text-sm text-yellow-500">
            {arbOpportunities.length} Arb Opportunit
            {arbOpportunities.length === 1 ? "y" : "ies"}
          </div>
        )}
      </div>

      {/* Placeholder for markets grid - to be replaced by MarketsGrid component */}
      <div className="mb-6">
        <h2 className="mb-4 text-lg font-semibold">Active Markets</h2>
        {markets.length === 0 ? (
          <div className="rounded-lg border border-border bg-card p-8 text-center text-muted-foreground">
            No active markets
          </div>
        ) : (
          <div className="overflow-x-auto rounded-lg border border-border">
            <table className="w-full">
              <thead className="border-b border-border bg-muted/50">
                <tr>
                  <th className="px-4 py-3 text-left text-sm font-medium text-muted-foreground">
                    Asset
                  </th>
                  <th className="px-4 py-3 text-left text-sm font-medium text-muted-foreground">
                    Strike
                  </th>
                  <th className="px-4 py-3 text-left text-sm font-medium text-muted-foreground">
                    Time Left
                  </th>
                  <th className="px-4 py-3 text-left text-sm font-medium text-muted-foreground">
                    Spot
                  </th>
                  <th className="px-4 py-3 text-left text-sm font-medium text-muted-foreground">
                    Arb Spread
                  </th>
                </tr>
              </thead>
              <tbody>
                {markets.map((market) => (
                  <tr
                    key={market.event_id}
                    className="border-b border-border last:border-0 hover:bg-muted/50"
                  >
                    <td className="px-4 py-3 font-medium">{market.asset}</td>
                    <td className="px-4 py-3">
                      {formatUsd(market.strike_price)}
                    </td>
                    <td className="px-4 py-3">
                      {Math.floor(market.seconds_remaining / 60)}m{" "}
                      {market.seconds_remaining % 60}s
                    </td>
                    <td className="px-4 py-3">{formatUsd(market.spot_price)}</td>
                    <td
                      className={`px-4 py-3 ${market.has_arb_opportunity ? "text-yellow-500" : ""}`}
                    >
                      {(parseFloat(market.arb_spread) * 100).toFixed(2)}%
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </div>

      {/* Placeholder for equity curve and logs - to be added in subsequent tasks */}
      <div className="grid grid-cols-1 gap-6 lg:grid-cols-2">
        <div className="rounded-lg border border-border bg-card p-4">
          <h3 className="mb-2 font-semibold">Equity Curve</h3>
          <div className="flex h-48 items-center justify-center text-muted-foreground">
            Chart component coming soon
          </div>
        </div>
        <div className="rounded-lg border border-border bg-card p-4">
          <h3 className="mb-2 font-semibold">Recent Logs</h3>
          <div className="flex h-48 items-center justify-center text-muted-foreground">
            Log window component coming soon
          </div>
        </div>
      </div>
    </div>
  );
}

export default Dashboard;
