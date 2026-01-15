import { useParams, Link } from "react-router-dom";
import { ArrowLeft } from "lucide-react";
import { useDashboardState } from "@/hooks";
import { formatUsd, formatPercent, formatTimeRemaining } from "@/lib/types";
import { PriceChart, OrderBookDisplay } from "@/components/market";

/**
 * Market detail page showing price chart, order book, position, and trades.
 * Components will be added in subsequent tasks.
 */
export function MarketDetail() {
  const { eventId } = useParams<{ eventId: string }>();
  const { initialized, markets, positions, recentTrades } = useDashboardState();

  const market = markets.find((m) => m.event_id === eventId);
  const position = positions.find((p) => p.event_id === eventId);
  const marketTrades = recentTrades.filter((t) => t.event_id === eventId);

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

  if (!market) {
    return (
      <div className="p-6">
        <Link
          to="/"
          className="mb-6 inline-flex items-center gap-2 text-sm text-muted-foreground hover:text-foreground"
        >
          <ArrowLeft className="h-4 w-4" />
          Back to Dashboard
        </Link>
        <div className="flex h-64 items-center justify-center rounded-lg border border-border bg-card">
          <div className="text-center">
            <div className="mb-2 text-lg text-muted-foreground">
              Market not found
            </div>
            <div className="text-sm text-muted-foreground">
              Event ID: {eventId}
            </div>
          </div>
        </div>
      </div>
    );
  }

  return (
    <div className="p-6">
      {/* Back navigation */}
      <Link
        to="/"
        className="mb-6 inline-flex items-center gap-2 text-sm text-muted-foreground hover:text-foreground"
      >
        <ArrowLeft className="h-4 w-4" />
        Back to Dashboard
      </Link>

      {/* Market header */}
      <div className="mb-6">
        <div className="flex items-center gap-4">
          <h1 className="text-2xl font-semibold">{market.asset}</h1>
          {market.has_arb_opportunity && (
            <span className="rounded-full bg-yellow-500/10 px-3 py-1 text-sm text-yellow-500">
              Arb Opportunity
            </span>
          )}
        </div>
        <div className="mt-2 flex gap-6 text-sm text-muted-foreground">
          <span>Strike: {formatUsd(market.strike_price)}</span>
          <span>Spot: {formatUsd(market.spot_price)}</span>
          <span>Time Left: {formatTimeRemaining(market.seconds_remaining)}</span>
          <span>
            Spread: {(parseFloat(market.arb_spread) * 100).toFixed(2)}%
          </span>
        </div>
      </div>

      {/* Main content grid */}
      <div className="grid grid-cols-1 gap-6 lg:grid-cols-2">
        {/* Price chart */}
        <PriceChart market={market} trades={marketTrades} />

        {/* Order book display */}
        <OrderBookDisplay market={market} />

        {/* Position panel */}
        <div className="rounded-lg border border-border bg-card p-4">
          <h3 className="mb-2 font-semibold">Position</h3>
          {position ? (
            <div className="space-y-3">
              <div className="grid grid-cols-2 gap-4 text-sm">
                <div>
                  <div className="text-muted-foreground">YES Shares</div>
                  <div className="font-medium">{position.yes_shares}</div>
                </div>
                <div>
                  <div className="text-muted-foreground">NO Shares</div>
                  <div className="font-medium">{position.no_shares}</div>
                </div>
                <div>
                  <div className="text-muted-foreground">YES Cost Basis</div>
                  <div className="font-medium">
                    {formatUsd(position.yes_cost_basis)}
                  </div>
                </div>
                <div>
                  <div className="text-muted-foreground">NO Cost Basis</div>
                  <div className="font-medium">
                    {formatUsd(position.no_cost_basis)}
                  </div>
                </div>
              </div>
              <div className="border-t border-border pt-3">
                <div className="grid grid-cols-3 gap-4 text-sm">
                  <div>
                    <div className="text-muted-foreground">Realized P&L</div>
                    <div
                      className={`font-medium ${parseFloat(position.realized_pnl) >= 0 ? "text-green-500" : "text-red-500"}`}
                    >
                      {formatUsd(position.realized_pnl)}
                    </div>
                  </div>
                  <div>
                    <div className="text-muted-foreground">Exposure</div>
                    <div className="font-medium">
                      {formatUsd(position.total_exposure)}
                    </div>
                  </div>
                  <div>
                    <div className="text-muted-foreground">Imbalance</div>
                    <div className="font-medium">
                      {formatPercent(position.imbalance_ratio)}
                    </div>
                  </div>
                </div>
              </div>
              <div className="border-t border-border pt-3">
                <span
                  className={`rounded-full px-2 py-0.5 text-xs ${
                    position.inventory_state === "balanced"
                      ? "bg-green-500/10 text-green-500"
                      : position.inventory_state === "skewed"
                        ? "bg-yellow-500/10 text-yellow-500"
                        : position.inventory_state === "exposed"
                          ? "bg-orange-500/10 text-orange-500"
                          : "bg-red-500/10 text-red-500"
                  }`}
                >
                  {position.inventory_state}
                </span>
              </div>
            </div>
          ) : (
            <div className="flex h-32 items-center justify-center text-muted-foreground">
              No position in this market
            </div>
          )}
        </div>

        {/* Trades table */}
        <div className="rounded-lg border border-border bg-card p-4">
          <h3 className="mb-2 font-semibold">Recent Trades</h3>
          {marketTrades.length === 0 ? (
            <div className="flex h-32 items-center justify-center text-muted-foreground">
              No trades in this market
            </div>
          ) : (
            <div className="max-h-64 overflow-y-auto">
              <table className="w-full text-sm">
                <thead className="sticky top-0 bg-card">
                  <tr className="border-b border-border">
                    <th className="pb-2 text-left text-muted-foreground">
                      Time
                    </th>
                    <th className="pb-2 text-left text-muted-foreground">
                      Side
                    </th>
                    <th className="pb-2 text-left text-muted-foreground">
                      Type
                    </th>
                    <th className="pb-2 text-right text-muted-foreground">
                      Price
                    </th>
                    <th className="pb-2 text-right text-muted-foreground">
                      Size
                    </th>
                  </tr>
                </thead>
                <tbody>
                  {marketTrades.slice(0, 10).map((trade) => (
                    <tr key={trade.trade_id} className="border-b border-border">
                      <td className="py-2 text-muted-foreground">
                        {new Date(trade.fill_time).toLocaleTimeString()}
                      </td>
                      <td
                        className={`py-2 ${trade.side === "BUY" ? "text-green-500" : "text-red-500"}`}
                      >
                        {trade.side}
                      </td>
                      <td className="py-2">{trade.outcome.toUpperCase()}</td>
                      <td className="py-2 text-right">
                        {formatUsd(trade.fill_price)}
                      </td>
                      <td className="py-2 text-right">{trade.fill_size}</td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          )}
        </div>
      </div>
    </div>
  );
}

export default MarketDetail;
