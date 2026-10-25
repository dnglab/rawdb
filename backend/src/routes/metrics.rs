use axum::body::Body;
use axum::extract::State;
use axum::http::{header::CONTENT_TYPE, StatusCode};
use axum::response::Response;
use opentelemetry::KeyValue;
use prometheus::{Encoder, TextEncoder};

use crate::state::AppState;

/// Records the Home page totals from this pod's cache. They describe the
/// whole catalog, not this pod, so every pod reports the same numbers and
/// dashboards use `max` across pods. Skipped until the first full scan:
/// a pod still filling its cache would report too little.
fn record_catalog(state: &AppState) {
    if !state.is_ready() {
        return;
    }
    let db = &state.db;
    let counts = [
        ("models", db.count_models()),
        ("special", db.count_special()),
        ("files", db.count_files()),
        ("pending", db.count_pending()),
    ];
    for (kind, count) in counts {
        match count {
            Ok(n) => state.metrics.catalog.record(n, &[KeyValue::new("kind", kind)]),
            Err(e) => tracing::warn!(kind, error = %format!("{e:#}"), "catalog metric"),
        }
    }
}

/// Prometheus exposition (text format `0.0.4`). Scraped by the operator's
/// Prometheus / a `ServiceMonitor`. Served on the dedicated metrics
/// listener (`RAWDB_METRICS_BIND`).
#[utoipa::path(
    get,
    path = "/metrics",
    tag = "observability",
    responses(
        (status = 200, description = "Prometheus text exposition", content_type = "text/plain"),
    ),
)]
pub async fn metrics(State(state): State<AppState>) -> std::result::Result<Response, StatusCode> {
    state.metrics.service_up.record(
        1,
        &[
            KeyValue::new("crate", crate::CRATE_NAME),
            KeyValue::new("version", crate::VERSION),
        ],
    );
    record_catalog(&state);

    // Scrape the exporter's own registry — the global `prometheus::gather()`
    // would be empty because nothing is registered there (the OTel exporter
    // writes into our `state.metrics.registry`).
    let metric_families = state.metrics.registry.gather();
    let mut buffer = Vec::new();
    let encoder = TextEncoder::new();

    let response = match encoder.encode(&metric_families, &mut buffer) {
        Ok(()) => Response::builder()
            .status(StatusCode::OK)
            .header(CONTENT_TYPE, encoder.format_type())
            .body(Body::from(buffer))
            .unwrap(),
        Err(err) => {
            tracing::error!("Failed to encode metrics: {}", err);
            Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(Body::from("error encoding metrics"))
                .unwrap()
        }
    };

    Ok(response)
}
