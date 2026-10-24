//! Reviewer previews: the embedded preview (or thumbnail) of a pending RAW
//! file, extracted by a [`worker`] process and cached on local disk.
//!
//! Layout under `RAWDB_PREVIEW_DIR`:
//! - `work/<job>/`: download and worker output of one extraction, removed
//!   when the job ends (and wiped at startup).
//! - `cache/<id>.jpg`: an extracted preview; `cache/<id>.err`: the
//!   decoder's reason why a file has none. Both expire
//!   `RAWDB_PREVIEW_CACHE_TTL_SECS` after they were written.
//!
//! Like the SQLite cache this is per instance and disposable.

pub mod worker;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use anyhow::Context;
use axum::body::Bytes;
use opentelemetry::metrics::{Counter, Histogram};
use opentelemetry::KeyValue;
use sha2::{Digest, Sha256};
use tokio::sync::Semaphore;

use crate::config::Config;
use crate::s3::{S3Error, S3};

/// Bump when the rendering changes (sizing, quality, orientation), so
/// previews cached by an older build aren't served.
const RENDER_VERSION: u32 = 1;

/// Worker stderr kept for error messages and logs.
const MAX_STDERR: usize = 1024;

type InflightMap = Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>;

pub struct Previews {
    work_dir: PathBuf,
    cache_dir: PathBuf,
    ttl: Duration,
    max_dim: u32,
    max_file_bytes: u64,
    timeout: Duration,
    worker_memory: u64,
    /// This binary; workers are started by re-executing it.
    exe: PathBuf,
    /// Bounds concurrent extractions (download + worker).
    slots: Semaphore,
    /// One lock per cache id, so concurrent requests for the same preview
    /// extract it once.
    inflight: InflightMap,
    /// `rawdb.preview.lookups`, labelled `result` (hit/miss/error).
    lookups: Counter<u64>,
    /// `rawdb.preview.duration`: download + worker time per extraction.
    duration: Histogram<f64>,
}

/// A JPEG preview ready to serve.
pub struct Preview {
    /// Cache id; doubles as the response ETag.
    pub id: String,
    pub jpeg: Bytes,
    /// Time left until the cache entry expires.
    pub fresh_for: Duration,
}

