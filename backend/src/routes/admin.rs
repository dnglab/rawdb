//! Admin/reviewer endpoints: pending review (approve/reject) and (in
//! Phase 8) user management.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::auth::{AuthGuard, ADMIN_ONLY, REVIEWER_OR_ADMIN};
use crate::error::{AppError, AppResult};
use crate::meta::{self, FileMeta, RawdbMeta, SetMeta};
use crate::s3::S3Error;
use crate::state::AppState;
use crate::users::{cas_update, User, UsersError};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/pending", get(list_pending))
        .route(
            "/pending/:upload_id",
            get(get_pending).put(edit_pending),
        )
        .route(
            "/pending/:upload_id/download/*path",
            get(download_pending),
        )
        .route("/pending/:upload_id/approve", post(approve_pending))
        .route("/pending/:upload_id/reject", post(reject_pending))
        .route("/pending/:upload_id/verify", post(verify_pending))
        .route(
            "/sets/:maker/:model",
            axum::routing::put(edit_set).delete(delete_set),
        )
        .route("/users", get(list_users).post(add_user))
        .route(
            "/users/:sub",
            axum::routing::patch(patch_user).delete(delete_user),
        )
}

#[derive(Debug, Serialize, ToSchema)]
pub struct PendingRow {
    pub maker: String,
    pub model: String,
    pub upload_id: String,
    pub license: String,
    pub notes: Option<String>,
    pub uploaded_at: Option<chrono::DateTime<chrono::Utc>>,
    pub uploaded_by: Option<String>,
}

/// List pending uploads in the reviewer queue. One row per
/// `(maker, model, upload_id)`.
#[utoipa::path(
    get,
    path = "/api/admin/pending",
    tag = "admin",
    security(("session_cookie" = [])),
    responses(
        (status = 200, body = [PendingRow]),
        (status = 401, description = "Not authenticated"),
        (status = 403, description = "Not a reviewer or admin"),
    ),
)]
pub async fn list_pending(
    _auth: AuthGuard<REVIEWER_OR_ADMIN>,
    State(state): State<AppState>,
) -> AppResult<Json<Vec<PendingRow>>> {
    let conn = state
        .db
        .pool()
        .get()
        .map_err(|e| AppError::Other(e.into()))?;
    let mut stmt = conn
        .prepare(
            "SELECT maker, model, upload_id, license, notes, uploaded_at, uploaded_by
             FROM pending_sets
             ORDER BY uploaded_at DESC NULLS LAST, upload_id",
        )
        .map_err(|e| AppError::Other(e.into()))?;
    let rows = stmt
        .query_map([], |r| {
            Ok(PendingRow {
                maker: r.get(0)?,
                model: r.get(1)?,
                upload_id: r.get(2)?,
                license: r.get(3)?,
                notes: r.get(4)?,
                uploaded_at: r
                    .get::<_, Option<String>>(5)?
                    .and_then(|s| chrono::DateTime::parse_from_rfc3339(&s).ok())
                    .map(|d| d.with_timezone(&chrono::Utc)),
                uploaded_by: r.get(6)?,
            })
        })
        .map_err(|e| AppError::Other(e.into()))?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|e| AppError::Other(e.into()))?;
    Ok(Json(rows))
}

// ---- pending detail ------------------------------------------------------

#[derive(Debug, Serialize, ToSchema)]
pub struct PendingFile {
    pub path: String,
    pub category: String,
    pub extension: String,
    pub size: u64,
    pub license: Option<String>,
    pub notes: Option<String>,
    /// Uploader-supplied SHA-256 hex; the reviewer can verify it via
    /// `POST /api/admin/pending/{upload_id}/verify`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    pub tags: Vec<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct PendingDetail {
    pub maker: String,
    pub model: String,
    pub license: String,
    pub notes: Option<String>,
    pub uploaded_at: Option<chrono::DateTime<chrono::Utc>>,
    pub uploaded_by: Option<String>,
    pub special: bool,
    pub files: Vec<PendingFile>,
}

