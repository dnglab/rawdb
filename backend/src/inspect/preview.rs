//! `preview/image`: the embedded preview (or thumbnail) of a RAW file as
//! JPEG.
//!
//! [`PreviewProcessor`] runs a worker: the rawdb binary re-executed with
//! [`ARG`] as its first argument, which extracts the preview with rawler.
//! Uploads are anonymous, so rawler parses untrusted input here — often
//! from cameras it doesn't support yet. Running it in its own process
//! under a memory limit keeps a panic, abort, runaway allocation or hang
//! from taking the server down; the processor kills the worker when it
//! runs into the timeout.
//!
//! Worker usage: `rawdb __preview-worker <input> <output> <max-dim> <memory-bytes>`

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::Context;
use async_trait::async_trait;
use image::codecs::jpeg::JpegEncoder;
use image::metadata::Orientation;
use rawler::decoders::RawDecodeParams;
use rawler::rawsource::RawSource;

use super::{extension, stderr_text, Limits, Outcome, OutputFile, Processor};

/// Processor name.
pub const NAME: &str = "preview";
/// Its only output: the preview JPEG.
const OUTPUT: &str = "image";
const OUTPUT_FILE: &str = "image.jpg";

/// Bump when the rendering changes (sizing, quality, orientation), so
/// previews cached by an older build aren't served.
const RENDER_VERSION: u32 = 1;

/// First command-line argument that selects worker mode.
pub const ARG: &str = "__preview-worker";

/// Exit code for a definitive answer from the decoder that the file has
/// no usable preview; the reason is on stderr. Any other non-zero exit
/// is a failure of the worker itself.
pub const EXIT_NO_PREVIEW: i32 = 2;

/// Exit code for bad arguments or local I/O errors.
const EXIT_USAGE: i32 = 64;

const JPEG_QUALITY: u8 = 85;

/// Runs the worker if the process was started as one and returns its exit
/// code; `None` for a regular server start.
pub fn run_if_requested() -> Option<i32> {
    let mut args = std::env::args_os().skip(1);
    if args.next().as_deref() != Some(OsStr::new(ARG)) {
        return None;
    }
    Some(run(&args.collect::<Vec<_>>()))
}

/// Renders previews by running the worker on a downloaded file.
pub(super) struct PreviewProcessor {
    /// This binary; workers are started by re-executing it.
    exe: PathBuf,
    /// Longest edge of a served preview; `0` keeps the embedded size.
    max_dim: u32,
}

impl PreviewProcessor {
    pub(super) fn new(exe: PathBuf, max_dim: u32) -> Self {
        Self { exe, max_dim }
    }
}

#[async_trait]
impl Processor for PreviewProcessor {
    fn name(&self) -> &'static str {
        NAME
    }

    fn outputs(&self) -> &'static [&'static str] {
        &[OUTPUT]
    }

    fn check(&self, file_name: &str) -> Result<(), String> {
        if raw_extension(file_name).is_some() {
            return Ok(());
        }
        Err(match extension(file_name) {
            Some(ext) => format!(".{ext} is not a RAW format rawler reads"),
            None => "file has no extension; not a RAW format rawler reads".into(),
        })
    }

    fn params(&self) -> String {
        format!("{}\0{RENDER_VERSION}", self.max_dim)
    }

    async fn run(&self, input: &Path, out: &Path, limits: &Limits) -> anyhow::Result<Outcome> {
        let child = tokio::process::Command::new(&self.exe)
            .arg(ARG)
            .arg(input)
            .arg(out.join(OUTPUT_FILE))
            .arg(self.max_dim.to_string())
            .arg(limits.memory.to_string())
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
        let Ok(done) = tokio::time::timeout(limits.timeout, child.wait_with_output()).await else {
            return Ok(Outcome::Failed(format!(
                "decoder timed out after {}s",
                limits.timeout.as_secs()
            )));
        };
        let done = done.context("wait for preview worker")?;
        let stderr = stderr_text(&done.stderr);
        Ok(match done.status.code() {
            Some(0) => Outcome::Done(vec![OutputFile::new(OUTPUT, OUTPUT_FILE, "image/jpeg")]),
            Some(EXIT_NO_PREVIEW) => Outcome::NoResult(stderr),
            _ if stderr.contains("memory allocation of") => Outcome::Failed(format!(
                "preview needs more memory than the worker may use \
                 (RAWDB_INSPECT_MEMORY_BYTES={})",
                limits.memory
            )),
            _ => Outcome::Failed(format!("decoder crashed ({}): {stderr}", done.status)),
        })
    }
}

/// The extension of `name` as listed by rawler (upper case), if rawler
/// reads that format.
fn raw_extension(name: &str) -> Option<&'static str> {
    let ext = extension(name)?;
    rawler::decoders::supported_extensions()
        .iter()
        .copied()
        .find(|known| known.eq_ignore_ascii_case(ext))
}

