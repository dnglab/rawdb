//! Reviewer file inspection: processors run on a downloaded pending file,
//! with their results cached on local disk.
//!
//! A download is a *job*: the file is fetched once and every processor that
//! applies and has no fresh result runs on it. Asking for one result (say
//! the preview) therefore fills the cache for the others too (say the
//! exiftool listing). The request that needs a result gets it as soon as
//! its processor is done; the job finishes the rest in the background.
//!
//! Processors and their outputs, served at
//! `/api/admin/pending/{upload_id}/inspect/<processor>/<output>/{path}`:
//! - [`preview`]: `preview/image`, the embedded preview (or thumbnail) as
//!   JPEG, extracted by a rawler worker process.
//! - [`exiftool`]: `exiftool/listing`, every tag as text.
//!
//! Adding a processor (e.g. an image analysis): implement [`Processor`] in
//! a new module and register it in [`Inspector::new`]. It then runs on every
//! download, and each of its outputs, in whatever format, is served by the
//! route above.
//!
//! Layout under `RAWDB_INSPECT_DIR`:
//! - `work/<job>/`: the download and the processors' scratch dirs of one
//!   job, removed when the job ends (and wiped at startup).
//! - `cache/<id>/`: one processor's result for one object version: the
//!   [`Manifest`] plus the output files it lists. Expires
//!   `RAWDB_INSPECT_CACHE_TTL_SECS` after it was written.
//!
//! Like the SQLite cache this is per instance and disposable.

pub mod exiftool;
pub mod preview;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use anyhow::Context;
use async_trait::async_trait;
use axum::body::Bytes;
use opentelemetry::metrics::{Counter, Histogram};
use opentelemetry::KeyValue;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::{watch, Semaphore};

use crate::config::Config;
use crate::s3::{HeadInfo, S3Error, S3};

/// Tool stderr kept for error messages and logs.
const MAX_STDERR: usize = 1024;

/// File name of a result's manifest in its cache dir.
const MANIFEST: &str = "manifest.json";

/// Where inspected files come from: the S3 bucket outside tests.
// async_trait marks the boxed futures of trait methods `#[must_use]` again.
#[allow(clippy::double_must_use)]
#[async_trait]
pub trait Source: Send + Sync {
    async fn head(&self, key: &str) -> anyhow::Result<Option<HeadInfo>>;
    /// Downloads `key` to `path`, failing unless exactly `len` bytes arrive.
    async fn download(&self, key: &str, path: &Path, len: u64) -> Result<u64, S3Error>;
}

#[async_trait]
impl Source for S3 {
    async fn head(&self, key: &str) -> anyhow::Result<Option<HeadInfo>> {
        S3::head(self, key).await
    }

    async fn download(&self, key: &str, path: &Path, len: u64) -> Result<u64, S3Error> {
        self.download_to_file(key, path, len).await
    }
}

/// A processor of downloaded files; see the module docs for adding one.
#[allow(clippy::double_must_use)]
#[async_trait]
trait Processor: Send + Sync {
    /// Stable name: URL segment, metric label and part of the cache id.
    fn name(&self) -> &'static str;

    /// Outputs a run may produce. Requests for any other output are
    /// rejected before any work is done.
    fn outputs(&self) -> &'static [&'static str];

    /// Why the processor can't handle a file called `file_name`; checked
    /// before any download.
    fn check(&self, file_name: &str) -> Result<(), String>;

    /// Everything besides the object version that changes a result, such
    /// as the tool version and settings.
    fn params(&self) -> String;

    /// Runs on `input` and writes the outputs into the empty dir `out`.
    async fn run(&self, input: &Path, out: &Path, limits: &Limits) -> anyhow::Result<Outcome>;
}

/// Resource limits of one processor run.
struct Limits {
    timeout: Duration,
    /// `RLIMIT_DATA` of tool processes; `0` = none.
    memory: u64,
}

/// One output file of a processor run, as listed in its manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct OutputFile {
    /// Name the route addresses, e.g. `image`.
    name: String,
    /// File name in the result dir, e.g. `image.jpg`.
    file: String,
    content_type: String,
}

impl OutputFile {
    fn new(name: &str, file: &str, content_type: &str) -> Self {
        Self {
            name: name.to_string(),
            file: file.to_string(),
            content_type: content_type.to_string(),
        }
    }
}

/// How a processor run ended.
enum Outcome {
    /// These outputs were written into the run's output dir.
    Done(Vec<OutputFile>),
    /// The tool's definitive answer that there is no result; cached.
    NoResult(String),
    /// Crash, timeout or memory limit; not cached.
    Failed(String),
}

/// `cache/<id>/manifest.json`: what a processor run left for one object
/// version.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Manifest {
    Outputs(Vec<OutputFile>),
    /// Why there is no result.
    Unavailable(String),
}

/// An output ready to serve.
pub struct Artifact {
    /// Unique per output and object version; the response ETag.
    pub etag: String,
    pub body: Bytes,
    pub content_type: String,
    /// Extension of the output file, for the download name.
    pub ext: String,
    /// Time left until the cache entry expires.
    pub fresh_for: Duration,
}

