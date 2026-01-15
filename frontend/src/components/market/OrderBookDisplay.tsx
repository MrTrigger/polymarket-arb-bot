import { useMemo } from "react";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Badge } from "@/components/ui/badge";
import { TrendingUp, AlertTriangle } from "lucide-react";
import type { ActiveMarket, OrderBookSummary } from "@/lib/types";
import { parseDecimal } from "@/lib/types";

interface OrderBookDisplayProps {
  /** The market containing order book data. */
  market: ActiveMarket;
}

/**
 * Calculate staleness indicator from last update timestamp.
 * Returns null if data is fresh, otherwise a label and color.
 */
function calculateStaleness(
  lastUpdateMs: number,
  nowMs: number
): { label: string; color: string } | null {
  const age = nowMs - lastUpdateMs;
  if (age < 1000) return null;
  if (age < 5000) return { label: "1s ago", color: "text-yellow-500" };
  if (age < 10000)
    return { label: `${Math.floor(age / 1000)}s ago`, color: "text-orange-500" };
  return { label: `${Math.floor(age / 1000)}s ago`, color: "text-red-500" };
}

/**
 * Displays a single order book (YES or NO) with bid/ask/spread.
 */
interface SingleBookProps {
  /** Label for this book (YES or NO). */
  label: string;
  /** Order book summary data. */
  book: OrderBookSummary | null;
  /** Whether this side participates in an arb opportunity. */
  hasArb: boolean;
  /** Current timestamp for staleness calculation. */
  nowMs: number;
}

function SingleBook({ label, book, hasArb, nowMs }: SingleBookProps) {
  if (!book) {
    return (
      <div className="flex-1 rounded-lg border border-border bg-muted/20 p-4">
        <div className="mb-3 flex items-center justify-between">
          <span className="font-semibold">{label}</span>
        </div>
        <div className="flex h-32 items-center justify-center text-sm text-muted-foreground">
          No order book data
        </div>
      </div>
    );
  }

  const bid = parseDecimal(book.best_bid);
  const ask = parseDecimal(book.best_ask);
  const bidSize = parseDecimal(book.best_bid_size);
  const askSize = parseDecimal(book.best_ask_size);

  // Format price as cents (Polymarket uses 0-1 range)
  const formatCents = (value: number) => `${(value * 100).toFixed(1)}c`;
  const formatSize = (value: number) => value.toFixed(2);

  // Calculate how stale the data is using passed-in timestamp
  const staleness = calculateStaleness(book.last_update_ms, nowMs);

  return (
    <div
      className={`flex-1 rounded-lg border p-4 ${
        hasArb
          ? "border-yellow-500/50 bg-yellow-500/5"
          : "border-border bg-muted/20"
      }`}
    >
      <div className="mb-3 flex items-center justify-between">
        <span className="font-semibold">{label}</span>
        {staleness && (
          <span className={`text-xs ${staleness.color}`}>{staleness.label}</span>
        )}
      </div>

      {/* Bid row */}
      <div className="mb-2 flex items-center justify-between text-sm">
        <span className="text-muted-foreground">Bid</span>
        <div className="flex items-center gap-3">
          <span className="text-green-500 font-mono font-medium">
            {formatCents(bid)}
          </span>
          <span className="text-xs text-muted-foreground">
            {formatSize(bidSize)} qty
          </span>
        </div>
      </div>

      {/* Ask row */}
      <div className="mb-3 flex items-center justify-between text-sm">
        <span className="text-muted-foreground">Ask</span>
        <div className="flex items-center gap-3">
          <span className="text-red-500 font-mono font-medium">
            {formatCents(ask)}
          </span>
          <span className="text-xs text-muted-foreground">
            {formatSize(askSize)} qty
          </span>
        </div>
      </div>

      {/* Spread row */}
      <div className="border-t border-border pt-2">
        <div className="flex items-center justify-between text-sm">
          <span className="text-muted-foreground">Spread</span>
          <span className="font-mono text-muted-foreground">
            {book.spread_bps} bps
          </span>
        </div>
      </div>
    </div>
  );
}

