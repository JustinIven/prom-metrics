use std::{
    sync::{atomic::Ordering, Arc},
    time::{Duration, Instant},
};

use tokio::net::TcpListener;
use tracing::{error, info, warn};

use prom_metrics::{
    api::{self, AppState},
    config::Config,
    error::Error,
    metrics::{collect, Store},
    prometheus::PromClient,
};

fn main() -> std::process::ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(false)
        .init();

    // Single provider is compiled in; install it so rustls never has to guess.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            error!(error = %e, "failed to start runtime");
            return std::process::ExitCode::FAILURE;
        }
    };

    match runtime.block_on(run()) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            error!(error = %e, "fatal");
            std::process::ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), Error> {
    let cfg = Config::from_env()?;
    info!(
        prometheus_url = %cfg.prometheus_url,
        poll_interval_s = cfg.poll_interval.as_secs(),
        max_metrics_age_s = cfg.max_metrics_age.as_secs(),
        cpu_rate_window_s = cfg.cpu_rate_window.as_secs(),
        listen = %cfg.listen_addr,
        tls = cfg.tls.is_some(),
        "starting prom-metrics"
    );

    let client = PromClient::new(&cfg.prometheus_url, cfg.poll_interval)?;
    let store = Arc::new(Store::default());
    tokio::spawn(poll_loop(client, store.clone(), cfg.poll_interval, cfg.cpu_rate_window));

    let app = api::router(AppState {
        store,
        max_metrics_age: cfg.max_metrics_age,
    });

    if let Some((cert, key)) = cfg.tls {
        let tls = axum_server::tls_rustls::RustlsConfig::from_pem_file(&cert, &key)
            .await
            .map_err(|e| Error::Config(format!("failed to load TLS material: {e}")))?;
        axum_server::bind_rustls(cfg.listen_addr, tls)
            .serve(app.into_make_service())
            .await?;
    } else {
        warn!("serving plain HTTP; the Kubernetes aggregation layer requires TLS");
        let listener = TcpListener::bind(cfg.listen_addr).await?;
        axum::serve(listener, app).await?;
    }
    Ok(())
}

async fn poll_loop(client: PromClient, store: Arc<Store>, interval: Duration, window: Duration) {
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut ready = false;
    loop {
        ticker.tick().await;
        let started = Instant::now();
        match collect(&client, window).await {
            Ok(snapshot) => {
                let (pods, nodes) = (snapshot.pods.len(), snapshot.nodes.len());
                let micros = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
                store.stats.last_duration_micros.store(micros, Ordering::Relaxed);
                store.replace(snapshot);
                if ready {
                    tracing::debug!(pods, nodes, duration_ms = started.elapsed().as_millis(), "refreshed");
                } else {
                    ready = true;
                    info!(pods, nodes, "initial metrics snapshot ready");
                }
            }
            Err(e) => {
                store.stats.query_errors.fetch_add(1, Ordering::Relaxed);
                warn!(error = %e, "prometheus refresh failed; serving previous snapshot");
            }
        }
    }
}
