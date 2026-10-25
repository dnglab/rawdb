//! Three-pass reconciliation between S3 and the local SQLite cache:
//!
//! - **A**: approved sets under `samples/<maker>/<model>/`
//! - **B**: pending uploads under `pending/<upload_id>/` (maker/model from meta)
//! - **C**: users mirror at `_system/users.toml`
//!
//! Each pass uses an ETag fast-path: if the relevant control object's ETag
//! matches what's already in SQLite, the whole prefix is skipped. This is
//! the >10k-set scalability path.
//!
//! The scanner runs once on startup (flipping the readiness watcher to
//! `true` afterwards) then every `RAWDB_RESCAN_SECS` thereafter.
//!
//! **Consistency rule:** a set's (or pending upload's) cache row is only
//! replaced after *every* S3 read for it succeeded and every file listed
//! in its meta was found in S3. Anything less leaves the existing row
//! untouched and does not store the meta ETag, so the next scan retries.
//! A partial read must never shrink the catalogue: `/api/export` serves
//! this cache, and `rawdb_sync.py --delete` prunes whatever it omits.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::Utc;
use opentelemetry::KeyValue;
use tokio::sync::{watch, Semaphore};
use tokio::task::JoinSet;
use tokio::time::Instant;

use crate::cache::db::{Db, FileSizeInfo, UserRow};
use crate::config::Config;
use crate::events::RawdbEvents;
use crate::meta::{self, FileMeta, RawdbMeta};
use crate::s3::{ObjectInfo, S3};
use crate::state::AppMetrics;

pub const SAMPLES_PREFIX: &str = "samples/";
pub const PENDING_PREFIX: &str = "pending/";
pub const USERS_KEY: &str = "_system/users.toml";
pub const META_FILE: &str = "rawdb-meta.toml";

/// Retry delay while the pod has not yet completed a clean initial scan.
/// Short so a transient S3 hiccup at startup doesn't keep the pod
/// not-ready for a full `RAWDB_RESCAN_SECS`.
const INITIAL_SCAN_RETRY: Duration = Duration::from_secs(15);

#[derive(Clone)]
pub struct Scanner {
    db: Db,
    s3: S3,
    concurrency: usize,
    interval: Duration,
    metrics: Arc<AppMetrics>,
    events: RawdbEvents,
}

impl Scanner {
    pub fn new(
        db: Db,
        s3: S3,
        cfg: &Config,
        metrics: Arc<AppMetrics>,
        events: RawdbEvents,
    ) -> Self {
        Self {
            db,
            s3,
            concurrency: cfg.scan_concurrency.max(1),
            interval: Duration::from_secs(cfg.rescan_secs.max(1)),
            metrics,
            events,
        }
    }

    /// Spawn the scanner loop. Returns immediately. The pod only turns
    /// ready after a `run_once()` that completed with **zero** transient
    /// S3 failures — otherwise sets that failed to load would simply be
    /// missing from a fresh (empty) cache and `/api/export` would serve a
    /// partial catalogue. Until then the scan retries every
    /// [`INITIAL_SCAN_RETRY`]. Content problems (`incomplete` sets, e.g.
    /// unparseable meta) don't block readiness: they can't heal by
    /// waiting. Once ready, the flag never flips back; later failures
    /// keep existing rows untouched, so the cache stays complete.
    pub fn spawn(self, ready_tx: watch::Sender<bool>) {
        tokio::spawn(async move {
            loop {
                match self.run_once().await {
                    Ok(summary) => {
                        tracing::info!(
                            samples = summary.samples_seen,
                            samples_updated = summary.samples_updated,
                            samples_deleted = summary.samples_deleted,
                            pending = summary.pending_seen,
                            pending_updated = summary.pending_updated,
                            pending_deleted = summary.pending_deleted,
                            users_updated = summary.users_updated,
                            transient_failures = summary.transient_failures,
                            incomplete = summary.incomplete,
                            "scan complete",
                        );
                        if !*ready_tx.borrow() {
                            if summary.transient_failures == 0 {
                                let _ = ready_tx.send(true);
                            } else {
                                tracing::warn!(
                                    transient_failures = summary.transient_failures,
                                    "initial scan had S3 read failures; staying not-ready and retrying",
                                );
                            }
                        }
                    }
                    Err(e) => {
                        tracing::error!(error = %format!("{e:#}"), "scan failed");
                        self.record_error("run_once");
                        self.events.scan_error("run_once", &format!("{e:#}"));
                    }
                }
                let wait = if *ready_tx.borrow() {
                    self.interval
                } else {
                    INITIAL_SCAN_RETRY.min(self.interval)
                };
                tokio::time::sleep(wait).await;
            }
        });
    }