#[derive(Debug, thiserror::Error)]
pub enum InspectError {
    #[error("not found")]
    NotFound,
    /// There is no result we can serve; the message is meant for the
    /// reviewer.
    #[error("{0}")]
    Unavailable(String),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// How a job's processor ended, as seen by the requests waiting for it.
#[derive(Debug, Clone)]
enum Failure {
    NotFound,
    Unavailable(String),
    Internal(String),
}

impl From<Failure> for InspectError {
    fn from(f: Failure) -> Self {
        match f {
            Failure::NotFound => InspectError::NotFound,
            Failure::Unavailable(reason) => InspectError::Unavailable(reason),
            Failure::Internal(e) => InspectError::Other(anyhow::anyhow!(e)),
        }
    }
}

/// Progress of a job, watched by the requests waiting for it.
#[derive(Default)]
struct JobState {
    /// Processors done so far; `Ok` means their result is in the cache.
    done: HashMap<&'static str, Result<(), Failure>>,
    /// The job ended. Processors missing from `done` weren't planned.
    finished: bool,
}

type JobMap = Mutex<HashMap<String, Arc<watch::Sender<JobState>>>>;

/// The object version a job works on.
#[derive(Clone)]
struct Object {
    key: String,
    /// ETag, else last-modified: changes with the content.
    version: String,
    size: u64,
    /// The upload's file name, for [`Processor::check`].
    file_name: String,
}

impl Object {
    /// Identifies the job for this object version.
    fn job_key(&self) -> String {
        format!("{}\0{}\0{}", self.key, self.version, self.size)
    }
}

pub struct Inspector {
    source: Arc<dyn Source>,
    processors: Vec<Arc<dyn Processor>>,
    work_dir: PathBuf,
    cache_dir: PathBuf,
    ttl: Duration,
    max_file_bytes: u64,
    limits: Limits,
    /// Bounds concurrent jobs (download + processors).
    slots: Semaphore,
    /// Running jobs by object version, so concurrent requests share one
    /// download.
    jobs: JobMap,
    /// `rawdb.inspect.lookups`, labelled `processor` and `result`
    /// (hit/miss/error).
    lookups: Counter<u64>,
    /// `rawdb.inspect.duration`, labelled `processor`: time per processor
    /// run, and per download as `processor="download"`.
    duration: Histogram<f64>,
}

impl Inspector {
    /// Creates the inspection directories, clears what was left of jobs
    /// running when the process last stopped and registers the processors.
    pub async fn new(
        config: &Config,
        source: Arc<dyn Source>,
        lookups: Counter<u64>,
        duration: Histogram<f64>,
    ) -> anyhow::Result<Self> {
        let processors: Vec<Arc<dyn Processor>> = vec![
            Arc::new(preview::PreviewProcessor::new(
                std::env::current_exe().context("locate own executable for preview workers")?,
                config.preview_max_dim,
            )),
            Arc::new(exiftool::ExiftoolProcessor::new(
                exiftool::Exiftool::probe(&config.exiftool_bin).await,
            )),
        ];
        let inspector = Self::with_processors(
            &config.inspect_dir(),
            source,
            processors,
            Duration::from_secs(config.inspect_cache_ttl_secs),
            config.inspect_max_file_bytes,
            Limits {
                timeout: Duration::from_secs(config.inspect_timeout_secs),
                memory: config.inspect_memory_bytes,
            },
            config.inspect_concurrency.max(1) as usize,
            lookups,
            duration,
        )?;
        Ok(inspector)
    }

