//! Dynamic ATR (Average True Range) calculation.
//!
//! Tracks price volatility in real-time to provide accurate ATR values
//! for position sizing. Unlike static estimates, this adapts to current
//! market conditions.

use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

use poly_common::types::CryptoAsset;

/// A single price observation with timestamp.
#[derive(Debug, Clone)]
#[allow(dead_code)]
struct PricePoint {
    price: Decimal,
    timestamp: Instant,
}

/// Tracks a rolling window of prices for one asset.
#[derive(Debug)]
struct AssetTracker {
    /// Recent price observations.
    prices: VecDeque<PricePoint>,
    /// Window duration for range calculation.
    window_duration: Duration,
    /// Number of windows to average for ATR.
    num_windows: usize,
    /// Completed range values for averaging.
    ranges: VecDeque<Decimal>,
    /// Current window high.
    current_high: Option<Decimal>,
    /// Current window low.
    current_low: Option<Decimal>,
    /// Start of current window.
    window_start: Option<Instant>,
    /// Minimum ATR (fallback for low volatility).
    min_atr: Decimal,
}

impl AssetTracker {
    fn new(window_duration: Duration, num_windows: usize, min_atr: Decimal) -> Self {
        Self {
            prices: VecDeque::with_capacity(1000),
            window_duration,
            num_windows,
            ranges: VecDeque::with_capacity(num_windows),
            current_high: None,
            current_low: None,
            window_start: None,
            min_atr,
        }
    }

    /// Record a new price observation.
    fn record_price(&mut self, price: Decimal) {
        let now = Instant::now();

        // Initialize window if needed
        if self.window_start.is_none() {
            self.window_start = Some(now);
            self.current_high = Some(price);
            self.current_low = Some(price);
        }

        // Update current window high/low
        if let Some(high) = self.current_high
            && price > high
        {
            self.current_high = Some(price);
        }
        if let Some(low) = self.current_low
            && price < low
        {
            self.current_low = Some(price);
        }

        // Check if window is complete
        if let Some(start) = self.window_start
            && now.duration_since(start) >= self.window_duration
        {
            // Complete the window
            if let (Some(high), Some(low)) = (self.current_high, self.current_low) {
                let range = high - low;
                self.ranges.push_back(range);

                // Keep only num_windows ranges
                while self.ranges.len() > self.num_windows {
                    self.ranges.pop_front();
                }
            }

            // Start new window
            self.window_start = Some(now);
            self.current_high = Some(price);
            self.current_low = Some(price);
        }

        // Store price point (keep last minute for debugging)
        self.prices.push_back(PricePoint { price, timestamp: now });
        let cutoff = now - Duration::from_secs(60);
        while let Some(front) = self.prices.front() {
            if front.timestamp < cutoff {
                self.prices.pop_front();
            } else {
                break;
            }
        }
    }

    /// Get the current ATR (average of completed ranges).
    fn get_atr(&self) -> Decimal {
        if self.ranges.is_empty() {
            // Not enough data yet, use minimum
            return self.min_atr;
        }

        let sum: Decimal = self.ranges.iter().copied().sum();
        let count = Decimal::from(self.ranges.len() as u32);
        let atr = sum / count;

        // Ensure minimum ATR
        atr.max(self.min_atr)
    }

    /// Get current window's range (incomplete, for debugging).
    fn current_range(&self) -> Option<Decimal> {
        match (self.current_high, self.current_low) {
            (Some(high), Some(low)) => Some(high - low),
            _ => None,
        }
    }

    /// Number of completed windows.
    fn completed_windows(&self) -> usize {
        self.ranges.len()
    }
}

/// Tracks ATR for all supported assets.
#[derive(Debug)]
pub struct AtrTracker {
    trackers: std::collections::HashMap<CryptoAsset, AssetTracker>,
    /// Window duration for range calculation (default 1 minute).
    #[allow(dead_code)]
    window_duration: Duration,
    /// Number of windows to average (default 5 = 5-minute ATR).
    #[allow(dead_code)]
    num_windows: usize,
}