    /// Bump the shared internal-errors counter from a scanner code path.
    fn record_error(&self, stage: &'static str) {
        self.metrics.errors.add(
            1,
            &[
                KeyValue::new("source", "scanner"),
                KeyValue::new("stage", stage),
            ],
        );
    }

    /// Fold one per-set / per-upload task result into `res`. Transient
    /// S3 errors and content problems are logged, counted in the
    /// `errors` metric and emitted as `ScanError` events; neither ever
    /// touches the cache row (see the module-level consistency rule).
    fn tally(
        &self,
        res: &mut PassResult,
        stage: &'static str,
        joined: std::result::Result<Result<SetOutcome>, tokio::task::JoinError>,
    ) {
        match joined {
            Ok(Ok(SetOutcome::Updated)) => res.updated += 1,
            Ok(Ok(SetOutcome::Unchanged | SetOutcome::MetaMissing)) => {}
            Ok(Ok(SetOutcome::Incomplete(reason))) => {
                res.incomplete += 1;
                tracing::warn!(stage, %reason, "skipped incomplete entry; kept previous cache row");
                self.record_error("incomplete");
                self.events.scan_error(stage, &reason);
            }
            Ok(Err(e)) => {
                res.transient_failures += 1;
                tracing::warn!(stage, error = %format!("{e:#}"), "S3 read failed; kept previous cache row");
                self.record_error(stage);
                self.events.scan_error(stage, &format!("{e:#}"));
            }
            Err(e) => {
                res.transient_failures += 1;
                tracing::warn!(stage, error = %format!("{e:#}"), "scan task join failed");
                self.record_error(stage);
                self.events.scan_error(stage, &format!("{e:#}"));
            }
        }
    }

    pub async fn run_once(&self) -> Result<ScanSummary> {
        let started = Instant::now();
        let mut summary = ScanSummary::default();
        let sem = Arc::new(Semaphore::new(self.concurrency));

        // Pass A
        let a = self.scan_samples(sem.clone()).await?;
        summary.samples_seen = a.seen;
        summary.samples_updated = a.updated;
        summary.samples_deleted = a.deleted;
        summary.transient_failures += a.transient_failures;
        summary.incomplete += a.incomplete;

        // Pass B
        let b = self.scan_pending(sem.clone()).await?;
        summary.pending_seen = b.seen;
        summary.pending_updated = b.updated;
        summary.pending_deleted = b.deleted;
        summary.transient_failures += b.transient_failures;
        summary.incomplete += b.incomplete;

        // Pass C
        summary.users_updated = self.scan_users().await? as u64;

        self.db.set_last_full_scan_at(Utc::now())?;
        self.metrics
            .scan_duration_s
            .record(started.elapsed().as_secs_f64(), &[]);
        Ok(summary)
    }

    // ---- Individual-pass entry points ------------------------------------
    //
    // Used by the [`crate::sync_tick`] watcher to run *only* the
    // domain(s) whose tick changed. Each method is a thin wrapper that
    // constructs the per-call `Semaphore` so the watcher doesn't have to
    // know about scanner internals. Errors are propagated to the
    // watcher, which logs them; metrics are still recorded the same way
    // via `record_error()` should the caller want to.

