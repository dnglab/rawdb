use std::future::IntoFuture;
use std::sync::Arc;

use anyhow::Result;
use tokio::net::TcpListener;
use tokio::sync::watch;
use tracing_subscriber::EnvFilter;

mod auth;
mod auth_oidc;
mod cache;
mod config;
mod error;
mod meta;
mod openapi;
mod routes;
mod s3;
mod state;
mod users;

use cache::db::Db;
use cache::scanner::Scanner;
use config::Config;
use s3::S3;
use state::AppState;

use crate::state::AppMetrics;

const CRATE_NAME: &str = env!("CARGO_CRATE_NAME");
const VERSION: &str = env!("CARGO_PKG_VERSION");

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let config = Config::parse_and_validate()?;
    tracing::info!(bind = %config.bind, "starting rawdb");

    let db = Db::open(&config.cache_dir)?;
    let s3 = S3::from_config(&config).await?;
    let metrics = Arc::new(AppMetrics::new());
    let scanner = Scanner::new(db.clone(), s3.clone(), &config, metrics.clone());
    let oidc = auth_oidc::OidcClient::from_config(&config).await?;

    // Readiness flips to true after the first successful full scan.
    let (ready_tx, ready_rx) = watch::channel(false);
    scanner.clone().spawn(ready_tx);

    let state = AppState {
        config: Arc::new(config),
        db,
        s3,
        scanner: Arc::new(scanner),
        oidc: oidc.map(Arc::new),
        ready: ready_rx,
        metrics,
    };

    let app = routes::build_router(state.clone());
    let metrics_app = routes::build_metrics_router(state.clone());

    let listener = TcpListener::bind(state.config.bind).await?;
    let metrics_listener = TcpListener::bind(state.config.metrics_bind).await?;
    tracing::info!("listening on {} (api)", state.config.bind);
    tracing::info!("listening on {} (metrics)", state.config.metrics_bind);

    // One signal task drives a watch that both servers observe; this avoids
    // the Notify race where late .notified() awaiters would miss the wake.
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    tokio::spawn(async move {
        shutdown_signal().await;
        let _ = shutdown_tx.send(true);
    });
    let api_shutdown = wait_for_shutdown(shutdown_rx.clone());
    let metrics_shutdown = wait_for_shutdown(shutdown_rx);

    let api_fut = axum::serve(listener, app).with_graceful_shutdown(api_shutdown);
    let metrics_fut =
        axum::serve(metrics_listener, metrics_app).with_graceful_shutdown(metrics_shutdown);
    tokio::try_join!(api_fut.into_future(), metrics_fut.into_future())?;
    tracing::info!("shutdown complete");

    Ok(())
}

async fn wait_for_shutdown(mut rx: watch::Receiver<bool>) {
    while !*rx.borrow() {
        if rx.changed().await.is_err() {
            break;
        }
    }
}

/// Resolve when the process receives a termination signal. Honors both
/// `SIGTERM` (containers/k8s) and `SIGINT` (Ctrl-C), so the HTTP server
/// drains in-flight requests instead of waiting for SIGKILL.
async fn shutdown_signal() {
    #[cfg(unix)]
    let term = async {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sig = signal(SignalKind::terminate()).expect("install SIGTERM handler");
        sig.recv().await;
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();

    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("install ctrl-c handler");
    };

    tokio::select! {
        _ = ctrl_c => tracing::info!("received SIGINT, shutting down"),
        _ = term => tracing::info!("received SIGTERM, shutting down"),
    }
}