#[derive(Debug, thiserror::Error)]
pub enum PreviewError {
    #[error("not found")]
    NotFound,
    /// The file has no preview we can serve; the message is meant for the
    /// reviewer.
    #[error("{0}")]
    Unavailable(String),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// How a worker run ended.
enum Outcome {
    Preview,
    /// The decoder's definitive answer; cached like a preview.
    NoPreview(String),
    /// Crash, timeout or memory limit; not cached.
    Failed(String),
}

impl Previews {
    /// Creates the preview directories and clears what was left of jobs
    /// running when the process last stopped.
    pub fn new(
        config: &Config,
        lookups: Counter<u64>,
        duration: Histogram<f64>,
    ) -> anyhow::Result<Self> {
        let dir = config.preview_dir();
        let work_dir = dir.join("work");
        let cache_dir = dir.join("cache");
        match std::fs::remove_dir_all(&work_dir) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("clear {}", work_dir.display())),
        }
        for d in [&work_dir, &cache_dir] {
            std::fs::create_dir_all(d)
                .with_context(|| format!("create preview dir {}", d.display()))?;
        }
        Ok(Self {
            work_dir,
            cache_dir,
            ttl: Duration::from_secs(config.preview_cache_ttl_secs),
            max_dim: config.preview_max_dim,
            max_file_bytes: config.preview_max_file_bytes,
            timeout: Duration::from_secs(config.preview_timeout_secs),
            worker_memory: config.preview_worker_memory_bytes,
            exe: std::env::current_exe().context("locate own executable for preview workers")?,
            slots: Semaphore::new(config.preview_concurrency.max(1) as usize),
            inflight: Mutex::default(),
            lookups,
            duration,
        })
    }

    /// The preview of the S3 object `key`, whose file name is `name`.
    pub async fn get(&self, s3: &S3, key: &str, name: &str) -> Result<Preview, PreviewError> {
        // Cheap checks first: neither needs a download.
        let Some(ext) = raw_extension(name) else {
            return Err(PreviewError::Unavailable(match extension(name) {
                Some(ext) => format!(".{ext} is not a RAW format rawler reads"),
                None => "file has no extension; not a RAW format rawler reads".into(),
            }));
        };
        let head = s3
            .head(key)
            .await
            .map_err(PreviewError::Other)?
            .ok_or(PreviewError::NotFound)?;
        if head.size > self.max_file_bytes {
            return Err(PreviewError::Unavailable(format!(
                "file is too large for a preview ({} bytes; limit is {} bytes)",
                head.size, self.max_file_bytes
            )));
        }
        let version = head
            .etag
            .or_else(|| head.last_modified.map(|t| t.to_rfc3339()))
            .unwrap_or_default();
        let id = cache_id(key, &version, head.size, self.max_dim);

        let inflight = self.inflight_entry(&id);
        let _turn = inflight.lock.lock().await;
        if let Some(cached) = self.lookup(&id).await {
            self.count("hit");
            return cached;
        }
        let result = self.extract(s3, key, ext, head.size, &id).await;
        self.count(if matches!(result, Err(PreviewError::Other(_))) {
            "error"
        } else {
            "miss"
        });
        result
    }

    /// Downloads the object, runs the worker on it and caches the outcome.
    async fn extract(
        &self,
        s3: &S3,
        key: &str,
        ext: &str,
        size: u64,
        id: &str,
    ) -> Result<Preview, PreviewError> {
        let _slot = self.slots.acquire().await.context("preview slots closed")?;
        let started = Instant::now();
        let job = WorkDir::create(&self.work_dir).await?;
        let input = job.path().join(format!("input.{ext}"));
        let output = job.path().join("preview.jpg");
        s3.download_to_file(key, &input, size)
            .await
            .map_err(|e| match e {
                S3Error::NotFound(_) | S3Error::PreconditionFailed => PreviewError::NotFound,
                S3Error::Other(e) => PreviewError::Other(e),
            })?;
        let outcome = self.run_worker(&input, &output).await;
        self.duration.record(started.elapsed().as_secs_f64(), &[]);

        match outcome? {
            Outcome::Preview => {
                let jpeg = tokio::fs::read(&output)
                    .await
                    .with_context(|| format!("read {}", output.display()))?;
                self.store(id, "jpg", &output).await;
                Ok(Preview {
                    id: id.to_string(),
                    jpeg: jpeg.into(),
                    fresh_for: self.ttl,
                })
            }
            Outcome::NoPreview(reason) => {
                let file = job.path().join("reason.err");
                match tokio::fs::write(&file, &reason).await {
                    Ok(()) => self.store(id, "err", &file).await,
                    Err(e) => tracing::warn!(error = %e, "write preview reason"),
                }
                Err(PreviewError::Unavailable(reason))
            }
            Outcome::Failed(reason) => {
                tracing::warn!(key, reason = %reason, "preview worker failed");
                Err(PreviewError::Unavailable(reason))
            }
        }
    }

    async fn run_worker(&self, input: &Path, output: &Path) -> anyhow::Result<Outcome> {
        let child = tokio::process::Command::new(&self.exe)
            .arg(worker::ARG)
            .arg(input)
            .arg(output)
            .arg(self.max_dim.to_string())
            .arg(self.worker_memory.to_string())
            // The worker parses untrusted files and gets none of the
            // server's secrets. A rayon thread per host CPU would also eat
            // into its memory limit (thread stacks count against it).
            .env_clear()
            .env("RAYON_NUM_THREADS", "2")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .context("spawn preview worker")?;
        // On timeout the wait future, and with it the child, is dropped,
        // which kills the worker.
        let Ok(out) = tokio::time::timeout(self.timeout, child.wait_with_output()).await else {
            return Ok(Outcome::Failed(format!(
                "decoder timed out after {}s",
                self.timeout.as_secs()
            )));
        };
        let out = out.context("wait for preview worker")?;
        let stderr = stderr_text(&out.stderr);
        Ok(match out.status.code() {
            Some(0) => Outcome::Preview,
            Some(worker::EXIT_NO_PREVIEW) => Outcome::NoPreview(stderr),
            _ if stderr.contains("memory allocation of") => Outcome::Failed(format!(
                "preview needs more memory than the worker may use \
                 (RAWDB_PREVIEW_WORKER_MEMORY_BYTES={})",
                self.worker_memory
            )),
            _ => Outcome::Failed(format!("decoder crashed ({}): {stderr}", out.status)),
        })
    }

    /// The cached outcome for `id`, if there is a fresh one.
    async fn lookup(&self, id: &str) -> Option<Result<Preview, PreviewError>> {
        let jpg = self.cache_path(id, "jpg");
        if let Some(fresh_for) = self.fresh_for(&jpg).await {
            if let Ok(jpeg) = tokio::fs::read(&jpg).await {
                return Some(Ok(Preview {
                    id: id.to_string(),
                    jpeg: jpeg.into(),
                    fresh_for,
                }));
            }
        }
        let err = self.cache_path(id, "err");
        if self.fresh_for(&err).await.is_some() {
            if let Ok(reason) = tokio::fs::read_to_string(&err).await {
                return Some(Err(PreviewError::Unavailable(reason)));
            }
        }
        None
    }

    /// Moves `file` into the cache as `<id>.<kind>` and drops a stale entry
    /// of the other kind. Failures only cost a later re-extraction.
    async fn store(&self, id: &str, kind: &str, file: &Path) {
        let dst = self.cache_path(id, kind);
        if let Err(e) = tokio::fs::rename(file, &dst).await {
            tracing::warn!(error = %e, path = %dst.display(), "store preview");
        }
        let other = if kind == "jpg" { "err" } else { "jpg" };
        let _ = tokio::fs::remove_file(self.cache_path(id, other)).await;
    }

    fn cache_path(&self, id: &str, kind: &str) -> PathBuf {
        self.cache_dir.join(format!("{id}.{kind}"))
    }

    /// Time left until the cache file at `path` expires; `None` when it is
    /// missing or expired.
    async fn fresh_for(&self, path: &Path) -> Option<Duration> {
        let modified = tokio::fs::metadata(path).await.ok()?.modified().ok()?;
        let age = SystemTime::now().duration_since(modified).unwrap_or_default();
        self.ttl.checked_sub(age).filter(|d| !d.is_zero())
    }

    /// Periodically deletes expired cache entries.
    pub fn spawn_sweeper(self: Arc<Self>) {
        let period = (self.ttl / 4).clamp(Duration::from_secs(60), Duration::from_secs(15 * 60));
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(period);
            loop {
                tick.tick().await;
                let removed = self.sweep().await;
                if removed > 0 {
                    tracing::debug!(removed, "preview cache sweep");
                }
            }
        });
    }

    /// Deletes expired cache entries; returns how many.
    async fn sweep(&self) -> usize {
        let mut entries = match tokio::fs::read_dir(&self.cache_dir).await {
            Ok(entries) => entries,
            Err(e) => {
                tracing::warn!(error = %e, dir = %self.cache_dir.display(), "preview cache sweep");
                return 0;
            }
        };
        let mut removed = 0;
        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            if self.fresh_for(&path).await.is_none() && tokio::fs::remove_file(&path).await.is_ok() {
                removed += 1;
            }
        }
        removed
    }

    fn inflight_entry(&self, id: &str) -> Inflight<'_> {
        let lock = self
            .inflight
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(id.to_string())
            .or_default()
            .clone();
        Inflight {
            map: &self.inflight,
            id: id.to_string(),
            lock,
        }
    }

    fn count(&self, result: &'static str) {
        self.lookups.add(1, &[KeyValue::new("result", result)]);
    }
}