    /// Run only the approved-samples pass.
    pub async fn run_samples_pass(&self) -> Result<()> {
        let sem = Arc::new(Semaphore::new(self.concurrency));
        self.scan_samples(sem).await?;
        Ok(())
    }

    /// Run only the pending-uploads pass.
    pub async fn run_pending_pass(&self) -> Result<()> {
        let sem = Arc::new(Semaphore::new(self.concurrency));
        self.scan_pending(sem).await?;
        Ok(())
    }

    /// Run only the users-file pass.
    pub async fn run_users_pass(&self) -> Result<()> {
        let _ = self.scan_users().await?;
        Ok(())
    }

    // ---- Pass A: approved sets --------------------------------------------

    async fn scan_samples(&self, sem: Arc<Semaphore>) -> Result<PassResult> {
        let mut res = PassResult::default();

        let maker_prefixes = self.s3.list_common_prefixes(SAMPLES_PREFIX).await?;
        let mut model_prefixes = Vec::new();
        for mp in &maker_prefixes {
            let sub = self.s3.list_common_prefixes(mp).await?;
            model_prefixes.extend(sub);
        }
        res.seen = model_prefixes.len() as u64;

        // Snapshot existing keys + ETags for the fast path and the deletion sweep.
        let live_in_s3: HashSet<(String, String)> = model_prefixes
            .iter()
            .filter_map(|p| split_set_prefix(p))
            .collect();
        let prior_keys: HashSet<(String, String)> = self.db.list_set_keys()?.into_iter().collect();

        let mut tasks: JoinSet<Result<SetOutcome>> = JoinSet::new();
        for prefix in model_prefixes {
            let Some((maker, model)) = split_set_prefix(&prefix) else {
                continue;
            };
            let db = self.db.clone();
            let s3 = self.s3.clone();
            let sem = sem.clone();
            tasks.spawn(async move {
                let _permit = sem.acquire_owned().await?;
                scan_one_set(&db, &s3, &prefix, &maker, &model).await
            });
        }
        while let Some(j) = tasks.join_next().await {
            self.tally(&mut res, "scan_one_set", j);
        }

        // Delete sets present in DB but absent from S3. Safe against read
        // errors: `live_in_s3` comes from a listing that paginated to the
        // end without error (any failed page aborted this pass above), and
        // a set whose per-set read failed is still in `live_in_s3`.
        for key in prior_keys.difference(&live_in_s3) {
            if let Err(e) = self.db.delete_set(&key.0, &key.1) {
                tracing::warn!(maker = %key.0, model = %key.1, error = %format!("{e:#}"), "delete_set failed");
                self.record_error("delete_set");
            } else {
                res.deleted += 1;
            }
        }
        Ok(res)
    }

    // ---- Pass B: pending uploads ------------------------------------------

    async fn scan_pending(&self, sem: Arc<Semaphore>) -> Result<PassResult> {
        let mut res = PassResult::default();

        // Pending uploads live one level deep now: `pending/<upload_id>/`.
        // maker/model are read from each upload's meta, not the path.
        let upload_prefixes = self.s3.list_common_prefixes(PENDING_PREFIX).await?;
        res.seen = upload_prefixes.len() as u64;

        let live: HashSet<String> = upload_prefixes
            .iter()
            .filter_map(|p| split_pending_prefix(p))
            .collect();
        let prior: HashSet<String> = self.db.list_pending_upload_ids()?.into_iter().collect();

        let mut tasks: JoinSet<Result<SetOutcome>> = JoinSet::new();
        for prefix in upload_prefixes {
            let Some(upload_id) = split_pending_prefix(&prefix) else {
                continue;
            };
            let db = self.db.clone();
            let s3 = self.s3.clone();
            let sem = sem.clone();
            tasks.spawn(async move {
                let _permit = sem.acquire_owned().await?;
                scan_one_pending(&db, &s3, &prefix, &upload_id).await
            });
        }
        while let Some(j) = tasks.join_next().await {
            self.tally(&mut res, "scan_one_pending", j);
        }

        for upload_id in prior.difference(&live) {
            if let Err(e) = self.db.delete_pending_by_upload_id(upload_id) {
                tracing::warn!(error = %format!("{e:#}"), "delete_pending failed");
                self.record_error("delete_pending");
            } else {
                res.deleted += 1;
            }
        }
        Ok(res)
    }

