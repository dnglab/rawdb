//! Preview worker: the rawdb binary re-executed with [`ARG`] as its first
//! argument. Extracts the embedded preview (or thumbnail) of one RAW file
//! with rawler and writes it as a JPEG.
//!
//! Uploads are anonymous, so rawler parses untrusted input here — often
//! from cameras it doesn't support yet. Running it in its own process
//! under a memory limit keeps a panic, abort, runaway allocation or hang
//! from taking the server down; the server side ([`super::Previews`])
//! kills the worker when it runs into the timeout.
//!
//! Usage: `rawdb __preview-worker <input> <output> <max-dim> <memory-bytes>`

use std::ffi::{OsStr, OsString};
use std::path::Path;

use image::codecs::jpeg::JpegEncoder;
use image::metadata::Orientation;
use rawler::decoders::RawDecodeParams;
use rawler::rawsource::RawSource;

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

fn run(args: &[OsString]) -> i32 {
    let [input, output, max_dim, memory] = args else {
        eprintln!("usage: rawdb {ARG} <input> <output> <max-dim> <memory-bytes>");
        return EXIT_USAGE;
    };
    let (Some(max_dim), Some(memory)) = (parse::<u32>(max_dim), parse::<u64>(memory)) else {
        eprintln!("invalid <max-dim> or <memory-bytes>");
        return EXIT_USAGE;
    };
    if let Err(e) = limit_memory(memory) {
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

/// Caps the data segment (heap and anonymous mappings), so an oversized
/// allocation fails inside the worker instead of pushing the container
/// into the OOM killer. The read-only mapping of the input file doesn't
/// count against it. `0` leaves the limit alone.
#[cfg(unix)]
fn limit_memory(bytes: u64) -> std::io::Result<()> {
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
fn limit_memory(_bytes: u64) -> std::io::Result<()> {
    Ok(())
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
