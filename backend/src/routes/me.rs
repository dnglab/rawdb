//! Self-service endpoints for the logged-in user. Currently: personal
//! API-key management. Only `apiservice`-role users may hold a key; one
//! key per user (regenerating replaces the old one).

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;
use utoipa::ToSchema;

use crate::apikey::{self, ROLE_APISERVICE};
use crate::auth::{SessionExtractor, BOOTSTRAP_SUB};
use crate::error::{AppError, AppResult};
use crate::routes::admin::map_users_error;
use crate::state::AppState;
use crate::users::{cas_update, UsersError};

pub fn router() -> Router<AppState> {
    Router::new().route(
        "/me/api-key",
        get(get_api_key).post(create_api_key).delete(delete_api_key),
    )
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ApiKeyStatus {
    /// Whether the caller currently holds an API key.
    pub has_key: bool,
    /// Whether the caller is allowed to hold one (has the `apiservice` role).
    pub eligible: bool,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ApiKeyCreated {
    /// The plaintext key — returned exactly once, never retrievable again.
    /// Sent as `X-API-Key` on download / export requests.
    pub api_key: String,
}

/// Current API-key status for the caller: whether one exists and whether
/// the caller is eligible to create one.
#[utoipa::path(
    get,
    path = "/api/me/api-key",
    tag = "auth",
    security(("session_cookie" = [])),
    responses(
        (status = 200, body = ApiKeyStatus),
        (status = 401, description = "Not authenticated"),
    ),
)]
pub async fn get_api_key(
    State(state): State<AppState>,
    SessionExtractor(sess): SessionExtractor,
) -> AppResult<Json<ApiKeyStatus>> {
    // The bootstrap admin isn't a users.toml record and can't hold a key.
    if sess.sub == BOOTSTRAP_SUB {
        return Ok(Json(ApiKeyStatus {
            has_key: false,
            eligible: false,
        }));
    }
    let user = state.db.get_user(&sess.sub).map_err(AppError::Other)?;
    let (has_key, eligible) = match user {
        Some(u) => (
            u.api_key_hash.is_some(),
            u.roles.iter().any(|r| r == ROLE_APISERVICE),
        ),
        None => (false, false),
    };
    Ok(Json(ApiKeyStatus { has_key, eligible }))
}

/// Generate (or regenerate) the caller's API key. Requires the
/// `apiservice` role — checked live against the users file, not the
/// session token. The returned plaintext is shown only here.
#[utoipa::path(
    post,
    path = "/api/me/api-key",
    tag = "auth",
    security(("session_cookie" = [])),
    responses(
        (status = 200, body = ApiKeyCreated),
        (status = 400, description = "Bootstrap admin cannot hold a key"),
        (status = 403, description = "Caller lacks the `apiservice` role"),
    ),
)]
pub async fn create_api_key(
    State(state): State<AppState>,
    SessionExtractor(sess): SessionExtractor,
) -> AppResult<Json<ApiKeyCreated>> {
    if sess.sub == BOOTSTRAP_SUB {
        return Err(AppError::BadRequest(
            "the bootstrap admin account cannot hold an API key".into(),
        ));
    }
    let user = state
        .db
        .get_user(&sess.sub)
        .map_err(AppError::Other)?
        .ok_or(AppError::Forbidden)?;
    if !user.roles.iter().any(|r| r == ROLE_APISERVICE) {
        return Err(AppError::Forbidden);
    }

    let key = apikey::generate();
    let key_hash = apikey::hash(&key);
    let target = sess.sub.clone();
    cas_update(&state, move |f| {
        match f.users.iter_mut().find(|u| u.sub == target) {
            Some(u) => {
                u.api_key_hash = Some(key_hash.clone());
                Ok(())
            }
            None => Err(UsersError::NotFound(target.clone())),
        }
    })
    .await
    .map_err(map_users_error)?;

    Ok(Json(ApiKeyCreated { api_key: key }))
}

/// Revoke the caller's API key, if any. Idempotent.
#[utoipa::path(
    delete,
    path = "/api/me/api-key",
    tag = "auth",
    security(("session_cookie" = [])),
    responses(
        (status = 204, description = "Key revoked (or none existed)"),
        (status = 400, description = "Bootstrap admin has no key"),
    ),
)]
pub async fn delete_api_key(
    State(state): State<AppState>,
    SessionExtractor(sess): SessionExtractor,
) -> AppResult<StatusCode> {
    if sess.sub == BOOTSTRAP_SUB {
        return Err(AppError::BadRequest(
            "the bootstrap admin account has no API key".into(),
        ));
    }
    let target = sess.sub.clone();
    cas_update(&state, move |f| {
        if let Some(u) = f.users.iter_mut().find(|u| u.sub == target) {
            u.api_key_hash = None;
        }
        // Missing user / no key → no-op; cas_update's fast path returns Ok.
        Ok(())
    })
    .await
    .map_err(map_users_error)?;
    Ok(StatusCode::NO_CONTENT)
}
