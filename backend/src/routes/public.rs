//! Public (unauthenticated) API: browse, search, set detail, download.

use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

use crate::cache::db::{SetQuery, SetSummary, SortField, SortOrder};
use crate::error::{AppError, AppResult};
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/sets", get(list_sets))
        .route("/sets/:maker/:model", get(set_detail))
        .route("/search", get(search))
        .route("/tags", get(tags))
        .route("/makers", get(makers))
        .route("/models", get(models))
        .route("/stats", get(stats))
        .route("/download/:maker/:model/*path", get(download))
}

// ---- types ----------------------------------------------------------------

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub(crate) struct SearchParams {
    /// Full-text search across maker, model, notes and tags.
    pub q: Option<String>,
    pub maker: Option<String>,
    pub model: Option<String>,
    pub license: Option<String>,
    pub extension: Option<String>,
    /// Comma-separated; every tag must be present on the set.
    pub tags: Option<String>,
    /// `1`/`true` includes non-camera ("special") sets (default: hidden).
    pub include_special: Option<String>,
    /// Column to sort by: `maker`, `model`, `license`, `file_count`,
    /// `total_size`, `tags`, or `uploaded_at`. Unknown values fall back
    /// to the default `(maker, model)` ordering.
    pub sort: Option<String>,
    /// `asc` (default) or `desc`.
    pub order: Option<String>,
    pub limit: Option<u32>,
    pub offset: Option<u32>,
}

