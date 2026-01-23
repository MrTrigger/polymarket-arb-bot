//! Shared functionality between trading modes.
//!
//! This module contains common code used by both paper and live trading modes
//! to ensure identical behavior except for the executor.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::broadcast;
use tracing::{debug, error, info, warn};

use poly_common::ClickHouseClient;
use poly_market::{DiscoveryConfig, MarketDiscovery};

use crate::data_source::live::{ActiveMarket, ActiveMarketsState, EventSender};
use crate::data_source::{MarketEvent, WindowOpenEvent};
use crate::observability::{
    create_shared_analyzer, create_shared_detector_with_capture, AnomalyConfig, CaptureConfig,
    CounterfactualConfig, InMemoryIdLookup, ObservabilityCapture, ProcessorConfig,
};
use crate::config::ObservabilityConfig;

/// Run market discovery in a background task.
///
/// Periodically queries the Gamma API for new 15-minute markets
/// and adds them to the active markets state for CLOB subscription.
/// Also sends WindowOpenEvent to the strategy for each discovered market.
pub async fn run_market_discovery(
    config: DiscoveryConfig,
    active_markets: ActiveMarketsState,
    event_sender: EventSender,
    mut shutdown: broadcast::Receiver<()>,
) {
    use chrono::Utc;

    info!(
        "Starting market discovery for {:?}",
        config.assets.iter().map(|a| a.as_str()).collect::<Vec<_>>()
    );

    let mut discovery = MarketDiscovery::new(config.clone());
    let poll_interval = config.poll_interval;

    loop {
        // Clean up expired markets from active_markets
        {
            let mut active = active_markets.write().await;
            let before = active.len();
            active.retain(|_event_id, market| {
                let now = chrono::Utc::now();
                now < market.window_end
            });
            let removed = before - active.len();
            if removed > 0 {
                info!("Removed {} expired markets from active tracking", removed);
            }
        }

        // Discover new markets
        match discovery.discover().await {
            Ok(markets) => {
                // Always log total active markets for monitoring
                let total_active = active_markets.read().await.len();

                if !markets.is_empty() {
                    info!("Discovered {} new markets (total active: {})", markets.len(), total_active);

                    // Only add markets with valid strike prices.
                    // Markets with strike=0 haven't started yet - discovery will
                    // fetch historical price when they become active on next poll.
                    let ready_markets: Vec<_> = markets
                        .iter()
                        .filter(|m| !m.strike_price.is_zero())
                        .collect();

                    let pending_markets: Vec<_> = markets
                        .iter()
                        .filter(|m| m.strike_price.is_zero())
                        .collect();

                    // Get total active markets count for context
                    let total_active = active_markets.read().await.len();
                    info!(
                        "{} new markets ready (with strike), {} pending (strike not yet available), {} total active",
                        ready_markets.len(),
                        pending_markets.len(),
                        total_active
                    );

                    // Log pending markets at debug level
                    for m in &pending_markets {
                        let secs_to_start = (m.window_start - chrono::Utc::now()).num_seconds();
                        debug!(
                            "  Pending: {} {} starts in {}s (window: {} - {})",
                            m.event_id, m.asset, secs_to_start, m.window_start, m.window_end
                        );
                    }

                    // Add discovered markets to the active markets state
                    let mut active = active_markets.write().await;
                    for market in ready_markets {
                        let active_market = ActiveMarket {
                            event_id: market.event_id.clone(),
                            yes_token_id: market.yes_token_id.clone(),
                            no_token_id: market.no_token_id.clone(),
                            asset: market.asset,
                            strike_price: market.strike_price,
                            window_end: market.window_end,
                        };

                        // Generate Polymarket URL for easy tracking
                        let url = format!(
                            "https://polymarket.com/event/{}",
                            market.event_id
                        );
                        info!(
                            "Adding market {} ({} strike={}) to active markets\n    URL: {}",
                            market.event_id, market.asset, market.strike_price, url
                        );
                        active.insert(market.event_id.clone(), active_market);

                        // Send WindowOpenEvent to strategy
                        let window_event = MarketEvent::WindowOpen(WindowOpenEvent {
                            event_id: market.event_id.clone(),
                            condition_id: market.condition_id.clone(),
                            asset: market.asset,
                            yes_token_id: market.yes_token_id.clone(),
                            no_token_id: market.no_token_id.clone(),
                            strike_price: market.strike_price,
                            window_start: market.window_start,
                            window_end: market.window_end,
                            timestamp: Utc::now(),
                        });

                        if let Err(e) = event_sender.send(window_event).await {
                            warn!("Failed to send WindowOpen event: {}", e);
                        }
                    }
                } else if total_active > 0 {
                    // No new markets, but we have active ones
                    debug!("No new markets discovered, {} active markets being tracked", total_active);
                } else {
                    // No markets at all - this is what we want to fix
                    debug!("No markets discovered or active, waiting for markets to start");
                }
            }
            Err(e) => {
                error!("Market discovery error: {}", e);
            }
        }

        // Wait for next poll or shutdown
        tokio::select! {
            _ = tokio::time::sleep(poll_interval) => {}
            _ = shutdown.recv() => {
                info!("Market discovery received shutdown signal");
                break;
            }
        }
    }
}

