//! Public upload flow. Three endpoints:
//!
//! 1. `POST /api/upload/begin` — client posts `{maker, model, files: [{path}]}`.
//!    Server mints an `upload_id` and returns either presigned PUT URLs
//!    (one per file) or stream-endpoint metadata, depending on
//!    `RAWDB_UPLOAD_MODE`.
//!
//! 2. `PUT /api/upload/stream/:maker/:model/:upload_id/*path` — streaming
//!    fallback for clients that can't or shouldn't talk directly to S3.
//!    Body is buffered up to `UPLOAD_BUFFER_LIMIT` bytes then sent to S3.
//!
//! 3. `POST /api/upload/complete` — client posts the full
//!    `rawdb-meta.toml` payload. Server validates the meta, verifies every
//!    declared file exists under the pending prefix, and writes the meta
//!    to S3. The set becomes visible in the reviewer queue on the next
//!    scanner tick.

use std::collections::HashSet;

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::StatusCode;
use axum::routing::{post, put};
use axum::{Json, Router};
use chrono::Utc;
use rand::Rng;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::config::UploadMode;
use crate::error::{AppError, AppResult};
use crate::meta;
use crate::state::AppState;

pub fn router(max_upload_bytes: u64) -> Router<AppState> {
    // The body-limit layer takes a `usize`; clamp to platform usize and
    // give a small headroom over the configured limit so the handler's
    // own check is the source of the 413 (cleaner error than axum's
    // default "length limit exceeded").
    let body_limit: usize = max_upload_bytes
        .saturating_add(1024 * 1024)
        .try_into()
        .unwrap_or(usize::MAX);
    Router::new()
        .route("/upload/begin", post(begin))
        .route(
            "/upload/stream/:upload_id/*path",
            put(stream).layer(DefaultBodyLimit::max(body_limit)),
        )
        .route("/upload/complete", post(complete))
}

// ---- request/response shapes ---------------------------------------------

#[derive(Debug, Deserialize, ToSchema)]
pub struct BeginRequest {
    pub maker: String,
    pub model: String,
    pub files: Vec<BeginFile>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct BeginFile {
    /// Path relative to the model directory, e.g. `raw_modes/IMG_0001.cr3`.
    /// Must contain at least one `/` (the category folder).
    pub path: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct BeginResponse {
    /// Server-minted identifier for this upload attempt; format
    /// `YYYYMMDDTHHMMSSZ-<8 hex>`.
    pub upload_id: String,
    /// `presigned`, `stream`, or `either` — mirrors `RAWDB_UPLOAD_MODE`.
    pub mode: String,
    /// Per-file presigned PUT URLs, present when mode is `presigned` or `either`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub urls: Option<std::collections::BTreeMap<String, String>>,
    /// Stream endpoint base (PUT relative paths under it), present when
    /// mode is `stream` or `either`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_base: Option<String>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct CompleteRequest {
    pub maker: String,
    pub model: String,
    pub upload_id: String,
    /// Full hand-authored set metadata in TOML form (one `[set]` table and
    /// one `[[files]]` entry per uploaded file).
    pub meta_toml: String,
}

// ---- handlers ------------------------------------------------------------

/// Start an upload. Server mints an `upload_id` and (per `RAWDB_UPLOAD_MODE`)
/// either presigned PUT URLs for each declared file, a streaming base URL,
/// or both.
#[utoipa::path(
    post,
    path = "/api/upload/begin",
    tag = "upload",
    request_body = BeginRequest,
    responses(
        (status = 200, body = BeginResponse),
        (status = 400, description = "Validation error (empty maker/model, no files, blocked extension, invalid path)"),
    ),
)]
pub async fn begin(
    State(state): State<AppState>,
    Json(req): Json<BeginRequest>,
) -> AppResult<Json<BeginResponse>> {
    if req.maker.trim().is_empty() || req.model.trim().is_empty() {
        return Err(AppError::BadRequest("maker and model are required".into()));
    }
    if req.files.is_empty() {
        return Err(AppError::BadRequest("at least one file required".into()));
    }
    let blocked: HashSet<&str> = state
        .config
        .blocked_extensions
        .iter()
        .map(String::as_str)
        .collect();
    for f in &req.files {
        validate_rel_path(&f.path)?;
        if let Some(ext) = blocked_extension(&f.path, &blocked) {
            return Err(AppError::BadRequest(format!(
                "file type not allowed (.{ext}): {}",
                f.path
            )));
        }
    }

    let upload_id = mint_upload_id();
    let mode = state.config.upload_mode;

    let (urls, mode_str) = match mode {
        UploadMode::Presigned | UploadMode::Either => {
            let mut map = std::collections::BTreeMap::new();
            for f in &req.files {
                let key = pending_key(&upload_id, &f.path);
                let url = state.s3.presign_put(&key).await.map_err(AppError::Other)?;
                map.insert(f.path.clone(), url);
            }
            (
                Some(map),
                if matches!(mode, UploadMode::Presigned) {
                    "presigned"
                } else {
                    "either"
                },
            )
        }
        UploadMode::Stream => (None, "stream"),
    };
    let mode_str = mode_str.to_string();

    let stream_base = matches!(mode, UploadMode::Stream | UploadMode::Either)
        .then(|| format!("/api/upload/stream/{upload_id}"));

    // One interaction per file in this upload (presigned URL minted or
    // streaming slot reserved); approve/reject doesn't double-count.
    state.metrics.http_samples.add(
        req.files.len() as u64,
        &[opentelemetry::KeyValue::new("mode", "upload")],
    );

    Ok(Json(BeginResponse {
        upload_id,
        mode: mode_str,
        urls,
        stream_base,
    }))
}

