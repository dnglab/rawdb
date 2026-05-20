use axum::extract::{Query, Request, State};
use axum::http::header::ACCEPT;
use axum::http::{Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use opentelemetry::KeyValue;
use serde::Deserialize;
use tower::ServiceExt;
use tower_http::compression::CompressionLayer;
use tower_http::services::ServeDir;
use tower_http::trace::TraceLayer;

use utoipa::OpenApi;
use utoipa_swagger_ui::SwaggerUi;

use crate::error::AppError;
use crate::openapi::ApiDoc;
use crate::state::AppState;

pub mod admin;
pub mod auth;
pub mod errors;
pub mod metrics;
pub mod public;
pub mod upload;

pub fn build_router(state: AppState) -> Router {
    let api = Router::new()
        .merge(public::router())
        .merge(upload::router())
        .nest("/admin", admin::router());

    // Interactive OpenAPI docs. Gated by `RAWDB_DOCS_ENABLED` so locked-down
    // deployments can suppress them entirely. `SwaggerUi::new("/docs")`
    // serves the bundled assets and the configured spec URL together.
    let docs = if state.config.docs_enabled {
        Some(SwaggerUi::new("/docs").url("/openapi.json", ApiDoc::openapi()))
    } else {
        None
    };

    let router = Router::new()
        .nest("/api", api)
        .nest("/auth", auth::router());
    let router = if let Some(docs) = docs {
        router.merge(docs)
    } else {
        router
    };
    router
        // Everything not matched above goes through `spa_fallback`, which
        // serves real static files, rewrites browser navigations to the SPA
        // index (so deep links / refresh work), and returns embedded HTML
        // error pages for non-API misses.
        .fallback(spa_fallback)
        // Per-request metrics layered before compression so the timing
        // covers the full handler+body work and the status reflects what
        // the client actually receives. Applied only to the main router —
        // the metrics listener's /metrics scrapes shouldn't pollute the
        // request counters.
        .layer(middleware::from_fn_with_state(state.clone(), track_http))
        .layer(CompressionLayer::new())
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

/// Records `http.requests`, `http.requests.duration` and (on 5xx)
/// `core.internal_errors` for every request that reaches the main router.
/// Attributes are intentionally low cardinality: HTTP method + the
/// numeric status code (≲60 distinct values).
async fn track_http(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let method = req.method().as_str().to_owned();
    let start = std::time::Instant::now();
    let resp = next.run(req).await;
    let status = resp.status();
    let elapsed = start.elapsed().as_secs_f64();
    let attrs = [
        KeyValue::new("method", method.clone()),
        KeyValue::new("status_code", status.as_u16() as i64),
    ];
    state.metrics.http_requests.add(1, &attrs);
    state
        .metrics
        .http_requests_duration_s
        .record(elapsed, &attrs);
    if status.is_server_error() {
        state.metrics.errors.add(
            1,
            &[
                KeyValue::new("source", "http"),
                KeyValue::new("method", method),
                KeyValue::new("status_code", status.as_u16() as i64),
            ],
        );
    }
    resp
}

/// Separate router for the public observability surface: Prometheus
/// metrics and the Kubernetes health/readiness probes. Mounted on its own
/// listener (`RAWDB_METRICS_BIND`) so it can be scraped without exposing
/// the main API/SPA port to the internet.
pub fn build_metrics_router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/live", get(live))
        .route("/ready", get(ready))
        .route("/metrics", get(metrics::metrics))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

/// Catch-all for requests that didn't match `/healthz`, `/api/*` or `/auth/*`.
///
/// Order of resolution:
/// 1. `/api` and `/auth` paths never produce HTML — preserve the JSON 404
///    contract via [`AppError::NotFound`].
/// 2. Try to serve a real file from the static dir.
/// 3. On a file miss: a browser navigation (`GET` + `Accept: text/html`) gets
///    the SPA `index.html` (Vue Router then renders the route or its own
///    NotFound view); anything else gets the embedded 404 page.
/// 4. Any other 4xx/5xx from the static layer is rendered as the matching
///    embedded error page when the client accepts HTML.
async fn spa_fallback(State(state): State<AppState>, req: Request) -> Response {
    let path = req.uri().path().to_owned();
    let method = req.method().clone();
    let accept_html = req
        .headers()
        .get(ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("text/html"));

    if path.starts_with("/api") || path.starts_with("/auth") {
        return AppError::NotFound.into_response();
    }

    let serve = ServeDir::new(&state.config.static_dir).append_index_html_on_directories(true);
    // ServeDir is infallible: IO errors surface as 4xx/5xx responses, not Err.
    let Ok(resp) = serve.oneshot(req).await;
    let status = resp.status();

    if status == StatusCode::NOT_FOUND {
        return if method == Method::GET && accept_html {
            serve_index(&state).await
        } else {
            errors::error_response(StatusCode::NOT_FOUND)
        };
    }

    if (status.is_client_error() || status.is_server_error()) && accept_html {
        return errors::error_response(status);
    }

    resp.into_response()
}

/// Serve the SPA shell so client-side routing can take over.
async fn serve_index(state: &AppState) -> Response {
    let index = state.config.static_dir.join("index.html");
    match tokio::fs::read(&index).await {
        Ok(bytes) => Html(bytes).into_response(),
        Err(e) => {
            tracing::error!(error = %e, path = %index.display(), "failed to read SPA index.html");
            errors::error_response(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub(crate) struct HealthQuery {
    /// `1` switches `/healthz` from liveness to readiness semantics.
    #[serde(default)]
    pub ready: u8,
}

/// Legacy combined health endpoint. Returns liveness unless
/// `?ready=1`, in which case it mirrors `/ready`. Kept for callers that
/// still hit the original path; new probes should use `/live` and `/ready`.
#[utoipa::path(
    get,
    path = "/healthz",
    tag = "observability",
    params(HealthQuery),
    responses(
        (status = 200, description = "Alive (or ready when ?ready=1)"),
        (status = 503, description = "Not ready (only when ?ready=1)"),
    ),
)]
pub async fn healthz(
    State(state): State<AppState>,
    Query(q): Query<HealthQuery>,
) -> impl IntoResponse {
    if q.ready == 1 {
        if state.is_ready() {
            (StatusCode::OK, Json(serde_json::json!({ "ready": true })))
        } else {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({ "ready": false })),
            )
        }
    } else {
        (StatusCode::OK, Json(serde_json::json!({ "alive": true })))
    }
}

/// Kubernetes liveness probe — always 200 while the process is up.
#[utoipa::path(
    get,
    path = "/live",
    tag = "observability",
    responses((status = 200, description = "Process alive")),
)]
pub async fn live() -> impl IntoResponse {
    (StatusCode::OK, Json(serde_json::json!({ "alive": true })))
}

/// Kubernetes readiness probe — 200 only once the first full S3 scan has
/// completed; 503 otherwise so the LB doesn't route traffic to a pod
/// whose cache is still empty.
#[utoipa::path(
    get,
    path = "/ready",
    tag = "observability",
    responses(
        (status = 200, description = "First full scan complete"),
        (status = 503, description = "Cache still warming"),
    ),
)]
pub async fn ready(State(state): State<AppState>) -> impl IntoResponse {
    if state.is_ready() {
        (StatusCode::OK, Json(serde_json::json!({ "ready": true })))
    } else {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "ready": false })),
        )
    }
}
