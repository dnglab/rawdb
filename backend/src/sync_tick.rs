//! Cross-pod resync notification via per-domain S3 "tick" objects.
//!
//! When a pod mutates S3 (admin approves a set, edits/deletes a set,
//! uploads land in pending/, admin user CRUD, …) it PUTs a fresh body
//! to one or more of:
//!
//! - `_system/sync/users.tick`
//! - `_system/sync/sets.tick`
//! - `_system/sync/pending.tick`
//!
//! Other pods HEAD each tick on a short interval and react to ETag
//! changes by running ONLY the matching scan pass. The catch-up runs in
//! the background; **readiness is never toggled** — a flurry of
//! concurrent admin actions must not cascade into a service-wide
//! not-ready window.
//!
//! See the matching design note in
//! `/home/cytrinox/.claude/plans/i-m-the-developer-of-effervescent-sphinx.md`.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use opentelemetry::KeyValue;
use serde::Serialize;
use tokio::sync::Mutex;

use crate::cache::scanner::Scanner;
use crate::events::RawdbEvents;
use crate::s3::S3;
use crate::state::AppState;

/// The three pieces of S3 state a pod might need to refresh, mapped 1:1
/// to the scanner's three passes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Domain {
    Users,
    Sets,
    Pending,
}

impl Domain {
    pub fn key(self) -> &'static str {
        match self {
            Domain::Users => "_system/sync/users.tick",
            Domain::Sets => "_system/sync/sets.tick",
            Domain::Pending => "_system/sync/pending.tick",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Domain::Users => "users",
            Domain::Sets => "sets",
            Domain::Pending => "pending",
        }
    }
}

/// Per-domain "last seen ETag" cell. Shared between the bump path
/// (writer) and the watcher loop (reader) so a pod can recognise its
/// own writes and skip the self-triggered scan. Peer writes still
/// trigger because their ETag won't match the value in the cell.
#[derive(Default)]
pub struct TickState {
    users: Mutex<Option<String>>,
    sets: Mutex<Option<String>>,
    pending: Mutex<Option<String>>,
}

impl TickState {
    pub fn new() -> Self {
        Self::default()
    }

    fn cell(&self, d: Domain) -> &Mutex<Option<String>> {
        match d {
            Domain::Users => &self.users,
            Domain::Sets => &self.sets,
            Domain::Pending => &self.pending,
        }
    }
}

#[derive(Debug, Serialize)]
struct TickBody<'a> {
    nonce: String,
    ts: String,
    reason: &'a str,
}

/// Bump the tick for each `domain`. Errors are deliberately swallowed:
/// the caller's user-facing operation must not fail for a tick-write
/// hiccup. They're surfaced via the `errors` metric and an ERROR log.
///
/// The ETag returned by each successful PUT is stored in the shared
/// `TickState`, so the watcher loop on this same pod recognises its own
/// write on its next HEAD and skips the self-triggered scan.
pub async fn bump(state: &AppState, domains: &[Domain], reason: &str) {
    for d in domains {
        let body = TickBody {
            nonce: uuid::Uuid::new_v4().to_string(),
            ts: chrono::Utc::now().to_rfc3339(),
            reason,
        };
        let bytes = match serde_json::to_vec(&body) {
            Ok(b) => b,
            Err(e) => {
                tracing::error!(error = %e, domain = d.label(), "sync tick body serialize");
                continue;
            }
        };
        match state
            .s3
            .put_bytes(d.key(), bytes, Some("application/json"), None, None)
            .await
        {
            Ok(new_etag) => {
                tracing::info!(domain = d.label(), reason, "sync tick bumped");
                let mut cell = state.sync_ticks.cell(*d).lock().await;
                *cell = Some(new_etag);
            }
            Err(e) => {
                state.metrics.errors.add(
                    1,
                    &[
                        KeyValue::new("source", "sync_tick"),
                        KeyValue::new("stage", "bump"),
                        KeyValue::new("domain", d.label()),
                    ],
                );
                tracing::error!(error = %e, domain = d.label(), "sync tick bump failed");
            }
        }
    }
}

/// Background loop: HEADs the three tick keys at `interval`, runs the
/// matching scanner pass when an ETag changes. Never toggles readiness.
/// On each triggered resync (not the periodic full scan) emits a K8s
/// `Normal SyncTriggered` event so cluster operators can correlate
/// catch-up scans with admin actions on peer pods.
pub fn spawn_watcher(
    s3: S3,
    scanner: Arc<Scanner>,
    sync_ticks: Arc<TickState>,
    events: RawdbEvents,
    interval: Duration,
) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // Skip the first immediate tick: the periodic scanner runs
        // run_once() at startup itself, so the very first poll has no
        // baseline to compare against.
        tick.tick().await;
        loop {
            tick.tick().await;
            let changed = match poll_once(&s3, &sync_ticks).await {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(error = %e, "sync tick poll failed");
                    continue;
                }
            };
            if changed.is_empty() {
                continue;
            }
            // Deterministic order so tests / log greps don't have to
            // accommodate HashSet iteration order.
            let mut labels: Vec<&str> = changed.iter().map(|d| d.label()).collect();
            labels.sort_unstable();
            let joined = labels.join(",");
            tracing::info!(domains = %joined, "sync tick observed change, running affected passes");
            events.sync_triggered(&joined);
            run_passes(&scanner, &changed).await;
        }
    });
}

/// HEAD all three keys concurrently, return the domains whose ETag
/// differs from the value in our shared cell.
async fn poll_once(s3: &S3, sync_ticks: &TickState) -> anyhow::Result<HashSet<Domain>> {
    // futures::join! would be nicer but we already depend on tokio.
    let (u, s, p) = tokio::join!(
        s3.head(Domain::Users.key()),
        s3.head(Domain::Sets.key()),
        s3.head(Domain::Pending.key()),
    );
    let mut out = HashSet::new();
    for (d, head) in [(Domain::Users, u), (Domain::Sets, s), (Domain::Pending, p)] {
        let head = head?;
        // Missing object = "nobody has bumped this domain yet". Quietly
        // skip; bumping it later will create the object and the next
        // poll will pick up the first ETag as the baseline.
        let Some(info) = head else { continue };
        let Some(etag) = info.etag else { continue };
        let mut cell = sync_ticks.cell(d).lock().await;
        match cell.as_deref() {
            Some(prev) if prev == etag => { /* unchanged */ }
            _ => {
                // First sighting or genuine change. Either way: refresh
                // the cell and tell the caller to run the matching pass.
                *cell = Some(etag);
                out.insert(d);
            }
        }
    }
    Ok(out)
}

/// Sequentially run the scanner passes mapped to the changed domains.
/// Errors in one pass don't abort the others.
async fn run_passes(scanner: &Scanner, domains: &HashSet<Domain>) {
    if domains.contains(&Domain::Pending) {
        if let Err(e) = scanner.run_pending_pass().await {
            tracing::error!(error = ?e, "sync-tick pending pass failed");
        }
    }
    if domains.contains(&Domain::Sets) {
        if let Err(e) = scanner.run_samples_pass().await {
            tracing::error!(error = ?e, "sync-tick samples pass failed");
        }
    }
    if domains.contains(&Domain::Users) {
        if let Err(e) = scanner.run_users_pass().await {
            tracing::error!(error = ?e, "sync-tick users pass failed");
        }
    }
}