/// A request's handle on the per-preview lock. On drop the map entry goes
/// away unless another request still waits for the same preview.
struct Inflight<'a> {
    map: &'a InflightMap,
    id: String,
    lock: Arc<tokio::sync::Mutex<()>>,
}

impl Drop for Inflight<'_> {
    fn drop(&mut self) {
        let mut map = self.map.lock().unwrap_or_else(|e| e.into_inner());
        // One reference is the map's and one ours; any other belongs to a
        // request waiting for this preview.
        if Arc::strong_count(&self.lock) <= 2
            && map.get(&self.id).is_some_and(|l| Arc::ptr_eq(l, &self.lock))
        {
            map.remove(&self.id);
        }
    }
}

/// Scratch directory of one extraction. Removed on drop, which also covers
/// requests cancelled mid-download.
struct WorkDir(PathBuf);

impl WorkDir {
    async fn create(parent: &Path) -> anyhow::Result<Self> {
        let path = parent.join(uuid::Uuid::now_v7().to_string());
        tokio::fs::create_dir(&path)
            .await
            .with_context(|| format!("create {}", path.display()))?;
        Ok(Self(path))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for WorkDir {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_dir_all(&self.0) {
            tracing::warn!(error = %e, dir = %self.0.display(), "remove preview work dir");
        }
    }
}

/// Cache key of one rendering of one object version.
fn cache_id(key: &str, version: &str, size: u64, max_dim: u32) -> String {
    let digest = Sha256::digest(format!(
        "{key}\0{version}\0{size}\0{max_dim}\0{RENDER_VERSION}"
    ));
    digest[..16].iter().map(|b| format!("{b:02x}")).collect()
}

/// Final extension of a file name.
fn extension(name: &str) -> Option<&str> {
    name.rsplit_once('.')
        .map(|(_, ext)| ext)
        .filter(|ext| !ext.is_empty())
}

/// The extension of `name` as listed by rawler (upper case), if rawler
/// reads that format. Also names the downloaded file.
fn raw_extension(name: &str) -> Option<&'static str> {
    let ext = extension(name)?;
    rawler::decoders::supported_extensions()
        .iter()
        .copied()
        .find(|known| known.eq_ignore_ascii_case(ext))
}

