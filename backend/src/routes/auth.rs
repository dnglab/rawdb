//! Auth endpoints: password login, logout, current-session probe, and
//! the OIDC discovery hint. OIDC start/callback are stubbed here and
//! filled in by Phase 9.

use axum::extract::{Query, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;
use utoipa::{IntoParams, ToSchema};

use crate::auth::{
    clear_cookie_header, decode_session, encode_session, set_cookie_header, verify_admin_password,
    SessionExtractor, BOOTSTRAP_SUB, OIDC_SOURCE, PASSWORD_SOURCE, ROLE_ADMIN,
};
use crate::config::OidcSubFormat;
use crate::error::{AppError, AppResult};
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/login", post(login))
        .route("/logout", post(logout))
        .route("/me", get(me))
        .route("/methods", get(methods))
        .route("/oidc/enabled", get(oidc_enabled))
        .route("/oidc/start", get(oidc_start_stub))
        .route("/oidc/callback", get(oidc_callback_stub))
        .route("/github/start", get(github_start))
        .route("/github/callback", get(github_callback))
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct LoginRequest {
    pub password: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct LoginResponse {
    pub ok: bool,
    pub sub: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct OkResponse {
    pub ok: bool,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct MeResponse {
    pub sub: String,
    /// Either `password` (bootstrap admin) or `oidc`.
    pub source: String,
    pub roles: Vec<String>,
    pub display_name: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct OidcEnabledResponse {
    pub enabled: bool,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct AuthMethodsResponse {
    /// `true` when `RAWDB_PASSWORD_AUTH_ENABLED` is true — the login form
    /// should render the password field.
    pub password: bool,
    /// `true` when all `RAWDB_OIDC_*` env vars are configured — the login
    /// form should render the "Sign in with SSO" button.
    pub oidc: bool,
    /// `true` when all `RAWDB_GITHUB_*` env vars are configured — the
    /// login form should render the "Sign in with GitHub" button.
    pub github: bool,
}

/// Password login → JWT session cookie. The single bootstrap admin
/// (synthetic `sub = bootstrap:admin`) is always authoritative; no other
/// passwords exist.
#[utoipa::path(
    post,
    path = "/auth/login",
    tag = "auth",
    request_body = LoginRequest,
    responses(
        (status = 200, body = LoginResponse, description = "Session cookie set"),
        (status = 401, description = "Wrong password"),
    ),
)]
pub async fn login(
    State(state): State<AppState>,
    Json(req): Json<LoginRequest>,
) -> AppResult<Response> {
    if !state.config.password_auth_enabled {
        // Don't leak whether a password is set when the path is closed.
        return Err(AppError::Forbidden);
    }
    if !verify_admin_password(&state.config, &req.password) {
        // Constant delay regardless of outcome would be nicer here, but
        // a uniform 401 is fine for a small site.
        return Err(AppError::Unauthorized);
    }
    let token = encode_session(
        &state.config,
        BOOTSTRAP_SUB,
        PASSWORD_SOURCE,
        vec![ROLE_ADMIN.to_string()],
    )
    .map_err(AppError::Other)?;

    let mut resp = (
        StatusCode::OK,
        Json(json!({ "ok": true, "sub": BOOTSTRAP_SUB })),
    )
        .into_response();
    resp.headers_mut().append(
        axum::http::header::SET_COOKIE,
        set_cookie_header(&state.config, &token),
    );
    Ok(resp)
}

/// Clear the session cookie.
#[utoipa::path(
    post,
    path = "/auth/logout",
    tag = "auth",
    security(("session_cookie" = [])),
    responses((status = 200, body = OkResponse)),
)]
pub async fn logout() -> Response {
    let mut resp = (StatusCode::OK, Json(json!({ "ok": true }))).into_response();
    resp.headers_mut().append(
        axum::http::header::SET_COOKIE,
        crate::auth::clear_cookie_header(),
    );
    resp
}

/// Current session info: subject, source, roles, display name. 401 if
/// not authenticated.
#[utoipa::path(
    get,
    path = "/auth/me",
    tag = "auth",
    security(("session_cookie" = [])),
    responses(
        (status = 200, body = MeResponse),
        (status = 401, description = "Not authenticated"),
    ),
)]
pub async fn me(
    State(state): State<AppState>,
    SessionExtractor(sess): SessionExtractor,
) -> Json<MeResponse> {
    // For OIDC sessions, refresh role/display from users.toml mirror.
    let mut roles = sess.roles.clone();
    let mut display_name = None;
    if sess.sub != BOOTSTRAP_SUB {
        if let Ok(Some(u)) = state.db.get_user(&sess.sub) {
            roles = u.roles;
            display_name = u.display_name;
        }
    }
    Json(MeResponse {
        sub: sess.sub,
        source: sess.source,
        roles,
        display_name,
    })
}