/// Full view of one pending upload: parsed set metadata joined with the
/// stored file sizes. Drives the reviewer's Review page.
#[utoipa::path(
    get,
    path = "/api/admin/pending/{upload_id}",
    tag = "admin",
    security(("session_cookie" = [])),
    params(("upload_id" = String, Path, description = "Upload ID")),
    responses(
        (status = 200, body = PendingDetail),
        (status = 404, description = "Pending upload not found"),
    ),
)]
pub async fn get_pending(
    _auth: AuthGuard<REVIEWER_OR_ADMIN>,
    State(state): State<AppState>,
    Path(upload_id): Path<String>,
) -> AppResult<Json<PendingDetail>> {
    let prefix = format!("pending/{upload_id}/");

    let objects = state
        .s3
        .list_objects(&prefix)
        .await
        .map_err(AppError::Other)?;
    // path (relative to the pending prefix) -> object size
    let mut sizes = std::collections::HashMap::new();
    for o in &objects {
        if let Some(rel) = o.key.strip_prefix(&prefix) {
            sizes.insert(rel.to_string(), o.size);
        }
    }

    let meta_key = format!("{prefix}rawdb-meta.toml");
    // Fall back to the meta object's S3 LastModified when the TOML omits
    // `uploaded_at` (it's optional and uploads don't set it).
    let meta_last_modified = objects
        .iter()
        .find(|o| o.key == meta_key)
        .and_then(|o| o.last_modified);

    let (bytes, _) = state.s3.get_bytes(&meta_key).await.map_err(|e| match e {
        S3Error::NotFound(_) => AppError::NotFound,
        S3Error::PreconditionFailed => AppError::NotFound,
        S3Error::Other(e) => AppError::Other(e),
    })?;
    let toml = String::from_utf8(bytes)
        .map_err(|e| AppError::BadRequest(format!("rawdb-meta.toml is not UTF-8: {e}")))?;
    let parsed = meta::parse(&toml)
        .map_err(|e| AppError::BadRequest(format!("invalid rawdb-meta.toml: {e}")))?;

    // The reviewer edits the *actual* stored values, so expose each file's
    // own tags here (set-tag inheritance is a read/search concern handled
    // in the public API, not in the editor).
    let files = parsed
        .files
        .iter()
        .map(|f| PendingFile {
            category: f.category().to_string(),
            extension: f.extension().unwrap_or_default(),
            size: sizes.get(&f.path).copied().unwrap_or(0),
            path: f.path.clone(),
            license: f.license.clone(),
            notes: f.notes.clone(),
            sha256: f.sha256.clone(),
            tags: f.tags.clone(),
        })
        .collect();

    Ok(Json(PendingDetail {
        maker: parsed.set.maker,
        model: parsed.set.model,
        license: parsed.set.license,
        notes: parsed.set.notes,
        uploaded_at: parsed.set.uploaded_at.or(meta_last_modified),
        uploaded_by: parsed.set.uploaded_by,
        special: parsed.set.special,
        files,
    }))
}