    // ---- Pass C: users -----------------------------------------------------

    /// Returns 1 if the users table was rewritten, 0 if unchanged.
    async fn scan_users(&self) -> Result<u32> {
        let head = self.s3.head(USERS_KEY).await?;
        let Some(head) = head else {
            // File absent — empty user table, etag null.
            if self.db.get_users_etag()?.is_some() {
                self.db.replace_users(None, &[])?;
                return Ok(1);
            }
            return Ok(0);
        };
        if head.etag == self.db.get_users_etag()? {
            return Ok(0);
        }
        let (bytes, etag) = self.s3.get_bytes(USERS_KEY).await?;
        let users = parse_users_toml(&bytes).context("parse users.toml")?;
        self.db.replace_users(etag.as_deref(), &users)?;
        Ok(1)
    }

    /// Refresh a single approved-set prefix immediately (used after
    /// approve/edit/delete so the originating pod's UI reflects the
    /// change without waiting for the next tick).
    ///
    /// Drops the local row **only** when the set's meta is authoritatively
    /// gone from S3. An unchanged ETag, an incomplete set or a read error
    /// never deletes.
    pub async fn refresh_one_set(&self, maker: &str, model: &str) -> Result<()> {
        let prefix = format!("{SAMPLES_PREFIX}{maker}/{model}/");
        match scan_one_set(&self.db, &self.s3, &prefix, maker, model).await? {
            SetOutcome::MetaMissing => self.db.delete_set(maker, model)?,
            SetOutcome::Incomplete(reason) => {
                tracing::warn!(maker, model, %reason, "refresh: incomplete set; kept previous cache row");
                self.record_error("incomplete");
                self.events.scan_error("refresh_one_set", &reason);
            }
            SetOutcome::Updated | SetOutcome::Unchanged => {}
        }
        Ok(())
    }

