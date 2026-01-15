import { useParams, Link } from "react-router-dom";
import { ArrowLeft } from "lucide-react";
import { useDashboardState } from "@/hooks";
import { formatUsd, formatTimeRemaining } from "@/lib/types";
import { PriceChart, OrderBookDisplay, PositionPanel, TradesTable } from "@/components/market";

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
        <PositionPanel position={position ?? null} market={market} />

        {/* Trades table */}
        <TradesTable trades={marketTrades} />
      </div>
    </div>
  );
}

export default MarketDetail;
