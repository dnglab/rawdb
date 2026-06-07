//! `_system/users.toml` read/conditional-write helpers.
//!
//! The file is the source-of-truth for OIDC user identities and their
//! role assignments. Writes use S3's `If-Match` header so concurrent
//! edits from a peer pod can be detected and the change re-applied
//! against fresh state.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::cache::db::UserRow;
use crate::s3::{S3Error, S3};
use crate::state::AppState;
use crate::sync_tick::{self, Domain};

pub const USERS_KEY: &str = "_system/users.toml";
const MAX_CAS_RETRIES: usize = 5;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    pub sub: String,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub blocked: bool,
    #[serde(default)]
    pub added_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub added_by: Option<String>,
    #[serde(default)]
    pub roles: Vec<String>,
    /// SHA-256 (hex) of the user's personal API key, if one has been
    /// generated. Only `apiservice`-role users may hold one. The plaintext
    /// key is shown to the user exactly once at generation time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_hash: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UsersFile {
    #[serde(default)]
    pub users: Vec<User>,
}

#[derive(Debug, thiserror::Error)]
pub enum UsersError {
    #[error("concurrent write — retries exhausted")]
    ConcurrentWrite,
    #[error("user not found: {0}")]
    NotFound(String),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// Apply `mutate` to the current users file. Reads, calls the mutator with
/// a `&mut UsersFile`, then writes back with `If-Match` against the original
/// ETag. Retries on precondition failure up to `MAX_CAS_RETRIES` times.
///
/// On a successful write, bumps the `Users` sync-tick so peer pods pick
/// up the change within `RAWDB_SYNC_POLL_SECS` instead of waiting for
/// the next periodic full scan.
pub async fn cas_update<F>(state: &AppState, mut mutate: F) -> Result<UsersFile, UsersError>
where
    F: FnMut(&mut UsersFile) -> Result<(), UsersError>,
{
    let s3 = &state.s3;
    let db = &state.db;
    for _ in 0..MAX_CAS_RETRIES {
        let (current, current_etag) = read_with_etag(s3).await?;
        let mut next = current.clone();
        mutate(&mut next)?;
        if next.users == current.users {
            // Nothing to do.
            return Ok(next);
        }

        let body = toml::to_string_pretty(&next)
            .map_err(|e| anyhow::anyhow!("serialize users.toml: {e}"))?
            .into_bytes();

        let write = if !s3.conditional_writes() {
            // Backend doesn't honor conditional PUTs — plain write,
            // last-writer-wins. Acceptable for human-paced user edits.
            s3.put_bytes(USERS_KEY, body.clone(), Some("application/toml"), None, None)
                .await
        } else if let Some(etag) = current_etag.as_deref() {
            s3.put_bytes(
                USERS_KEY,
                body.clone(),
                Some("application/toml"),
                Some(&format!("\"{etag}\"")),
                None,
            )
            .await
        } else {
            // File didn't exist — create-only.
            s3.put_bytes(
                USERS_KEY,
                body.clone(),
                Some("application/toml"),
                None,
                Some("*"),
            )
            .await
        };

        match write {
            Ok(new_etag) => {
                let rows: Vec<UserRow> = next.users.iter().cloned().map(to_db_row).collect();
                db.replace_users(Some(&new_etag), &rows).map_err(|e| {
                    UsersError::Other(anyhow::anyhow!("replace_users cache update: {e}"))
                })?;
                sync_tick::bump(state, &[Domain::Users], "users_cas_update").await;
                return Ok(next);
            }
            Err(S3Error::PreconditionFailed) => continue,
            Err(S3Error::NotFound(k)) => {
                return Err(UsersError::Other(anyhow::anyhow!("unexpected NotFound: {k}")))
            }
            Err(S3Error::Other(e)) => return Err(UsersError::Other(e)),
        }
    }
    // Every conditional PUT was rejected. A genuine write storm is unlikely
    // for human-paced user edits — the usual cause is an S3 backend that
    // doesn't honor `If-Match` on PutObject (Ceph RGW / Hetzner). Point the
    // operator at the escape hatch.
    tracing::error!(
        "users.toml conditional write exhausted {MAX_CAS_RETRIES} retries — \
         if your S3 backend doesn't support If-Match on PutObject \
         (e.g. Hetzner / Ceph RGW), set RAWDB_S3_CONDITIONAL_WRITES=false"
    );
    Err(UsersError::ConcurrentWrite)
}

/// Read the users file, returning the parsed content + its current ETag.
/// A missing file yields an empty `UsersFile` and `None` etag.
pub async fn read_with_etag(s3: &S3) -> Result<(UsersFile, Option<String>), anyhow::Error> {
    match s3.get_bytes(USERS_KEY).await {
        Ok((bytes, etag)) => {
            let s = std::str::from_utf8(&bytes).context("users.toml utf-8")?;
            let file: UsersFile =
                toml::from_str(s).context("users.toml parse")?;
            Ok((file, etag))
        }
        Err(S3Error::NotFound(_)) => Ok((UsersFile::default(), None)),
        Err(S3Error::PreconditionFailed) => Err(anyhow::anyhow!("unexpected precondition on GET")),
        Err(S3Error::Other(e)) => Err(e),
    }
}

pub fn to_db_row(u: User) -> UserRow {
    UserRow {
        sub: u.sub,
        display_name: u.display_name,
        blocked: u.blocked,
        added_at: u.added_at,
        added_by: u.added_by,
        roles: u.roles,
        api_key_hash: u.api_key_hash,
    }
}

// Hand-comparable equality for the CAS no-op fast path.
impl PartialEq for User {
    fn eq(&self, other: &Self) -> bool {
        self.sub == other.sub
            && self.display_name == other.display_name
            && self.blocked == other.blocked
            && self.added_at == other.added_at
            && self.added_by == other.added_by
            && self.roles == other.roles
            && self.api_key_hash == other.api_key_hash
    }
}
impl Eq for User {}