    #[allow(clippy::too_many_arguments)]
    fn with_processors(
        dir: &Path,
        source: Arc<dyn Source>,
        processors: Vec<Arc<dyn Processor>>,
        ttl: Duration,
        max_file_bytes: u64,
        limits: Limits,
        slots: usize,
        lookups: Counter<u64>,
        duration: Histogram<f64>,
    ) -> anyhow::Result<Self> {
        let work_dir = dir.join("work");
        let cache_dir = dir.join("cache");
        match std::fs::remove_dir_all(&work_dir) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("clear {}", work_dir.display())),
        }
        for d in [&work_dir, &cache_dir] {
            std::fs::create_dir_all(d)
                .with_context(|| format!("create inspect dir {}", d.display()))?;
        }
        Ok(Self {
            source,
            processors,
            work_dir,
            cache_dir,
            ttl,
            max_file_bytes,
            limits,
            slots: Semaphore::new(slots),
            jobs: Mutex::default(),
            lookups,
            duration,
        })
    }

    /// `output` of `processor` for the S3 object `key`, whose file name is
    /// `file_name`.
    pub async fn get(
        self: &Arc<Self>,
        processor: &str,
        output: &str,
        key: &str,
        file_name: &str,
    ) -> Result<Artifact, InspectError> {
        // Cheap checks first: none of them needs a download.
        let Some(wanted) = self.processors.iter().find(|p| p.name() == processor) else {
            return Err(InspectError::NotFound);
        };
        if !wanted.outputs().contains(&output) {
            return Err(InspectError::NotFound);
        }
        wanted.check(file_name).map_err(InspectError::Unavailable)?;
        let head = self
            .source
            .head(key)
            .await
            .map_err(InspectError::Other)?
            .ok_or(InspectError::NotFound)?;
        if head.size > self.max_file_bytes {
            return Err(InspectError::Unavailable(format!(
                "file is too large to inspect ({} bytes; limit is {} bytes)",
                head.size, self.max_file_bytes
            )));
        }
        let object = Object {
            key: key.to_string(),
            version: head
                .etag
                .or_else(|| head.last_modified.map(|t| t.to_rfc3339()))
                .unwrap_or_default(),
            size: head.size,
            file_name: file_name.to_string(),
        };
        let id = cache_id(wanted.as_ref(), &object);

        // A job planned while this result was still fresh doesn't produce
        // it; one retry starts a job that does.
        for _ in 0..2 {
            if let Some(cached) = self.lookup(&id, processor, output).await {
                self.count(processor, "hit");
                return cached;
            }
            let Some(mut job) = self.join_or_start(wanted, &object) else {
                // Fresh by now.
                continue;
            };
            let done = job
                .wait_for(|s| s.done.contains_key(processor) || s.finished)
                .await
                .ok()
                .and_then(|s| s.done.get(processor).cloned());
            let result = match done {
                Some(Ok(())) => self.lookup(&id, processor, output).await.unwrap_or_else(|| {
                    Err(InspectError::Other(anyhow::anyhow!(
                        "{processor} result of {key} missing from the cache"
                    )))
                }),
                Some(Err(failure)) => Err(failure.into()),
                None => continue,
            };
            self.count(
                processor,
                if matches!(result, Err(InspectError::Other(_))) {
                    "error"
                } else {
                    "miss"
                },
            );
            return result;
        }
        self.count(processor, "error");
        Err(InspectError::Other(anyhow::anyhow!(
            "inspection of {key} did not run {processor}"
        )))
    }

    /// Subscribes to the running job for `object`, or plans and starts one.
    /// `None` when `wanted` has a fresh result by now and needs no job.
    fn join_or_start(
        self: &Arc<Self>,
        wanted: &Arc<dyn Processor>,
        object: &Object,
    ) -> Option<watch::Receiver<JobState>> {
        let job_key = object.job_key();
        let mut jobs = self.jobs.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(job) = jobs.get(&job_key) {
            return Some(job.subscribe());
        }
        // Every processor that applies and has no fresh result, `wanted`
        // first. The freshness checks are plain metadata reads, so nothing
        // awaits under the lock.
        let others = self.processors.iter().filter(|p| p.name() != wanted.name());
        let plan: Vec<Arc<dyn Processor>> = std::iter::once(wanted)
            .chain(others)
            .filter(|p| {
                p.check(&object.file_name).is_ok() && !self.is_fresh(&cache_id(p.as_ref(), object))
            })
            .cloned()
            .collect();
        if plan.first().map(|p| p.name()) != Some(wanted.name()) {
            return None;
        }
        let (state, job) = watch::channel(JobState::default());
        let state = Arc::new(state);
        jobs.insert(job_key.clone(), state.clone());
        drop(jobs);
        tokio::spawn(self.clone().run_job(job_key, state, object.clone(), plan));
        Some(job)
    }

    /// Downloads `object` once and runs `plan` on it, publishing each
    /// processor's outcome once it is stored.
    async fn run_job(
        self: Arc<Self>,
        job_key: String,
        state: Arc<watch::Sender<JobState>>,
        object: Object,
        plan: Vec<Arc<dyn Processor>>,
    ) {
        // Unregisters the job and wakes every waiter however the job ends,
        // even when a processor panics.
        let _done = JobDone {
            jobs: &self.jobs,
            job_key: &job_key,
            state: &state,
        };
        let names: Vec<&'static str> = plan.iter().map(|p| p.name()).collect();
        tracing::debug!(key = %object.key, processors = ?names, "inspect job");
        if let Err(failure) = self.run_plan(&object, &plan, &state).await {
            // No download: every processor still without an outcome fails.
            state.send_modify(|s| {
                for name in &names {
                    s.done.entry(name).or_insert_with(|| Err(failure.clone()));
                }
            });
        }
    }

    async fn run_plan(
        &self,
        object: &Object,
        plan: &[Arc<dyn Processor>],
        state: &watch::Sender<JobState>,
    ) -> Result<(), Failure> {
        let _slot = self.slots.acquire().await.map_err(internal)?;
        let job = WorkDir::create(&self.work_dir).await.map_err(internal)?;
        let input = job.path().join(input_name(&object.file_name));
        let started = Instant::now();
        self.source
            .download(&object.key, &input, object.size)
            .await
            .map_err(|e| match e {
                S3Error::NotFound(_) | S3Error::PreconditionFailed => Failure::NotFound,
                S3Error::Other(e) => {
                    tracing::warn!(key = %object.key, error = %format!("{e:#}"), "inspect download failed");
                    Failure::Internal(format!("{e:#}"))
                }
            })?;
        self.record("download", started);
        for processor in plan {
            let outcome = self.run_processor(processor.as_ref(), &job, &input, object).await;
            state.send_modify(|s| {
                s.done.insert(processor.name(), outcome);
            });
        }
        Ok(())
    }

    /// Runs `processor` on the downloaded `input` and stores its result.
    async fn run_processor(
        &self,
        processor: &dyn Processor,
        job: &WorkDir,
        input: &Path,
        object: &Object,
    ) -> Result<(), Failure> {
        let name = processor.name();
        let out = job.path().join(name);
        tokio::fs::create_dir(&out).await.map_err(internal)?;
        let started = Instant::now();
        let outcome = processor.run(input, &out, &self.limits).await;
        self.record(name, started);
        let manifest = match outcome {
            Ok(Outcome::Done(outputs)) => {
                checked_outputs(processor, &out, outputs).map_err(|e| {
                    tracing::warn!(key = %object.key, processor = name, error = %e, "inspection failed");
                    Failure::Internal(e)
                })?
            }
            Ok(Outcome::NoResult(reason)) => Manifest::Unavailable(reason),
            Ok(Outcome::Failed(reason)) => {
                tracing::warn!(key = %object.key, processor = name, %reason, "inspection failed");
                return Err(Failure::Unavailable(reason));
            }
            Err(e) => {
                tracing::warn!(key = %object.key, processor = name, error = %format!("{e:#}"), "inspection failed");
                return Err(Failure::Internal(format!("{e:#}")));
            }
        };
        self.store(&cache_id(processor, object), &out, &manifest, job)
            .await
            .map_err(internal)
    }

    /// Publishes the processor's output dir `out` as `cache/<id>`. The
    /// manifest is written last and the whole dir renamed into place, so a
    /// reader sees either the old result or the complete new one.
    async fn store(
        &self,
        id: &str,
        out: &Path,
        manifest: &Manifest,
        job: &WorkDir,
    ) -> anyhow::Result<()> {
        tokio::fs::write(out.join(MANIFEST), serde_json::to_vec(manifest)?)
            .await
            .context("write manifest")?;
        let dst = self.cache_dir.join(id);
        // An expired result can't be renamed over: move it into the job
        // dir, which goes away with the job.
        if tokio::fs::try_exists(&dst).await.unwrap_or(false) {
            let _ = tokio::fs::rename(&dst, job.path().join(format!("stale-{id}"))).await;
        }
        tokio::fs::rename(out, &dst)
            .await
            .with_context(|| format!("store {}", dst.display()))
    }

    /// The fresh cached `output` of result `id`, if there is one.
    async fn lookup(
        &self,
        id: &str,
        processor: &str,
        output: &str,
    ) -> Option<Result<Artifact, InspectError>> {
        let dir = self.cache_dir.join(id);
        let manifest = dir.join(MANIFEST);
        let fresh_for = self.fresh_for(&manifest).await?;
        let manifest: Manifest = serde_json::from_slice(&tokio::fs::read(&manifest).await.ok()?).ok()?;
        let outputs = match manifest {
            Manifest::Unavailable(reason) => return Some(Err(InspectError::Unavailable(reason))),
            Manifest::Outputs(outputs) => outputs,
        };
        let Some(file) = outputs.into_iter().find(|f| f.name == output) else {
            return Some(Err(InspectError::Unavailable(format!(
                "{processor} produced no {output} for this file"
            ))));
        };
        // Gone in the meantime (swept): a miss.
        let body = tokio::fs::read(dir.join(&file.file)).await.ok()?;
        Some(Ok(Artifact {
            etag: format!("{id}.{output}"),
            body: body.into(),
            ext: extension(&file.file).unwrap_or("bin").to_string(),
            content_type: file.content_type,
            fresh_for,
        }))
    }

    /// Whether result `id` is cached and fresh. Blocking, but only a
    /// metadata read, so it may run under the jobs lock.
    fn is_fresh(&self, id: &str) -> bool {
        std::fs::metadata(self.cache_dir.join(id).join(MANIFEST))
            .and_then(|m| m.modified())
            .is_ok_and(|modified| self.remaining(modified).is_some())
    }

    /// Time left until the cache file at `path` expires; `None` when it is
    /// missing or expired.
    async fn fresh_for(&self, path: &Path) -> Option<Duration> {
        let modified = tokio::fs::metadata(path).await.ok()?.modified().ok()?;
        self.remaining(modified)
    }

    fn remaining(&self, modified: SystemTime) -> Option<Duration> {
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
                    tracing::debug!(removed, "inspect cache sweep");
                }
            }
        });
    }

    /// Deletes every cache entry that isn't a fresh result dir, which also
    /// covers files of older cache layouts; returns how many.
    async fn sweep(&self) -> usize {
        let mut entries = match tokio::fs::read_dir(&self.cache_dir).await {
            Ok(entries) => entries,
            Err(e) => {
                tracing::warn!(error = %e, dir = %self.cache_dir.display(), "inspect cache sweep");
                return 0;
            }
        };
        let mut removed = 0;
        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            let is_dir = entry.file_type().await.is_ok_and(|t| t.is_dir());
            if is_dir && self.fresh_for(&path.join(MANIFEST)).await.is_some() {
                continue;
            }
            let gone = if is_dir {
                tokio::fs::remove_dir_all(&path).await
            } else {
                tokio::fs::remove_file(&path).await
            };
            if gone.is_ok() {
                removed += 1;
            }
        }
        removed
    }

    fn count(&self, processor: &str, result: &'static str) {
        self.lookups.add(
            1,
            &[
                KeyValue::new("processor", processor.to_string()),
                KeyValue::new("result", result),
            ],
        );
    }

    fn record(&self, processor: &'static str, started: Instant) {
        self.duration.record(
            started.elapsed().as_secs_f64(),
            &[KeyValue::new("processor", processor)],
        );
    }
}

