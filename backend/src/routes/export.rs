//! Bulk export: the whole approved catalogue — every set with all its
//! files — as one JSON document. Protected by a personal API key
//! (`X-API-Key`), so only `apiservice`-role users can call it.

use axum::extract::State;
use axum::http::HeaderMap;
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;
use utoipa::ToSchema;

use crate::apikey;
use crate::cache::db::{ExportFile, ExportSet};
use crate::error::{AppError, AppResult};
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new().route("/export", get(export))
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ExportFileEnvelope {
    pub path: String,
    pub category: String,
    pub extension: String,
    pub size: u64,
    pub license: Option<String>,
    pub notes: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    pub tags: Vec<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ExportSetEnvelope {
    pub maker: String,
    pub model: String,
    pub license: String,
    pub notes: Option<String>,
    pub uploaded_at: Option<chrono::DateTime<chrono::Utc>>,
    pub uploaded_by: Option<String>,
    pub special: bool,
    pub tags: Vec<String>,
    pub files: Vec<ExportFileEnvelope>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ExportResponse {
    pub generated_at: chrono::DateTime<chrono::Utc>,
    /// Number of sets in `sets`.
    pub set_count: usize,
    pub sets: Vec<ExportSetEnvelope>,
}

impl From<ExportFile> for ExportFileEnvelope {
    fn from(f: ExportFile) -> Self {
        Self {
            path: f.path,
            category: f.category,
            extension: f.extension.unwrap_or_default(),
            size: f.size,
            license: f.license,
            notes: f.notes,
            sha256: f.sha256,
            tags: f.tags,
        }
    }
}

impl From<ExportSet> for ExportSetEnvelope {
    fn from(s: ExportSet) -> Self {
        Self {
            maker: s.maker,
            model: s.model,
            license: s.license,
            notes: s.notes,
            uploaded_at: s.uploaded_at,
            uploaded_by: s.uploaded_by,
            special: s.special,
            tags: s.tags,
            files: s.files.into_iter().map(Into::into).collect(),
        }
    }
}

/// Full catalogue dump. Requires a valid `X-API-Key` belonging to a
/// non-blocked `apiservice`-role user. Includes non-camera ("special")
/// sets — the export is for dedicated downstream consumers, not the
/// public browse UI.
#[utoipa::path(
    get,
    path = "/api/export",
    tag = "public",
    security(("api_key" = [])),
    responses(
        (status = 200, body = ExportResponse),
        (status = 401, description = "Missing or invalid API key"),
    ),
)]
pub async fn export(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> AppResult<Json<ExportResponse>> {
    match apikey::lookup(&state.db, &headers) {
        Ok(Some(_)) => {}
        Ok(None) => return Err(AppError::Unauthorized),
        Err(e) => {
            tracing::error!(error = %e, "apikey lookup failed on export");
            return Err(AppError::Other(e));
        }
    }
    let sets: Vec<ExportSetEnvelope> = state
        .db
        .export_all()
        .map_err(AppError::Other)?
        .into_iter()
        .map(Into::into)
        .collect();
    Ok(Json(ExportResponse {
        generated_at: chrono::Utc::now(),
        set_count: sets.len(),
        sets,
    }))
}
