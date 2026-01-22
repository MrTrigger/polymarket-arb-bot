import { useParams, Link, useNavigate } from "react-router-dom";
import { useEffect, useRef } from "react";
import { ArrowLeft, ChevronLeft, ChevronRight } from "lucide-react";
import { useDashboardState } from "@/hooks";
import { formatUsd, formatTimeRemaining } from "@/lib/types";
import { PriceChart, OrderBookDisplay, PositionPanel, TradesTable } from "@/components/market";

/**
 * Market detail page showing price chart, order book, position, and trades.
 * Auto-navigates to next market when current one expires.
 */
export function MarketDetail() {
  const { eventId } = useParams<{ eventId: string }>();
  const navigate = useNavigate();
  const { initialized, markets, positions, recentTrades } = useDashboardState();

  // Track the asset of the current market for finding the next one
  const lastAssetRef = useRef<string | null>(null);

  const market = markets.find((m) => m.event_id === eventId);
  const position = positions.find((p) => p.event_id === eventId);
  const marketTrades = recentTrades.filter((t) => t.event_id === eventId);

  // Update the last known asset when we have a market
  useEffect(() => {
    if (market) {
      lastAssetRef.current = market.asset;
    }
  }, [market]);

  // Find next/prev markets for navigation
  const currentIndex = markets.findIndex((m) => m.event_id === eventId);
  const prevMarket = currentIndex > 0 ? markets[currentIndex - 1] : null;
  const nextMarket = currentIndex >= 0 && currentIndex < markets.length - 1
    ? markets[currentIndex + 1]
    : null;

  // Auto-navigate when current market expires (time runs out or disappears from list)
  useEffect(() => {
    if (!initialized || markets.length === 0) return;

    // Check if we should navigate away from current market
    const shouldNavigate = !market || market.seconds_remaining <= 0;
    if (!shouldNavigate) return;

    const lastAsset = lastAssetRef.current;

    // Find next market - prefer same asset with time remaining, then any market with time
    let nextMarketToShow = lastAsset
      ? markets.find((m) => m.asset === lastAsset && m.seconds_remaining > 0 && m.event_id !== eventId)
      : null;

    // If no market with same asset, pick any market with time remaining
    if (!nextMarketToShow) {
      nextMarketToShow = markets.find((m) => m.seconds_remaining > 0 && m.event_id !== eventId);
    }

    // Last resort: any market that's not the current one
    if (!nextMarketToShow) {
      nextMarketToShow = markets.find((m) => m.event_id !== eventId);
    }

    if (nextMarketToShow) {
      navigate(`/market/${nextMarketToShow.event_id}`, { replace: true });
    }
  }, [initialized, markets, market, eventId, navigate]);

  // Keyboard navigation (left/right arrows)
  useEffect(() => {
    const handleKeyDown = (e: KeyboardEvent) => {
      // Don't navigate if user is typing in an input
      if (e.target instanceof HTMLInputElement || e.target instanceof HTMLTextAreaElement) {
        return;
      }

      if (e.key === "ArrowLeft" && prevMarket) {
        navigate(`/market/${prevMarket.event_id}`);
      } else if (e.key === "ArrowRight" && nextMarket) {
        navigate(`/market/${nextMarket.event_id}`);
      }
    };

    window.addEventListener("keydown", handleKeyDown);
    return () => window.removeEventListener("keydown", handleKeyDown);
  }, [prevMarket, nextMarket, navigate]);

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
      {/* Navigation bar */}
      <div className="mb-6 flex items-center justify-between">
        <Link
          to="/"
          className="inline-flex items-center gap-2 text-sm text-muted-foreground hover:text-foreground"
        >
          <ArrowLeft className="h-4 w-4" />
          Back to Dashboard
        </Link>

        {/* Prev/Next market navigation */}
        <div className="flex items-center gap-2">
          {prevMarket ? (
            <Link
              to={`/market/${prevMarket.event_id}`}
              className="inline-flex items-center gap-1 rounded-md border border-border px-3 py-1.5 text-sm text-muted-foreground hover:bg-muted hover:text-foreground"
            >
              <ChevronLeft className="h-4 w-4" />
              {prevMarket.asset}
            </Link>
          ) : (
            <span className="inline-flex items-center gap-1 rounded-md border border-border/50 px-3 py-1.5 text-sm text-muted-foreground/50">
              <ChevronLeft className="h-4 w-4" />
              Prev
            </span>
          )}
          <span className="text-sm text-muted-foreground">
            {currentIndex + 1} / {markets.length}
          </span>
          {nextMarket ? (
            <Link
              to={`/market/${nextMarket.event_id}`}
              className="inline-flex items-center gap-1 rounded-md border border-border px-3 py-1.5 text-sm text-muted-foreground hover:bg-muted hover:text-foreground"
            >
              {nextMarket.asset}
              <ChevronRight className="h-4 w-4" />
            </Link>
          ) : (
            <span className="inline-flex items-center gap-1 rounded-md border border-border/50 px-3 py-1.5 text-sm text-muted-foreground/50">
              Next
              <ChevronRight className="h-4 w-4" />
            </span>
          )}
        </div>
      </div>

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

      {/* Price chart - full width */}
      <div className="mb-6">
        <PriceChart market={market} trades={marketTrades} />
      </div>

      {/* Main content grid */}
      <div className="grid grid-cols-1 gap-6 lg:grid-cols-2">
        {/* Order book display */}
        <OrderBookDisplay market={market} />

        {/* Arbitrage Analysis placeholder - right side */}
        <div /> {/* Empty placeholder for grid alignment */}

        {/* Position panel */}
        <PositionPanel position={position ?? null} market={market} />

        {/* Trades table */}
        <TradesTable trades={marketTrades} />
      </div>
    </div>
  );
}

export default MarketDetail;