    /// Like [`Self::refresh_one_set`] for a pending upload.
    pub async fn refresh_one_pending(&self, upload_id: &str) -> Result<()> {
        let prefix = format!("{PENDING_PREFIX}{upload_id}/");
        match scan_one_pending(&self.db, &self.s3, &prefix, upload_id).await? {
            SetOutcome::MetaMissing => self.db.delete_pending_by_upload_id(upload_id)?,
            SetOutcome::Incomplete(reason) => {
                tracing::warn!(upload_id, %reason, "refresh: incomplete pending upload; kept previous cache row");
                self.record_error("incomplete");
                self.events.scan_error("refresh_one_pending", &reason);
            }
            SetOutcome::Updated | SetOutcome::Unchanged => {}
        }
        Ok(())
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct ScanSummary {
    pub samples_seen: u64,
    pub samples_updated: u64,
    pub samples_deleted: u64,
    pub pending_seen: u64,
    pub pending_updated: u64,
    pub pending_deleted: u64,
    pub users_updated: u64,
    /// Per-entry S3 read errors (HTTP/timeout/listing). Blocks the
    /// initial readiness flip.
    pub transient_failures: u64,
    /// Entries skipped for content problems (bad meta, meta listing a
    /// file not present in S3). Does not block readiness.
    pub incomplete: u64,
}

#[derive(Default)]
struct PassResult {
    seen: u64,
    updated: u64,
    deleted: u64,
    transient_failures: u64,
    incomplete: u64,
}

/// Result of reconciling one set / pending upload. S3 read errors are
/// returned as `Err` instead. Only `Updated` writes to the cache; only
/// `MetaMissing` may justify deleting a row (and only in the targeted
/// `refresh_one_*` paths — the full passes delete based on the listing).
#[derive(Debug, PartialEq, Eq)]
enum SetOutcome {
    /// Every read succeeded and every declared file was present; row upserted.
    Updated,
    /// Meta ETag matches the cached one; nothing to do.
    Unchanged,
    /// HEAD on the meta returned an authoritative NotFound.
    MetaMissing,
    /// Content problem (unparseable meta, or meta lists files absent
    /// from S3). Row and stored ETag left untouched so the next scan
    /// retries.
    Incomplete(String),
}

// ---------------------------------------------------------------------------
// Per-set / per-pending workers
// ---------------------------------------------------------------------------

async fn scan_one_set(
    db: &Db,
    s3: &S3,
    prefix: &str,
    maker: &str,
    model: &str,
) -> Result<SetOutcome> {
    let meta_key = format!("{prefix}{META_FILE}");
    let Some(head) = s3.head(&meta_key).await? else {
        return Ok(SetOutcome::MetaMissing);
    };
    let prior = db.get_set_meta_etag(maker, model)?;
    if prior.as_deref() == head.etag.as_deref() {
        return Ok(SetOutcome::Unchanged);
    }
    let (bytes, etag) = s3.get_bytes(&meta_key).await?;
    let mut meta = match parse_meta(&bytes) {
        Ok(m) => m,
        Err(reason) => {
            return Ok(SetOutcome::Incomplete(format!("{maker}/{model}: {reason}")));
        }
    };
    // `uploaded_at` is optional in the meta; fall back to the meta object's
    // S3 LastModified so the UI always shows a date.
    if meta.set.uploaded_at.is_none() {
        meta.set.uploaded_at = head.last_modified;
    }
    let objects = s3.list_objects(prefix).await?;
    let files_with_sizes = match pair_files(&meta, prefix, objects) {
        Ok(f) => f,
        Err(missing) => {
            return Ok(SetOutcome::Incomplete(format!(
                "{maker}/{model}: meta lists files not present in S3: {}",
                missing.join(", ")
            )));
        }
    };
    db.upsert_set(&meta, etag.as_deref(), &files_with_sizes)?;
    Ok(SetOutcome::Updated)
}

async fn scan_one_pending(
    db: &Db,
    s3: &S3,
    prefix: &str,
    upload_id: &str,
) -> Result<SetOutcome> {
    let meta_key = format!("{prefix}{META_FILE}");
    let Some(head) = s3.head(&meta_key).await? else {
        return Ok(SetOutcome::MetaMissing);
    };
    let prior = db.get_pending_meta_etag(upload_id)?;
    if prior.as_deref() == head.etag.as_deref() {
        return Ok(SetOutcome::Unchanged);
    }
    let (bytes, etag) = s3.get_bytes(&meta_key).await?;
    let mut meta = match parse_meta(&bytes) {
        Ok(m) => m,
        Err(reason) => {
            return Ok(SetOutcome::Incomplete(format!("pending {upload_id}: {reason}")));
        }
    };
    // `uploaded_at` is optional; fall back to the meta object's S3
    // LastModified so the pending overview shows the upload time.
    if meta.set.uploaded_at.is_none() {
        meta.set.uploaded_at = head.last_modified;
    }
    let objects = s3.list_objects(prefix).await?;
    let files_with_sizes = match pair_files(&meta, prefix, objects) {
        Ok(f) => f,
        Err(missing) => {
            return Ok(SetOutcome::Incomplete(format!(
                "pending {upload_id}: meta lists files not present in S3: {}",
                missing.join(", ")
            )));
        }
    };
    db.upsert_pending(upload_id, &meta, etag.as_deref(), &files_with_sizes)?;
    Ok(SetOutcome::Updated)
}

/// Decode + parse a `rawdb-meta.toml` body. The error is a human-readable
/// reason for an `Incomplete` outcome (content problem, not an S3 error).
fn parse_meta(bytes: &[u8]) -> std::result::Result<RawdbMeta, String> {
    let s = std::str::from_utf8(bytes).map_err(|e| format!("meta is not UTF-8: {e}"))?;
    meta::parse(s).map_err(|e| format!("invalid rawdb-meta.toml: {e}"))
}

/// Pair each meta `[[files]]` entry with the size + ETag observed in the
/// (complete) S3 listing of `prefix`. Strict: if any declared file is
/// absent from the listing, returns `Err` with *all* missing paths and no
/// partial list — a shortened file list must never reach the cache.
/// Objects in S3 that the meta doesn't mention are ignored.
fn pair_files(
    meta: &RawdbMeta,
    prefix: &str,
    objects: Vec<ObjectInfo>,
) -> std::result::Result<Vec<(FileMeta, FileSizeInfo)>, Vec<String>> {
    let mut by_rel: HashMap<String, (u64, Option<String>)> = HashMap::new();
    for obj in objects {
        let Some(rel) = obj.key.strip_prefix(prefix) else {
            continue;
        };
        if rel == META_FILE {
            continue;
        }
        by_rel.insert(rel.to_string(), (obj.size, obj.etag));
    }
    let mut out = Vec::with_capacity(meta.files.len());
    let mut missing = Vec::new();
    for f in &meta.files {
        match by_rel.get(&f.path) {
            Some((size, etag)) => out.push((
                f.clone(),
                FileSizeInfo {
                    size: *size,
                    etag: etag.clone(),
                },
            )),
            None => missing.push(f.path.clone()),
        }
    }
    if missing.is_empty() {
        Ok(out)
    } else {
        Err(missing)
    }
}

// ---------------------------------------------------------------------------
// Users TOML parsing
// ---------------------------------------------------------------------------

#[derive(Debug, serde::Deserialize)]
struct UsersFile {
    #[serde(default)]
    users: Vec<UserEntry>,
}

#[derive(Debug, serde::Deserialize)]
struct UserEntry {
    sub: String,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    blocked: bool,
    #[serde(default)]
    added_at: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(default)]
    added_by: Option<String>,
    #[serde(default)]
    roles: Vec<String>,
    #[serde(default)]
    api_key_hash: Option<String>,
}

fn parse_users_toml(bytes: &[u8]) -> Result<Vec<UserRow>> {
    let s = std::str::from_utf8(bytes).context("users.toml utf-8")?;
    let file: UsersFile = toml::from_str(s).context("users.toml parse")?;
    Ok(file
        .users
        .into_iter()
        .map(|e| UserRow {
            sub: e.sub,
            display_name: e.display_name,
            blocked: e.blocked,
            added_at: e.added_at,
            added_by: e.added_by,
            roles: e.roles,
            api_key_hash: e.api_key_hash,
        })
        .collect())
}

// ---------------------------------------------------------------------------
// Prefix parsing helpers
// ---------------------------------------------------------------------------

/// "samples/Canon/EOS R5/" -> Some(("Canon", "EOS R5"))
fn split_set_prefix(prefix: &str) -> Option<(String, String)> {
    let rest = prefix.strip_prefix(SAMPLES_PREFIX)?.trim_end_matches('/');
    let (maker, model) = rest.split_once('/')?;
    if maker.is_empty() || model.is_empty() || model.contains('/') {
        return None;
    }
    Some((maker.to_string(), model.to_string()))
}

/// "pending/20260514T180000Z-a1b2c3d4/" -> Some("20260514T180000Z-a1b2c3d4")
fn split_pending_prefix(prefix: &str) -> Option<String> {
    let rest = prefix.strip_prefix(PENDING_PREFIX)?.trim_end_matches('/');
    if rest.is_empty() || rest.contains('/') {
        return None;
    }
    Some(rest.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_users_toml() {
        let toml = r#"
            [[users]]
            sub = "github:cytrinox"
            display_name = "Daniel"
            roles = ["admin"]
            blocked = false

            [[users]]
            sub = "github:other"
            roles = ["reviewer"]
            blocked = true
        "#;
        let users = parse_users_toml(toml.as_bytes()).unwrap();
        assert_eq!(users.len(), 2);
        assert_eq!(users[0].roles, vec!["admin"]);
        assert!(users[1].blocked);
    }

    #[test]
    fn empty_users_toml_yields_no_rows() {
        let users = parse_users_toml(b"").unwrap();
        assert!(users.is_empty());
    }

    #[test]
    fn splits_set_prefix() {
        assert_eq!(
            split_set_prefix("samples/Canon/EOS R5/"),
            Some(("Canon".into(), "EOS R5".into()))
        );
        assert_eq!(split_set_prefix("samples/Canon/"), None);
        assert_eq!(split_set_prefix("pending/Canon/EOS R5/"), None);
    }

    fn obj(key: &str, size: u64) -> ObjectInfo {
        ObjectInfo {
            key: key.to_string(),
            size,
            etag: Some(format!("etag-{size}")),
            last_modified: None,
        }
    }

    fn two_file_meta() -> RawdbMeta {
        meta::parse(
            r#"
            [set]
            maker = "Canon"
            model = "EOS R5"

            [[files]]
            path = "raw/a.cr3"

            [[files]]
            path = "lossless/b.cr3"
            "#,
        )
        .unwrap()
    }

    #[test]
    fn pair_files_complete_listing_pairs_everything() {
        let prefix = "samples/Canon/EOS R5/";
        let objects = vec![
            obj("samples/Canon/EOS R5/rawdb-meta.toml", 1),
            obj("samples/Canon/EOS R5/raw/a.cr3", 10),
            obj("samples/Canon/EOS R5/lossless/b.cr3", 20),
            obj("samples/Canon/EOS R5/unlisted.txt", 30),
        ];
        let out = pair_files(&two_file_meta(), prefix, objects).unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].0.path, "raw/a.cr3");
        assert_eq!(out[0].1.size, 10);
        assert_eq!(out[1].0.path, "lossless/b.cr3");
        assert_eq!(out[1].1.etag.as_deref(), Some("etag-20"));
    }