impl SearchParams {
    fn into_query(self) -> SetQuery {
        let tags = self
            .tags
            .map(|s| {
                s.split(',')
                    .map(|t| t.trim().to_string())
                    .filter(|t| !t.is_empty())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let sort_field = self.sort.as_deref().and_then(|s| match s {
            "maker" => Some(SortField::Maker),
            "model" => Some(SortField::Model),
            "license" => Some(SortField::License),
            "file_count" => Some(SortField::FileCount),
            "total_size" => Some(SortField::TotalSize),
            "tags" => Some(SortField::Tags),
            "uploaded_at" => Some(SortField::UploadedAt),
            _ => None,
        });
        let sort_order = match self.order.as_deref() {
            Some("desc") | Some("DESC") => SortOrder::Desc,
            _ => SortOrder::Asc,
        };
        SetQuery {
            maker: self.maker,
            model: self.model,
            license: self.license,
            extension: self.extension.map(|s| s.to_ascii_lowercase()),
            tags,
            fts: self.q,
            include_special: matches!(
                self.include_special.as_deref(),
                Some("1") | Some("true")
            ),
            sort_field,
            sort_order,
            limit: self.limit,
            offset: self.offset,
        }
    }
}

#[derive(Debug, Serialize, ToSchema)]
pub struct SetEnvelope {
    pub maker: String,
    pub model: String,
    pub license: String,
    pub notes: Option<String>,
    pub uploaded_at: Option<chrono::DateTime<chrono::Utc>>,
    pub uploaded_by: Option<String>,
    pub file_count: u64,
    pub total_size: u64,
    pub special: bool,
    pub tags: Vec<String>,
}

impl From<SetSummary> for SetEnvelope {
    fn from(s: SetSummary) -> Self {
        Self {
            maker: s.maker,
            model: s.model,
            license: s.license,
            notes: s.notes,
            uploaded_at: s.uploaded_at,
            uploaded_by: s.uploaded_by,
            file_count: s.file_count,
            total_size: s.total_size,
            special: s.special,
            tags: s.tags,
        }
    }
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ListResponse {
    pub sets: Vec<SetEnvelope>,
    pub total: u64,
    pub limit: u32,
    pub offset: u32,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct FileEnvelope {
    pub path: String,
    pub category: String,
    pub extension: String,
    pub size: u64,
    pub license: String,
    pub notes: Option<String>,
    /// Lowercase hex SHA-256 of the file content (advisory; surfaced for
    /// fingerprinting and verifiable on demand from the admin UI).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    pub tags: Vec<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct SetDetailResponse {
    pub maker: String,
    pub model: String,
    pub license: String,
    pub notes: Option<String>,
    pub uploaded_at: Option<chrono::DateTime<chrono::Utc>>,
    pub uploaded_by: Option<String>,
    pub special: bool,
    /// Files grouped by category (raw_modes, crm, heif, …).
    pub categories: std::collections::BTreeMap<String, Vec<FileEnvelope>>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct StatsResponse {
    pub models: u64,
    pub special: u64,
    pub pending: u64,
    pub last_full_scan_at: Option<chrono::DateTime<chrono::Utc>>,
    pub ready: bool,
    /// Server-enforced ceiling on a single uploaded file, in bytes. The
    /// upload form refuses files larger than this client-side; the server
    /// re-checks at upload time and again at finalize.
    pub max_upload_bytes: u64,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct TagCount {
    pub tag: String,
    /// How many `(set, file)` rows reference this tag. Operator-curated
    /// tags with no current usage are still included with `count = 0`.
    pub count: u64,
    /// `true` for operator-curated tags from `RAWDB_TAGS_SUGGESTED`. The
    /// frontend always shows these as suggestion chips, on top of the
    /// top-N most-used tags.
    pub suggested: bool,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct TagsResponse {
    /// All tags across approved sets, ordered most-used first then
    /// alphabetically. Operator-curated tags appear with `suggested: true`
    /// even when unused. Clients build "suggested tags" UIs by taking the
    /// top-N by `count` and unioning with every entry where
    /// `suggested == true`.
    pub tags: Vec<TagCount>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct MakersResponse {
    pub makers: Vec<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct MakerModel {
    pub maker: String,
    pub model: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ModelsResponse {
    pub models: Vec<MakerModel>,
}

// ---- handlers -------------------------------------------------------------

/// Paginated list of approved sets, with optional filters identical to
/// `/search`.
#[utoipa::path(
    get,
    path = "/api/sets",
    tag = "public",
    params(SearchParams),
    responses((status = 200, body = ListResponse)),
)]
pub async fn list_sets(
    State(state): State<AppState>,
    Query(params): Query<SearchParams>,
) -> AppResult<Json<ListResponse>> {
    let page = state
        .db
        .search_sets(&params.into_query())
        .map_err(AppError::Other)?;
    Ok(Json(ListResponse {
        sets: page.sets.into_iter().map(Into::into).collect(),
        total: page.total,
        limit: page.limit,
        offset: page.offset,
    }))
}

/// Alias of `/sets` with the same filter semantics — kept so the UI can
/// pick whichever name reads better in context.
#[utoipa::path(
    get,
    path = "/api/search",
    tag = "public",
    params(SearchParams),
    responses((status = 200, body = ListResponse)),
)]
pub async fn search(
    state: State<AppState>,
    params: Query<SearchParams>,
) -> AppResult<Json<ListResponse>> {
    list_sets(state, params).await
}

/// Set detail: metadata plus all files grouped by category.
#[utoipa::path(
    get,
    path = "/api/sets/{maker}/{model}",
    tag = "public",
    params(
        ("maker" = String, Path, description = "Camera maker (URL-encoded)"),
        ("model" = String, Path, description = "Camera model (URL-encoded)"),
    ),
    responses(
        (status = 200, body = SetDetailResponse),
        (status = 404, description = "Set not found"),
    ),
)]
pub async fn set_detail(
    State(state): State<AppState>,
    Path((maker, model)): Path<(String, String)>,
) -> AppResult<Json<SetDetailResponse>> {
    let set = state
        .db
        .get_set(&maker, &model)
        .map_err(AppError::Other)?
        .ok_or(AppError::NotFound)?;
    let files = state
        .db
        .list_files(&maker, &model)
        .map_err(AppError::Other)?;
    let mut file_tags = state
        .db
        .file_tags(&maker, &model)
        .map_err(AppError::Other)?;

    let mut categories: std::collections::BTreeMap<String, Vec<FileEnvelope>> =
        std::collections::BTreeMap::new();
    for f in files {
        let tags = file_tags.remove(&f.path).unwrap_or_default();
        categories
            .entry(f.category.clone())
            .or_default()
            .push(FileEnvelope {
                path: f.path,
                category: f.category,
                extension: f.extension.unwrap_or_default(),
                size: f.size,
                license: f.license.unwrap_or_else(|| set.license.clone()),
                notes: f.notes,
                sha256: f.sha256,
                tags,
            });
    }

    Ok(Json(SetDetailResponse {
        maker: set.maker,
        model: set.model,
        license: set.license,
        notes: set.notes,
        uploaded_at: set.uploaded_at,
        uploaded_by: set.uploaded_by,
        special: set.special,
        categories,
    }))
}

/// All tags across approved sets with their usage counts, ordered
/// most-used first. Clients build suggestion UIs (e.g. "top 10") by
/// taking the head of this list.
#[utoipa::path(
    get,
    path = "/api/tags",
    tag = "public",
    responses((status = 200, body = TagsResponse)),
)]
pub async fn tags(State(state): State<AppState>) -> AppResult<Json<TagsResponse>> {
    let rows = state.db.tag_counts().map_err(AppError::Other)?;
    // Curated tags (case-insensitive lookup) — keep their canonical
    // spelling from config so admins control casing.
    let curated: Vec<String> = state
        .config
        .tags_suggested
        .iter()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .collect();
    let curated_lc: std::collections::HashSet<String> =
        curated.iter().map(|t| t.to_ascii_lowercase()).collect();

    let mut out: Vec<TagCount> = rows
        .into_iter()
        .map(|(tag, count)| {
            let suggested = curated_lc.contains(&tag.to_ascii_lowercase());
            TagCount {
                tag,
                count,
                suggested,
            }
        })
        .collect();

    // Append curated tags that don't appear in the data yet so the
    // frontend can still surface them as chips.
    let present_lc: std::collections::HashSet<String> =
        out.iter().map(|t| t.tag.to_ascii_lowercase()).collect();
    for t in &curated {
        if !present_lc.contains(&t.to_ascii_lowercase()) {
            out.push(TagCount {
                tag: t.clone(),
                count: 0,
                suggested: true,
            });
        }
    }

    Ok(Json(TagsResponse { tags: out }))
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub(crate) struct CatalogParams {
    /// `1`/`true` includes non-camera ("special") sets. Default is to
    /// exclude them so the public maker/model pickers stay a clean list of
    /// real cameras.
    pub include_special: Option<String>,
}

fn include_special(p: &CatalogParams) -> bool {
    matches!(p.include_special.as_deref(), Some("1") | Some("true"))
}

/// Distinct camera makers from approved sets, for the upload form's
/// maker picker (the user may still type a new one). Non-camera "special"
/// sets are excluded unless `?include_special=1`.
#[utoipa::path(
    get,
    path = "/api/makers",
    tag = "public",
    params(CatalogParams),
    responses((status = 200, body = MakersResponse)),
)]
pub async fn makers(
    State(state): State<AppState>,
    Query(params): Query<CatalogParams>,
) -> AppResult<Json<MakersResponse>> {
    let makers = state
        .db
        .distinct_makers(include_special(&params))
        .map_err(AppError::Other)?;
    Ok(Json(MakersResponse { makers }))
}

/// Distinct `(maker, model)` pairs from approved sets, for the upload
/// form's model picker (filtered by the chosen maker on the client).
/// Non-camera "special" sets are excluded unless `?include_special=1`.
#[utoipa::path(
    get,
    path = "/api/models",
    tag = "public",
    params(CatalogParams),
    responses((status = 200, body = ModelsResponse)),
)]
pub async fn models(
    State(state): State<AppState>,
    Query(params): Query<CatalogParams>,
) -> AppResult<Json<ModelsResponse>> {
    let pairs = state
        .db
        .distinct_maker_models(include_special(&params))
        .map_err(AppError::Other)?;
    let models: Vec<MakerModel> = pairs
        .into_iter()
        .map(|(maker, model)| MakerModel { maker, model })
        .collect();
    Ok(Json(ModelsResponse { models }))
}

