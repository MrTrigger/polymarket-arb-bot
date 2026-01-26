//! Pivot-based confidence factor for adaptive trading.
//!
//! Tracks support/resistance levels and provides a confidence modifier based on:
//! - Distance to nearest pivot
//! - Whether the pivot supports or opposes our trade direction
//! - Current market regime (ranging vs trending)
//!
//! ## Integration with Confidence System
//!
//! Pivot confidence is multiplied into the overall confidence with regime-based weighting:
//!
//! ```text
//! In RANGING market (pivots matter more):
//!   - Near support + betting UP = confidence BOOST (+20-40%)
//!   - Near resistance + betting DOWN = confidence BOOST (+20-40%)
//!   - Near support + betting DOWN = confidence PENALTY (-30-50%)
//!   - Near resistance + betting UP = confidence PENALTY (-30-50%)
//!   - Mid-range = no adjustment
//!
//! In TRENDING market (pivots matter less):
//!   - Same logic but with smaller adjustments (±10-20%)
//!   - Breakouts through pivots get confidence boost
//! ```

use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::collections::{HashMap, VecDeque};

use poly_common::types::CryptoAsset;

/// Market regime affects how much weight pivots get in confidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Regime {
    /// Price oscillating between S/R - pivots are very important.
    Ranging,
    /// Clear directional movement - pivots less important, breakouts possible.
    Trending,
    /// Not enough data to determine.
    #[default]
    Unknown,
}

impl Regime {
    /// Weight multiplier for pivot confidence in this regime.
    /// Higher = pivots matter more.
    pub fn pivot_weight(&self) -> Decimal {
        match self {
            Regime::Ranging => dec!(0.40),  // Pivots strongly influence confidence
            Regime::Trending => dec!(0.15), // Pivots have minor influence
            Regime::Unknown => dec!(0.25),  // Middle ground
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Regime::Ranging => "ranging",
            Regime::Trending => "trending",
            Regime::Unknown => "unknown",
        }
    }
}

impl std::fmt::Display for Regime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.name())
    }
}

/// Result of pivot analysis - provides confidence adjustment.
#[derive(Debug, Clone, Default)]
pub struct PivotConfidence {
    /// Current regime.
    pub regime: Regime,
    /// Resistance level (nearest swing high).
    pub resistance: Option<Decimal>,
    /// Support level (nearest swing low).
    pub support: Option<Decimal>,
    /// Position in range: 0.0 = at support, 1.0 = at resistance.
    pub range_position: Option<Decimal>,
    /// Raw pivot factor before regime weighting (-1.0 to +1.0).
    /// Positive = pivot supports our direction, Negative = pivot opposes.
    pub raw_factor: Decimal,
    /// Final confidence multiplier (0.5 to 1.5 typically).
    pub multiplier: Decimal,
}

impl PivotConfidence {
    /// Apply this pivot confidence to a base confidence value.
    pub fn apply(&self, base_confidence: Decimal) -> Decimal {
        (base_confidence * self.multiplier).min(Decimal::ONE).max(Decimal::ZERO)
    }
}

/// Tracks price history and detects pivots for confidence adjustment.
#[derive(Debug)]
pub struct PivotTracker {
    /// Price history per asset (1-minute OHLC bars).
    history: HashMap<CryptoAsset, VecDeque<Bar>>,
    /// Current ATR per asset.
    atr: HashMap<CryptoAsset, Decimal>,
    /// Detected support levels per asset.
    support_levels: HashMap<CryptoAsset, Vec<Decimal>>,
    /// Detected resistance levels per asset.
    resistance_levels: HashMap<CryptoAsset, Vec<Decimal>>,
    /// Configuration.
    config: PivotConfig,
}

#[derive(Debug, Clone)]
pub struct PivotConfig {
    /// How many bars to keep for analysis.
    pub lookback_bars: usize,
    /// Distance threshold (in ATR) to consider "near" a pivot.
    pub near_pivot_atr: Decimal,
    /// Maximum confidence boost when perfectly aligned with pivot.
    pub max_boost: Decimal,
    /// Maximum confidence penalty when fighting a pivot.
    pub max_penalty: Decimal,
    /// ATR ratio threshold: below this = ranging, above = trending.
    pub range_atr_threshold: Decimal,
}