/// Reviewer-gated single-file download from a pending upload. Honors the
/// shared `RAWDB_DOWNLOAD_MODE` (presigned / stream / either + `?stream=1`).
#[utoipa::path(
    get,
    path = "/api/admin/pending/{upload_id}/download/{path}",
    tag = "admin",
    security(("session_cookie" = [])),
    params(
        ("upload_id" = String, Path, description = "Upload ID"),
        ("path" = String, Path, description = "Relative file path within the upload"),
    ),
    responses(
        (status = 302, description = "Redirect to a presigned download URL"),
        (status = 200, description = "Streamed bytes", content_type = "application/octet-stream"),
        (status = 404, description = "File not found"),
    ),
)]
pub async fn download_pending(
    _auth: AuthGuard<REVIEWER_OR_ADMIN>,
    State(state): State<AppState>,
    Path((upload_id, file_path)): Path<(String, String)>,
    axum::extract::Query(q): axum::extract::Query<crate::routes::public::DownloadQuery>,
) -> AppResult<Response> {
    if file_path.contains("..") {
        return Err(AppError::BadRequest("invalid path".into()));
    }
    let key = format!("pending/{upload_id}/{file_path}");
    if state.s3.head(&key).await.map_err(AppError::Other)?.is_none() {
        return Err(AppError::NotFound);
    }
    let filename = file_path.rsplit('/').next().unwrap_or(&file_path);
    crate::routes::public::serve_download(&state, &key, filename, q.stream.as_deref()).await
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct EditRequest {
    pub maker: String,
    pub model: String,
    pub license: String,
    #[serde(default)]
    pub notes: Option<String>,
    #[serde(default)]
    pub uploaded_by: Option<String>,
    #[serde(default)]
    pub special: bool,
    pub files: Vec<EditFile>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct EditFile {
    /// Current relative path of the file within the upload.
    pub old_path: String,
    /// Desired path (rename of prefix and/or filename). May equal `old_path`.
    pub path: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub notes: Option<String>,
    #[serde(default)]
    pub license: Option<String>,
}

/// Edit a pending upload in place (without approving it): rename files,
/// edit per-file tags/notes/license, and edit set-level fields. Renamed
/// files are moved server-side.
#[utoipa::path(
    put,
    path = "/api/admin/pending/{upload_id}",
    tag = "admin",
    security(("session_cookie" = [])),
    params(("upload_id" = String, Path, description = "Upload ID")),
    request_body = EditRequest,
    responses(
        (status = 200, description = "Edits applied"),
        (status = 400, description = "Invalid path / duplicate / blocked extension"),
    ),
)]
pub async fn edit_pending(
    _auth: AuthGuard<REVIEWER_OR_ADMIN>,
    State(state): State<AppState>,
    Path(upload_id): Path<String>,
    Json(req): Json<EditRequest>,
) -> AppResult<StatusCode> {
    if req.maker.trim().is_empty() || req.model.trim().is_empty() {
        return Err(AppError::BadRequest("maker and model are required".into()));
    }
    let prefix = format!("pending/{upload_id}/");

    // Validate destination paths.
    let blocked: std::collections::HashSet<&str> = state
        .config
        .blocked_extensions
        .iter()
        .map(String::as_str)
        .collect();
    let mut seen = std::collections::HashSet::new();
    for f in &req.files {
        let p = f.path.trim();
        if p.is_empty()
            || p.starts_with('/')
            || p.contains("..")
            || !p.contains('/')
            || p.split('/').any(|s| s.is_empty())
        {
            return Err(AppError::BadRequest(format!("invalid path: {p}")));
        }
        if !seen.insert(p.to_string()) {
            return Err(AppError::BadRequest(format!("duplicate path: {p}")));
        }
        let ext = p
            .rsplit('/')
            .next()
            .and_then(|n| n.rsplit_once('.'))
            .map(|(_, e)| e.to_ascii_lowercase());
        if let Some(e) = ext {
            if blocked.contains(e.as_str()) {
                return Err(AppError::BadRequest(format!(
                    "file type not allowed (.{e}): {p}"
                )));
            }
        }
    }

    // Carry server-managed fields (currently: sha256) forward from the
    // existing meta. The EditFile shape doesn't surface them, so without
    // this step the edit would silently wipe them.
    let existing_by_old_path = load_existing_file_meta(&state, &prefix).await;

    // Move renamed files in S3 (copy new, delete old afterwards). Targets
    // that collide with another file's old path are handled by deleting
    // only old paths that are not also destinations.
    let destinations: std::collections::HashSet<&str> =
        req.files.iter().map(|f| f.path.as_str()).collect();
    let mut to_delete: Vec<String> = Vec::new();
    for f in &req.files {
        if f.old_path == f.path {
            continue;
        }
        let src = format!("{prefix}{}", f.old_path);
        let dst = format!("{prefix}{}", f.path);
        state.s3.copy(&src, &dst).await.map_err(AppError::Other)?;
        if !destinations.contains(f.old_path.as_str()) {
            to_delete.push(src);
        }
    }
    for key in to_delete {
        state.s3.delete(&key).await.map_err(AppError::Other)?;
    }

    // Rebuild and persist the meta TOML.
    let new_meta = RawdbMeta {
        set: SetMeta {
            maker: req.maker.trim().to_string(),
            model: req.model.trim().to_string(),
            license: if req.license.trim().is_empty() {
                crate::meta::DEFAULT_LICENSE.to_string()
            } else {
                req.license.clone()
            },
            uploaded_by: req
                .uploaded_by
                .clone()
                .filter(|s| !s.trim().is_empty()),
            uploaded_at: None,
            notes: req.notes.clone().filter(|s| !s.trim().is_empty()),
            special: req.special,
        },
        files: req
            .files
            .iter()
            .map(|f| FileMeta {
                path: f.path.trim().to_string(),
                sha256: existing_by_old_path
                    .get(&f.old_path)
                    .and_then(|prev| prev.sha256.clone()),
                license: f.license.clone().filter(|s| !s.trim().is_empty()),
                notes: f.notes.clone().filter(|s| !s.trim().is_empty()),
                tags: clean_tags(&f.tags),
            })
            .collect(),
    };
    let body = meta::to_toml(&new_meta).into_bytes();
    state
        .s3
        .put_bytes(
            &format!("{prefix}rawdb-meta.toml"),
            body,
            Some("application/toml"),
            None,
            None,
        )
        .await
        .map_err(|e| AppError::Other(anyhow::anyhow!("write meta: {e}")))?;

    // Refresh the local cache so the change is visible immediately.
    let _ = state.scanner.refresh_one_pending(&upload_id).await;

    Ok(StatusCode::OK)
}