#[utoipa::path(
    get,
    path = "/api/stats",
    tag = "public",
    responses((status = 200, body = StatsResponse)),
)]
pub async fn stats(State(state): State<AppState>) -> AppResult<Json<StatsResponse>> {
    Ok(Json(StatsResponse {
        models: state.db.count_models().map_err(AppError::Other)?,
        special: state.db.count_special().map_err(AppError::Other)?,
        pending: state.db.count_pending().map_err(AppError::Other)?,
        last_full_scan_at: state.db.last_full_scan_at().map_err(AppError::Other)?,
        ready: state.is_ready(),
        max_upload_bytes: state.config.max_upload_bytes,
    }))
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub(crate) struct DownloadQuery {
    /// `1`/`true` requests streaming when `RAWDB_DOWNLOAD_MODE=either`.
    pub stream: Option<String>,
}

/// Download a single file. By default (or always, depending on
/// `RAWDB_DOWNLOAD_MODE`) responds with a 302 redirect to a presigned
/// download URL; can also stream the bytes through the backend.
#[utoipa::path(
    get,
    path = "/api/download/{maker}/{model}/{path}",
    tag = "public",
    params(
        ("maker" = String, Path, description = "Camera maker"),
        ("model" = String, Path, description = "Camera model"),
        ("path" = String, Path, description = "File path relative to the model directory (may contain slashes)"),
        DownloadQuery,
    ),
    responses(
        (status = 302, description = "Redirect to a presigned download URL"),
        (status = 200, description = "Streamed file bytes (when streaming mode is selected)", content_type = "application/octet-stream"),
        (status = 404, description = "File not found"),
        (status = 429, description = "Per-IP download rate limit exceeded; see Retry-After"),
    ),
)]
pub async fn download(
    State(state): State<AppState>,
    Path((maker, model, file_path)): Path<(String, String, String)>,
    Query(q): Query<DownloadQuery>,
    headers: axum::http::HeaderMap,
    axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<std::net::SocketAddr>,
) -> AppResult<Response> {
    // Second-tier, per-instance rate limit on sample downloads. Checked
    // before any DB/S3 work so abuse is rejected cheaply. A valid personal
    // API key (`X-API-Key`) from an `unlimited`-role user bypasses it.
    // A *lookup error* (DB pool exhausted etc.) is surfaced as 5xx so we
    // never silently downgrade an authenticated request to anonymous.
    let has_api_key = crate::apikey::lookup(&state.db, &headers)
        .map_err(|e| {
            tracing::error!(error = %e, "apikey lookup failed on download");
            AppError::Other(e)
        })?
        .is_some();
    if state.downloads.enabled() && !has_api_key {
        let ip = client_ip(&headers, peer);
        if let crate::ratelimit::Decision::Limited { retry_after } =
            state.downloads.check(ip, &file_path)
        {
            return Err(AppError::TooManyRequests {
                retry_after_secs: retry_after.as_secs(),
            });
        }
    }

    // Verify the file exists in the approved cache before producing a
    // presigned URL or opening a stream — keeps misses cheap.
    let conn = state
        .db
        .pool()
        .get()
        .map_err(|e| AppError::Other(e.into()))?;
    let exists: bool = conn
        .query_row(
            "SELECT 1 FROM files WHERE maker = ? AND model = ? AND path = ?",
            rusqlite::params![&maker, &model, &file_path],
            |_| Ok(true),
        )
        .unwrap_or(false);
    if !exists {
        return Err(AppError::NotFound);
    }

    let s3_key = format!("samples/{maker}/{model}/{file_path}");
    let filename = file_path.rsplit('/').next().unwrap_or(&file_path);
    serve_download(&state, &s3_key, filename, q.stream.as_deref()).await
}

