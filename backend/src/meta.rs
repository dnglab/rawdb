//! `rawdb-meta.toml` schema + parser.
//!
//! The schema is intentionally hand-authorable:
//! - **No `set.id`** — identity is `(maker, model)` derived from the S3 prefix.
//! - **No per-file `size`** — populated from S3 listing at cache time.
//! - All non-`maker`/`model` fields are optional with sensible defaults.
//! - Unknown fields are ignored silently (forward-compatible).

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const DEFAULT_LICENSE: &str = "CC0 1.0";

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RawdbMeta {
    pub set: SetMeta,
    #[serde(default, rename = "files")]
    pub files: Vec<FileMeta>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SetMeta {
    pub maker: String,
    pub model: String,
    #[serde(default = "default_license")]
    pub license: String,
    #[serde(default)]
    pub uploaded_by: Option<String>,
    #[serde(default)]
    pub uploaded_at: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(default)]
    pub notes: Option<String>,
    /// `true` for non-camera sample sets (special encodings, error-trigger
    /// files, software-generated samples). Hidden from browsing/search by
    /// default. Settable only by reviewers, never by uploaders.
    #[serde(default)]
    pub special: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FileMeta {
    /// Relative path inside the model directory, must start with a category folder.
    pub path: String,
    /// Lowercase hex SHA-256 of the file content, computed by the uploader.
    /// Advisory: surfaced in the UI for fingerprinting and verified on
    /// demand from the reviewer page; not enforced on every read.
    #[serde(default)]
    pub sha256: Option<String>,
    #[serde(default)]
    pub license: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
}

#[derive(Debug, Error)]
pub enum MetaError {
    #[error("TOML parse error: {0}")]
    Parse(#[from] toml::de::Error),

    #[error("maker must be non-empty")]
    EmptyMaker,

    #[error("model must be non-empty")]
    EmptyModel,

    #[error("file path {0:?} is invalid: {1}")]
    BadPath(String, &'static str),

    #[error("duplicate file path {0:?}")]
    DuplicatePath(String),
}

impl FileMeta {
    /// First segment of `path` — the category subfolder (e.g. `raw_modes`).
    pub fn category(&self) -> &str {
        self.path.split('/').next().unwrap_or("")
    }

    /// File name (last path segment).
    pub fn name(&self) -> &str {
        self.path.rsplit('/').next().unwrap_or("")
    }

    /// Lowercase extension, without the leading dot.
    pub fn extension(&self) -> Option<String> {
        let name = self.name();
        let dot = name.rfind('.')?;
        if dot + 1 >= name.len() {
            return None;
        }
        Some(name[dot + 1..].to_ascii_lowercase())
    }
}

fn default_license() -> String {
    DEFAULT_LICENSE.to_string()
}

/// Parse and validate a `rawdb-meta.toml` payload.
pub fn parse(input: &str) -> Result<RawdbMeta, MetaError> {
    let meta: RawdbMeta = toml::from_str(input)?;
    validate(&meta)?;
    Ok(meta)
}

pub fn validate(meta: &RawdbMeta) -> Result<(), MetaError> {
    if meta.set.maker.trim().is_empty() {
        return Err(MetaError::EmptyMaker);
    }
    if meta.set.model.trim().is_empty() {
        return Err(MetaError::EmptyModel);
    }
    let mut seen = BTreeSet::new();
    for f in &meta.files {
        validate_path(&f.path)?;
        if !seen.insert(f.path.clone()) {
            return Err(MetaError::DuplicatePath(f.path.clone()));
        }
    }
    Ok(())
}

fn validate_path(path: &str) -> Result<(), MetaError> {
    if path.is_empty() {
        return Err(MetaError::BadPath(path.into(), "empty"));
    }
    if path.starts_with('/') {
        return Err(MetaError::BadPath(path.into(), "must be relative"));
    }
    if !path.contains('/') {
        return Err(MetaError::BadPath(
            path.into(),
            "must include a category folder before the file name",
        ));
    }
    if path.split('/').any(|seg| seg == "." || seg == ".." || seg.is_empty()) {
        return Err(MetaError::BadPath(path.into(), "contains invalid segment"));
    }
    Ok(())
}

/// Serialize a `RawdbMeta` back to canonical `rawdb-meta.toml` text.
/// Used when the reviewer edits a pending upload. Optional/empty fields
/// are omitted so the output stays clean and hand-editable.
pub fn to_toml(meta: &RawdbMeta) -> String {
    fn q(s: &str) -> String {
        let e = s
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n")
            .replace('\t', "\\t");
        format!("\"{e}\"")
    }
    fn arr(items: &[String]) -> String {
        let inner: Vec<String> = items.iter().map(|s| q(s)).collect();
        format!("[{}]", inner.join(", "))
    }

    let mut out = String::from("[set]\n");
    out.push_str(&format!("maker = {}\n", q(&meta.set.maker)));
    out.push_str(&format!("model = {}\n", q(&meta.set.model)));
    out.push_str(&format!("license = {}\n", q(&meta.set.license)));
    if let Some(by) = meta.set.uploaded_by.as_deref().filter(|s| !s.is_empty()) {
        out.push_str(&format!("uploaded_by = {}\n", q(by)));
    }
    if let Some(at) = meta.set.uploaded_at {
        out.push_str(&format!("uploaded_at = {}\n", q(&at.to_rfc3339())));
    }
    if let Some(n) = meta.set.notes.as_deref().filter(|s| !s.is_empty()) {
        out.push_str(&format!("notes = {}\n", q(n)));
    }
    if meta.set.special {
        out.push_str("special = true\n");
    }

    for f in &meta.files {
        out.push_str("\n[[files]]\n");
        out.push_str(&format!("path = {}\n", q(&f.path)));
        if let Some(h) = f.sha256.as_deref().filter(|s| !s.is_empty()) {
            out.push_str(&format!("sha256 = {}\n", q(h)));
        }
        if let Some(l) = f.license.as_deref().filter(|s| !s.is_empty()) {
            out.push_str(&format!("license = {}\n", q(l)));
        }
        if !f.tags.is_empty() {
            out.push_str(&format!("tags = {}\n", arr(&f.tags)));
        }
        if let Some(n) = f.notes.as_deref().filter(|s| !s.is_empty()) {
            out.push_str(&format!("notes = {}\n", q(n)));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hand_authored_minimal() {
        let toml = r#"
            [set]
            maker = "Canon"
            model = "EOS R5"

            [[files]]
            path = "raw_modes/IMG_0001.cr3"
        "#;
        let m = parse(toml).unwrap();
        assert_eq!(m.set.maker, "Canon");
        assert_eq!(m.set.model, "EOS R5");
        assert_eq!(m.set.license, DEFAULT_LICENSE);
        assert_eq!(m.files.len(), 1);
        assert_eq!(m.files[0].category(), "raw_modes");
        assert_eq!(m.files[0].name(), "IMG_0001.cr3");
        assert_eq!(m.files[0].extension().as_deref(), Some("cr3"));
    }

    #[test]
    fn full_example_with_all_fields() {
        let toml = r#"
            [set]
            maker = "Canon"
            model = "EOS R5"
            license = "CC-BY-4.0"
            uploaded_by = "github:cytrinox"
            uploaded_at = "2026-05-14T12:00:00Z"
            notes = "Example"
            tags = ["dual-pixel"]

            [[files]]
            path = "raw_modes/IMG_0001.cr3"
            license = "CC0 1.0"
            tags = ["high-iso"]
            notes = "low-light"
        "#;
        let m = parse(toml).unwrap();
        assert_eq!(m.set.license, "CC-BY-4.0");
        assert!(m.set.uploaded_at.is_some());
        assert_eq!(m.files[0].tags, vec!["high-iso"]);
    }

    #[test]
    fn unknown_fields_are_ignored() {
        let toml = r#"
            [set]
            maker = "X"
            model = "Y"
            future_field = "ignored"

            [[files]]
            path = "raw_modes/a.dng"
            mystery = 42
        "#;
        parse(toml).unwrap();
    }

    #[test]
    fn rejects_missing_category_folder() {
        let toml = r#"
            [set]
            maker = "X"
            model = "Y"

            [[files]]
            path = "IMG_0001.cr3"
        "#;
        let err = parse(toml).unwrap_err();
        assert!(matches!(err, MetaError::BadPath(_, _)));
    }

    #[test]
    fn rejects_absolute_path() {
        let toml = r#"
            [set]
            maker = "X"
            model = "Y"

            [[files]]
            path = "/raw_modes/a.cr3"
        "#;
        assert!(matches!(parse(toml).unwrap_err(), MetaError::BadPath(..)));
    }

    #[test]
    fn rejects_dotdot() {
        let toml = r#"
            [set]
            maker = "X"
            model = "Y"

            [[files]]
            path = "raw_modes/../escape.cr3"
        "#;
        assert!(matches!(parse(toml).unwrap_err(), MetaError::BadPath(..)));
    }

    #[test]
    fn rejects_duplicate_paths() {
        let toml = r#"
            [set]
            maker = "X"
            model = "Y"

            [[files]]
            path = "raw_modes/a.cr3"

            [[files]]
            path = "raw_modes/a.cr3"
        "#;
        assert!(matches!(parse(toml).unwrap_err(), MetaError::DuplicatePath(_)));
    }

    #[test]
    fn rejects_empty_maker() {
        let toml = r#"
            [set]
            maker = ""
            model = "Y"
        "#;
        assert!(matches!(parse(toml).unwrap_err(), MetaError::EmptyMaker));
    }

    #[test]
    fn sha256_round_trips_through_toml() {
        let toml = r#"
            [set]
            maker = "Canon"
            model = "EOS R5"

            [[files]]
            path = "raw_modes/IMG_0001.cr3"
            sha256 = "3789b1ba5d880613104637c40a06619cd9f3b54b0cf62d2d95b9f9e885edcf6e"
        "#;
        let m = parse(toml).unwrap();
        assert_eq!(
            m.files[0].sha256.as_deref(),
            Some("3789b1ba5d880613104637c40a06619cd9f3b54b0cf62d2d95b9f9e885edcf6e")
        );
        // Re-serialize and re-parse: the hash survives.
        let again = parse(&to_toml(&m)).unwrap();
        assert_eq!(again.files[0].sha256, m.files[0].sha256);
    }

    #[test]
    fn extension_is_lowercased() {
        let f = FileMeta {
            path: "raw_modes/IMG.CR3".into(),
            sha256: None,
            license: None,
            notes: None,
            tags: vec![],
        };
        assert_eq!(f.extension().as_deref(), Some("cr3"));
    }
}
