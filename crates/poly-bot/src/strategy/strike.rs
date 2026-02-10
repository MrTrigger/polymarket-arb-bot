//! Fetch oracle prices from Polymarket's event page.
//!
//! Polymarket's 15M up/down markets use Chainlink Data Streams for settlement.
//! The prices are embedded in the SSR-rendered `__NEXT_DATA__` JSON on the event page
//! as `openPrice` (strike) and `closePrice` (settlement) in the `crypto-prices` query.
//!
//! We fetch them directly from the page to get the exact same values Polymarket uses,
//! avoiding any timing mismatch with our own Chainlink RTDS feed.

use chrono::{DateTime, Utc};
use poly_common::types::CryptoAsset;
use rust_decimal::Decimal;
use std::str::FromStr;
use tracing::{info, warn};

/// Both prices from the Polymarket event page.
pub struct PagePrices {
    pub open_price: Option<Decimal>,
    pub close_price: Option<Decimal>,
}

/// Build the Polymarket event page URL for a given market window.
fn build_event_url(asset: CryptoAsset, window_start: DateTime<Utc>, window_duration: &str) -> String {
    let asset_slug = match asset {
        CryptoAsset::Btc => "btc",
        CryptoAsset::Eth => "eth",
        CryptoAsset::Sol => "sol",
        CryptoAsset::Xrp => "xrp",
    };
    let start_unix = window_start.timestamp();
    format!(
        "https://polymarket.com/event/{}-updown-{}-{}",
        asset_slug, window_duration, start_unix
    )
}

/// Fetch both openPrice and closePrice from Polymarket's event page.
async fn fetch_page_prices(
    http: &reqwest::Client,
    asset: CryptoAsset,
    window_start: DateTime<Utc>,
    window_duration: &str,
) -> Option<PagePrices> {
    let url = build_event_url(asset, window_start, window_duration);
    info!("Fetching oracle prices from {}", url);

    let response = match http.get(&url).send().await {
        Ok(r) => r,
        Err(e) => {
            warn!("Failed to fetch from {}: {}", url, e);
            return None;
        }
    };

    if !response.status().is_success() {
        warn!("Fetch returned status {} for {}", response.status(), url);
        return None;
    }

    let html = match response.text().await {
        Ok(t) => t,
        Err(e) => {
            warn!("Failed to read response body: {}", e);
            return None;
        }
    };

    parse_prices_from_html(&html, asset)
}