/// Shared download dispatcher honoring `RAWDB_DOWNLOAD_MODE`:
/// - **`presigned`** → 302 to a presigned S3 GET URL (the historic default).
/// - **`stream`** → backend proxies the bytes from S3 to the client.
/// - **`either`** → presigned by default; client passes `?stream=1` to
///   opt in to streaming.
pub(crate) async fn serve_download(
    state: &AppState,
    key: &str,
    filename: &str,
    stream_param: Option<&str>,
) -> AppResult<Response> {
    use crate::config::DownloadMode;
    let want_stream = match state.config.download_mode {
        DownloadMode::Stream => true,
        DownloadMode::Presigned => false,
        DownloadMode::Either => matches!(stream_param, Some("1") | Some("true")),
    };
    // One interaction per download request, whether we serve the bytes
    // ourselves or just hand out a presigned URL.
    state
        .metrics
        .http_samples
        .add(1, &[opentelemetry::KeyValue::new("mode", "download")]);
    if !want_stream {
        let url = state
            .s3
            .presign_get(key)
            .await
            .map_err(AppError::Other)?;
        let _ = (StatusCode::FOUND, header::LOCATION); // keep imports used
        return Ok(Redirect::to(&url).into_response());
    }

    use axum::body::Body;
    use axum::http::header::{CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_TYPE, ETAG};
    use axum::http::HeaderValue;

    let s = state.s3.get_stream(key).await.map_err(|e| match e {
        crate::s3::S3Error::NotFound(_) | crate::s3::S3Error::PreconditionFailed => {
            AppError::NotFound
        }
        crate::s3::S3Error::Other(e) => AppError::Other(e),
    })?;
    // `ByteStream` exposes its bytes via an `AsyncRead`; turn it into a
    // `Stream<Item = io::Result<Bytes>>` so axum can plug it into the body.
    let reader = s.body.into_async_read();
    let bytes_stream = tokio_util::io::ReaderStream::new(reader);
    let mut resp = Response::new(Body::from_stream(bytes_stream));
    let h = resp.headers_mut();
    let ct = s
        .content_type
        .as_deref()
        .and_then(|v| HeaderValue::from_str(v).ok())
        .unwrap_or_else(|| HeaderValue::from_static("application/octet-stream"));
    h.insert(CONTENT_TYPE, ct);
    if let Some(len) = s.content_length {
        if let Ok(v) = HeaderValue::from_str(&len.to_string()) {
            h.insert(CONTENT_LENGTH, v);
        }
    }
    if let Some(etag) = s.etag {
        if let Ok(v) = HeaderValue::from_str(&format!("\"{etag}\"")) {
            h.insert(ETAG, v);
        }
    }
    let disp = format!(
        "attachment; filename*=UTF-8''{}",
        percent_encode_filename(filename)
    );
    if let Ok(v) = HeaderValue::from_str(&disp) {
        h.insert(CONTENT_DISPOSITION, v);
    }
    Ok(resp)
}