/// Streaming upload fallback. Client PUTs the raw file bytes; the server
/// buffers up to 1 GiB and stores them. Only available when
/// `RAWDB_UPLOAD_MODE` is `stream` or `either`.
#[utoipa::path(
    put,
    path = "/api/upload/stream/{upload_id}/{path}",
    tag = "upload",
    params(
        ("upload_id" = String, Path, description = "Upload ID returned by /upload/begin"),
        ("path" = String, Path, description = "Relative file path; must contain a category folder, may contain slashes"),
    ),
    request_body(content = Vec<u8>, content_type = "application/octet-stream"),
    responses(
        (status = 201, description = "Stored"),
        (status = 400, description = "Validation error or upload too large"),
        (status = 403, description = "Streaming uploads disabled by RAWDB_UPLOAD_MODE"),
    ),
)]
pub async fn stream(
    State(state): State<AppState>,
    Path((upload_id, file_path)): Path<(String, String)>,
    body: Bytes,
) -> AppResult<StatusCode> {
    if !matches!(
        state.config.upload_mode,
        UploadMode::Stream | UploadMode::Either
    ) {
        return Err(AppError::Forbidden);
    }
    validate_upload_id(&upload_id)?;
    validate_rel_path(&file_path)?;

    let max = state.config.max_upload_bytes;
    if (body.len() as u64) > max {
        return Err(AppError::PayloadTooLarge(format!(
            "file exceeds the maximum upload size of {} bytes",
            max
        )));
    }

    let key = pending_key(&upload_id, &file_path);
    state
        .s3
        .put_bytes(&key, body.to_vec(), None, None, None)
        .await
        .map_err(|e| match e {
            crate::s3::S3Error::Other(e) => AppError::Other(e),
            _ => AppError::Other(anyhow::anyhow!("s3 put failed: {e}")),
        })?;
    Ok(StatusCode::CREATED)
}