impl Default for AtrTracker {
    fn default() -> Self {
        Self::new(Duration::from_secs(60), 5)
    }
}

impl AtrTracker {
    /// Create a new ATR tracker.
    ///
    /// # Arguments
    /// * `window_duration` - Duration of each range window (e.g., 1 minute)
    /// * `num_windows` - Number of windows to average for ATR (e.g., 5 for 5-minute ATR)
    pub fn new(window_duration: Duration, num_windows: usize) -> Self {
        let mut trackers = std::collections::HashMap::new();

        // Initialize trackers with minimum ATR values per asset
        // These minimums prevent over-trading in extremely low volatility
        trackers.insert(
            CryptoAsset::Btc,
            AssetTracker::new(window_duration, num_windows, dec!(20)), // Min $20 ATR for BTC
        );
        trackers.insert(
            CryptoAsset::Eth,
            AssetTracker::new(window_duration, num_windows, dec!(1)), // Min $1 ATR for ETH
        );
        trackers.insert(
            CryptoAsset::Sol,
            AssetTracker::new(window_duration, num_windows, dec!(0.20)), // Min $0.20 ATR for SOL
        );
        trackers.insert(
            CryptoAsset::Xrp,
            AssetTracker::new(window_duration, num_windows, dec!(0.002)), // Min $0.002 ATR for XRP
        );

        Self {
            trackers,
            window_duration,
            num_windows,
        }
    }

    /// Warm up the ATR tracker with historical prices.
    ///
    /// This should be called before trading starts to ensure accurate ATR
    /// from the first trade. Pass at least 5+ prices to get meaningful ATR.
    ///
    /// # Arguments
    /// * `asset` - The asset to warm up
    /// * `prices` - Historical prices in chronological order (oldest first)
    pub fn warmup(&mut self, asset: CryptoAsset, prices: &[Decimal]) {
        if prices.is_empty() {
            return;
        }

        if let Some(tracker) = self.trackers.get_mut(&asset) {
            // Calculate ranges from price windows
            let window_size = std::cmp::max(1, prices.len() / tracker.num_windows);

            for chunk in prices.chunks(window_size) {
                if chunk.len() >= 2 {
                    let high = chunk.iter().max().copied().unwrap_or(Decimal::ZERO);
                    let low = chunk.iter().min().copied().unwrap_or(Decimal::ZERO);
                    let range = high - low;

                    // Add to ranges
                    tracker.ranges.push_back(range);
                    while tracker.ranges.len() > tracker.num_windows {
                        tracker.ranges.pop_front();
                    }
                }
            }

            // Set current window from the last few prices
            if prices.len() >= 2 {
                let last_prices = if prices.len() > window_size {
                    &prices[prices.len() - window_size..]
                } else {
                    prices
                };
                tracker.current_high = last_prices.iter().max().copied();
                tracker.current_low = last_prices.iter().min().copied();
                tracker.window_start = Some(Instant::now());
            }
        }
    }

    /// Check if ATR is warmed up (has calculated at least one range).
    pub fn is_warmed_up_all(&self) -> bool {
        self.trackers.values().all(|t| !t.ranges.is_empty())
    }

    /// Record a price update for an asset.
    pub fn record_price(&mut self, asset: CryptoAsset, price: Decimal) {
        if let Some(tracker) = self.trackers.get_mut(&asset) {
            tracker.record_price(price);
        }
    }

    /// Get the current ATR for an asset.
    pub fn get_atr(&self, asset: CryptoAsset) -> Decimal {
        self.trackers
            .get(&asset)
            .map(|t| t.get_atr())
            .unwrap_or_else(|| asset.estimated_atr_15m()) // Fallback to static estimate
    }

    /// Check if we have enough data to provide accurate ATR.
    pub fn is_warmed_up(&self, asset: CryptoAsset) -> bool {
        self.trackers
            .get(&asset)
            .map(|t| t.completed_windows() >= 2) // Need at least 2 windows
            .unwrap_or(false)
    }