impl Default for PivotConfig {
    fn default() -> Self {
        Self {
            lookback_bars: 60,           // 60 minutes of history
            near_pivot_atr: dec!(0.5),   // Within 0.5 ATR = "near" pivot
            max_boost: dec!(0.40),       // Up to +40% confidence boost
            max_penalty: dec!(0.50),     // Up to -50% confidence penalty
            range_atr_threshold: dec!(2.0), // Range < 2 ATR = ranging
        }
    }
}

/// Simple OHLC bar.
#[derive(Debug, Clone, Copy)]
struct Bar {
    high: Decimal,
    low: Decimal,
    close: Decimal,
    timestamp: i64,
}

impl Default for PivotTracker {
    fn default() -> Self {
        Self::new(PivotConfig::default())
    }
}

impl PivotTracker {
    pub fn new(config: PivotConfig) -> Self {
        Self {
            history: HashMap::new(),
            atr: HashMap::new(),
            support_levels: HashMap::new(),
            resistance_levels: HashMap::new(),
            config,
        }
    }

    /// Update ATR for an asset.
    pub fn set_atr(&mut self, asset: CryptoAsset, atr: Decimal) {
        self.atr.insert(asset, atr);
    }

    /// Get ATR for asset (falls back to estimate).
    fn get_atr(&self, asset: CryptoAsset) -> Decimal {
        self.atr.get(&asset).copied()
            .unwrap_or_else(|| asset.estimated_atr_15m())
            .max(dec!(0.01)) // Prevent division by zero
    }

    /// Record a price update.
    pub fn record_price(&mut self, asset: CryptoAsset, price: Decimal, timestamp: i64) {
        let history = self.history.entry(asset).or_insert_with(|| {
            VecDeque::with_capacity(self.config.lookback_bars + 10)
        });

        // Update existing bar or create new one (1-minute bars)
        let minute = timestamp / 60;
        if let Some(last) = history.back_mut() {
            if last.timestamp / 60 == minute {
                // Update existing bar
                last.high = last.high.max(price);
                last.low = last.low.min(price);
                last.close = price;
                return;
            }
        }

        // Add new bar
        history.push_back(Bar {
            high: price,
            low: price,
            close: price,
            timestamp,
        });

        // Trim old bars
        while history.len() > self.config.lookback_bars {
            history.pop_front();
        }

        // Recalculate pivots periodically (every 5 bars)
        if history.len() % 5 == 0 {
            self.update_pivots(asset);
        }
    }

    /// Detect swing highs and lows.
    fn update_pivots(&mut self, asset: CryptoAsset) {
        let history = match self.history.get(&asset) {
            Some(h) if h.len() >= 7 => h,
            _ => return,
        };

        let bars: Vec<_> = history.iter().collect();
        let mut supports = Vec::new();
        let mut resistances = Vec::new();

        // Simple pivot detection: compare each bar to its neighbors
        for i in 2..bars.len().saturating_sub(2) {
            let curr = bars[i];
            let is_swing_high = curr.high > bars[i-1].high
                && curr.high > bars[i-2].high
                && curr.high > bars[i+1].high
                && curr.high > bars[i+2].high;

            let is_swing_low = curr.low < bars[i-1].low
                && curr.low < bars[i-2].low
                && curr.low < bars[i+1].low
                && curr.low < bars[i+2].low;

            if is_swing_high {
                resistances.push(curr.high);
            }
            if is_swing_low {
                supports.push(curr.low);
            }
        }

        // Keep most recent/relevant levels (cluster nearby ones)
        let atr = self.get_atr(asset);
        self.support_levels.insert(asset, Self::cluster_levels(supports, atr));
        self.resistance_levels.insert(asset, Self::cluster_levels(resistances, atr));
    }

    /// Cluster nearby price levels into single levels.
    fn cluster_levels(mut levels: Vec<Decimal>, atr: Decimal) -> Vec<Decimal> {
        if levels.is_empty() {
            return levels;
        }

        levels.sort();
        let mut clustered = Vec::new();
        let threshold = atr * dec!(0.3); // Cluster levels within 0.3 ATR

        let mut cluster_sum = levels[0];
        let mut cluster_count = 1u32;

        for i in 1..levels.len() {
            if levels[i] - levels[i-1] < threshold {
                // Same cluster
                cluster_sum += levels[i];
                cluster_count += 1;
            } else {
                // New cluster - save previous
                clustered.push(cluster_sum / Decimal::from(cluster_count));
                cluster_sum = levels[i];
                cluster_count = 1;
            }
        }
        // Don't forget last cluster
        clustered.push(cluster_sum / Decimal::from(cluster_count));

        // Keep only most recent 5 levels
        if clustered.len() > 5 {
            clustered.split_off(clustered.len() - 5)
        } else {
            clustered
        }
    }