/// Finalize an upload. Server parses + validates the supplied set
/// metadata, verifies every declared file has been received, then commits
/// the upload to the reviewer queue.
#[utoipa::path(
    post,
    path = "/api/upload/complete",
    tag = "upload",
    request_body = CompleteRequest,
    responses(
        (status = 201, description = "Pending set written; awaiting review"),
        (status = 400, description = "Invalid meta, missing declared file, or blocked extension"),
    ),
)]
pub async fn complete(
    State(state): State<AppState>,
    Json(req): Json<CompleteRequest>,
) -> AppResult<StatusCode> {
    if req.maker.trim().is_empty() || req.model.trim().is_empty() {
        return Err(AppError::BadRequest("maker and model required".into()));
    }
    validate_upload_id(&req.upload_id)?;

    let mut parsed = meta::parse(&req.meta_toml)
        .map_err(|e| AppError::BadRequest(format!("invalid rawdb-meta.toml: {e}")))?;
    if parsed.set.maker != req.maker || parsed.set.model != req.model {
        return Err(AppError::BadRequest(
            "meta maker/model must match request".into(),
        ));
    }
    // Uploaders cannot mark a set "special" — only reviewers can.
    parsed.set.special = false;

    // Enforce extension denylist server-side.
    let blocked: HashSet<&str> = state
        .config
        .blocked_extensions
        .iter()
        .map(String::as_str)
        .collect();
    for f in &parsed.files {
        if let Some(ext) = blocked_extension(&f.path, &blocked) {
            return Err(AppError::BadRequest(format!(
                "file type not allowed (.{ext}): {}",
                f.path
            )));
        }
    }

    // Verify every declared file exists under the upload prefix, and that
    // none of them exceeds the configured max size. The size check also
    // catches presigned PUTs that bypass our streaming handler.
    let pending_prefix = format!("pending/{}/", req.upload_id);
    let objects = state
        .s3
        .list_objects(&pending_prefix)
        .await
        .map_err(AppError::Other)?;
    let mut present: std::collections::HashMap<String, u64> =
        std::collections::HashMap::new();
    for o in objects {
        if let Some(rel) = o.key.strip_prefix(&pending_prefix) {
            present.insert(rel.to_string(), o.size);
        }
    }
    let max = state.config.max_upload_bytes;
    for f in &parsed.files {
        match present.get(&f.path) {
            None => {
                return Err(AppError::BadRequest(format!(
                    "declared file missing: {}",
                    f.path
                )));
            }
            Some(&size) if size > max => {
                return Err(AppError::PayloadTooLarge(format!(
                    "{} is {} bytes; maximum is {} bytes",
                    f.path, size, max
                )));
            }
            Some(_) => {}
        }
    }

    // Write the meta TOML.
    let meta_key = format!("{pending_prefix}rawdb-meta.toml");
    state
        .s3
        .put_bytes(
            &meta_key,
            meta::to_toml(&parsed).into_bytes(),
            Some("application/toml"),
            None,
            None,
        )
        .await
        .map_err(|e| AppError::Other(anyhow::anyhow!("put meta: {e}")))?;

    // Refresh local cache so the originating pod sees the new pending set
    // before the next scan tick.
    let scanner = state.scanner.clone();
    let upload_id = req.upload_id;
    tokio::spawn(async move {
        if let Err(e) = scanner.refresh_one_pending(&upload_id).await {
            tracing::warn!(error = ?e, "post-complete refresh failed");
        }
    });

    Ok(StatusCode::CREATED)
}

// ---- helpers -------------------------------------------------------------

fn mint_upload_id() -> String {
    let ts = Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let suffix: [u8; 4] = rand::thread_rng().gen();
    let hex = suffix
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    format!("{ts}-{hex}")
}

fn validate_upload_id(id: &str) -> AppResult<()> {
    // Format: YYYYMMDDTHHMMSSZ-<8 hex>
    if id.len() != 25 || !id.contains('-') {
        return Err(AppError::BadRequest("bad upload_id".into()));
    }
    Ok(())
}

fn validate_rel_path(path: &str) -> AppResult<()> {
    if path.is_empty()
        || path.starts_with('/')
        || path.contains("..")
        || path.split('/').any(|seg| seg.is_empty())
    {
        return Err(AppError::BadRequest(format!("invalid path: {path}")));
    }
    if !path.contains('/') {
        return Err(AppError::BadRequest(format!(
            "missing category folder in: {path}"
        )));
    }
    Ok(())
}

/// Returns the offending (lowercased) extension if the path's file extension
/// is on the denylist, else `None`. Files without an extension are allowed.
fn blocked_extension(path: &str, blocked: &HashSet<&str>) -> Option<String> {
    let name = path.rsplit('/').next().unwrap_or(path);
    let ext = name.rsplit_once('.')?.1.to_ascii_lowercase();
    if blocked.contains(ext.as_str()) {
        Some(ext)
    } else {
        None
    }
}

fn pending_key(upload_id: &str, rel: &str) -> String {
    format!("pending/{upload_id}/{rel}")
}
