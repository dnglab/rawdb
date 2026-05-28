//! Authentication primitives: password verification, JWT session cookie,
//! and a `RequireRole` extractor used by admin/reviewer handlers.
//!
//! Two login paths produce the same `Session` JWT cookie:
//! - **password**: `POST /auth/login` checks `RAWDB_ADMIN_PASSWORD[_HASH]`
//!   and issues a session for the synthetic `bootstrap:admin` user.
//! - **oidc**: `/auth/oidc/callback` validates the IdP response and looks
//!   the user up in `_system/users.toml` (wired in Phase 9).
//!
//! The JWT is symmetric HS256 over `RAWDB_SESSION_KEY`. Sessions are
//! stateless — there is no server-side session store — which keeps the
//! multi-pod deployment trivial.

use std::sync::Arc;

use argon2::password_hash::PasswordVerifier;
use argon2::{Argon2, PasswordHash, PasswordHasher};
use axum::async_trait;
use axum::extract::{FromRequestParts};
use axum::http::header::{COOKIE, SET_COOKIE};
use axum::http::request::Parts;
use axum::http::HeaderValue;
use axum::response::{IntoResponse, Response};
use chrono::Utc;
use jsonwebtoken::{decode, encode, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::error::AppError;
use crate::state::AppState;

pub const SESSION_COOKIE: &str = "rawdb_session";
pub const BOOTSTRAP_SUB: &str = "bootstrap:admin";
pub const PASSWORD_SOURCE: &str = "password";
pub const OIDC_SOURCE: &str = "oidc";
pub const ROLE_ADMIN: &str = "admin";
pub const ROLE_REVIEWER: &str = "reviewer";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub sub: String,
    pub source: String,
    pub roles: Vec<String>,
    pub exp: i64,
}

impl Session {
    pub fn has_role(&self, role: &str) -> bool {
        self.roles.iter().any(|r| r == role)
    }

    pub fn has_any(&self, roles: &[&str]) -> bool {
        roles.iter().any(|r| self.has_role(r))
    }
}

// ---- password verification ------------------------------------------------

/// Verify `submitted` against the configured password / hash.
pub fn verify_admin_password(cfg: &Config, submitted: &str) -> bool {
    if let Some(hash) = &cfg.admin_password_hash {
        if let Ok(parsed) = PasswordHash::new(hash) {
            return Argon2::default()
                .verify_password(submitted.as_bytes(), &parsed)
                .is_ok();
        }
        return false;
    }
    if let Some(plain) = &cfg.admin_password {
        // Constant-time comparison (argon2's verify is constant-time; for
        // plain-text comparison, fall back to a manual constant-time check).
        let a = plain.as_bytes();
        let b = submitted.as_bytes();
        if a.len() != b.len() {
            return false;
        }
        let mut diff: u8 = 0;
        for i in 0..a.len() {
            diff |= a[i] ^ b[i];
        }
        return diff == 0;
    }
    false
}

/// Optional helper exposed so admins can pre-hash a password into a value
/// suitable for `RAWDB_ADMIN_PASSWORD_HASH`.
#[allow(dead_code)]
pub fn hash_admin_password(plain: &str) -> anyhow::Result<String> {
    use argon2::password_hash::{rand_core::OsRng, SaltString};
    let salt = SaltString::generate(&mut OsRng);
    Ok(Argon2::default()
        .hash_password(plain.as_bytes(), &salt)
        .map_err(|e| anyhow::anyhow!("{e}"))?
        .to_string())
}

// ---- session encoding / cookie -------------------------------------------

pub fn encode_session(cfg: &Config, sub: &str, source: &str, roles: Vec<String>) -> anyhow::Result<String> {
    let exp = Utc::now()
        .timestamp()
        .saturating_add(cfg.session_ttl_secs as i64);
    let session = Session {
        sub: sub.to_string(),
        source: source.to_string(),
        roles,
        exp,
    };
    let token = encode(
        &Header::new(jsonwebtoken::Algorithm::HS256),
        &session,
        &EncodingKey::from_secret(cfg.session_key.as_bytes()),
    )?;
    Ok(token)
}

pub fn decode_session(cfg: &Config, token: &str) -> Option<Session> {
    let validation = Validation::new(jsonwebtoken::Algorithm::HS256);
    decode::<Session>(
        token,
        &DecodingKey::from_secret(cfg.session_key.as_bytes()),
        &validation,
    )
    .ok()
    .map(|t| t.claims)
}

