//! Higher-timeframe trend tracker for confidence adjustment.
//!
//! Tracks price over a configurable window (e.g. 30-60 minutes) per asset
//! and determines if the current signal aligns with or opposes the trend.
//!
//! - Trend confirms signal → boost confidence
//! - Trend opposes signal → cut confidence
//!
//! Uses event timestamps (not wall-clock) so it works correctly in backtest.

use std::collections::HashMap;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

use poly_common::types::CryptoAsset;

/// A single price observation.
#[derive(Debug, Clone, Copy)]
struct PricePoint {
    timestamp_ms: i64,
    price: Decimal,
}

/// Per-asset circular buffer of price observations.
#[derive(Debug, Clone)]
struct AssetTrend {
    /// Circular buffer of price points.
    buffer: Vec<PricePoint>,
    /// Write position in circular buffer.
    write_pos: usize,
    /// Number of valid entries (up to capacity).
    count: usize,
}

impl AssetTrend {
    fn new(capacity: usize) -> Self {
        Self {
            buffer: vec![PricePoint { timestamp_ms: 0, price: Decimal::ZERO }; capacity],
            write_pos: 0,
            count: 0,
        }
    }

    fn push(&mut self, timestamp_ms: i64, price: Decimal) {
        self.buffer[self.write_pos] = PricePoint { timestamp_ms, price };
        self.write_pos = (self.write_pos + 1) % self.buffer.len();
        if self.count < self.buffer.len() {
            self.count += 1;
        }
    }

    /// Get the oldest price within the window, and the newest price.
    /// Returns (oldest_price, newest_price) or None if insufficient data.
    fn trend_prices(&self, now_ms: i64, window_ms: i64) -> Option<(Decimal, Decimal)> {
        if self.count < 2 {
            return None;
        }

        let cutoff = now_ms - window_ms;
        let mut oldest: Option<PricePoint> = None;
        let mut newest: Option<PricePoint> = None;

        // Scan all valid entries
        let start = if self.count < self.buffer.len() {
            0
        } else {
            self.write_pos // oldest entry in a full circular buffer
        };

        for i in 0..self.count {
            let idx = (start + i) % self.buffer.len();
            let point = &self.buffer[idx];

            if point.timestamp_ms < cutoff {
                continue; // Too old
            }

            if oldest.is_none() || point.timestamp_ms < oldest.unwrap().timestamp_ms {
                oldest = Some(*point);
            }
            if newest.is_none() || point.timestamp_ms > newest.unwrap().timestamp_ms {
                newest = Some(*point);
            }
        }

        match (oldest, newest) {
            (Some(o), Some(n)) if o.timestamp_ms != n.timestamp_ms => {
                Some((o.price, n.price))
            }
            _ => None,
        }
    }
}

/// Trend direction derived from price history.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrendDirection {
    Up,
    Down,
    Flat,
}

/// Configuration for the trend tracker.
#[derive(Debug, Clone)]
pub struct TrendConfig {
    /// Window size in milliseconds for trend calculation.
    /// Default: 30 minutes (1_800_000 ms).
    pub window_ms: i64,
    /// Minimum price change (as fraction) to consider a trend.
    /// Below this, trend is Flat. Default: 0.0001 (0.01%).
    pub min_trend_pct: Decimal,
    /// Capacity of circular buffer per asset (max data points).
    pub buffer_capacity: usize,
    /// Confidence boost when trend confirms signal direction.
    pub confirm_boost: Decimal,
    /// Confidence cut when trend opposes signal direction.
    pub oppose_cut: Decimal,
}

impl Default for TrendConfig {
    fn default() -> Self {
        Self {
            window_ms: 30 * 60 * 1000, // 30 minutes
            min_trend_pct: dec!(0.0001), // 0.01%
            buffer_capacity: 3600, // ~1 sample/sec for 1 hour
            confirm_boost: Decimal::ZERO, // disabled by default
            oppose_cut: Decimal::ZERO,
        }
    }
}

/// Tracks higher-timeframe price trends per asset.
#[derive(Debug, Clone)]
pub struct TrendTracker {
    config: TrendConfig,
    assets: HashMap<CryptoAsset, AssetTrend>,
}

impl TrendTracker {
    /// Create a new trend tracker with the given config.
    pub fn new(config: TrendConfig) -> Self {
        Self {
            config,
            assets: HashMap::new(),
        }
    }

    /// Record a price observation for an asset.
    pub fn record_price(&mut self, asset: CryptoAsset, timestamp_ms: i64, price: Decimal) {
        let trend = self.assets
            .entry(asset)
            .or_insert_with(|| AssetTrend::new(self.config.buffer_capacity));
        trend.push(timestamp_ms, price);
    }