/**
 * OrderBookDisplay component shows YES and NO order books side by side.
 *
 * Features:
 * - Displays best bid/ask prices in cents (Polymarket format)
 * - Shows order sizes for each level
 * - Spread calculation in basis points
 * - Combined arbitrage calculation: 1.0 - YES_ask - NO_ask
 * - Visual highlight when arbitrage opportunity exists
 * - Staleness indicator for old data
 */
export function OrderBookDisplay({ market }: OrderBookDisplayProps) {
  // Get stable timestamp for staleness calculations
  // We derive it from market data's own timestamps so it's deterministic
  const nowMs = useMemo(() => {
    // Use the latest update timestamp from either book, plus a small buffer
    const yesUpdate = market.yes_book?.last_update_ms ?? 0;
    const noUpdate = market.no_book?.last_update_ms ?? 0;
    return Math.max(yesUpdate, noUpdate) + 100;
  }, [market.yes_book?.last_update_ms, market.no_book?.last_update_ms]);

  // Calculate arbitrage metrics
  const arbMetrics = useMemo(() => {
    const yesAsk = market.yes_book ? parseDecimal(market.yes_book.best_ask) : null;
    const noAsk = market.no_book ? parseDecimal(market.no_book.best_ask) : null;

    if (yesAsk === null || noAsk === null) {
      return {
        combinedAsk: null,
        arbSpread: null,
        arbSpreadBps: null,
        hasArb: false,
        maxBuySize: null,
      };
    }

    const combinedAsk = yesAsk + noAsk;
    const arbSpread = 1.0 - combinedAsk; // Positive = opportunity
    const arbSpreadBps = Math.round(arbSpread * 10000);

    // Calculate max size that can be traded (min of YES and NO ask sizes)
    const yesAskSize = parseDecimal(market.yes_book!.best_ask_size);
    const noAskSize = parseDecimal(market.no_book!.best_ask_size);
    const maxBuySize = Math.min(yesAskSize, noAskSize);

    return {
      combinedAsk,
      arbSpread,
      arbSpreadBps,
      hasArb: arbSpread > 0,
      maxBuySize,
    };
  }, [market.yes_book, market.no_book]);

  // Get arb spread color
  const getArbColor = (bps: number | null) => {
    if (bps === null) return "text-muted-foreground";
    if (bps > 50) return "text-green-400";
    if (bps > 20) return "text-green-500";
    if (bps > 0) return "text-yellow-500";
    return "text-red-500";
  };

  return (
    <Card className="h-full">
      <CardHeader className="flex flex-row items-center justify-between space-y-0 pb-2">
        <CardTitle className="text-sm font-medium">Order Book</CardTitle>
        {market.has_arb_opportunity && (
          <Badge variant="secondary" className="bg-yellow-500/10 text-yellow-500">
            <TrendingUp className="mr-1 h-3 w-3" />
            Arb Active
          </Badge>
        )}
      </CardHeader>
      <CardContent className="space-y-4">
        {/* Order books side by side */}
        <div className="flex gap-4">
          <SingleBook
            label="YES"
            book={market.yes_book}
            hasArb={arbMetrics.hasArb}
            nowMs={nowMs}
          />
          <SingleBook
            label="NO"
            book={market.no_book}
            hasArb={arbMetrics.hasArb}
            nowMs={nowMs}
          />
        </div>

        {/* Combined arbitrage calculation */}
        <div
          className={`rounded-lg border p-4 ${
            arbMetrics.hasArb
              ? "border-yellow-500/50 bg-yellow-500/10"
              : "border-border bg-muted/20"
          }`}
        >
          <div className="mb-2 flex items-center justify-between">
            <span className="text-sm font-medium">Arbitrage Analysis</span>
            {arbMetrics.hasArb && (
              <AlertTriangle className="h-4 w-4 text-yellow-500" />
            )}
          </div>

          {arbMetrics.combinedAsk !== null ? (
            <div className="space-y-2 text-sm">
              {/* Formula row */}
              <div className="flex items-center justify-between text-muted-foreground">
                <span>Combined Ask (YES + NO)</span>
                <span className="font-mono">
                  {(arbMetrics.combinedAsk * 100).toFixed(1)}c
                </span>
              </div>

              {/* Arb spread row */}
              <div className="flex items-center justify-between">
                <span className="text-muted-foreground">
                  Arb Spread (1.00 - Combined)
                </span>
                <div className="flex items-center gap-2">
                  <span className={`font-mono font-semibold ${getArbColor(arbMetrics.arbSpreadBps)}`}>
                    {arbMetrics.arbSpread! >= 0 ? "+" : ""}
                    {(arbMetrics.arbSpread! * 100).toFixed(2)}c
                  </span>
                  <span className={`text-xs ${getArbColor(arbMetrics.arbSpreadBps)}`}>
                    ({arbMetrics.arbSpreadBps} bps)
                  </span>
                </div>
              </div>

              {/* Max size row */}
              {arbMetrics.hasArb && arbMetrics.maxBuySize !== null && (
                <div className="flex items-center justify-between border-t border-border pt-2">
                  <span className="text-muted-foreground">Max Arb Size</span>
                  <span className="font-mono text-green-500">
                    {arbMetrics.maxBuySize.toFixed(2)} shares
                  </span>
                </div>
              )}

              {/* Profit estimate */}
              {arbMetrics.hasArb && arbMetrics.maxBuySize !== null && arbMetrics.arbSpread !== null && (
                <div className="flex items-center justify-between">
                  <span className="text-muted-foreground">Est. Profit (before fees)</span>
                  <span className="font-mono font-semibold text-green-400">
                    ${(arbMetrics.maxBuySize * arbMetrics.arbSpread).toFixed(4)}
                  </span>
                </div>
              )}
            </div>
          ) : (
            <div className="flex h-16 items-center justify-center text-sm text-muted-foreground">
              Insufficient order book data
            </div>
          )}
        </div>

        {/* Explanation tooltip */}
        <div className="text-xs text-muted-foreground">
          <p>
            Arbitrage exists when buying 1 YES + 1 NO costs less than $1.00.
            The spread shows the guaranteed profit per share if market resolves.
          </p>
        </div>
      </CardContent>
    </Card>
  );
}