/// Unregisters a job and marks it finished when dropped. Removing it first
/// means a request that misses a just-stored result afterwards plans a new
/// job (which then finds the result fresh) rather than joining a finished
/// one.
struct JobDone<'a> {
    jobs: &'a JobMap,
    job_key: &'a str,
    state: &'a Arc<watch::Sender<JobState>>,
}

impl Drop for JobDone<'_> {
    fn drop(&mut self) {
        let mut jobs = self.jobs.lock().unwrap_or_else(|e| e.into_inner());
        if jobs
            .get(self.job_key)
            .is_some_and(|s| Arc::ptr_eq(s, self.state))
        {
            jobs.remove(self.job_key);
        }
        drop(jobs);
        self.state.send_modify(|s| s.finished = true);
    }
}

/// Scratch directory of one job. Removed on drop, which also covers a job
/// cancelled at shutdown.
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
            tracing::warn!(error = %e, dir = %self.0.display(), "remove inspect work dir");
        }
    }
}

/// The manifest for a processor's reported outputs, after checking that
/// each is declared and was actually written.
fn checked_outputs(
    processor: &dyn Processor,
    out: &Path,
    outputs: Vec<OutputFile>,
) -> Result<Manifest, String> {
    for f in &outputs {
        if !processor.outputs().contains(&f.name.as_str()) {
            return Err(format!("{} reported undeclared output {}", processor.name(), f.name));
        }
        if f.file.contains('/') || f.file == MANIFEST || !out.join(&f.file).is_file() {
            return Err(format!("{} reported a missing output file {}", processor.name(), f.file));
        }
    }
    Ok(Manifest::Outputs(outputs))
}