/// Drives SSO button visibility on the login screen. `enabled: true` only
/// when all four `RAWDB_OIDC_*` env vars are set. Prefer `/auth/methods`
/// when you also need the password-auth flag.
#[utoipa::path(
    get,
    path = "/auth/oidc/enabled",
    tag = "auth",
    responses((status = 200, body = OidcEnabledResponse)),
)]
pub async fn oidc_enabled(State(state): State<AppState>) -> Json<serde_json::Value> {
    Json(json!({ "enabled": state.config.oidc_enabled() }))
}

/// Which login methods this deployment accepts. Lets the login screen
/// hide the password field when password auth is disabled (OIDC-only
/// deployments) and the SSO button when OIDC isn't configured.
#[utoipa::path(
    get,
    path = "/auth/methods",
    tag = "auth",
    responses((status = 200, body = AuthMethodsResponse)),
)]
pub async fn methods(State(state): State<AppState>) -> Json<AuthMethodsResponse> {
    Json(AuthMethodsResponse {
        password: state.config.password_auth_enabled,
        oidc: state.config.oidc_enabled(),
        github: state.config.github_enabled(),
    })
}

/// Begin the OIDC auth-code+PKCE flow. Redirects to the IdP and sets a
/// short-lived pending-flow cookie. Returns 404 when OIDC isn't configured.
#[utoipa::path(
    get,
    path = "/auth/oidc/start",
    tag = "auth",
    responses(
        (status = 307, description = "Redirect to OIDC IdP"),
        (status = 404, description = "OIDC not configured"),
    ),
)]
#[cfg(feature = "oidc")]
pub async fn oidc_start_stub(State(state): State<AppState>) -> Response {
    use crate::auth_oidc::PendingFlow;
    use jsonwebtoken::{encode, EncodingKey, Header};

    let Some(oidc) = state.oidc.clone() else {
        return AppError::NotFound.into_response();
    };
    let (url, pending) = oidc.start_flow();
    // Sign the pending flow into a short-lived cookie.
    let token = match encode(
        &Header::new(jsonwebtoken::Algorithm::HS256),
        &pending,
        &EncodingKey::from_secret(state.config.session_key.as_bytes()),
    ) {
        Ok(t) => t,
        Err(e) => return AppError::Other(e.into()).into_response(),
    };
    let cookie = format!(
        "rawdb_oidc_pending={token}; Path=/auth/oidc; HttpOnly; SameSite=Lax; Max-Age=600"
    );
    let mut resp = Redirect::temporary(&url).into_response();
    resp.headers_mut()
        .append(header::SET_COOKIE, HeaderValue::from_str(&cookie).unwrap());
    resp
}