fn clean_tags(items: &[String]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for raw in items {
        let t = raw.trim();
        if !t.is_empty() && seen.insert(t.to_string()) {
            out.push(t.to_string());
        }
    }
    out
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct EditSetRequest {
    pub license: String,
    #[serde(default)]
    pub notes: Option<String>,
    #[serde(default)]
    pub uploaded_by: Option<String>,
    #[serde(default)]
    pub special: bool,
    pub files: Vec<EditFile>,
}

/// Edit an already-approved set in place: rename files, edit per-file
/// tags/notes/license, and edit set-level license/notes/uploaded_by.
/// `maker`/`model` are the set's identity and are not editable here.
#[utoipa::path(
    put,
    path = "/api/admin/sets/{maker}/{model}",
    tag = "admin",
    security(("session_cookie" = [])),
    params(
        ("maker" = String, Path, description = "Camera maker"),
        ("model" = String, Path, description = "Camera model"),
    ),
    request_body = EditSetRequest,
    responses(
        (status = 200, description = "Set updated"),
        (status = 404, description = "Set does not exist"),
    ),
)]
pub async fn edit_set(
    _auth: AuthGuard<ADMIN_ONLY>,
    State(state): State<AppState>,
    Path((maker, model)): Path<(String, String)>,
    Json(req): Json<EditSetRequest>,
) -> AppResult<StatusCode> {
    // The set must already exist.
    if state
        .db
        .get_set(&maker, &model)
        .map_err(AppError::Other)?
        .is_none()
    {
        return Err(AppError::NotFound);
    }

    let prefix = format!("samples/{maker}/{model}/");

    // Validate destination paths + extension denylist.
    let blocked: std::collections::HashSet<&str> = state
        .config
        .blocked_extensions
        .iter()
        .map(String::as_str)
        .collect();
    let mut seen = std::collections::HashSet::new();
    for f in &req.files {
        let p = f.path.trim();
        if p.is_empty()
            || p.starts_with('/')
            || p.contains("..")
            || !p.contains('/')
            || p.split('/').any(|s| s.is_empty())
        {
            return Err(AppError::BadRequest(format!("invalid path: {p}")));
        }
        if !seen.insert(p.to_string()) {
            return Err(AppError::BadRequest(format!("duplicate path: {p}")));
        }
        if let Some(e) = p
            .rsplit('/')
            .next()
            .and_then(|n| n.rsplit_once('.'))
            .map(|(_, e)| e.to_ascii_lowercase())
        {
            if blocked.contains(e.as_str()) {
                return Err(AppError::BadRequest(format!(
                    "file type not allowed (.{e}): {p}"
                )));
            }
        }
    }

    // Carry server-managed fields (sha256) forward; the edit shape doesn't
    // surface them.
    let existing_by_old_path = load_existing_file_meta(&state, &prefix).await;

    // Move renamed files in S3 (copy new, delete old that isn't a target).
    let destinations: std::collections::HashSet<&str> =
        req.files.iter().map(|f| f.path.as_str()).collect();
    let referenced_old: std::collections::HashSet<&str> =
        req.files.iter().map(|f| f.old_path.as_str()).collect();
    let mut to_delete: Vec<String> = Vec::new();
    for f in &req.files {
        if f.old_path == f.path {
            continue;
        }
        let src = format!("{prefix}{}", f.old_path);
        let dst = format!("{prefix}{}", f.path);
        state.s3.copy(&src, &dst).await.map_err(AppError::Other)?;
        if !destinations.contains(f.old_path.as_str()) {
            to_delete.push(src);
        }
    }
    // Files in the existing meta but absent from the new request are
    // intentional removals — drop them from S3 so they don't linger as
    // orphans under the set prefix.
    for existing_path in existing_by_old_path.keys() {
        if !referenced_old.contains(existing_path.as_str()) {
            to_delete.push(format!("{prefix}{existing_path}"));
        }
    }
    for key in to_delete {
        state.s3.delete(&key).await.map_err(AppError::Other)?;
    }

    let new_meta = RawdbMeta {
        set: SetMeta {
            maker: maker.clone(),
            model: model.clone(),
            license: if req.license.trim().is_empty() {
                crate::meta::DEFAULT_LICENSE.to_string()
            } else {
                req.license.clone()
            },
            uploaded_by: req.uploaded_by.clone().filter(|s| !s.trim().is_empty()),
            uploaded_at: None,
            notes: req.notes.clone().filter(|s| !s.trim().is_empty()),
            special: req.special,
        },
        files: req
            .files
            .iter()
            .map(|f| FileMeta {
                path: f.path.trim().to_string(),
                sha256: existing_by_old_path
                    .get(&f.old_path)
                    .and_then(|prev| prev.sha256.clone()),
                license: f.license.clone().filter(|s| !s.trim().is_empty()),
                notes: f.notes.clone().filter(|s| !s.trim().is_empty()),
                tags: clean_tags(&f.tags),
            })
            .collect(),
    };
    state
        .s3
        .put_bytes(
            &format!("{prefix}rawdb-meta.toml"),
            meta::to_toml(&new_meta).into_bytes(),
            Some("application/toml"),
            None,
            None,
        )
        .await
        .map_err(|e| AppError::Other(anyhow::anyhow!("write meta: {e}")))?;

    let _ = state.scanner.refresh_one_set(&maker, &model).await;
    Ok(StatusCode::OK)
}