fn run(args: &[OsString]) -> i32 {
    let [input, output, max_dim, memory] = args else {
        eprintln!("usage: rawdb {ARG} <input> <output> <max-dim> <memory-bytes>");
        return EXIT_USAGE;
    };
    let (Some(max_dim), Some(memory)) = (parse::<u32>(max_dim), parse::<u64>(memory)) else {
        eprintln!("invalid <max-dim> or <memory-bytes>");
        return EXIT_USAGE;
    };
    if let Err(e) = super::limit_memory(memory) {
        eprintln!("setrlimit(RLIMIT_DATA): {e}");
        return EXIT_USAGE;
    }
    let jpeg = match render(Path::new(input), max_dim) {
        Ok(jpeg) => jpeg,
        Err(reason) => {
            eprintln!("{reason}");
            return EXIT_NO_PREVIEW;
        }
    };
    if let Err(e) = std::fs::write(output, jpeg) {
        eprintln!("write {}: {e}", Path::new(output).display());
        return EXIT_USAGE;
    }
    0
}

fn parse<T: std::str::FromStr>(arg: &OsStr) -> Option<T> {
    arg.to_str()?.parse().ok()
}

/// The embedded preview of the RAW file at `path` (else its thumbnail) as
/// JPEG, downscaled to `max_dim` on the long edge (`0` keeps the size)
/// and turned upright by the file's EXIF orientation.
///
/// Never develops the raw data itself (`full_image()`, `raw_to_srgb()`):
/// that is slow, needs far more memory and fails for cameras rawler
/// doesn't support yet — which are the ones reviewers care about here.
pub fn render(path: &Path, max_dim: u32) -> Result<Vec<u8>, String> {
    let source = RawSource::new(path).map_err(|e| format!("cannot read file: {e}"))?;
    let decoder = rawler::get_decoder(&source).map_err(|e| e.to_string())?;
    let params = RawDecodeParams::default();

    let mut image = match decoder.preview_image(&source, &params) {
        Ok(Some(image)) => image,
        preview => match decoder.thumbnail_image(&source, &params) {
            Ok(Some(image)) => image,
            thumbnail => {
                return Err(match (preview, thumbnail) {
                    (Err(e), _) => format!("preview: {e}"),
                    (_, Err(e)) => format!("thumbnail: {e}"),
                    _ => "no embedded preview or thumbnail".to_string(),
                });
            }
        },
    };

    if max_dim > 0 && (image.width() > max_dim || image.height() > max_dim) {
        image = image.thumbnail(max_dim, max_dim);
    }
    // Embedded previews are stored as the sensor sees them, and viewers
    // rotate them by the RAW's EXIF orientation, which the re-encoded JPEG
    // doesn't carry. Best effort: unknown cameras may lack the metadata.
    let orientation = decoder
        .raw_metadata(&source, &params)
        .ok()
        .and_then(|m| m.exif.orientation)
        .and_then(|o| u8::try_from(o).ok())
        .and_then(Orientation::from_exif);
    if let Some(orientation) = orientation {
        image.apply_orientation(orientation);
    }

    let mut jpeg = Vec::new();
    JpegEncoder::new_with_quality(&mut jpeg, JPEG_QUALITY)
        .encode_image(&image.into_rgb8())
        .map_err(|e| format!("encode JPEG: {e}"))?;
    Ok(jpeg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_extension_is_case_insensitive() {
        assert_eq!(raw_extension("DSC_0001.nef"), Some("NEF"));
        assert_eq!(raw_extension("IMG_0001.CR3"), Some("CR3"));
        assert_eq!(raw_extension("clip.mp4"), None);
        assert_eq!(raw_extension("photo.heif"), None);
        assert_eq!(raw_extension("noext"), None);
        assert_eq!(raw_extension("trailingdot."), None);
    }

    #[test]
    fn check_and_params() {
        let p = PreviewProcessor::new(PathBuf::new(), 2048);
        assert!(p.check("a.NEF").is_ok());
        assert_eq!(p.check("clip.mp4").unwrap_err(), ".mp4 is not a RAW format rawler reads");
        assert_ne!(p.params(), PreviewProcessor::new(PathBuf::new(), 1024).params());
    }

    #[test]
    fn rejects_non_raw_content() {
        // Same shape as the seed upload's placeholder `DSC_0001.nef`.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("DSC_0001.nef");
        std::fs::write(&path, b"placeholder\n").unwrap();
        assert!(render(&path, 2048).is_err());
    }

    #[test]
    fn rejects_empty_and_missing_files() {
        let dir = tempfile::tempdir().unwrap();
        let empty = dir.path().join("empty.cr3");
        std::fs::write(&empty, b"").unwrap();
        assert!(render(&empty, 2048).is_err());
        assert!(render(&dir.path().join("missing.nef"), 2048).is_err());
    }

    #[test]
    fn bad_arguments_are_a_usage_error() {
        assert_eq!(run(&[]), EXIT_USAGE);
        let args: Vec<OsString> = ["in", "out", "big", "0"].map(OsString::from).into();
        assert_eq!(run(&args), EXIT_USAGE);
    }

    /// Needs a real RAW file:
    /// `RAWDB_TEST_RAW=/path/to/sample.cr3 cargo test -- --ignored`
    #[test]
    #[ignore]
    fn renders_real_file() {
        let path = std::env::var_os("RAWDB_TEST_RAW").expect("set RAWDB_TEST_RAW");
        let jpeg = render(Path::new(&path), 2048).unwrap();
        assert_eq!(&jpeg[..2], &[0xFF, 0xD8], "not a JPEG");
        let img = image::load_from_memory(&jpeg).unwrap();
        assert!(img.width().max(img.height()) <= 2048);
    }
}