#[cfg(not(feature = "oidc"))]
pub async fn oidc_start_stub(_state: State<AppState>) -> Response {
    AppError::NotFound.into_response()
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct OidcCallbackParams {
    /// Authorization code returned by the IdP.
    pub code: String,
    /// CSRF state token issued at `/auth/oidc/start`.
    pub state: String,
}

/// OIDC code-exchange callback. Validates the pending-flow cookie, swaps
/// the code, resolves the canonical `sub`, looks up the user, and issues
/// a session cookie. Redirects to `/admin` on success.
#[utoipa::path(
    get,
    path = "/auth/oidc/callback",
    tag = "auth",
    params(OidcCallbackParams),
    responses(
        (status = 307, description = "Session cookie set; redirect to /admin"),
        (status = 401, description = "Pending-flow cookie missing or invalid"),
        (status = 403, description = "User unknown or blocked"),
        (status = 404, description = "OIDC not configured"),
    ),
)]
#[cfg(feature = "oidc")]
pub async fn oidc_callback_stub(
    State(state): State<AppState>,
    Query(q): Query<OidcCallbackParams>,
    headers: axum::http::HeaderMap,
) -> Response {
    use crate::auth_oidc::PendingFlow;
    use crate::users::{cas_update, User, UsersError};
    use jsonwebtoken::{decode, DecodingKey, Validation};

    let Some(oidc) = state.oidc.clone() else {
        return AppError::NotFound.into_response();
    };

    // Recover pending flow from cookie.
    let pending_token = match cookie_value(&headers, "rawdb_oidc_pending") {
        Some(v) => v,
        None => return AppError::BadRequest("missing oidc pending cookie".into()).into_response(),
    };
    let validation = Validation::new(jsonwebtoken::Algorithm::HS256);
    let pending: PendingFlow = match decode::<PendingFlow>(
        &pending_token,
        &DecodingKey::from_secret(state.config.session_key.as_bytes()),
        &validation,
    ) {
        Ok(t) => t.claims,
        Err(_) => return AppError::Unauthorized.into_response(),
    };

    let identity = match oidc.finish_flow(pending, &q.state, &q.code).await {
        Ok(i) => i,
        Err(e) => {
            return AppError::Other(e.context("oidc callback")).into_response();
        }
    };

    // Resolve the canonical sub according to configured format.
    let canonical_sub = match state.config.oidc_sub_format {
        OidcSubFormat::Raw => identity.sub.clone(),
        OidcSubFormat::Github => identity
            .preferred_username
            .as_deref()
            .map(|u| format!("github:{u}"))
            .unwrap_or_else(|| identity.sub.clone()),
    };

    // First-admin bootstrap convenience.
    if state.config.oidc_initial_admin_sub.as_deref() == Some(&canonical_sub) {
        // If users.toml is empty, auto-add this user as admin.
        let bootstrap_sub = canonical_sub.clone();
        let display = identity.name.clone();
        let _ = cas_update(&state.s3, &state.db, move |f| {
            if f.users.iter().any(|u| u.sub == bootstrap_sub) {
                return Ok(());
            }
            f.users.push(User {
                sub: bootstrap_sub.clone(),
                display_name: display.clone(),
                blocked: false,
                added_at: Some(chrono::Utc::now()),
                added_by: Some("oidc:initial".into()),
                roles: vec!["admin".into()],
                api_key_hash: None,
            });
            Ok(())
        })
        .await;
    }

    // Look up the user in the cache (which mirrors users.toml).
    let user = match state.db.get_user(&canonical_sub) {
        Ok(Some(u)) => u,
        Ok(None) => {
            return AppError::Forbidden.into_response();
        }
        Err(e) => return AppError::Other(e).into_response(),
    };
    if user.blocked {
        return AppError::Forbidden.into_response();
    }

    let token = match encode_session(&state.config, &canonical_sub, OIDC_SOURCE, user.roles) {
        Ok(t) => t,
        Err(e) => return AppError::Other(e).into_response(),
    };

    // Redirect to /admin and set the session cookie + clear the pending one.
    let mut resp = Redirect::temporary("/admin").into_response();
    resp.headers_mut().append(
        header::SET_COOKIE,
        set_cookie_header(&state.config, &token),
    );
    let clear = HeaderValue::from_static(
        "rawdb_oidc_pending=; Path=/auth/oidc; HttpOnly; SameSite=Lax; Max-Age=0",
    );
    resp.headers_mut().append(header::SET_COOKIE, clear);
    resp
}

#[cfg(not(feature = "oidc"))]
pub async fn oidc_callback_stub(_state: State<AppState>) -> Response {
    AppError::NotFound.into_response()
}

// ---- GitHub OAuth 2.0 -----------------------------------------------------
//
// Mirrors the OIDC start/callback pair. GitHub isn't an OIDC provider for
// user login, so this uses the OAuth 2.0 client in `auth_github.rs` and
// resolves the identity by calling `api.github.com/user`. The synthetic
// `sub` is always `github:<login>` so users.toml entries are stable across
// the user changing their display name on GitHub.