/// Delete an approved set in its entirety. Wipes
/// `samples/<maker>/<model>/` (meta + all files) and refreshes the local
/// cache. Admin only — irreversible.
#[utoipa::path(
    delete,
    path = "/api/admin/sets/{maker}/{model}",
    tag = "admin",
    security(("session_cookie" = [])),
    params(
        ("maker" = String, Path, description = "Camera maker"),
        ("model" = String, Path, description = "Camera model"),
    ),
    responses(
        (status = 204, description = "Set removed"),
        (status = 404, description = "Set does not exist"),
    ),
)]
pub async fn delete_set(
    _auth: AuthGuard<ADMIN_ONLY>,
    State(state): State<AppState>,
    Path((maker, model)): Path<(String, String)>,
) -> AppResult<StatusCode> {
    // Existence check — saves an unnecessary delete_prefix on a typo and
    // gives the UI a meaningful 404 instead of a silent no-op.
    if state
        .db
        .get_set(&maker, &model)
        .map_err(AppError::Other)?
        .is_none()
    {
        return Err(AppError::NotFound);
    }
    let prefix = format!("samples/{maker}/{model}/");
    state
        .s3
        .delete_prefix(&prefix)
        .await
        .map_err(AppError::Other)?;

    // Refresh the local cache so the originating pod's UI is immediately
    // consistent; peers pick it up on the next scanner tick.
    let scanner = state.scanner.clone();
    let (m, mo) = (maker.clone(), model.clone());
    tokio::spawn(async move {
        if let Err(e) = scanner.refresh_one_set(&m, &mo).await {
            tracing::warn!(error = ?e, "delete_set: refresh failed");
        }
    });
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize, Default, ToSchema)]
pub struct ApproveRequest {
    /// `refuse` (default), `merge`, or `replace`. Controls behavior when an
    /// approved set with the same `(maker, model)` already exists.
    #[serde(default)]
    pub conflict: Option<String>,
    /// Relative paths to promote. Empty → promote every file in the
    /// upload. Files not listed here are dropped.
    #[serde(default)]
    pub files: Vec<String>,
}

/// Promote a pending upload to an approved set under its
/// `(maker, model)`. Conflict resolution (`refuse`/`merge`/`replace`)
/// controls what happens when an approved set with the same
/// `(maker, model)` already exists.
#[utoipa::path(
    post,
    path = "/api/admin/pending/{upload_id}/approve",
    tag = "admin",
    security(("session_cookie" = [])),
    params(("upload_id" = String, Path, description = "Upload ID")),
    request_body = ApproveRequest,
    responses(
        (status = 200, description = "Approved; files copied and pending wiped"),
        (status = 400, description = "Bad conflict mode or empty selection"),
        (status = 404, description = "Pending upload not found"),
        (status = 409, description = "Conflict with existing set under `refuse` mode"),
    ),
)]
pub async fn approve_pending(
    _auth: AuthGuard<REVIEWER_OR_ADMIN>,
    State(state): State<AppState>,
    Path(upload_id): Path<String>,
    Json(req): Json<ApproveRequest>,
) -> AppResult<StatusCode> {
    let conflict_mode = req.conflict.as_deref().unwrap_or("refuse");
    if !matches!(conflict_mode, "refuse" | "merge" | "replace") {
        return Err(AppError::BadRequest(
            "conflict must be refuse, merge, or replace".into(),
        ));
    }

    let pending_prefix = format!("pending/{upload_id}/");

    // Parse the pending meta to learn (maker, model) and the declared files.
    let meta_key = format!("{pending_prefix}rawdb-meta.toml");
    let (bytes, _) = state.s3.get_bytes(&meta_key).await.map_err(|e| match e {
        S3Error::NotFound(_) | S3Error::PreconditionFailed => AppError::NotFound,
        S3Error::Other(e) => AppError::Other(e),
    })?;
    let toml = String::from_utf8(bytes)
        .map_err(|e| AppError::BadRequest(format!("meta not UTF-8: {e}")))?;
    let parsed = meta::parse(&toml)
        .map_err(|e| AppError::BadRequest(format!("invalid meta: {e}")))?;
    let maker = parsed.set.maker.clone();
    let model = parsed.set.model.clone();

    // Selected subset: explicit list, else all declared files.
    let selected: std::collections::HashSet<String> = if req.files.is_empty() {
        parsed.files.iter().map(|f| f.path.clone()).collect()
    } else {
        req.files.iter().cloned().collect()
    };
    let promote: Vec<&crate::meta::FileMeta> = parsed
        .files
        .iter()
        .filter(|f| selected.contains(&f.path))
        .collect();
    if promote.is_empty() {
        return Err(AppError::BadRequest(
            "no files selected for approval".into(),
        ));
    }

    let samples_prefix = format!("samples/{maker}/{model}/");

    // Conflict resolution.
    let already_exists = state
        .db
        .get_set(&maker, &model)
        .map_err(AppError::Other)?
        .is_some();
    if already_exists {
        match conflict_mode {
            "refuse" => {
                return Err(AppError::Conflict(format!(
                    "set {maker}/{model} already approved; choose merge or replace"
                )));
            }
            "replace" => {
                state
                    .s3
                    .delete_prefix(&samples_prefix)
                    .await
                    .map_err(AppError::Other)?;
            }
            _ => {} // merge: overwrite per file
        }
    }

    // Copy only the selected files.
    for f in &promote {
        let src = format!("{pending_prefix}{}", f.path);
        let dst = format!("{samples_prefix}{}", f.path);
        state.s3.copy(&src, &dst).await.map_err(AppError::Other)?;
    }
    // Write a meta listing only the promoted files.
    let approved_meta = RawdbMeta {
        set: parsed.set.clone(),
        files: promote.iter().map(|f| (*f).clone()).collect(),
    };
    state
        .s3
        .put_bytes(
            &format!("{samples_prefix}rawdb-meta.toml"),
            meta::to_toml(&approved_meta).into_bytes(),
            Some("application/toml"),
            None,
            None,
        )
        .await
        .map_err(|e| AppError::Other(anyhow::anyhow!("write approved meta: {e}")))?;

    // Wipe the pending upload (this also removes any unchecked files).
    state
        .s3
        .delete_prefix(&pending_prefix)
        .await
        .map_err(AppError::Other)?;

    // Refresh local cache so the originating pod sees the change instantly.
    let scanner = state.scanner.clone();
    let (m, mo, uid) = (maker.clone(), model.clone(), upload_id.clone());
    tokio::spawn(async move {
        if let Err(e) = scanner.refresh_one_set(&m, &mo).await {
            tracing::warn!(error = ?e, "approve: refresh_one_set failed");
        }
        if let Err(e) = scanner.refresh_one_pending(&uid).await {
            tracing::warn!(error = ?e, "approve: refresh_one_pending failed");
        }
    });

    Ok(StatusCode::OK)
}