/// Best-effort client IP for rate limiting. Behind a proxy (Traefik), the
/// TCP peer is the proxy, so the real client is taken from
/// `X-Forwarded-For` (leftmost entry) or `X-Real-IP`. Falls back to the
/// TCP peer for direct-access deployments. XFF is spoofable, but for a
/// per-instance soft limit that's an acceptable trade-off.
fn client_ip(
    headers: &axum::http::HeaderMap,
    peer: std::net::SocketAddr,
) -> std::net::IpAddr {
    if let Some(xff) = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
        if let Some(first) = xff.split(',').next() {
            if let Ok(ip) = first.trim().parse() {
                return ip;
            }
        }
    }
    if let Some(xri) = headers.get("x-real-ip").and_then(|v| v.to_str().ok()) {
        if let Ok(ip) = xri.trim().parse() {
            return ip;
        }
    }
    peer.ip()
}

fn percent_encode_filename(name: &str) -> String {
    use percent_encoding::{utf8_percent_encode, AsciiSet, CONTROLS};
    // RFC 5987 attr-char: anything outside this gets percent-encoded.
    const SET: &AsciiSet = &CONTROLS
        .add(b' ')
        .add(b'"')
        .add(b'#')
        .add(b'$')
        .add(b'%')
        .add(b'&')
        .add(b'+')
        .add(b',')
        .add(b'/')
        .add(b':')
        .add(b';')
        .add(b'<')
        .add(b'=')
        .add(b'>')
        .add(b'?')
        .add(b'@')
        .add(b'[')
        .add(b'\\')
        .add(b']')
        .add(b'{')
        .add(b'}');
    utf8_percent_encode(name, SET).to_string()
}