fn stderr_text(raw: &[u8]) -> String {
    let text = String::from_utf8_lossy(&raw[..raw.len().min(MAX_STDERR)]);
    match text.trim() {
        "" => "no error output".to_string(),
        text => text.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn previews(dir: &Path, ttl: Duration) -> Previews {
        let meter = opentelemetry::global::meter("test");
        Previews {
            work_dir: dir.join("work"),
            cache_dir: dir.to_path_buf(),
            ttl,
            max_dim: 2048,
            max_file_bytes: u64::MAX,
            timeout: Duration::from_secs(1),
            worker_memory: 0,
            exe: PathBuf::new(),
            slots: Semaphore::new(1),
            inflight: Mutex::default(),
            lookups: meter.u64_counter("test").build(),
            duration: meter.f64_histogram("test").build(),
        }
    }

    /// Sets a file's mtime `age` into the past.
    fn age(path: &Path, age: Duration) {
        let file = std::fs::File::options().write(true).open(path).unwrap();
        file.set_modified(SystemTime::now() - age).unwrap();
    }

    #[test]
    fn cache_id_tracks_object_version_and_size_cap() {
        let a = cache_id("pending/u/raw/a.nef", "etag1", 10, 2048);
        assert_eq!(a.len(), 32);
        assert_eq!(a, cache_id("pending/u/raw/a.nef", "etag1", 10, 2048));
        assert_ne!(a, cache_id("pending/u/raw/a.nef", "etag2", 10, 2048));
        assert_ne!(a, cache_id("pending/u/raw/a.nef", "etag1", 10, 1024));
        assert_ne!(a, cache_id("pending/u/raw/b.nef", "etag1", 10, 2048));
    }

    #[test]
    fn raw_extension_is_case_insensitive() {
        assert_eq!(raw_extension("DSC_0001.nef"), Some("NEF"));
        assert_eq!(raw_extension("IMG_0001.CR3"), Some("CR3"));
        assert_eq!(raw_extension("clip.mp4"), None);
        assert_eq!(raw_extension("photo.heif"), None);
        assert_eq!(raw_extension("noext"), None);
        assert_eq!(raw_extension("trailingdot."), None);
    }

    #[tokio::test]
    async fn lookup_serves_fresh_entries_only() {
        let dir = tempfile::tempdir().unwrap();
        let p = previews(dir.path(), Duration::from_secs(3600));

        std::fs::write(p.cache_path("fresh", "jpg"), b"jpeg").unwrap();
        let hit = p.lookup("fresh").await.unwrap().unwrap();
        assert_eq!(&hit.jpeg[..], b"jpeg");
        assert!(hit.fresh_for > Duration::from_secs(3500));

        std::fs::write(p.cache_path("reason", "err"), b"no embedded preview").unwrap();
        match p.lookup("reason").await {
            Some(Err(PreviewError::Unavailable(m))) => assert_eq!(m, "no embedded preview"),
            _ => panic!("expected cached reason"),
        }

        let stale = p.cache_path("stale", "jpg");
        std::fs::write(&stale, b"jpeg").unwrap();
        age(&stale, Duration::from_secs(3601));
        assert!(p.lookup("stale").await.is_none());
        assert!(p.lookup("missing").await.is_none());
    }

    #[tokio::test]
    async fn sweep_removes_only_expired_entries() {
        let dir = tempfile::tempdir().unwrap();
        let p = previews(dir.path(), Duration::from_secs(60));
        let fresh = p.cache_path("a", "jpg");
        let old_jpg = p.cache_path("b", "jpg");
        let old_err = p.cache_path("c", "err");
        for f in [&fresh, &old_jpg, &old_err] {
            std::fs::write(f, b"x").unwrap();
        }
        age(&old_jpg, Duration::from_secs(61));
        age(&old_err, Duration::from_secs(600));

        assert_eq!(p.sweep().await, 2);
        assert!(fresh.exists());
        assert!(!old_jpg.exists() && !old_err.exists());
    }

    #[tokio::test]
    async fn inflight_entry_lives_while_requests_wait() {
        let dir = tempfile::tempdir().unwrap();
        let p = previews(dir.path(), Duration::from_secs(60));
        let first = p.inflight_entry("id");
        let second = p.inflight_entry("id");
        assert!(Arc::ptr_eq(&first.lock, &second.lock));
        drop(first);
        assert!(p.inflight.lock().unwrap().contains_key("id"));
        drop(second);
        assert!(p.inflight.lock().unwrap().is_empty());
    }
}