// ---- user management ------------------------------------------------------

#[derive(Debug, Serialize, ToSchema)]
pub struct UserView {
    pub sub: String,
    pub display_name: Option<String>,
    pub blocked: bool,
    pub roles: Vec<String>,
    pub added_at: Option<chrono::DateTime<chrono::Utc>>,
    pub added_by: Option<String>,
}

impl From<User> for UserView {
    fn from(u: User) -> Self {
        Self {
            sub: u.sub,
            display_name: u.display_name,
            blocked: u.blocked,
            roles: u.roles,
            added_at: u.added_at,
            added_by: u.added_by,
        }
    }
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct AddUserRequest {
    pub sub: String,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub roles: Vec<String>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct PatchUserRequest {
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub roles: Option<Vec<String>>,
    #[serde(default)]
    pub blocked: Option<bool>,
}

/// List all OIDC users known to this deployment. Does not include the
/// synthetic bootstrap admin (which is not managed through this API).
#[utoipa::path(
    get,
    path = "/api/admin/users",
    tag = "admin",
    security(("session_cookie" = [])),
    responses(
        (status = 200, body = [UserView]),
        (status = 403, description = "Admin only"),
    ),
)]
pub async fn list_users(
    _auth: AuthGuard<ADMIN_ONLY>,
    State(state): State<AppState>,
) -> AppResult<Json<Vec<UserView>>> {
    let (file, _etag) = crate::users::read_with_etag(&state.s3)
        .await
        .map_err(AppError::Other)?;
    Ok(Json(file.users.into_iter().map(UserView::from).collect()))
}

/// Add an OIDC user. The synthetic bootstrap admin cannot be modified
/// through this endpoint.
#[utoipa::path(
    post,
    path = "/api/admin/users",
    tag = "admin",
    security(("session_cookie" = [])),
    request_body = AddUserRequest,
    responses(
        (status = 200, body = UserView),
        (status = 400, description = "Missing sub or invalid role"),
        (status = 409, description = "User with that sub already exists"),
    ),
)]
pub async fn add_user(
    AuthGuard(actor): AuthGuard<ADMIN_ONLY>,
    State(state): State<AppState>,
    Json(req): Json<AddUserRequest>,
) -> AppResult<Json<UserView>> {
    if req.sub.trim().is_empty() {
        return Err(AppError::BadRequest("sub required".into()));
    }
    validate_roles(&req.roles)?;
    let actor_sub = actor.sub.clone();
    let display = req.display_name.clone();
    let sub = req.sub.clone();
    let roles = req.roles.clone();

    let file = cas_update(&state.s3, &state.db, move |f| {
        if f.users.iter().any(|u| u.sub == sub) {
            return Err(UsersError::Other(anyhow::anyhow!(
                "user {sub} already exists"
            )));
        }
        f.users.push(User {
            sub: sub.clone(),
            display_name: display.clone(),
            blocked: false,
            added_at: Some(chrono::Utc::now()),
            added_by: Some(actor_sub.clone()),
            roles: roles.clone(),
            api_key_hash: None,
        });
        Ok(())
    })
    .await
    .map_err(map_users_error)?;

    let added = file
        .users
        .into_iter()
        .find(|u| u.sub == req.sub)
        .map(UserView::from)
        .ok_or_else(|| AppError::Other(anyhow::anyhow!("added user disappeared")))?;
    Ok(Json(added))
}