/// Set up observability components (shared between paper and live modes).
///
/// Returns the capture instance and a list of background task handles.
pub async fn setup_observability(
    obs_config: &ObservabilityConfig,
    clickhouse: Option<&ClickHouseClient>,
    shutdown_tx: &broadcast::Sender<()>,
) -> anyhow::Result<(Option<Arc<ObservabilityCapture>>, Vec<tokio::task::JoinHandle<()>>)> {
    let mut tasks = Vec::new();

    if !obs_config.capture_decisions {
        return Ok((None, tasks));
    }

    // Create capture channel using from_config
    let capture_config = CaptureConfig {
        enabled: true,
        channel_capacity: obs_config.channel_buffer_size,
        log_drops: true,
        drop_log_threshold: 100,
    };

    let (capture, receiver) = ObservabilityCapture::from_config(capture_config);
    let capture = Arc::new(capture);

    // Create ID lookup for enrichment
    let id_lookup = Arc::new(InMemoryIdLookup::new());

    // Create processor if we have both ClickHouse and a receiver
    if let (Some(client), Some(receiver)) = (clickhouse, receiver) {
        let processor_config = ProcessorConfig {
            batch_size: obs_config.batch_size,
            flush_interval: Duration::from_secs(obs_config.flush_interval_secs),
            max_buffer_size: obs_config.channel_buffer_size,
            process_counterfactuals: obs_config.capture_counterfactuals,
            process_anomalies: obs_config.detect_anomalies,
        };

        // Create processor and run it
        let processor = crate::observability::ObservabilityProcessor::new(
            processor_config,
            id_lookup.clone(),
        );

        let shutdown_rx = shutdown_tx.subscribe();
        let client_clone = client.clone();
        let handle = tokio::spawn(async move {
            processor.run(receiver, client_clone, shutdown_rx).await;
        });
        tasks.push(handle);
    }

    // Create counterfactual analyzer if enabled
    if obs_config.capture_counterfactuals {
        let cf_config = CounterfactualConfig::default();
        let analyzer = create_shared_analyzer(cf_config);

        // Spawn cleanup loop
        let analyzer_clone = analyzer.clone();
        let shutdown_rx = shutdown_tx.subscribe();
        let handle = tokio::spawn(async move {
            analyzer_clone
                .run_cleanup_loop(Duration::from_secs(60), shutdown_rx)
                .await;
        });
        tasks.push(handle);
    }

    // Create anomaly detector if enabled
    if obs_config.detect_anomalies {
        let _detector = create_shared_detector_with_capture(
            AnomalyConfig::from_observability_config(obs_config),
            capture.clone(),
        );
        // Detector is used by strategy loop for real-time detection
    }

    Ok((Some(capture), tasks))
}

/// Warm up ATR tracker with recent historical prices.
///
/// This ensures accurate ATR from the first trade instead of waiting for data to accumulate.
pub async fn warmup_atr<D, E>(
    strategy: &mut crate::strategy::StrategyLoop<D, E>,
    assets: &[poly_common::CryptoAsset],
) where
    D: crate::data_source::DataSource,
    E: crate::executor::Executor,
{
    info!("Warming up ATR tracker with recent prices...");
    let warmup_discovery = MarketDiscovery::with_assets(assets.to_vec());

    for asset in assets {
        match warmup_discovery.fetch_recent_prices(*asset, 10).await {
            Ok(prices) if !prices.is_empty() => {
                strategy.warmup_atr(*asset, &prices);
                info!(
                    "Warmed up ATR for {} with {} recent prices",
                    asset,
                    prices.len()
                );
            }
            Ok(_) => {
                warn!("No recent prices available for {} warmup, using default ATR", asset);
            }
            Err(e) => {
                warn!("Failed to fetch warmup prices for {}: {}, using default ATR", asset, e);
            }
        }
    }
    info!("ATR warmup complete");
}

/// Set up observability forwarding from strategy to capture.
///
/// Creates a channel that forwards TradeDecision events to the ObservabilityCapture.
pub fn setup_observability_forwarding<D, E>(
    strategy: crate::strategy::StrategyLoop<D, E>,
    capture: &Arc<ObservabilityCapture>,
) -> crate::strategy::StrategyLoop<D, E>
where
    D: crate::data_source::DataSource,
    E: crate::executor::Executor,
{
    let (tx, mut rx) = tokio::sync::mpsc::channel(1024);
    let strategy = strategy.with_observability(tx);

    // Spawn task to forward decisions to capture
    let cap_clone = capture.clone();
    tokio::spawn(async move {
        while let Some(decision) = rx.recv().await {
            // Convert TradeDecision to DecisionSnapshot and capture
            let snapshot = crate::observability::SnapshotBuilder::new()
                .event_id(&decision.event_id)
                .yes_token_id(&decision.opportunity.yes_token_id)
                .no_token_id(&decision.opportunity.no_token_id)
                .yes_ask(decision.opportunity.yes_ask)
                .no_ask(decision.opportunity.no_ask)
                .margin(decision.opportunity.margin)
                .size(decision.opportunity.max_size)
                .seconds_remaining(decision.opportunity.seconds_remaining)
                .confidence(decision.opportunity.confidence)
                .asset(decision.asset)
                .phase(decision.opportunity.phase)
                .action(match decision.action {
                    crate::strategy::TradeAction::Execute => {
                        crate::observability::ActionType::Execute
                    }
                    crate::strategy::TradeAction::SkipSizing => {
                        crate::observability::ActionType::SkipSizing
                    }
                    crate::strategy::TradeAction::SkipToxic => {
                        crate::observability::ActionType::SkipToxic
                    }
                    crate::strategy::TradeAction::SkipDisabled => {
                        crate::observability::ActionType::SkipDisabled
                    }
                    crate::strategy::TradeAction::SkipCircuitBreaker => {
                        crate::observability::ActionType::SkipCircuitBreaker
                    }
                })
                .latency_us(decision.latency_us)
                .build();

            cap_clone.try_capture(snapshot);
        }
    });

    strategy
}