    /// Get status summary for logging.
    pub fn status_summary(&self) -> String {
        let mut parts = Vec::new();
        for (asset, tracker) in &self.trackers {
            let atr = tracker.get_atr();
            let windows = tracker.completed_windows();
            let current = tracker.current_range().unwrap_or(Decimal::ZERO);
            parts.push(format!(
                "{}:${:.2}({}w,cur=${:.2})",
                asset, atr, windows, current
            ));
        }
        parts.join(" ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_atr_tracker_basic() {
        let mut tracker = AtrTracker::new(Duration::from_millis(100), 3);

        // Record some prices
        tracker.record_price(CryptoAsset::Btc, dec!(95000));
        tracker.record_price(CryptoAsset::Btc, dec!(95100));
        tracker.record_price(CryptoAsset::Btc, dec!(94900));

        // Should return minimum ATR since no windows completed
        let atr = tracker.get_atr(CryptoAsset::Btc);
        assert_eq!(atr, dec!(20)); // Minimum for BTC
    }

    #[test]
    fn test_atr_tracker_after_windows() {
        let mut tracker = AtrTracker::new(Duration::from_millis(10), 2);

        // First window: range $100
        tracker.record_price(CryptoAsset::Btc, dec!(95000));
        tracker.record_price(CryptoAsset::Btc, dec!(95100));
        std::thread::sleep(Duration::from_millis(15));

        // Second window: range $200
        tracker.record_price(CryptoAsset::Btc, dec!(95000));
        tracker.record_price(CryptoAsset::Btc, dec!(95200));
        std::thread::sleep(Duration::from_millis(15));

        // Third price to trigger window completion
        tracker.record_price(CryptoAsset::Btc, dec!(95000));

        // Should have at least one completed window
        assert!(tracker.trackers.get(&CryptoAsset::Btc).unwrap().completed_windows() >= 1);
    }

    #[test]
    fn test_minimum_atr_enforced() {
        let tracker = AtrTracker::default();

        // Without any price data, should return minimum
        assert!(tracker.get_atr(CryptoAsset::Btc) >= dec!(20));
        assert!(tracker.get_atr(CryptoAsset::Eth) >= dec!(1));
        assert!(tracker.get_atr(CryptoAsset::Sol) >= dec!(0.20));
    }

    #[test]
    fn test_warmup_with_historical_prices() {
        let mut tracker = AtrTracker::default();

        // Warmup with 10 prices showing ~$100 range
        let prices: Vec<Decimal> = vec![
            dec!(95000), dec!(95050), dec!(95100), dec!(95000), dec!(95080),
            dec!(95020), dec!(95090), dec!(95030), dec!(95060), dec!(95010),
        ];

        tracker.warmup(CryptoAsset::Btc, &prices);

        // Should be warmed up with calculated ATR
        assert!(tracker.is_warmed_up(CryptoAsset::Btc));

        // ATR should reflect the price range (roughly $50-100)
        let atr = tracker.get_atr(CryptoAsset::Btc);
        assert!(atr >= dec!(20), "ATR should be at least minimum: {}", atr);
        assert!(atr <= dec!(150), "ATR should be reasonable: {}", atr);
    }

    #[test]
    fn test_warmup_empty_prices() {
        let mut tracker = AtrTracker::default();

        // Empty warmup should not crash
        tracker.warmup(CryptoAsset::Btc, &[]);

        // Should return minimum ATR
        assert_eq!(tracker.get_atr(CryptoAsset::Btc), dec!(20));
    }

    #[test]
    fn test_warmup_single_price() {
        let mut tracker = AtrTracker::default();

        // Single price warmup
        tracker.warmup(CryptoAsset::Btc, &[dec!(95000)]);

        // Should return minimum ATR (not enough data)
        assert_eq!(tracker.get_atr(CryptoAsset::Btc), dec!(20));
    }
}