    #[test]
    fn pair_files_missing_file_yields_no_partial_list() {
        let prefix = "samples/Canon/EOS R5/";
        // `lossless/b.cr3` is declared in the meta but absent from the listing
        // (e.g. still uploading, or a lagging listing). Must not produce a
        // shortened file list.
        let objects = vec![
            obj("samples/Canon/EOS R5/rawdb-meta.toml", 1),
            obj("samples/Canon/EOS R5/raw/a.cr3", 10),
        ];
        let err = pair_files(&two_file_meta(), prefix, objects).unwrap_err();
        assert_eq!(err, vec!["lossless/b.cr3".to_string()]);
    }

    #[test]
    fn pair_files_reports_all_missing_paths() {
        let err = pair_files(&two_file_meta(), "samples/Canon/EOS R5/", vec![]).unwrap_err();
        assert_eq!(err, vec!["raw/a.cr3".to_string(), "lossless/b.cr3".to_string()]);
    }

    #[test]
    fn parse_meta_rejects_garbage_as_content_problem() {
        assert!(parse_meta(b"not = [valid").is_err());
        assert!(parse_meta(&[0xff, 0xfe]).is_err());
    }

    #[test]
    fn splits_pending_prefix() {
        assert_eq!(
            split_pending_prefix("pending/20260514T180000Z-a1b2c3d4/"),
            Some("20260514T180000Z-a1b2c3d4".to_string())
        );
        assert_eq!(split_pending_prefix("pending/a/b/"), None);
        assert_eq!(split_pending_prefix("pending/"), None);
    }
}