    /// Detect current regime (ranging vs trending).
    fn detect_regime(&self, asset: CryptoAsset) -> Regime {
        let history = match self.history.get(&asset) {
            Some(h) if h.len() >= 20 => h,
            _ => return Regime::Unknown,
        };

        let atr = self.get_atr(asset);

        // Calculate range over recent history
        let recent: Vec<_> = history.iter().rev().take(30).collect();
        let high = recent.iter().map(|b| b.high).max().unwrap_or(Decimal::ZERO);
        let low = recent.iter().map(|b| b.low).min().unwrap_or(Decimal::ZERO);
        let range = high - low;

        // Also check for directional bias
        let first_close = recent.last().map(|b| b.close).unwrap_or(Decimal::ZERO);
        let last_close = recent.first().map(|b| b.close).unwrap_or(Decimal::ZERO);
        let direction = (last_close - first_close).abs();

        // Ranging: small range relative to ATR, or direction < 50% of range
        let range_atr = range / atr;
        let directional_ratio = if range > Decimal::ZERO { direction / range } else { Decimal::ZERO };

        if range_atr < self.config.range_atr_threshold && directional_ratio < dec!(0.5) {
            Regime::Ranging
        } else if directional_ratio > dec!(0.6) {
            Regime::Trending
        } else {
            Regime::Unknown
        }
    }

    /// Calculate pivot confidence factor for a trade direction.
    ///
    /// # Arguments
    /// * `asset` - The asset being traded
    /// * `current_price` - Current spot price
    /// * `favor_up` - True if we're considering betting UP, false for DOWN
    ///
    /// # Returns
    /// PivotConfidence with multiplier to apply to base confidence
    pub fn calculate_confidence(
        &self,
        asset: CryptoAsset,
        current_price: Decimal,
        favor_up: bool,
    ) -> PivotConfidence {
        let regime = self.detect_regime(asset);
        let atr = self.get_atr(asset);
        let weight = regime.pivot_weight();

        // Find nearest support and resistance
        let support = self.support_levels.get(&asset)
            .and_then(|levels| levels.iter().filter(|&&l| l < current_price).max().copied());
        let resistance = self.resistance_levels.get(&asset)
            .and_then(|levels| levels.iter().filter(|&&l| l > current_price).min().copied());

        // Calculate range position (0 = at support, 1 = at resistance)
        let range_position = match (support, resistance) {
            (Some(s), Some(r)) if r > s => Some((current_price - s) / (r - s)),
            _ => None,
        };

        // Calculate raw factor based on pivot proximity and trade direction
        let raw_factor = self.calc_raw_factor(
            current_price, support, resistance, atr, favor_up, regime
        );

        // Convert raw factor to multiplier with regime weighting
        // raw_factor: -1.0 (bad) to +1.0 (good)
        // multiplier: (1 - max_penalty) to (1 + max_boost)
        let multiplier = if raw_factor >= Decimal::ZERO {
            // Positive factor = boost
            Decimal::ONE + (raw_factor * self.config.max_boost * weight / dec!(0.4))
        } else {
            // Negative factor = penalty
            Decimal::ONE + (raw_factor * self.config.max_penalty * weight / dec!(0.4))
        };

        PivotConfidence {
            regime,
            resistance,
            support,
            range_position,
            raw_factor,
            multiplier: multiplier.max(dec!(0.3)).min(dec!(1.5)),
        }
    }

