use std::sync::Arc;
use std::time::Instant;

use opentelemetry::metrics::{Counter, Gauge, Histogram};
use opentelemetry_sdk::metrics::SdkMeterProvider;
use prometheus::Registry;
use tokio::sync::watch;

use crate::auth_github::GithubClient;
use crate::auth_oidc::OidcClient;
use crate::cache::db::Db;
use crate::cache::scanner::Scanner;
use crate::config::Config;
use crate::events::RawdbEvents;
use crate::ratelimit::DownloadRateLimiter;
use crate::s3::S3;
use crate::sync_tick::TickState;

/// Shared state passed to every handler.
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub db: Db,
    pub s3: S3,
    pub scanner: Arc<Scanner>,
    /// `None` when OIDC is not configured.
    pub oidc: Option<Arc<OidcClient>>,
    /// `None` when GitHub OAuth is not configured.
    pub github: Option<Arc<GithubClient>>,
    /// Becomes `true` once the first full S3 scan completes; gates readiness probe.
    pub ready: watch::Receiver<bool>,
    pub metrics: Arc<AppMetrics>,
    /// Per-instance, per-IP download rate limiter.
    pub downloads: Arc<DownloadRateLimiter>,
    /// Best-effort Kubernetes Event publisher. No-op outside a cluster.
    pub events: RawdbEvents,
    /// Per-domain "last-seen tick ETag" cells, shared between
    /// [`crate::sync_tick::bump`] (writer) and the watcher loop
    /// (reader). See [`crate::sync_tick`].
    pub sync_ticks: Arc<TickState>,
}

impl AppState {
    pub fn is_ready(&self) -> bool {
        *self.ready.borrow()
    }
}

pub struct AppMetrics {
  /// Prometheus registry the exporter writes into. Scraped by the
  /// `/metrics` handler. Held here (not just the global default registry)
  /// so the binary controls its lifecycle and can be torn down/replaced.
  pub registry: Registry,
  pub start_time: Instant,
  pub http_requests: Counter<u64>,
  pub http_requests_duration_s: Histogram<f64>,
  pub http_samples: Counter<u64>,
  pub errors: Counter<u64>,
  pub service_up: Gauge<u64>,
  pub scan_duration_s: Histogram<f64>,
}
impl AppMetrics {
  pub fn new() -> Self {
    // Build a Prometheus exporter that feeds a fresh registry, and install
    // an `SdkMeterProvider` over it as the *global* OTel provider. Without
    // this, `global::meter(...)` returns the no-op meter and every
    // recorded sample is silently dropped — i.e. `/metrics` ends up empty.
    let registry = Registry::new();
    let exporter = opentelemetry_prometheus::exporter()
      .with_registry(registry.clone())
      .build()
      .expect("build prometheus exporter");
    let provider = SdkMeterProvider::builder().with_reader(exporter).build();
    opentelemetry::global::set_meter_provider(provider);

    let meter = opentelemetry::global::meter("rawdb");

    Self {
      registry,
      start_time: Instant::now(),
      http_requests: meter.u64_counter("rawdb.core.http.requests").with_description("Count of requests").build(),
      http_requests_duration_s: meter
        .f64_histogram("rawdb.core.http.requests.duration")
        .with_boundaries(vec![0.001, 0.01, 0.05, 0.1, 0.2, 0.5, 1.0, 2.0, 5.0, 10.0, 30.0, 60.0])
        .with_description("Count of requests")
        .build(),
      http_samples: meter
        .u64_counter("rawdb.http.samples")
        .with_description("Number of interactions with samples")
        .build(),
      errors: meter
        .u64_counter("rawdb.core.internal_errors")
        .with_description("Count of internal errors")
        .build(),
      service_up: meter.u64_gauge("rawdb.service.up").with_description("Instance information").build(),

      scan_duration_s: meter
        .f64_histogram("rawdb.scan.duration")
        .with_unit("s")
        .with_boundaries(vec![0.01, 0.1, 0.5, 1.0, 2.0, 5.0, 10.0, 30.0, 60.0, 120.0, 300.0, 600.0, 1200.0])
        .with_description("Duration per scan")
        .build(),
    }
  }
}