/// Update an OIDC user: display name, roles, blocked state. Any combination
/// of fields can be sent.
#[utoipa::path(
    patch,
    path = "/api/admin/users/{sub}",
    tag = "admin",
    security(("session_cookie" = [])),
    params(("sub" = String, Path, description = "User sub")),
    request_body = PatchUserRequest,
    responses(
        (status = 200, body = UserView),
        (status = 400, description = "Invalid role"),
        (status = 404, description = "User not found"),
    ),
)]
pub async fn patch_user(
    _auth: AuthGuard<ADMIN_ONLY>,
    State(state): State<AppState>,
    Path(sub): Path<String>,
    Json(req): Json<PatchUserRequest>,
) -> AppResult<Json<UserView>> {
    if let Some(roles) = &req.roles {
        validate_roles(roles)?;
    }
    let target = sub.clone();
    let file = cas_update(&state.s3, &state.db, move |f| {
        let Some(u) = f.users.iter_mut().find(|u| u.sub == target) else {
            return Err(UsersError::NotFound(target.clone()));
        };
        if let Some(dn) = &req.display_name {
            u.display_name = Some(dn.clone());
        }
        if let Some(r) = &req.roles {
            u.roles = r.clone();
        }
        if let Some(b) = req.blocked {
            u.blocked = b;
        }
        Ok(())
    })
    .await
    .map_err(map_users_error)?;

    let view = file
        .users
        .into_iter()
        .find(|u| u.sub == sub)
        .map(UserView::from)
        .ok_or(AppError::NotFound)?;
    Ok(Json(view))
}

/// Remove an OIDC user.
#[utoipa::path(
    delete,
    path = "/api/admin/users/{sub}",
    tag = "admin",
    security(("session_cookie" = [])),
    params(("sub" = String, Path, description = "User sub")),
    responses(
        (status = 204, description = "User removed"),
        (status = 404, description = "User not found"),
    ),
)]
pub async fn delete_user(
    _auth: AuthGuard<ADMIN_ONLY>,
    State(state): State<AppState>,
    Path(sub): Path<String>,
) -> AppResult<StatusCode> {
    let target = sub.clone();
    cas_update(&state.s3, &state.db, move |f| {
        let before = f.users.len();
        f.users.retain(|u| u.sub != target);
        if f.users.len() == before {
            return Err(UsersError::NotFound(target.clone()));
        }
        Ok(())
    })
    .await
    .map_err(map_users_error)?;
    Ok(StatusCode::NO_CONTENT)
}

fn validate_roles(roles: &[String]) -> AppResult<()> {
    for r in roles {
        // `unlimited`: may hold a personal API key (download rate-limit
        // bypass + access to /api/export).
        if !matches!(r.as_str(), "admin" | "reviewer" | "unlimited") {
            return Err(AppError::BadRequest(format!("unknown role: {r}")));
        }
    }
    Ok(())
}

pub(crate) fn map_users_error(e: UsersError) -> AppError {
    match e {
        UsersError::NotFound(_) => AppError::NotFound,
        UsersError::ConcurrentWrite => {
            AppError::Conflict("concurrent edit — please retry".into())
        }
        UsersError::Other(e) => AppError::Other(e),
    }
}

/// Delete the pending upload's prefix. Other pending uploads for the same
/// `(maker, model)` are untouched.
#[utoipa::path(
    post,
    path = "/api/admin/pending/{upload_id}/reject",
    tag = "admin",
    security(("session_cookie" = [])),
    params(("upload_id" = String, Path, description = "Upload ID")),
    responses(
        (status = 200, description = "Pending upload deleted"),
        (status = 404, description = "No such upload"),
    ),
)]
pub async fn reject_pending(
    _auth: AuthGuard<REVIEWER_OR_ADMIN>,
    State(state): State<AppState>,
    Path(upload_id): Path<String>,
) -> AppResult<StatusCode> {
    let prefix = format!("pending/{upload_id}/");
    let deleted = state
        .s3
        .delete_prefix(&prefix)
        .await
        .map_err(AppError::Other)?;
    if deleted == 0 {
        return Err(AppError::NotFound);
    }
    let scanner = state.scanner.clone();
    tokio::spawn(async move {
        if let Err(e) = scanner.refresh_one_pending(&upload_id).await {
            tracing::warn!(error = ?e, "reject: refresh failed");
        }
    });
    Ok(StatusCode::OK)
}

// ---- checksum verification ------------------------------------------------

