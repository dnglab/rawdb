//! Best-effort Kubernetes Event recorder.
//!
//! When the binary runs inside a cluster we want a handful of business
//! actions to surface as Kubernetes `Events` so an operator can grep for
//! them with `kubectl get events`:
//!
//! - `SetApproved` (Normal) — a reviewer promoted a pending upload to
//!   an approved set.
//! - `UploadCreated` (Normal) — an authenticated user finalized a new
//!   pending upload.
//! - `ScanError` (Warning) — the background scanner failed against S3.
//!
//! The recorder is **best-effort**: outside a cluster, or when the
//! `POD_NAME`/`POD_NAMESPACE` env vars are missing (local dev,
//! docker-compose), the recorder no-ops. Inside a cluster, publish
//! failures are logged but never propagated to the request path — a
//! broken kube API connection must not turn into a 500 for the user.
//!
//! Wiring:
//!
//! 1. `RawdbEvents::init()` is called once at startup and stored on
//!    [`crate::state::AppState`]. It tries [`kube::Client::try_default`]
//!    and reads `POD_NAME` / `POD_NAMESPACE` from the Downward API. On
//!    any failure it returns an `Inner::Disabled` recorder.
//! 2. Call sites use the public methods (`set_approved`, …) which
//!    spawn a detached task; the caller never awaits the API write.

use std::sync::Arc;

use k8s_openapi::api::core::v1::ObjectReference;
use kube::runtime::events::{Event, EventType, Recorder, Reporter};
use kube::Client;

/// Reporter name we identify ourselves as on every emitted event. Shows
/// up under `kubectl describe`'s `Source` column.
const REPORTER_CONTROLLER: &str = "rawdb";

#[derive(Clone)]
pub struct RawdbEvents {
    inner: Arc<Inner>,
}

enum Inner {
    /// Either we're not in a cluster, or initialization failed. All
    /// public methods are cheap no-ops.
    Disabled,
    /// Real recorder bound to the rawdb Pod that owns this process.
    Enabled {
        recorder: Recorder,
        object_ref: ObjectReference,
    },
}

impl RawdbEvents {
    /// Build a recorder. Returns `Disabled` (silently) when not running
    /// inside a Kubernetes pod. Logs an informational line either way so
    /// operators can tell from startup logs whether events are flowing.
    pub async fn init() -> Self {
        let (Ok(pod_name), Ok(pod_namespace)) = (
            std::env::var("POD_NAME"),
            std::env::var("POD_NAMESPACE"),
        ) else {
            tracing::info!(
                "k8s events disabled: POD_NAME/POD_NAMESPACE not set (not in cluster?)"
            );
            return Self {
                inner: Arc::new(Inner::Disabled),
            };
        };

        let client = match Client::try_default().await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "k8s events disabled: kube client init failed");
                return Self {
                    inner: Arc::new(Inner::Disabled),
                };
            }
        };

        let reporter = Reporter {
            controller: REPORTER_CONTROLLER.into(),
            instance: Some(pod_name.clone()),
        };
        let object_ref = ObjectReference {
            api_version: Some("v1".into()),
            kind: Some("Pod".into()),
            name: Some(pod_name.clone()),
            namespace: Some(pod_namespace.clone()),
            ..Default::default()
        };
        tracing::info!(
            pod_name = %pod_name,
            pod_namespace = %pod_namespace,
            "k8s events enabled"
        );
        Self {
            inner: Arc::new(Inner::Enabled {
                recorder: Recorder::new(client, reporter),
                object_ref,
            }),
        }
    }

    /// Emit `Normal SetApproved`. Caller is the reviewer's `sub`.
    pub fn set_approved(&self, maker: &str, model: &str, by_sub: &str) {
        self.publish(Event {
            type_: EventType::Normal,
            reason: "SetApproved".into(),
            note: Some(format!(
                "{by_sub} approved {maker}/{model}"
            )),
            action: "Approve".into(),
            secondary: None,
        });
    }

    /// Emit `Normal UploadCreated`. Caller is the uploader's `sub`.
    pub fn upload_created(&self, maker: &str, model: &str, by_sub: &str) {
        self.publish(Event {
            type_: EventType::Normal,
            reason: "UploadCreated".into(),
            note: Some(format!(
                "{by_sub} uploaded {maker}/{model}"
            )),
            action: "Upload".into(),
            secondary: None,
        });
    }

    /// Emit `Normal SyncTriggered` when the cross-pod sync-tick
    /// watcher observes one or more domain ticks change and runs the
    /// matching catch-up scan pass(es). Not fired for the periodic
    /// full scan — only for notified resyncs.
    pub fn sync_triggered(&self, domains: &str) {
        self.publish(Event {
            type_: EventType::Normal,
            reason: "SyncTriggered".into(),
            note: Some(format!("catch-up scan: {domains}")),
            action: "Sync".into(),
            secondary: None,
        });
    }

    /// Emit `Warning ScanError`. `stage` is a short label naming where
    /// in the scan it failed (e.g. `scan_one_set`, `list_prefixes`),
    /// `err` is the human-readable error.
    pub fn scan_error(&self, stage: &str, err: &str) {
        self.publish(Event {
            type_: EventType::Warning,
            reason: "ScanError".into(),
            note: Some(format!("{stage}: {err}")),
            action: "Scan".into(),
            secondary: None,
        });
    }

    fn publish(&self, event: Event) {
        let Inner::Enabled { recorder, object_ref } = self.inner.as_ref() else {
            return;
        };
        // Clone the bits we need into a detached task so the caller's
        // hot path never waits on the API server. Failures are logged
        // but otherwise ignored — event emission is observability, not
        // a transactional guarantee.
        let recorder = recorder.clone();
        let object_ref = object_ref.clone();
        let reason = event.reason.clone();
        tokio::spawn(async move {
            if let Err(e) = recorder.publish(&event, &object_ref).await {
                tracing::warn!(error = %e, reason = %reason, "publish k8s event failed");
            }
        });
    }
}