/**
 * Skeleton loader for OrderBookDisplay when data is loading.
 */
export function OrderBookDisplaySkeleton() {
  return (
    <Card className="h-full">
      <CardHeader className="flex flex-row items-center justify-between space-y-0 pb-2">
        <div className="h-4 w-24 animate-pulse rounded bg-muted" />
      </CardHeader>
      <CardContent className="space-y-4">
        <div className="flex gap-4">
          <div className="flex-1 rounded-lg border border-border bg-muted/20 p-4">
            <div className="mb-3 h-5 w-12 animate-pulse rounded bg-muted" />
            <div className="space-y-2">
              <div className="h-4 w-full animate-pulse rounded bg-muted" />
              <div className="h-4 w-full animate-pulse rounded bg-muted" />
              <div className="h-4 w-2/3 animate-pulse rounded bg-muted" />
            </div>
          </div>
          <div className="flex-1 rounded-lg border border-border bg-muted/20 p-4">
            <div className="mb-3 h-5 w-12 animate-pulse rounded bg-muted" />
            <div className="space-y-2">
              <div className="h-4 w-full animate-pulse rounded bg-muted" />
              <div className="h-4 w-full animate-pulse rounded bg-muted" />
              <div className="h-4 w-2/3 animate-pulse rounded bg-muted" />
            </div>
          </div>
        </div>
        <div className="rounded-lg border border-border p-4">
          <div className="mb-2 h-4 w-32 animate-pulse rounded bg-muted" />
          <div className="space-y-2">
            <div className="h-4 w-full animate-pulse rounded bg-muted" />
            <div className="h-4 w-2/3 animate-pulse rounded bg-muted" />
          </div>
        </div>
      </CardContent>
    </Card>
  );
}

export default OrderBookDisplay;
