//! `exiftool/listing`: every tag of a file, as listed by ExifTool.
//!
//! ExifTool is a Perl program, so it already runs in a process of its own;
//! it gets the same memory cap and timeout as the preview worker and none
//! of the server's environment.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use anyhow::Context;
use async_trait::async_trait;
use tokio::io::{AsyncRead, AsyncReadExt};

use super::{stderr_text, Limits, Outcome, OutputFile, Processor, MAX_STDERR};

/// Processor name.
pub const NAME: &str = "exiftool";
/// Its only output: the listing as UTF-8 text.
const OUTPUT: &str = "listing";
const OUTPUT_FILE: &str = "listing.txt";

/// Options before the file name:
/// - `-a`: keep duplicate tags (RAW files repeat tags across IFDs)
/// - `-u`: also unknown tags, e.g. maker-note tags ExifTool can't name yet
/// - `-G1`: prefix each tag with its group (`[ExifIFD]`, `[Canon]`, …)
/// - `-s`: tag names (`ExposureTime`) instead of descriptions
/// - `--System:all`: drop the file-system tags, which would describe the
///   server's temp copy (its name, path and download dates)
pub const ARGS: &[&str] = &["-a", "-u", "-G1", "-s", "--System:all"];

/// Listing bytes kept per file; listings are typically 10–200 KB.
const MAX_OUTPUT: usize = 4 * 1024 * 1024;

/// How long `exiftool -ver` may take at startup.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// Lists the tags of a downloaded file with exiftool.
pub(super) struct ExiftoolProcessor {
    /// `None` when exiftool can't be run on this host.
    tool: Option<Exiftool>,
}

impl ExiftoolProcessor {
    pub(super) fn new(tool: Option<Exiftool>) -> Self {
        Self { tool }
    }
}

#[async_trait]
impl Processor for ExiftoolProcessor {
    fn name(&self) -> &'static str {
        NAME
    }

    fn outputs(&self) -> &'static [&'static str] {
        &[OUTPUT]
    }

    /// ExifTool reads far more than RAW formats, so only its absence rules
    /// a file out.
    fn check(&self, _file_name: &str) -> Result<(), String> {
        match self.tool {
            Some(_) => Ok(()),
            None => Err("exiftool is not available on this server".into()),
        }
    }

    fn params(&self) -> String {
        let version = self.tool.as_ref().map_or("", |t| t.version.as_str());
        format!("{version}\0{}", ARGS.join(" "))
    }

    async fn run(&self, input: &Path, out: &Path, limits: &Limits) -> anyhow::Result<Outcome> {
        let Some(tool) = &self.tool else {
            return Ok(Outcome::Failed("exiftool is not available on this server".into()));
        };
        Ok(
            match tool.run(input, &out.join(OUTPUT_FILE), limits.timeout, limits.memory).await? {
                Ok(()) => Outcome::Done(vec![OutputFile::new(
                    OUTPUT,
                    OUTPUT_FILE,
                    "text/plain; charset=utf-8",
                )]),
                Err(reason) => Outcome::Failed(reason),
            },
        )
    }
}

/// A working exiftool installation.
pub struct Exiftool {
    bin: PathBuf,
    /// Output of `exiftool -ver`. Part of the cache id, so an upgrade
    /// re-runs every file.
    pub version: String,
}