/// Fetch the strike price (openPrice) from Polymarket's event page.
///
/// Retries up to 4 times with 2s delay if openPrice is missing (SSR cache inconsistency,
/// especially for lower-traffic assets like XRP).
pub async fn fetch_polymarket_strike(
    http: &reqwest::Client,
    asset: CryptoAsset,
    window_start: DateTime<Utc>,
    window_duration: &str,
) -> Option<Decimal> {
    for attempt in 0..5 {
        if attempt > 0 {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
        if let Some(prices) = fetch_page_prices(http, asset, window_start, window_duration).await {
            if let Some(open) = prices.open_price {
                return Some(open);
            }
            info!("openPrice not populated for {:?} (attempt {})", asset, attempt + 1);
        }
    }
    warn!("openPrice not available after 5 attempts for {:?}", asset);
    None
}

/// Fetch the settlement price (closePrice) from Polymarket's event page.
///
/// Retries up to 3 times with 2s delay since closePrice may not be populated
/// immediately at window end.
pub async fn fetch_polymarket_close_price(
    http: &reqwest::Client,
    asset: CryptoAsset,
    window_start: DateTime<Utc>,
    window_duration: &str,
) -> Option<Decimal> {
    for attempt in 0..3 {
        if attempt > 0 {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
        if let Some(prices) = fetch_page_prices(http, asset, window_start, window_duration).await {
            if let Some(close) = prices.close_price {
                return Some(close);
            }
            info!("closePrice not yet populated for {:?} (attempt {})", asset, attempt + 1);
        }
    }
    warn!("closePrice not available after 3 attempts for {:?}", asset);
    None
}

/// Parse both openPrice and closePrice from the page's `__NEXT_DATA__` JSON.
fn parse_prices_from_html(html: &str, asset: CryptoAsset) -> Option<PagePrices> {
    // Find the __NEXT_DATA__ script tag — attributes may vary (e.g., crossorigin)
    let tag_start = match html.find("id=\"__NEXT_DATA__\"") {
        Some(pos) => pos,
        None => {
            warn!("No __NEXT_DATA__ script tag found for {:?} (html len={})", asset, html.len());
            return None;
        }
    };
    // Find the closing '>' of the opening tag, then content starts after it
    let start = match html[tag_start..].find('>') {
        Some(pos) => tag_start + pos + 1,
        None => {
            warn!("Malformed __NEXT_DATA__ tag for {:?}", asset);
            return None;
        }
    };
    let end = html[start..].find("</script>")? + start;
    let json_str = &html[start..end];

    let data: serde_json::Value = match serde_json::from_str(json_str) {
        Ok(v) => v,
        Err(e) => {
            warn!("Failed to parse __NEXT_DATA__ JSON for {:?}: {}", asset, e);
            return None;
        }
    };
    let queries = match data
        .get("props")
        .and_then(|p| p.get("pageProps"))
        .and_then(|p| p.get("dehydratedState"))
        .and_then(|p| p.get("queries"))
        .and_then(|q| q.as_array())
    {
        Some(q) => q,
        None => {
            warn!("Could not traverse __NEXT_DATA__ path for {:?}", asset);
            return None;
        }
    };

    for query in queries {
        let key = query.get("queryKey")?.as_array()?;
        if key.first()?.as_str()? == "crypto-prices" {
            let data_obj = query.get("state")?.get("data")?;

            let open_price = data_obj
                .get("openPrice")
                .and_then(|v| v.as_f64())
                .and_then(|f| Decimal::from_str(&format!("{}", f)).ok());

            let close_price = data_obj
                .get("closePrice")
                .and_then(|v| v.as_f64())
                .and_then(|f| Decimal::from_str(&format!("{}", f)).ok());

            info!("Parsed oracle prices for {:?}: open={:?} close={:?}", asset, open_price, close_price);

            return Some(PagePrices { open_price, close_price });
        }
    }

    warn!("No crypto-prices query found in __NEXT_DATA__ for {:?}", asset);
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_prices_open_only() {
        let html = r#"<script id="__NEXT_DATA__" type="application/json">{"props":{"pageProps":{"dehydratedState":{"queries":[{"queryKey":["crypto-prices","price","BTC","2026-02-10T17:00:00Z","fifteen","2026-02-10T17:15:00Z"],"state":{"data":{"openPrice":69483.31803071646,"closePrice":null}}}]}}}}</script>"#;
        let result = parse_prices_from_html(html, CryptoAsset::Btc).unwrap();
        let open = result.open_price.unwrap();
        assert!(open > Decimal::from(69483));
        assert!(open < Decimal::from(69484));
        assert!(result.close_price.is_none());
    }

    #[test]
    fn test_parse_prices_both() {
        let html = r#"<script id="__NEXT_DATA__" type="application/json">{"props":{"pageProps":{"dehydratedState":{"queries":[{"queryKey":["crypto-prices","price","BTC","2026-02-10T17:00:00Z","fifteen","2026-02-10T17:15:00Z"],"state":{"data":{"openPrice":69483.31803071646,"closePrice":69512.45}}}]}}}}</script>"#;
        let result = parse_prices_from_html(html, CryptoAsset::Btc).unwrap();
        assert!(result.open_price.is_some());
        let close = result.close_price.unwrap();
        assert!(close > Decimal::from(69512));
        assert!(close < Decimal::from(69513));
    }

    #[test]
    fn test_parse_prices_no_data() {
        let html = r#"<script id="__NEXT_DATA__" type="application/json">{"props":{"pageProps":{"dehydratedState":{"queries":[]}}}}</script>"#;
        let result = parse_prices_from_html(html, CryptoAsset::Btc);
        assert!(result.is_none());
    }

    #[test]
    fn test_parse_prices_crossorigin_attr() {
        // Real Polymarket pages include crossorigin="anonymous" in the script tag
        let html = r#"<script id="__NEXT_DATA__" type="application/json" crossorigin="anonymous">{"props":{"pageProps":{"dehydratedState":{"queries":[{"queryKey":["crypto-prices","price","BTC"],"state":{"data":{"openPrice":69882.28,"closePrice":null}}}]}}}}</script>"#;
        let result = parse_prices_from_html(html, CryptoAsset::Btc).unwrap();
        let open = result.open_price.unwrap();
        assert!(open > Decimal::from(69882));
        assert!(open < Decimal::from(69883));
    }

    #[test]
    fn test_build_event_url() {
        let ts = DateTime::parse_from_rfc3339("2026-02-10T17:00:00Z").unwrap().with_timezone(&Utc);
        let url = build_event_url(CryptoAsset::Btc, ts, "15m");
        assert!(url.contains("btc-updown-15m-"));
        assert!(url.contains("polymarket.com/event/"));
    }
}