    /// Calculate raw factor (-1.0 to +1.0) based on pivot alignment.
    fn calc_raw_factor(
        &self,
        price: Decimal,
        support: Option<Decimal>,
        resistance: Option<Decimal>,
        atr: Decimal,
        favor_up: bool,
        regime: Regime,
    ) -> Decimal {
        let near_threshold = self.config.near_pivot_atr * atr;

        // Distance to support and resistance (negative if past the level)
        let dist_to_support = support.map(|s| price - s);
        let dist_to_resistance = resistance.map(|r| r - price);

        // Check if near support
        if let Some(dist) = dist_to_support {
            if dist.abs() < near_threshold {
                let proximity = Decimal::ONE - (dist.abs() / near_threshold); // 0 to 1

                return if dist >= Decimal::ZERO {
                    // Price ABOVE support
                    if favor_up {
                        // Betting UP near support = GOOD (support should hold)
                        proximity * dec!(0.8)
                    } else {
                        // Betting DOWN near support = BAD (fighting support)
                        -proximity * dec!(0.9)
                    }
                } else {
                    // Price BELOW support (breakdown)
                    if regime == Regime::Trending {
                        if favor_up {
                            -proximity * dec!(0.5) // Betting UP after breakdown = risky
                        } else {
                            proximity * dec!(0.6) // Betting DOWN after breakdown = ok in trend
                        }
                    } else {
                        // In ranging, breakdown often fails (false breakout)
                        if favor_up {
                            proximity * dec!(0.4) // Might bounce back
                        } else {
                            -proximity * dec!(0.3) // Breakdown might fail
                        }
                    }
                };
            }
        }

        // Check if near resistance
        if let Some(dist) = dist_to_resistance {
            if dist.abs() < near_threshold {
                let proximity = Decimal::ONE - (dist.abs() / near_threshold);

                return if dist >= Decimal::ZERO {
                    // Price BELOW resistance
                    if favor_up {
                        // Betting UP near resistance = BAD (fighting resistance)
                        -proximity * dec!(0.9)
                    } else {
                        // Betting DOWN near resistance = GOOD (resistance should hold)
                        proximity * dec!(0.8)
                    }
                } else {
                    // Price ABOVE resistance (breakout)
                    if regime == Regime::Trending {
                        if favor_up {
                            proximity * dec!(0.6) // Betting UP after breakout = ok in trend
                        } else {
                            -proximity * dec!(0.5) // Betting DOWN after breakout = risky
                        }
                    } else {
                        // In ranging, breakout often fails
                        if favor_up {
                            -proximity * dec!(0.3) // Breakout might fail
                        } else {
                            proximity * dec!(0.4) // Might reject back down
                        }
                    }
                };
            }
        }

        // Not near any pivot - check range position
        if let Some(pos) = self.calc_range_position(price, support, resistance) {
            // Middle of range (0.3 to 0.7) = no edge from pivots
            if pos > dec!(0.3) && pos < dec!(0.7) {
                return Decimal::ZERO;
            }

            // Upper part of range (>0.7) - slight DOWN bias
            if pos >= dec!(0.7) {
                let factor = (pos - dec!(0.7)) / dec!(0.3); // 0 to 1
                return if favor_up { -factor * dec!(0.3) } else { factor * dec!(0.3) };
            }

            // Lower part of range (<0.3) - slight UP bias
            if pos <= dec!(0.3) {
                let factor = (dec!(0.3) - pos) / dec!(0.3); // 0 to 1
                return if favor_up { factor * dec!(0.3) } else { -factor * dec!(0.3) };
            }
        }

        Decimal::ZERO
    }

    fn calc_range_position(&self, price: Decimal, support: Option<Decimal>, resistance: Option<Decimal>) -> Option<Decimal> {
        match (support, resistance) {
            (Some(s), Some(r)) if r > s => Some(((price - s) / (r - s)).max(Decimal::ZERO).min(Decimal::ONE)),
            _ => None,
        }
    }

    /// Check if tracker has enough data.
    pub fn is_ready(&self, asset: CryptoAsset) -> bool {
        self.history.get(&asset).map(|h| h.len() >= 15).unwrap_or(false)
    }

