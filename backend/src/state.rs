use std::sync::Arc;

use opentelemetry::metrics::{Counter, Gauge, Histogram};
use opentelemetry_sdk::metrics::SdkMeterProvider;
use prometheus::Registry;
use tokio::sync::watch;

use crate::auth_github::GithubClient;
use crate::auth_oidc::OidcClient;
use crate::cache::db::Db;
use crate::cache::responses::ResponseCache;
use crate::cache::scanner::Scanner;
use crate::config::Config;
use crate::events::RawdbEvents;
use crate::inspect::Inspector;
use crate::ratelimit::DownloadRateLimiter;
use crate::s3::S3;
use crate::sync_tick::TickState;

/// Shared state passed to every handler.
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub db: Db,
    /// Rendered bodies of feeds/stats/catalog lists, invalidated by `db` writes.
    pub responses: Arc<ResponseCache>,
    pub s3: S3,
    pub scanner: Arc<Scanner>,
    /// `None` when OIDC is not configured.
    #[cfg_attr(not(feature = "oidc"), allow(dead_code))]
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
    /// Reviewer inspection of pending files: previews and exiftool
    /// listings, with a disk cache.
    pub inspect: Arc<Inspector>,
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
  pub http_requests: Counter<u64>,
  pub http_requests_duration_s: Histogram<f64>,
  pub http_samples: Counter<u64>,
  /// Bytes handed out by downloads, labelled `delivery` (presigned/stream).
  /// Approximate for presigned: counts the object size at redirect time,
  /// assuming the client follows it and fetches the whole file.
  pub download_bytes: Counter<u64>,
  pub errors: Counter<u64>,
  pub service_up: Gauge<u64>,
  /// Catalog totals as shown on the Home page, labelled `kind`
  /// (models/special/files/pending). Every pod reports the same
  /// catalog-wide numbers, so dashboards aggregate with `max`, not `sum`.
  pub catalog: Gauge<u64>,
  pub scan_duration_s: Histogram<f64>,
  /// Response-cache lookups, labelled `cache` (endpoint) and `result` (hit/miss).
  pub response_cache: Counter<u64>,
  /// Reviewer inspection requests, labelled `processor` (preview/exiftool)
  /// and `result` (hit/miss/error).
  pub inspect_lookups: Counter<u64>,
  /// Time per processor run and per download (`processor="download"`).
  pub inspect_duration_s: Histogram<f64>,
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
      download_bytes: meter
        .u64_counter("rawdb.http.samples.download")
        .with_unit("By")
        .with_description("Bytes handed out by sample downloads (presigned: object size, assumes the client follows the redirect)")
        .build(),
      errors: meter
        .u64_counter("rawdb.core.internal_errors")
        .with_description("Count of internal errors")
        .build(),
      service_up: meter.u64_gauge("rawdb.service.up").with_description("Instance information").build(),
      catalog: meter
        .u64_gauge("rawdb.catalog.size")
        .with_description("Catalog totals by kind (models, special, files, pending); every pod reports the whole catalog, so aggregate across pods with max, not sum")
        .build(),

      scan_duration_s: meter
        .f64_histogram("rawdb.scan.duration")
        .with_unit("s")
        .with_boundaries(vec![0.01, 0.1, 0.5, 1.0, 2.0, 5.0, 10.0, 30.0, 60.0, 120.0, 300.0, 600.0, 1200.0])
        .with_description("Duration per scan")
        .build(),
      response_cache: meter
        .u64_counter("rawdb.response_cache.lookups")
        .with_description("Response cache lookups by cache and result")
        .build(),
      inspect_lookups: meter
        .u64_counter("rawdb.inspect.lookups")
        .with_description("Reviewer inspection requests by processor and result (hit: cached, miss: waited for a download, error: internal failure)")
        .build(),
      inspect_duration_s: meter
        .f64_histogram("rawdb.inspect.duration")
        .with_unit("s")
        .with_boundaries(vec![0.1, 0.25, 0.5, 1.0, 2.0, 5.0, 10.0, 30.0, 60.0, 120.0])
        .with_description("Time per inspection processor run, and per download (processor=download)")
        .build(),
    }
  }
}