/// Begin the GitHub auth-code+PKCE flow. Redirects to GitHub and sets a
/// short-lived pending-flow cookie. Returns 404 when GitHub OAuth isn't
/// configured.
#[utoipa::path(
    get,
    path = "/auth/github/start",
    tag = "auth",
    responses(
        (status = 307, description = "Redirect to GitHub"),
        (status = 404, description = "GitHub OAuth not configured"),
    ),
)]
pub async fn github_start(State(state): State<AppState>) -> Response {
    use jsonwebtoken::{encode, EncodingKey, Header};

    let Some(gh) = state.github.clone() else {
        return AppError::NotFound.into_response();
    };
    let (url, pending) = gh.start_flow();
    let token = match encode(
        &Header::new(jsonwebtoken::Algorithm::HS256),
        &pending,
        &EncodingKey::from_secret(state.config.session_key.as_bytes()),
    ) {
        Ok(t) => t,
        Err(e) => return AppError::Other(e.into()).into_response(),
    };
    let cookie = format!(
        "rawdb_github_pending={token}; Path=/auth/github; HttpOnly; SameSite=Lax; Max-Age=600"
    );
    let mut resp = Redirect::temporary(&url).into_response();
    resp.headers_mut()
        .append(header::SET_COOKIE, HeaderValue::from_str(&cookie).unwrap());
    resp
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct GithubCallbackParams {
    /// Authorization code returned by GitHub.
    pub code: String,
    /// CSRF state token issued at `/auth/github/start`.
    pub state: String,
}

/// GitHub OAuth code-exchange callback. Validates the pending-flow cookie,
/// exchanges the code, builds the canonical `sub` (`github:<login>`),
/// looks up the user, and issues a session cookie. Redirects to `/admin`
/// on success.
#[utoipa::path(
    get,
    path = "/auth/github/callback",
    tag = "auth",
    params(GithubCallbackParams),
    responses(
        (status = 307, description = "Session cookie set; redirect to /admin"),
        (status = 401, description = "Pending-flow cookie missing or invalid"),
        (status = 403, description = "User unknown or blocked"),
        (status = 404, description = "GitHub OAuth not configured"),
    ),
)]
pub async fn github_callback(
    State(state): State<AppState>,
    Query(q): Query<GithubCallbackParams>,
    headers: axum::http::HeaderMap,
) -> Response {
    use crate::auth_github::PendingFlow as GhPendingFlow;
    use crate::users::{cas_update, User};
    use jsonwebtoken::{decode, DecodingKey, Validation};

    let Some(gh) = state.github.clone() else {
        return AppError::NotFound.into_response();
    };

    let pending_token = match cookie_value(&headers, "rawdb_github_pending") {
        Some(v) => v,
        None => {
            return AppError::BadRequest("missing github pending cookie".into()).into_response();
        }
    };
    let validation = Validation::new(jsonwebtoken::Algorithm::HS256);
    let pending: GhPendingFlow = match decode::<GhPendingFlow>(
        &pending_token,
        &DecodingKey::from_secret(state.config.session_key.as_bytes()),
        &validation,
    ) {
        Ok(t) => t.claims,
        Err(_) => return AppError::Unauthorized.into_response(),
    };

    let identity = match gh.finish_flow(pending, &q.state, &q.code).await {
        Ok(i) => i,
        Err(e) => return AppError::Other(e.context("github callback")).into_response(),
    };

    // Canonical sub: `github:<login>`. Matches OidcSubFormat::Github so a
    // user can move between RawDB's two GitHub paths without changing
    // their users.toml row.
    let canonical_sub = format!("github:{}", identity.login);

    // First-admin bootstrap convenience.
    if state.config.github_initial_admin_sub.as_deref() == Some(&canonical_sub) {
        let bootstrap_sub = canonical_sub.clone();
        let display = identity.name.clone().or_else(|| Some(identity.login.clone()));
        let _ = cas_update(&state.s3, &state.db, move |f| {
            if f.users.iter().any(|u| u.sub == bootstrap_sub) {
                return Ok(());
            }
            f.users.push(User {
                sub: bootstrap_sub.clone(),
                display_name: display.clone(),
                blocked: false,
                added_at: Some(chrono::Utc::now()),
                added_by: Some("github:initial".into()),
                roles: vec!["admin".into()],
                api_key_hash: None,
            });
            Ok(())
        })
        .await;
    }

    let user = match state.db.get_user(&canonical_sub) {
        Ok(Some(u)) => u,
        Ok(None) => return AppError::Forbidden.into_response(),
        Err(e) => return AppError::Other(e).into_response(),
    };
    if user.blocked {
        return AppError::Forbidden.into_response();
    }

    let token = match encode_session(&state.config, &canonical_sub, OIDC_SOURCE, user.roles) {
        Ok(t) => t,
        Err(e) => return AppError::Other(e).into_response(),
    };

    let mut resp = Redirect::temporary("/admin").into_response();
    resp.headers_mut().append(
        header::SET_COOKIE,
        set_cookie_header(&state.config, &token),
    );
    let clear = HeaderValue::from_static(
        "rawdb_github_pending=; Path=/auth/github; HttpOnly; SameSite=Lax; Max-Age=0",
    );
    resp.headers_mut().append(header::SET_COOKIE, clear);
    resp
}

fn cookie_value(headers: &axum::http::HeaderMap, name: &str) -> Option<String> {
    let raw = headers.get(header::COOKIE)?.to_str().ok()?;
    for kv in raw.split(';') {
        let kv = kv.trim();
        if let Some(v) = kv.strip_prefix(&format!("{name}=")) {
            return Some(v.to_string());
        }
    }
    None
}

// Re-export so other modules can decode without pulling the full auth module
// in their imports (kept for parity with the original skeleton).
#[allow(dead_code)]
pub fn decode_for_test(cfg: &crate::config::Config, token: &str) -> Option<crate::auth::Session> {
    decode_session(cfg, token)
}