#[derive(Debug, Serialize, ToSchema)]
pub struct VerifyFile {
    pub path: String,
    /// `ok` — claimed hash matches; `mismatch` — they differ;
    /// `missing` — the meta declared no `sha256` so there's nothing to
    /// compare against (the file was still hashed and `computed` is set).
    pub status: String,
    pub claimed: Option<String>,
    pub computed: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct VerifyResult {
    /// `true` when every file with a claimed hash matched.
    pub ok: bool,
    pub total: usize,
    pub verified: usize,
    pub mismatched: usize,
    pub missing: usize,
    pub files: Vec<VerifyFile>,
}

/// Stream every file in a pending upload through SHA-256 and compare to
/// the uploader's claim. Synchronous: the request stays open for the
/// duration; multi-GB sets can take minutes, so the UI shows a spinner.
#[utoipa::path(
    post,
    path = "/api/admin/pending/{upload_id}/verify",
    tag = "admin",
    security(("session_cookie" = [])),
    params(("upload_id" = String, Path, description = "Upload ID")),
    responses(
        (status = 200, body = VerifyResult),
        (status = 404, description = "Pending upload not found"),
    ),
)]
pub async fn verify_pending(
    _auth: AuthGuard<REVIEWER_OR_ADMIN>,
    State(state): State<AppState>,
    Path(upload_id): Path<String>,
) -> AppResult<Json<VerifyResult>> {
    use sha2::{Digest, Sha256};
    use tokio::io::AsyncReadExt;

    let prefix = format!("pending/{upload_id}/");

    // Load the meta to know which files are part of this upload and what
    // the claimed hashes are.
    let meta_key = format!("{prefix}rawdb-meta.toml");
    let (bytes, _) = state.s3.get_bytes(&meta_key).await.map_err(|e| match e {
        S3Error::NotFound(_) | S3Error::PreconditionFailed => AppError::NotFound,
        S3Error::Other(e) => AppError::Other(e),
    })?;
    let toml = std::str::from_utf8(&bytes)
        .map_err(|e| AppError::BadRequest(format!("meta not UTF-8: {e}")))?;
    let parsed = meta::parse(toml)
        .map_err(|e| AppError::BadRequest(format!("invalid meta: {e}")))?;

    let mut out = Vec::with_capacity(parsed.files.len());
    let mut verified = 0usize;
    let mut mismatched = 0usize;
    let mut missing = 0usize;
    let total = parsed.files.len();

    for f in &parsed.files {
        let key = format!("{prefix}{}", f.path);
        let stream = state.s3.get_stream(&key).await.map_err(|e| match e {
            S3Error::NotFound(_) => AppError::NotFound,
            S3Error::PreconditionFailed => AppError::NotFound,
            S3Error::Other(e) => AppError::Other(e),
        })?;

        // Hash without buffering the whole object: turn the SDK
        // ByteStream into an AsyncRead and update Sha256 from 1 MiB
        // chunks.
        let mut hasher = Sha256::new();
        let mut reader = stream.body.into_async_read();
        let mut buf = vec![0u8; 1024 * 1024];
        loop {
            let n = reader
                .read(&mut buf)
                .await
                .map_err(|e| AppError::Other(anyhow::anyhow!("read {}: {e}", f.path)))?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        let computed: String = hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let claimed = f.sha256.clone();
        let status = match claimed.as_deref() {
            None => {
                missing += 1;
                "missing"
            }
            Some(c) if c.eq_ignore_ascii_case(&computed) => {
                verified += 1;
                "ok"
            }
            _ => {
                mismatched += 1;
                "mismatch"
            }
        };
        out.push(VerifyFile {
            path: f.path.clone(),
            status: status.into(),
            claimed,
            computed: Some(computed),
        });
    }

    Ok(Json(VerifyResult {
        ok: mismatched == 0,
        total,
        verified,
        mismatched,
        missing,
        files: out,
    }))
}

/// Best-effort load of the existing `rawdb-meta.toml` under `prefix`,
/// keyed by file path. Used to carry server-managed fields (sha256) across
/// reviewer edits — the `EditFile` shape doesn't surface them, so a naive
/// rebuild would wipe them. Returns an empty map on any error so an edit
/// can still proceed if the existing meta is missing or malformed.
async fn load_existing_file_meta(
    state: &AppState,
    prefix: &str,
) -> std::collections::HashMap<String, FileMeta> {
    use crate::s3::S3Error;
    let meta_key = format!("{prefix}rawdb-meta.toml");
    let bytes = match state.s3.get_bytes(&meta_key).await {
        Ok((b, _)) => b,
        Err(S3Error::NotFound(_)) | Err(S3Error::PreconditionFailed) => {
            return std::collections::HashMap::new();
        }
        Err(S3Error::Other(e)) => {
            tracing::warn!(error = %e, key = %meta_key, "load existing meta failed");
            return std::collections::HashMap::new();
        }
    };
    let Ok(toml) = std::str::from_utf8(&bytes) else {
        return std::collections::HashMap::new();
    };
    let Ok(parsed) = meta::parse(toml) else {
        return std::collections::HashMap::new();
    };
    parsed
        .files
        .into_iter()
        .map(|f| (f.path.clone(), f))
        .collect()
}
