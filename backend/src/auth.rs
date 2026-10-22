//! Authentication primitives: password verification, JWT session cookie,
//! and a `RequireRole` extractor used by the role-gated admin handlers.
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
/// Curates pending uploads: edit tags/notes, rename and remove files.
pub const ROLE_REVIEWER: &str = "reviewer";
/// Approves or rejects pending uploads. Cannot edit them.
pub const ROLE_APPROVER: &str = "approver";
/// Edits metadata (and renames files) of approved sets. Cannot delete.
pub const ROLE_EDITOR: &str = "editor";
/// Deletes single files or whole approved sets.
pub const ROLE_DELETER: &str = "deleter";

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

pub const REVIEWER_BIT: u8 = 0b0_0001;
pub const ADMIN_BIT: u8 = 0b0_0010;
pub const APPROVER_BIT: u8 = 0b0_0100;
pub const EDITOR_BIT: u8 = 0b0_1000;
pub const DELETER_BIT: u8 = 0b1_0000;
pub const ADMIN_ONLY: u8 = ADMIN_BIT;
pub const REVIEWER_OR_ADMIN: u8 = ADMIN_BIT | REVIEWER_BIT;
pub const APPROVER_OR_ADMIN: u8 = ADMIN_BIT | APPROVER_BIT;
pub const EDITOR_OR_ADMIN: u8 = ADMIN_BIT | EDITOR_BIT;
pub const DELETER_OR_ADMIN: u8 = ADMIN_BIT | DELETER_BIT;
/// Read-only access to the pending queue (list, detail, download, verify).
pub const PENDING_VIEWERS: u8 = ADMIN_BIT | REVIEWER_BIT | APPROVER_BIT;

const ROLE_BIT_TABLE: &[(u8, &str)] = &[
    (ADMIN_BIT, ROLE_ADMIN),
    (REVIEWER_BIT, ROLE_REVIEWER),
    (APPROVER_BIT, ROLE_APPROVER),
    (EDITOR_BIT, ROLE_EDITOR),
    (DELETER_BIT, ROLE_DELETER),
];

pub struct AuthGuard<const ROLE_BITS: u8>(pub Session);

#[async_trait]
impl<const ROLE_BITS: u8> FromRequestParts<AppState> for AuthGuard<ROLE_BITS> {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let SessionExtractor(mut sess) = SessionExtractor::from_request_parts(parts, state).await?;
        // OIDC users: check blocked status and use their *live* roles from
        // the users.toml mirror rather than the ones baked into the JWT, so
        // role changes apply without a re-login. A user removed since
        // login is rejected, matching the login-time check. The bootstrap
        // admin has no users.toml entry and keeps its JWT roles.
        if sess.sub != BOOTSTRAP_SUB {
            let Some(u) = state.db.get_user(&sess.sub).map_err(internal)? else {
                return Err(AppError::Forbidden.into_response());
            };
            if u.blocked {
                return Err(AppError::Forbidden.into_response());
            }
            sess.roles = u.roles;
        }
        if !is_role_allowed::<ROLE_BITS>(&sess) {
            return Err(AppError::Forbidden.into_response());
        }
        Ok(AuthGuard(sess))
    }
}

fn is_role_allowed<const ROLE_BITS: u8>(sess: &Session) -> bool {
    ROLE_BIT_TABLE
        .iter()
        .any(|(bit, role)| (ROLE_BITS & bit) != 0 && sess.has_role(role))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn roles(r: &[&str]) -> Session {
        Session {
            sub: "oidc:test".into(),
            source: OIDC_SOURCE.into(),
            roles: r.iter().map(|s| s.to_string()).collect(),
            exp: 0,
        }
    }

    #[test]
    fn role_guards() {
        let admin = roles(&[ROLE_ADMIN]);
        let reviewer = roles(&[ROLE_REVIEWER]);
        let approver = roles(&[ROLE_APPROVER]);
        let editor = roles(&[ROLE_EDITOR]);
        let deleter = roles(&[ROLE_DELETER]);
        let api = roles(&["apiservice"]);

        // Admin passes every guard.
        assert!(is_role_allowed::<ADMIN_ONLY>(&admin));
        assert!(is_role_allowed::<PENDING_VIEWERS>(&admin));
        assert!(is_role_allowed::<REVIEWER_OR_ADMIN>(&admin));
        assert!(is_role_allowed::<APPROVER_OR_ADMIN>(&admin));
        assert!(is_role_allowed::<EDITOR_OR_ADMIN>(&admin));
        assert!(is_role_allowed::<DELETER_OR_ADMIN>(&admin));

        // Reviewer: view + edit pending, nothing else.
        assert!(is_role_allowed::<PENDING_VIEWERS>(&reviewer));
        assert!(is_role_allowed::<REVIEWER_OR_ADMIN>(&reviewer));
        assert!(!is_role_allowed::<APPROVER_OR_ADMIN>(&reviewer));
        assert!(!is_role_allowed::<EDITOR_OR_ADMIN>(&reviewer));
        assert!(!is_role_allowed::<DELETER_OR_ADMIN>(&reviewer));
        assert!(!is_role_allowed::<ADMIN_ONLY>(&reviewer));

        // Approver: view + approve/reject pending, no editing.
        assert!(is_role_allowed::<PENDING_VIEWERS>(&approver));
        assert!(is_role_allowed::<APPROVER_OR_ADMIN>(&approver));
        assert!(!is_role_allowed::<REVIEWER_OR_ADMIN>(&approver));
        assert!(!is_role_allowed::<EDITOR_OR_ADMIN>(&approver));
        assert!(!is_role_allowed::<DELETER_OR_ADMIN>(&approver));

        // Editor: approved-set metadata only.
        assert!(is_role_allowed::<EDITOR_OR_ADMIN>(&editor));
        assert!(!is_role_allowed::<DELETER_OR_ADMIN>(&editor));
        assert!(!is_role_allowed::<PENDING_VIEWERS>(&editor));
        assert!(!is_role_allowed::<ADMIN_ONLY>(&editor));

        // Deleter: approved-set deletion only.
        assert!(is_role_allowed::<DELETER_OR_ADMIN>(&deleter));
        assert!(!is_role_allowed::<EDITOR_OR_ADMIN>(&deleter));
        assert!(!is_role_allowed::<PENDING_VIEWERS>(&deleter));

        // Unrelated / no roles pass nothing.
        assert!(!is_role_allowed::<PENDING_VIEWERS>(&api));
        assert!(!is_role_allowed::<ADMIN_ONLY>(&roles(&[])));

        // Roles combine additively.
        let both = roles(&[ROLE_REVIEWER, ROLE_APPROVER]);
        assert!(is_role_allowed::<REVIEWER_OR_ADMIN>(&both));
        assert!(is_role_allowed::<APPROVER_OR_ADMIN>(&both));
    }
}