pub fn set_cookie_header(cfg: &Config, token: &str) -> HeaderValue {
    let secure = !cfg.bind.ip().is_loopback();
    let secure_flag = if secure { "; Secure" } else { "" };
    let value = format!(
        "{SESSION_COOKIE}={token}; Path=/; HttpOnly; SameSite=Lax{secure_flag}; Max-Age={}",
        cfg.session_ttl_secs
    );
    HeaderValue::from_str(&value).expect("cookie ASCII")
}

pub fn clear_cookie_header() -> HeaderValue {
    HeaderValue::from_static(
        "rawdb_session=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0",
    )
}

// ---- extractors ----------------------------------------------------------

/// Decoded session, derived from the `rawdb_session` cookie. Returns 401 if
/// missing or invalid.
pub struct SessionExtractor(pub Session);

/// Like [`SessionExtractor`] but never rejects — `None` when the cookie
/// is absent or the JWT is invalid/expired. Useful on endpoints that
/// accept both authenticated and anonymous callers but want to *record*
/// who the caller was when one is present (e.g. emitting events that
/// name the uploader on success but still allow public uploads).
pub struct OptionalSession(pub Option<Session>);

#[async_trait]
impl FromRequestParts<AppState> for OptionalSession {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let session = extract_cookie(parts, SESSION_COOKIE)
            .and_then(|token| decode_session(&state.config, &token));
        Ok(OptionalSession(session))
    }
}

#[async_trait]
impl FromRequestParts<AppState> for SessionExtractor {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let Some(token) = extract_cookie(parts, SESSION_COOKIE) else {
            return Err(AppError::Unauthorized.into_response());
        };
        let Some(sess) = decode_session(&state.config, &token) else {
            return Err(AppError::Unauthorized.into_response());
        };
        Ok(SessionExtractor(sess))
    }
}

/// Gate handlers behind any of the given roles. The synthetic
/// `bootstrap:admin` user always satisfies the check; "blocked" users
/// (per `users.toml`) are rejected.
pub struct RequireRole<const ROLE_BITS: u8>;

pub const REVIEWER_BIT: u8 = 0b01;
pub const ADMIN_BIT: u8 = 0b10;
pub const ADMIN_ONLY: u8 = ADMIN_BIT;
pub const REVIEWER_OR_ADMIN: u8 = ADMIN_BIT | REVIEWER_BIT;

pub struct AuthGuard<const ROLE_BITS: u8>(pub Session);

#[async_trait]
impl<const ROLE_BITS: u8> FromRequestParts<AppState> for AuthGuard<ROLE_BITS> {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let SessionExtractor(sess) = SessionExtractor::from_request_parts(parts, state).await?;
        // Check blocked status (skip for bootstrap admin).
        if sess.sub != BOOTSTRAP_SUB {
            if let Some(u) = state.db.get_user(&sess.sub).map_err(internal)? {
                if u.blocked {
                    return Err(AppError::Forbidden.into_response());
                }
            }
        }
        let allowed = is_role_allowed::<ROLE_BITS>(&sess);
        if !allowed {
            return Err(AppError::Forbidden.into_response());
        }
        Ok(AuthGuard(sess))
    }
}

fn is_role_allowed<const ROLE_BITS: u8>(sess: &Session) -> bool {
    if (ROLE_BITS & ADMIN_BIT) != 0 && sess.has_role(ROLE_ADMIN) {
        return true;
    }
    if (ROLE_BITS & REVIEWER_BIT) != 0 && sess.has_role(ROLE_REVIEWER) {
        return true;
    }
    false
}

fn internal(e: anyhow::Error) -> Response {
    AppError::Other(e).into_response()
}

fn extract_cookie(parts: &Parts, name: &str) -> Option<String> {
    let header = parts.headers.get(COOKIE)?.to_str().ok()?;
    for kv in header.split(';') {
        let kv = kv.trim();
        if let Some(v) = kv.strip_prefix(&format!("{name}=")) {
            return Some(v.to_string());
        }
    }
    None
}

// ---- header helper for setting cookies on responses ----------------------

pub fn with_session_cookie(cfg: &Arc<Config>, token: &str, mut resp: Response) -> Response {
    resp.headers_mut()
        .append(SET_COOKIE, set_cookie_header(cfg, token));
    resp
}

pub fn with_cleared_cookie(mut resp: Response) -> Response {
    resp.headers_mut().append(SET_COOKIE, clear_cookie_header());
    resp
}
