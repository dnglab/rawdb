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
//! `true` afterwards) then on a `tokio::time::interval` thereafter.

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
use crate::meta::{self, RawdbMeta};
use crate::s3::S3;
use crate::state::{AppMetrics, AppState};

pub const SAMPLES_PREFIX: &str = "samples/";
pub const PENDING_PREFIX: &str = "pending/";
pub const USERS_KEY: &str = "_system/users.toml";
pub const META_FILE: &str = "rawdb-meta.toml";

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

    /// Spawn the scanner loop. Returns immediately. The first successful
    /// `run_once()` flips `ready_tx` to `true` so the readiness probe lets
    /// the LB route traffic to this pod.
    pub fn spawn(self, ready_tx: watch::Sender<bool>) {
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(self.interval);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            // First tick fires immediately.
            loop {
                interval.tick().await;
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
                            "scan complete",
                        );
                        if !*ready_tx.borrow() {
                            let _ = ready_tx.send(true);
                        }
                    }
                    Err(e) => {
                        tracing::error!(error = ?e, "scan failed");
                        self.record_error("run_once");
                        self.events.scan_error("run_once", &format!("{e:#}"));
                    }
                }
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

    pub async fn run_once(&self) -> Result<ScanSummary> {
        let started = Instant::now();
        let mut summary = ScanSummary::default();
        let sem = Arc::new(Semaphore::new(self.concurrency));

        // Pass A
        let a = self.scan_samples(sem.clone()).await?;
        summary.samples_seen = a.seen;
        summary.samples_updated = a.updated;
        summary.samples_deleted = a.deleted;

        // Pass B
        let b = self.scan_pending(sem.clone()).await?;
        summary.pending_seen = b.seen;
        summary.pending_updated = b.updated;
        summary.pending_deleted = b.deleted;

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

        let mut tasks: JoinSet<Result<bool>> = JoinSet::new();
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
            match j {
                Ok(Ok(true)) => res.updated += 1,
                Ok(Ok(false)) => {}
                Ok(Err(e)) => {
                    tracing::warn!(error = ?e, "scan_one_set failed");
                    self.record_error("scan_one_set");
                    self.events.scan_error("scan_one_set", &format!("{e:#}"));
                }
                Err(e) => {
                    tracing::warn!(error = ?e, "scan_one_set join failed");
                    self.record_error("scan_one_set");
                    self.events.scan_error("scan_one_set", &format!("{e:#}"));
                }
            }
        }

        // Delete sets present in DB but absent from S3.
        for key in prior_keys.difference(&live_in_s3) {
            if let Err(e) = self.db.delete_set(&key.0, &key.1) {
                tracing::warn!(maker = %key.0, model = %key.1, error = ?e, "delete_set failed");
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

        let mut tasks: JoinSet<Result<bool>> = JoinSet::new();
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
            match j {
                Ok(Ok(true)) => res.updated += 1,
                Ok(Ok(false)) => {}
                Ok(Err(e)) => {
                    tracing::warn!(error = ?e, "scan_one_pending failed");
                    self.record_error("scan_one_pending");
                    self.events.scan_error("scan_one_pending", &format!("{e:#}"));
                }
                Err(e) => {
                    tracing::warn!(error = ?e, "scan_one_pending join failed");
                    self.record_error("scan_one_pending");
                    self.events.scan_error("scan_one_pending", &format!("{e:#}"));
                }
            }
        }

        for upload_id in prior.difference(&live) {
            if let Err(e) = self.db.delete_pending_by_upload_id(upload_id) {
                tracing::warn!(error = ?e, "delete_pending failed");
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
    /// approve/reject so the originating pod's UI reflects the change
    /// without waiting for the next tick).
    pub async fn refresh_one_set(&self, maker: &str, model: &str) -> Result<()> {
        let prefix = format!("{SAMPLES_PREFIX}{maker}/{model}/");
        let updated = scan_one_set(&self.db, &self.s3, &prefix, maker, model).await?;
        if !updated {
            // If S3 doesn't have it anymore, drop locally.
            if !self
                .s3
                .list_common_prefixes(&prefix)
                .await?
                .iter()
                .any(|_| true)
            {
                let _ = self.db.delete_set(maker, model);
            }
        }
        Ok(())
    }

    pub async fn refresh_one_pending(&self, upload_id: &str) -> Result<()> {
        let prefix = format!("{PENDING_PREFIX}{upload_id}/");
        let updated = scan_one_pending(&self.db, &self.s3, &prefix, upload_id).await?;
        if !updated {
            let _ = self.db.delete_pending_by_upload_id(upload_id);
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
}

#[derive(Default)]
struct PassResult {
    seen: u64,
    updated: u64,
    deleted: u64,
}

// ---------------------------------------------------------------------------
// Per-set / per-pending workers
// ---------------------------------------------------------------------------

async fn scan_one_set(db: &Db, s3: &S3, prefix: &str, maker: &str, model: &str) -> Result<bool> {
    let meta_key = format!("{prefix}{META_FILE}");
    let head = match s3.head(&meta_key).await? {
        Some(h) => h,
        None => return Ok(false), // model dir present but no meta yet
    };
    let prior = db.get_set_meta_etag(maker, model)?;
    if prior.as_deref() == head.etag.as_deref() {
        return Ok(false);
    }
    let (bytes, etag) = s3.get_bytes(&meta_key).await?;
    let mut meta = match meta::parse(std::str::from_utf8(&bytes).context("meta utf-8")?) {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(maker, model, error = ?e, "bad rawdb-meta.toml; skipping");
            return Ok(false);
        }
    };
    // `uploaded_at` is optional in the meta; fall back to the meta object's
    // S3 LastModified so the UI always shows a date.
    if meta.set.uploaded_at.is_none() {
        meta.set.uploaded_at = head.last_modified;
    }
    let files_with_sizes = match_files(&meta, s3, prefix).await?;
    db.upsert_set(&meta, etag.as_deref(), &files_with_sizes)?;
    Ok(true)
}

async fn scan_one_pending(db: &Db, s3: &S3, prefix: &str, upload_id: &str) -> Result<bool> {
    let meta_key = format!("{prefix}{META_FILE}");
    let head = match s3.head(&meta_key).await? {
        Some(h) => h,
        None => return Ok(false),
    };
    let prior = db.get_pending_meta_etag(upload_id)?;
    if prior.as_deref() == head.etag.as_deref() {
        return Ok(false);
    }
    let (bytes, etag) = s3.get_bytes(&meta_key).await?;
    let mut meta = match meta::parse(std::str::from_utf8(&bytes).context("meta utf-8")?) {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(upload_id, error = ?e, "bad pending meta; skipping");
            return Ok(false);
        }
    };
    // `uploaded_at` is optional; fall back to the meta object's S3
    // LastModified so the pending overview shows the upload time.
    if meta.set.uploaded_at.is_none() {
        meta.set.uploaded_at = head.last_modified;
    }
    let files_with_sizes = match_files(&meta, s3, prefix).await?;
    db.upsert_pending(upload_id, &meta, etag.as_deref(), &files_with_sizes)?;
    Ok(true)
}

/// Pair each meta `[[files]]` entry with the size + ETag observed on S3.
/// Files declared in meta but missing in S3 are dropped (logged); files in
/// S3 not mentioned in meta are ignored.
async fn match_files(
    meta: &RawdbMeta,
    s3: &S3,
    prefix: &str,
) -> Result<Vec<(crate::meta::FileMeta, FileSizeInfo)>> {
    let objects = s3.list_objects(prefix).await?;
    // Build map of rel-path -> (size, etag).
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
    for f in &meta.files {
        let Some((size, etag)) = by_rel.get(&f.path).cloned() else {
            tracing::warn!(maker = %meta.set.maker, model = %meta.set.model, path = %f.path, "meta lists a file not present in S3");
            continue;
        };
        out.push((f.clone(), FileSizeInfo { size, etag }));
    }
    Ok(out)
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