    /// Status summary for logging.
    pub fn status(&self, asset: CryptoAsset, price: Decimal) -> String {
        let conf = self.calculate_confidence(asset, price, true);
        format!(
            "{}: {} S={} R={} pos={:.0}% mult={:.2}",
            asset,
            conf.regime,
            conf.support.map(|s| format!("{:.0}", s)).unwrap_or("-".into()),
            conf.resistance.map(|r| format!("{:.0}", r)).unwrap_or("-".into()),
            conf.range_position.unwrap_or(dec!(-1)) * dec!(100),
            conf.multiplier,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_regime_weight() {
        assert!(Regime::Ranging.pivot_weight() > Regime::Trending.pivot_weight());
        assert!(Regime::Unknown.pivot_weight() > Regime::Trending.pivot_weight());
        assert!(Regime::Unknown.pivot_weight() < Regime::Ranging.pivot_weight());
    }

    #[test]
    fn test_basic_pivot_tracking() {
        let mut tracker = PivotTracker::default();
        tracker.set_atr(CryptoAsset::Btc, dec!(100));

        // Simulate price moving in a range
        let prices = [
            100000, 100050, 100100, 100080, 100120, // Up
            100150, 100100, 100050, 100020, 100000, // Down to support
            100030, 100080, 100130, 100150, 100180, // Up to resistance
            100150, 100100, 100070, 100030, 100010, // Down
        ];

        for (i, &price) in prices.iter().enumerate() {
            tracker.record_price(
                CryptoAsset::Btc,
                Decimal::from(price),
                (i * 60) as i64,
            );
        }

        assert!(tracker.is_ready(CryptoAsset::Btc));
    }

    #[test]
    fn test_confidence_near_support_favor_up() {
        let mut tracker = PivotTracker::default();
        tracker.set_atr(CryptoAsset::Btc, dec!(100));

        // Manually set support/resistance for testing
        tracker.support_levels.insert(CryptoAsset::Btc, vec![dec!(99900)]);
        tracker.resistance_levels.insert(CryptoAsset::Btc, vec![dec!(100100)]);

        // Price near support, betting UP = should be good
        let conf = tracker.calculate_confidence(CryptoAsset::Btc, dec!(99920), true);
        println!("Near support, favor UP: {:?}", conf);
        assert!(conf.multiplier > Decimal::ONE, "Should boost confidence near support when betting UP");

        // Price near support, betting DOWN = should be bad
        let conf = tracker.calculate_confidence(CryptoAsset::Btc, dec!(99920), false);
        println!("Near support, favor DOWN: {:?}", conf);
        assert!(conf.multiplier < Decimal::ONE, "Should penalize confidence near support when betting DOWN");
    }

    #[test]
    fn test_confidence_near_resistance_favor_down() {
        let mut tracker = PivotTracker::default();
        tracker.set_atr(CryptoAsset::Btc, dec!(100));

        tracker.support_levels.insert(CryptoAsset::Btc, vec![dec!(99900)]);
        tracker.resistance_levels.insert(CryptoAsset::Btc, vec![dec!(100100)]);

        // Price near resistance, betting DOWN = should be good
        let conf = tracker.calculate_confidence(CryptoAsset::Btc, dec!(100080), false);
        println!("Near resistance, favor DOWN: {:?}", conf);
        assert!(conf.multiplier > Decimal::ONE, "Should boost confidence near resistance when betting DOWN");

        // Price near resistance, betting UP = should be bad
        let conf = tracker.calculate_confidence(CryptoAsset::Btc, dec!(100080), true);
        println!("Near resistance, favor UP: {:?}", conf);
        assert!(conf.multiplier < Decimal::ONE, "Should penalize confidence near resistance when betting UP");
    }

    #[test]
    fn test_confidence_mid_range() {
        let mut tracker = PivotTracker::default();
        tracker.set_atr(CryptoAsset::Btc, dec!(100));

        tracker.support_levels.insert(CryptoAsset::Btc, vec![dec!(99900)]);
        tracker.resistance_levels.insert(CryptoAsset::Btc, vec![dec!(100100)]);

        // Price in middle of range - should be neutral
        let conf = tracker.calculate_confidence(CryptoAsset::Btc, dec!(100000), true);
        println!("Mid range: {:?}", conf);
        // Multiplier should be close to 1.0
        assert!(conf.multiplier > dec!(0.9) && conf.multiplier < dec!(1.1),
            "Mid-range should have neutral multiplier, got {}", conf.multiplier);
    }

    #[test]
    fn test_cluster_levels() {
        let atr = dec!(100);
        let levels = vec![dec!(100), dec!(102), dec!(105), dec!(150), dec!(152)];
        let clustered = PivotTracker::cluster_levels(levels, atr);

        // Should cluster 100,102,105 together and 150,152 together
        assert!(clustered.len() <= 3, "Should cluster nearby levels");
    }
}