fn internal(e: impl std::fmt::Display) -> Failure {
    Failure::Internal(e.to_string())
}

/// Caps the data segment (heap and anonymous mappings) of the calling
/// process, so an oversized allocation fails inside a tool process instead
/// of pushing the container into the OOM killer. Read-only file mappings
/// don't count against it. `0` leaves the limit alone. Only calls
/// `setrlimit`, so it is safe between fork and exec.
#[cfg(unix)]
pub(crate) fn limit_memory(bytes: u64) -> std::io::Result<()> {
    if bytes == 0 {
        return Ok(());
    }
    let limit = libc::rlimit {
        rlim_cur: bytes as libc::rlim_t,
        rlim_max: bytes as libc::rlim_t,
    };
    // SAFETY: setrlimit only reads the struct passed by reference, and
    // lowering a limit needs no privileges.
    if unsafe { libc::setrlimit(libc::RLIMIT_DATA, &limit) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(not(unix))]
pub(crate) fn limit_memory(_bytes: u64) -> std::io::Result<()> {
    Ok(())
}

/// Cache key of one processor's result for one object version.
fn cache_id(processor: &dyn Processor, object: &Object) -> String {
    let digest = Sha256::digest(format!(
        "{}\0{}\0{}\0{}\0{}",
        processor.name(),
        object.key,
        object.version,
        object.size,
        processor.params()
    ));
    digest[..16].iter().map(|b| format!("{b:02x}")).collect()
}

/// Name of the downloaded copy: `input.<ext>`, keeping the upload's
/// extension as a file-type hint for the tools when it is plain (1–10
/// ASCII alphanumerics), else just `input`.
fn input_name(file_name: &str) -> String {
    match extension(file_name) {
        Some(ext) if ext.len() <= 10 && ext.bytes().all(|b| b.is_ascii_alphanumeric()) => {
            format!("input.{ext}")
        }
        _ => "input".to_string(),
    }
}

/// Final extension of a file name.
fn extension(name: &str) -> Option<&str> {
    name.rsplit_once('.')
        .map(|(_, ext)| ext)
        .filter(|ext| !ext.is_empty())
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
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Serves one fixed file and counts downloads.
    struct FakeSource {
        body: &'static [u8],
        delay: Duration,
        downloads: AtomicUsize,
    }

    impl FakeSource {
        fn new(delay: Duration) -> Arc<Self> {
            Arc::new(Self {
                body: b"raw file",
                delay,
                downloads: AtomicUsize::new(0),
            })
        }

        fn downloads(&self) -> usize {
            self.downloads.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl Source for FakeSource {
        async fn head(&self, key: &str) -> anyhow::Result<Option<HeadInfo>> {
            Ok((key != "missing").then(|| HeadInfo {
                size: self.body.len() as u64,
                etag: Some("etag1".into()),
                last_modified: None,
            }))
        }

        async fn download(&self, _key: &str, path: &Path, _len: u64) -> Result<u64, S3Error> {
            self.downloads.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(self.delay).await;
            tokio::fs::write(path, self.body)
                .await
                .map_err(|e| S3Error::Other(e.into()))?;
            Ok(self.body.len() as u64)
        }
    }

    #[derive(Clone, Copy)]
    enum Behavior {
        Done,
        Failed,
        Panic,
    }

    /// Writes `produce` (output name, file, content type) with its own name
    /// as content, after `delay`.
    struct FakeProcessor {
        name: &'static str,
        outputs: &'static [&'static str],
        produce: &'static [(&'static str, &'static str, &'static str)],
        delay: Duration,
        behavior: Behavior,
        applies: bool,
        runs: AtomicUsize,
    }

    impl FakeProcessor {
        fn new(name: &'static str, output: &'static str) -> Self {
            Self {
                name,
                outputs: Box::leak(Box::new([output])),
                produce: Box::leak(Box::new([(output, "out.txt", "text/plain")])),
                delay: Duration::ZERO,
                behavior: Behavior::Done,
                applies: true,
                runs: AtomicUsize::new(0),
            }
        }

        fn runs(&self) -> usize {
            self.runs.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl Processor for FakeProcessor {
        fn name(&self) -> &'static str {
            self.name
        }
        fn outputs(&self) -> &'static [&'static str] {
            self.outputs
        }
        fn check(&self, _file_name: &str) -> Result<(), String> {
            if self.applies {
                Ok(())
            } else {
                Err(format!("{} does not apply", self.name))
            }
        }
        fn params(&self) -> String {
            "v1".into()
        }
        async fn run(&self, _input: &Path, out: &Path, _limits: &Limits) -> anyhow::Result<Outcome> {
            self.runs.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(self.delay).await;
            match self.behavior {
                Behavior::Panic => panic!("{} panicked", self.name),
                Behavior::Failed => Ok(Outcome::Failed(format!("{} broke", self.name))),
                Behavior::Done => {
                    let mut outputs = Vec::new();
                    for (output, file, content_type) in self.produce {
                        std::fs::write(out.join(file), format!("{}:{output}", self.name))?;
                        outputs.push(OutputFile::new(output, file, content_type));
                    }
                    Ok(Outcome::Done(outputs))
                }
            }
        }
    }

    fn inspector(
        dir: &Path,
        source: Arc<FakeSource>,
        processors: Vec<Arc<FakeProcessor>>,
        ttl: Duration,
    ) -> Arc<Inspector> {
        let meter = opentelemetry::global::meter("test");
        let processors = processors
            .into_iter()
            .map(|p| p as Arc<dyn Processor>)
            .collect();
        Arc::new(
            Inspector::with_processors(
                dir,
                source,
                processors,
                ttl,
                u64::MAX,
                Limits {
                    timeout: Duration::from_secs(5),
                    memory: 0,
                },
                1,
                meter.u64_counter("test").build(),
                meter.f64_histogram("test").build(),
            )
            .unwrap(),
        )
    }

    /// Waits until no job is running.
    async fn settle(i: &Inspector) {
        for _ in 0..500 {
            if i.jobs.lock().unwrap().is_empty() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("jobs did not finish");
    }

    const HOUR: Duration = Duration::from_secs(3600);

    #[tokio::test]
    async fn one_download_fills_every_processor() {
        let dir = tempfile::tempdir().unwrap();
        let source = FakeSource::new(Duration::ZERO);
        let a = Arc::new(FakeProcessor::new("a", "x"));
        let b = Arc::new(FakeProcessor::new("b", "y"));
        let i = inspector(dir.path(), source.clone(), vec![a.clone(), b.clone()], HOUR);

        let got = i.get("a", "x", "k", "f.nef").await.unwrap();
        assert_eq!(&got.body[..], b"a:x");
        assert_eq!(got.content_type, "text/plain");
        settle(&i).await;
        let got = i.get("b", "y", "k", "f.nef").await.unwrap();
        assert_eq!(&got.body[..], b"b:y");
        assert_eq!((source.downloads(), a.runs(), b.runs()), (1, 1, 1));
    }

    #[tokio::test]
    async fn concurrent_requests_share_a_download() {
        let dir = tempfile::tempdir().unwrap();
        let source = FakeSource::new(Duration::from_millis(200));
        let a = Arc::new(FakeProcessor::new("a", "x"));
        let b = Arc::new(FakeProcessor::new("b", "y"));
        let i = inspector(dir.path(), source.clone(), vec![a.clone(), b.clone()], HOUR);

        let (x, y) = tokio::join!(i.get("a", "x", "k", "f"), i.get("b", "y", "k", "f"));
        assert_eq!(&x.unwrap().body[..], b"a:x");
        assert_eq!(&y.unwrap().body[..], b"b:y");
        assert_eq!((source.downloads(), a.runs(), b.runs()), (1, 1, 1));
    }

    #[tokio::test]
    async fn requester_does_not_wait_for_later_processors() {
        let dir = tempfile::tempdir().unwrap();
        let source = FakeSource::new(Duration::ZERO);
        let a = Arc::new(FakeProcessor::new("a", "x"));
        let mut slow = FakeProcessor::new("b", "y");
        slow.delay = Duration::from_millis(1500);
        let b = Arc::new(slow);
        let i = inspector(dir.path(), source.clone(), vec![a, b.clone()], HOUR);

        let started = Instant::now();
        i.get("a", "x", "k", "f").await.unwrap();
        assert!(started.elapsed() < Duration::from_millis(1000), "{:?}", started.elapsed());
        settle(&i).await;
        assert_eq!(b.runs(), 1);
        let started = Instant::now();
        i.get("b", "y", "k", "f").await.unwrap();
        assert!(started.elapsed() < Duration::from_millis(500), "cached by the job");
        assert_eq!(source.downloads(), 1);
    }

    #[tokio::test]
    async fn outputs_keep_their_own_formats() {
        let dir = tempfile::tempdir().unwrap();
        let source = FakeSource::new(Duration::ZERO);
        let mut analysis = FakeProcessor::new("analysis", "report");
        analysis.outputs = &["report", "plot", "extra"];
        analysis.produce = &[
            ("report", "report.json", "application/json"),
            ("plot", "plot.png", "image/png"),
        ];
        let analysis = Arc::new(analysis);
        let i = inspector(dir.path(), source.clone(), vec![analysis.clone()], HOUR);

        let report = i.get("analysis", "report", "k", "f").await.unwrap();
        assert_eq!((report.content_type.as_str(), report.ext.as_str()), ("application/json", "json"));
        let plot = i.get("analysis", "plot", "k", "f").await.unwrap();
        assert_eq!((plot.content_type.as_str(), plot.ext.as_str()), ("image/png", "png"));
        assert_ne!(report.etag, plot.etag);
        // Declared but not produced for this file.
        assert!(matches!(
            i.get("analysis", "extra", "k", "f").await,
            Err(InspectError::Unavailable(_))
        ));
        // Neither declared nor a known processor: rejected without work.
        assert!(matches!(i.get("analysis", "nope", "k", "f").await, Err(InspectError::NotFound)));
        assert!(matches!(i.get("other", "x", "k", "f").await, Err(InspectError::NotFound)));
        assert_eq!((source.downloads(), analysis.runs()), (1, 1));
    }

    #[tokio::test]
    async fn failed_processor_blocks_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let source = FakeSource::new(Duration::ZERO);
        let mut broken = FakeProcessor::new("a", "x");
        broken.behavior = Behavior::Failed;
        let a = Arc::new(broken);
        let b = Arc::new(FakeProcessor::new("b", "y"));
        let i = inspector(dir.path(), source.clone(), vec![a.clone(), b.clone()], HOUR);

        match i.get("a", "x", "k", "f").await {
            Err(InspectError::Unavailable(reason)) => assert_eq!(reason, "a broke"),
            _ => panic!("expected the failure"),
        }
        settle(&i).await;
        assert_eq!(&i.get("b", "y", "k", "f").await.unwrap().body[..], b"b:y");
        // Failures aren't cached: asking again runs `a` (only) again.
        assert!(i.get("a", "x", "k", "f").await.is_err());
        settle(&i).await;
        assert_eq!((source.downloads(), a.runs(), b.runs()), (2, 2, 1));
    }

    #[tokio::test]
    async fn inapplicable_processor_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let source = FakeSource::new(Duration::ZERO);
        let a = Arc::new(FakeProcessor::new("a", "x"));
        let mut not_for_this = FakeProcessor::new("b", "y");
        not_for_this.applies = false;
        let b = Arc::new(not_for_this);
        let i = inspector(dir.path(), source.clone(), vec![a, b.clone()], HOUR);

        i.get("a", "x", "k", "f").await.unwrap();
        settle(&i).await;
        match i.get("b", "y", "k", "f").await {
            Err(InspectError::Unavailable(reason)) => assert_eq!(reason, "b does not apply"),
            _ => panic!("expected the check to reject"),
        }
        assert_eq!((source.downloads(), b.runs()), (1, 0));
    }

    #[tokio::test]
    async fn panicking_processor_does_not_hang_requests() {
        let dir = tempfile::tempdir().unwrap();
        let source = FakeSource::new(Duration::ZERO);
        let mut bomb = FakeProcessor::new("a", "x");
        bomb.behavior = Behavior::Panic;
        let i = inspector(dir.path(), source, vec![Arc::new(bomb)], HOUR);

        let result = tokio::time::timeout(Duration::from_secs(5), i.get("a", "x", "k", "f"))
            .await
            .expect("request hung");
        assert!(matches!(result, Err(InspectError::Other(_))));
        assert!(i.jobs.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn missing_object_is_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let source = FakeSource::new(Duration::ZERO);
        let i = inspector(dir.path(), source.clone(), vec![Arc::new(FakeProcessor::new("a", "x"))], HOUR);
        assert!(matches!(i.get("a", "x", "missing", "f").await, Err(InspectError::NotFound)));
        assert_eq!(source.downloads(), 0);
    }

    /// Sets a file's mtime `age` into the past.
    fn age(path: &Path, age: Duration) {
        let file = std::fs::File::options().write(true).open(path).unwrap();
        file.set_modified(SystemTime::now() - age).unwrap();
    }

    fn write_result(i: &Inspector, id: &str, manifest: &Manifest, files: &[(&str, &[u8])]) {
        let dir = i.cache_dir.join(id);
        std::fs::create_dir(&dir).unwrap();
        for (file, body) in files {
            std::fs::write(dir.join(file), body).unwrap();
        }
        std::fs::write(dir.join(MANIFEST), serde_json::to_vec(manifest).unwrap()).unwrap();
    }

    #[tokio::test]
    async fn lookup_serves_fresh_results_only() {
        let dir = tempfile::tempdir().unwrap();
        let i = inspector(dir.path(), FakeSource::new(Duration::ZERO), vec![], HOUR);

        let outputs = Manifest::Outputs(vec![OutputFile::new("image", "image.jpg", "image/jpeg")]);
        write_result(&i, "fresh", &outputs, &[("image.jpg", b"jpeg")]);
        let hit = i.lookup("fresh", "preview", "image").await.unwrap().unwrap();
        assert_eq!((&hit.body[..], hit.ext.as_str()), (&b"jpeg"[..], "jpg"));
        assert!(hit.fresh_for > Duration::from_secs(3500));

        let reason = Manifest::Unavailable("no embedded preview".into());
        write_result(&i, "reason", &reason, &[]);
        match i.lookup("reason", "preview", "image").await {
            Some(Err(InspectError::Unavailable(m))) => assert_eq!(m, "no embedded preview"),
            _ => panic!("expected cached reason"),
        }

        write_result(&i, "stale", &outputs, &[("image.jpg", b"jpeg")]);
        age(&i.cache_dir.join("stale").join(MANIFEST), Duration::from_secs(3601));
        assert!(i.lookup("stale", "preview", "image").await.is_none());
        assert!(!i.is_fresh("stale"));
        assert!(i.is_fresh("fresh"));
        assert!(i.lookup("missing", "preview", "image").await.is_none());
    }

    #[tokio::test]
    async fn sweep_keeps_only_fresh_result_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let i = inspector(dir.path(), FakeSource::new(Duration::ZERO), vec![], Duration::from_secs(60));
        let reason = Manifest::Unavailable("none".into());
        write_result(&i, "fresh", &reason, &[]);
        write_result(&i, "old", &reason, &[]);
        age(&i.cache_dir.join("old").join(MANIFEST), Duration::from_secs(61));
        // A dir without manifest, and a flat file of the previous layout.
        std::fs::create_dir(i.cache_dir.join("broken")).unwrap();
        std::fs::write(i.cache_dir.join("0123.jpg"), b"x").unwrap();

        assert_eq!(i.sweep().await, 3);
        let left: Vec<_> = std::fs::read_dir(&i.cache_dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(left, ["fresh"]);
    }

    #[test]
    fn cache_id_tracks_processor_object_version_and_params() {
        let a = FakeProcessor::new("a", "x");
        let b = FakeProcessor::new("b", "x");
        let object = |version: &str| Object {
            key: "pending/u/raw/a.nef".into(),
            version: version.into(),
            size: 10,
            file_name: "a.nef".into(),
        };
        let id = cache_id(&a, &object("etag1"));
        assert_eq!(id.len(), 32);
        assert_eq!(id, cache_id(&a, &object("etag1")));
        assert_ne!(id, cache_id(&b, &object("etag1")));
        assert_ne!(id, cache_id(&a, &object("etag2")));
    }

    #[test]
    fn input_name_keeps_plain_extensions_only() {
        assert_eq!(input_name("DSC_0001.NEF"), "input.NEF");
        assert_eq!(input_name("clip.mp4"), "input.mp4");
        assert_eq!(input_name("noext"), "input");
        assert_eq!(input_name("odd.ex-t"), "input");
        assert_eq!(input_name("long.abcdefghijk"), "input");
    }

    #[test]
    fn manifest_json_shape() {
        let outputs = Manifest::Outputs(vec![OutputFile::new("image", "image.jpg", "image/jpeg")]);
        assert_eq!(
            serde_json::to_string(&outputs).unwrap(),
            r#"{"outputs":[{"name":"image","file":"image.jpg","content_type":"image/jpeg"}]}"#
        );
        let reason = Manifest::Unavailable("none".into());
        assert_eq!(serde_json::to_string(&reason).unwrap(), r#"{"unavailable":"none"}"#);
    }
}