impl Exiftool {
    /// Locates `bin` (looked up in `PATH` unless it is a path) and asks it
    /// for its version. Logs and returns `None` when it can't be run.
    pub async fn probe(bin: &Path) -> Option<Self> {
        let Some(path) = resolve(bin) else {
            tracing::warn!(bin = %bin.display(), "exiftool not found; exiftool output disabled");
            return None;
        };
        let run = tokio::process::Command::new(&path)
            .arg("-ver")
            .env_clear()
            .env("PATH", search_path())
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output();
        match tokio::time::timeout(PROBE_TIMEOUT, run).await {
            Ok(Ok(out)) if out.status.success() => {
                let version = String::from_utf8_lossy(&out.stdout).trim().to_string();
                tracing::info!(%version, bin = %path.display(), "exiftool available");
                Some(Self { bin: path, version })
            }
            Ok(Ok(out)) => {
                tracing::warn!(
                    status = %out.status, stderr = %stderr_text(&out.stderr),
                    "exiftool -ver failed; exiftool output disabled",
                );
                None
            }
            Ok(Err(e)) => {
                tracing::warn!(
                    error = %e, bin = %path.display(),
                    "cannot run exiftool; exiftool output disabled",
                );
                None
            }
            Err(_) => {
                tracing::warn!(
                    bin = %path.display(),
                    "exiftool -ver timed out; exiftool output disabled",
                );
                None
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn fake(version: &str) -> Self {
        Self {
            bin: PathBuf::from("exiftool"),
            version: version.to_string(),
        }
    }

    /// Runs exiftool on `input` and writes the listing to `output`.
    /// `Ok(Err(reason))` when it produced none (crash, timeout, memory
    /// limit, unreadable file).
    async fn run(
        &self,
        input: &Path,
        output: &Path,
        timeout: Duration,
        memory: u64,
    ) -> anyhow::Result<Result<(), String>> {
        let mut cmd = tokio::process::Command::new(&self.bin);
        cmd.args(ARGS)
            .arg(input)
            // Uploads are untrusted: none of the server's secrets, and no
            // HOME, so no personal ~/.ExifTool_config gets loaded.
            .env_clear()
            .env("PATH", search_path())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        {
            // SAFETY: runs in the forked child before exec and only calls
            // setrlimit, which is async-signal-safe.
            unsafe {
                cmd.pre_exec(move || super::limit_memory(memory));
            }
        }
        let mut child = cmd.spawn().context("spawn exiftool")?;
        let stdout = child.stdout.take().context("exiftool stdout")?;
        let stderr = child.stderr.take().context("exiftool stderr")?;
        let collect = async {
            let (listing, errors, status) = tokio::join!(
                read_capped(stdout, MAX_OUTPUT),
                read_capped(stderr, MAX_STDERR),
                child.wait(),
            );
            anyhow::Ok((listing?, errors?.0, status?))
        };
        // On timeout `child` is dropped on return, which kills exiftool.
        let Ok(collected) = tokio::time::timeout(timeout, collect).await else {
            return Ok(Err(format!("exiftool timed out after {}s", timeout.as_secs())));
        };
        let ((listing, truncated), errors, status) = collected.context("run exiftool")?;
        if let Some(reason) = failure(&listing, &stderr_text(&errors), status, memory) {
            return Ok(Err(reason));
        }
        let mut text = to_utf8(&listing);
        if truncated {
            text.push_str(&format!(
                "[rawdb] output truncated after {} MiB\n",
                MAX_OUTPUT >> 20
            ));
        }
        tokio::fs::write(output, text)
            .await
            .with_context(|| format!("write {}", output.display()))?;
        Ok(Ok(()))
    }
}

/// Why a finished run has no usable listing, if it hasn't. ExifTool reports
/// trouble with a file as tags (`[ExifTool] Error : Unknown file type`) and
/// exits with 1, so any listing is the result; no listing means it couldn't
/// read the file at all.
fn failure(listing: &[u8], stderr: &str, status: ExitStatus, memory: u64) -> Option<String> {
    if stderr.contains("Out of memory") {
        return Some(format!(
            "exiftool needs more memory than it may use (RAWDB_INSPECT_MEMORY_BYTES={memory})"
        ));
    }
    if status.code().is_none() {
        return Some(format!("exiftool crashed ({status}): {stderr}"));
    }
    if listing.is_empty() {
        return Some(format!("exiftool failed ({status}): {stderr}"));
    }
    None
}

/// The listing as valid UTF-8. Unknown binary tags (`-u`) are printed as
/// raw bytes, so every byte that isn't part of valid UTF-8 becomes `\xNN`.
fn to_utf8(listing: &[u8]) -> String {
    let mut text = String::with_capacity(listing.len());
    for chunk in listing.utf8_chunks() {
        text.push_str(chunk.valid());
        for byte in chunk.invalid() {
            text.push_str(&format!("\\x{byte:02x}"));
        }
    }
    text
}

/// Reads `reader` to the end and keeps the first `cap` bytes; returns them
/// and whether more followed. Draining the rest keeps a chatty child from
/// blocking on a full pipe.
async fn read_capped(
    mut reader: impl AsyncRead + Unpin,
    cap: usize,
) -> std::io::Result<(Vec<u8>, bool)> {
    let mut kept = Vec::new();
    let mut more = false;
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = reader.read(&mut buf).await?;
        if n == 0 {
            return Ok((kept, more));
        }
        let room = cap - kept.len();
        kept.extend_from_slice(&buf[..n.min(room)]);
        more |= n > room;
    }
}

/// `PATH` for exiftool and for looking up a bare program name: the
/// server's own.
fn search_path() -> OsString {
    std::env::var_os("PATH").unwrap_or_else(|| "/usr/local/bin:/usr/bin:/bin".into())
}

/// `bin` itself when it is a path, else the first match in `PATH`.
fn resolve(bin: &Path) -> Option<PathBuf> {
    if bin.components().count() > 1 {
        return bin.is_file().then(|| bin.to_path_buf());
    }
    std::env::split_paths(&search_path())
        .map(|dir| dir.join(bin))
        .find(|p| p.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;

    fn exited(code: i32) -> ExitStatus {
        ExitStatus::from_raw(code << 8)
    }

    #[test]
    fn processor_needs_a_probed_exiftool() {
        let none = ExiftoolProcessor::new(None);
        assert_eq!(none.check("a.nef").unwrap_err(), "exiftool is not available on this server");
        let p = ExiftoolProcessor::new(Some(Exiftool::fake("13.59")));
        assert!(p.check("clip.mp4").is_ok(), "any file type");
        // An ExifTool upgrade changes every cache id.
        assert_ne!(p.params(), ExiftoolProcessor::new(Some(Exiftool::fake("13.58"))).params());
    }

    #[test]
    fn any_listing_is_a_result() {
        let listing = b"[ExifTool]      Error : Unknown file type\n";
        // ExifTool exits with 1 when it reports an error as a tag.
        assert_eq!(failure(listing, "", exited(1), 0), None);
        assert_eq!(failure(listing, "", exited(0), 0), None);
    }

    #[test]
    fn no_listing_or_a_crash_is_a_failure() {
        let missing = failure(b"", "Error: File not found - x", exited(1), 0).unwrap();
        assert!(missing.contains("File not found"), "{missing}");
        let killed = failure(b"partial", "", ExitStatus::from_raw(9), 0).unwrap();
        assert!(killed.contains("crashed"), "{killed}");
        let oom = failure(b"partial", "Out of memory!", exited(1), 1024).unwrap();
        assert!(oom.contains("RAWDB_INSPECT_MEMORY_BYTES=1024"), "{oom}");
    }

    #[test]
    fn invalid_utf8_bytes_are_escaped() {
        // As printed for an unknown Canon maker-note tag; `ü` stays as is.
        let raw = b"Canon_0x4037 : \x80.\xaa. M\xc3\xbcnchen\n";
        assert_eq!(to_utf8(raw), "Canon_0x4037 : \\x80.\\xaa. München\n");
    }

    #[tokio::test]
    async fn read_capped_keeps_the_head_and_flags_the_rest() {
        let data = vec![7u8; 200 * 1024];
        let (kept, more) = read_capped(&data[..], 100 * 1024).await.unwrap();
        assert_eq!(kept.len(), 100 * 1024);
        assert!(more);
        let (kept, more) = read_capped(&data[..], data.len()).await.unwrap();
        assert_eq!(kept.len(), data.len());
        assert!(!more);
    }

    /// Needs exiftool in `PATH`; with `RAWDB_TEST_RAW` set, also lists a
    /// real file: `RAWDB_TEST_RAW=/path/to/sample.cr3 cargo test -- --ignored`
    #[tokio::test]
    #[ignore]
    async fn lists_files_with_real_exiftool() {
        let exiftool = Exiftool::probe(Path::new("exiftool")).await.expect("exiftool in PATH");
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("out.txt");
        let timeout = Duration::from_secs(30);

        // Binary that matches no format (all-'Z' would pass as text).
        let garbage = dir.path().join("input.bin");
        std::fs::write(&garbage, [0x00u8, 0xa5].repeat(2048)).unwrap();
        exiftool.run(&garbage, &output, timeout, 0).await.unwrap().unwrap();
        let listing = std::fs::read_to_string(&output).unwrap();
        assert!(listing.contains("Unknown file type"), "{listing}");
        assert!(!listing.contains("[System]"), "{listing}");

        if let Some(raw) = std::env::var_os("RAWDB_TEST_RAW") {
            exiftool
                .run(Path::new(&raw), &output, timeout, 320 * 1024 * 1024)
                .await
                .unwrap()
                .unwrap();
            let listing = std::fs::read_to_string(&output).unwrap();
            assert!(listing.contains("ExifToolVersion"), "{listing}");
        }
    }
}
