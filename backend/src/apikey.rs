//! Personal API keys for `unlimited`-role users.
//!
//! A key lets a dedicated user (a) bypass the per-IP download rate limit
//! and (b) call the key-protected `/api/export` endpoint. Keys are
//! generated server-side, shown to the user exactly once, and stored only
//! as a SHA-256 hash on the user's `_system/users.toml` record — the
//! plaintext is never persisted.
//!
//! SHA-256 (not argon2) is deliberate: an API key is a 256-bit random
//! token, so there's nothing to brute-force, and the hash is computed on
//! every download request — it must be cheap.

use axum::http::HeaderMap;
use sha2::{Digest, Sha256};

use crate::cache::db::{Db, UserRow};

/// Header the client sends the key in.
pub const API_KEY_HEADER: &str = "x-api-key";

/// Role required to own an API key (and, by extension, to use one).
pub const ROLE_UNLIMITED: &str = "unlimited";

/// Human-recognizable prefix on every generated key.
const KEY_PREFIX: &str = "rawdb_";

/// Mint a fresh API key: `rawdb_` + 64 hex chars (256 bits of entropy).
pub fn generate() -> String {
    let bytes: [u8; 32] = rand::random();
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!("{KEY_PREFIX}{hex}")
}

/// SHA-256 of a key, lower-case hex. This is what's stored and compared.
pub fn hash(key: &str) -> String {
    let digest = Sha256::digest(key.as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// Resolve an `X-API-Key` request header to its owning user, or `None`
/// when the header is absent, malformed, or the key doesn't currently
/// authorize anything.
///
/// The check is intentionally re-evaluated live against the cache: a user
/// who is blocked or who has lost the `unlimited` role no longer passes,
/// even though their key string is unchanged.
pub fn lookup(db: &Db, headers: &HeaderMap) -> Option<UserRow> {
    let raw = headers.get(API_KEY_HEADER)?.to_str().ok()?.trim();
    if raw.is_empty() {
        return None;
    }
    let user = db.find_user_by_api_key_hash(&hash(raw)).ok()??;
    if user.blocked {
        return None;
    }
    if !user.roles.iter().any(|r| r == ROLE_UNLIMITED) {
        return None;
    }
    Some(user)
}