    /// Get the current trend direction for an asset.
    pub fn get_trend(&self, asset: CryptoAsset, now_ms: i64) -> TrendDirection {
        let Some(trend) = self.assets.get(&asset) else {
            return TrendDirection::Flat;
        };

        let Some((old_price, new_price)) = trend.trend_prices(now_ms, self.config.window_ms) else {
            return TrendDirection::Flat;
        };

        if old_price.is_zero() {
            return TrendDirection::Flat;
        }

        let change = (new_price - old_price) / old_price;
        if change > self.config.min_trend_pct {
            TrendDirection::Up
        } else if change < -self.config.min_trend_pct {
            TrendDirection::Down
        } else {
            TrendDirection::Flat
        }
    }

    /// Apply trend-based confidence adjustment.
    ///
    /// Returns adjusted confidence. If trend confirms the signal direction,
    /// confidence is boosted. If trend opposes, confidence is cut.
    pub fn adjust_confidence(
        &self,
        confidence: Decimal,
        asset: CryptoAsset,
        signal_is_up: bool,
        now_ms: i64,
    ) -> Decimal {
        // If both boost and cut are zero, skip entirely
        if self.config.confirm_boost.is_zero() && self.config.oppose_cut.is_zero() {
            return confidence;
        }

        let trend = self.get_trend(asset, now_ms);
        match trend {
            TrendDirection::Flat => confidence,
            TrendDirection::Up if signal_is_up => {
                (confidence * (Decimal::ONE + self.config.confirm_boost)).min(Decimal::ONE)
            }
            TrendDirection::Down if !signal_is_up => {
                (confidence * (Decimal::ONE + self.config.confirm_boost)).min(Decimal::ONE)
            }
            // Trend opposes signal
            _ => {
                (confidence * (Decimal::ONE - self.config.oppose_cut)).max(Decimal::ZERO)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_trend_tracker_empty() {
        let tracker = TrendTracker::new(TrendConfig::default());
        assert_eq!(tracker.get_trend(CryptoAsset::Btc, 1000), TrendDirection::Flat);
    }

    #[test]
    fn test_trend_tracker_up() {
        let mut config = TrendConfig::default();
        config.window_ms = 60_000; // 1 minute
        let mut tracker = TrendTracker::new(config);

        // Prices going up
        tracker.record_price(CryptoAsset::Btc, 0, dec!(100000));
        tracker.record_price(CryptoAsset::Btc, 30_000, dec!(100050));
        tracker.record_price(CryptoAsset::Btc, 60_000, dec!(100100));

        assert_eq!(tracker.get_trend(CryptoAsset::Btc, 60_000), TrendDirection::Up);
    }

    #[test]
    fn test_trend_tracker_down() {
        let mut config = TrendConfig::default();
        config.window_ms = 60_000;
        let mut tracker = TrendTracker::new(config);

        tracker.record_price(CryptoAsset::Btc, 0, dec!(100000));
        tracker.record_price(CryptoAsset::Btc, 30_000, dec!(99950));
        tracker.record_price(CryptoAsset::Btc, 60_000, dec!(99900));

        assert_eq!(tracker.get_trend(CryptoAsset::Btc, 60_000), TrendDirection::Down);
    }

    #[test]
    fn test_trend_confidence_boost() {
        let config = TrendConfig {
            window_ms: 60_000,
            confirm_boost: dec!(0.20),
            oppose_cut: dec!(0.30),
            ..Default::default()
        };
        let mut tracker = TrendTracker::new(config);

        tracker.record_price(CryptoAsset::Btc, 0, dec!(100000));
        tracker.record_price(CryptoAsset::Btc, 60_000, dec!(100100));

        // Trend is up, signal is up → boost
        let adjusted = tracker.adjust_confidence(dec!(0.50), CryptoAsset::Btc, true, 60_000);
        assert_eq!(adjusted, dec!(0.60)); // 0.50 * 1.20

        // Trend is up, signal is down → cut
        let adjusted = tracker.adjust_confidence(dec!(0.50), CryptoAsset::Btc, false, 60_000);
        assert_eq!(adjusted, dec!(0.35)); // 0.50 * 0.70
    }

    #[test]
    fn test_trend_disabled_when_zero() {
        let config = TrendConfig {
            confirm_boost: Decimal::ZERO,
            oppose_cut: Decimal::ZERO,
            ..Default::default()
        };
        let tracker = TrendTracker::new(config);

        // Should pass through unchanged
        let adjusted = tracker.adjust_confidence(dec!(0.50), CryptoAsset::Btc, true, 60_000);
        assert_eq!(adjusted, dec!(0.50));
    }
}
