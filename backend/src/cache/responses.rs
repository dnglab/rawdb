//! In-memory cache of complete response bodies for read endpoints whose
//! data changes rarely (Atom feeds, stats, catalog lists).
//!
//! An entry is valid while [`Db::stamp`](crate::cache::db::Db::stamp) over
//! the domains it depends on is unchanged; any committed write to one of
//! them makes the next request rebuild. Each body carries a weak ETag (a
//! content hash, so it holds across pods and restarts) and clients that
//! already have it get `304 Not Modified`.

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};

use axum::body::Bytes;
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE, ETAG, IF_NONE_MATCH};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use opentelemetry::metrics::Counter;
use opentelemetry::KeyValue;
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::error::{AppError, AppResult};

/// Upper bound on cached entries. Feed keys include the request-derived
/// base URL, so arbitrary `Host` headers must not grow the map unbounded;
/// inserting a new key into a full map clears it.
const MAX_ENTRIES: usize = 256;

pub const JSON: &str = "application/json";
/// Revalidate on every use: clients always see fresh data, mostly as 304s.
pub const NO_CACHE: &str = "no-cache";

#[derive(Debug, Clone)]
pub struct Cached {
    stamp: u64,
    pub body: Bytes,
    pub etag: String,
    pub content_type: &'static str,
}

#[derive(Debug)]
pub struct ResponseCache {
    entries: Mutex<HashMap<String, Cached>>,
    /// `rawdb.response_cache.lookups`, labelled `cache` and `result`.
    lookups: Counter<u64>,
}

impl ResponseCache {
    pub fn new(lookups: Counter<u64>) -> Self {
        Self {
            entries: Mutex::default(),
            lookups,
        }
    }

    /// The cached body for `name`/`variant` if it was built at `stamp`;
    /// otherwise run `build`, store and return the result. `name` is a
    /// fixed per-endpoint label for the lookup metric; `variant` holds the
    /// request-dependent part of the key (base URL, query flags). Take
    /// `stamp` *before* calling so a write landing mid-build leaves a
    /// stale stamp behind.
    pub fn get_or_build(
        &self,
        name: &'static str,
        variant: &str,
        stamp: u64,
        content_type: &'static str,
        build: impl FnOnce() -> AppResult<Vec<u8>>,
    ) -> AppResult<Cached> {
        let key = format!("{name}:{variant}");
        let hit = self.lock().get(&key).filter(|c| c.stamp == stamp).cloned();
        let result = if hit.is_some() { "hit" } else { "miss" };
        self.lookups.add(
            1,
            &[KeyValue::new("cache", name), KeyValue::new("result", result)],
        );
        if let Some(hit) = hit {
            return Ok(hit);
        }
        // Built outside the lock; concurrent misses may both build, which
        // is harmless.
        let body = Bytes::from(build()?);
        let entry = Cached {
            stamp,
            etag: etag_for(&body),
            body,
            content_type,
        };
        let mut entries = self.lock();
        if entries.len() >= MAX_ENTRIES && !entries.contains_key(&key) {
            entries.clear();
        }
        entries.insert(key, entry.clone());
        Ok(entry)
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<String, Cached>> {
        // A panic mid-insert leaves only cache entries behind; keep going.
        self.entries.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Serialize a JSON response body for [`ResponseCache::get_or_build`].
pub fn json<T: Serialize>(value: &T) -> AppResult<Vec<u8>> {
    serde_json::to_vec(value).map_err(|e| AppError::Other(e.into()))
}

/// `304` when the request's `If-None-Match` matches, else `200` with the
/// cached body.
pub fn respond(cached: &Cached, headers: &HeaderMap, cache_control: &'static str) -> Response {
    let etag = HeaderValue::from_str(&cached.etag).expect("hex ETag is a valid header value");
    let cache_control = HeaderValue::from_static(cache_control);
    if if_none_match(headers, &cached.etag) {
        return (
            StatusCode::NOT_MODIFIED,
            [(ETAG, etag), (CACHE_CONTROL, cache_control)],
        )
            .into_response();
    }
    (
        [
            (CONTENT_TYPE, HeaderValue::from_static(cached.content_type)),
            (ETAG, etag),
            (CACHE_CONTROL, cache_control),
        ],
        cached.body.clone(),
    )
        .into_response()
}

/// Weak comparison (RFC 9110): `W/` prefixes are ignored; `*` matches.
fn if_none_match(headers: &HeaderMap, etag: &str) -> bool {
    let ours = etag.trim_start_matches("W/");
    headers
        .get_all(IF_NONE_MATCH)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(str::trim)
        .any(|t| t == "*" || t.trim_start_matches("W/") == ours)
}

/// Weak, because `CompressionLayer` may serve a gzip representation of
/// the same body.
fn etag_for(body: &[u8]) -> String {
    let digest = Sha256::digest(body);
    let hex: String = digest[..16].iter().map(|b| format!("{b:02x}")).collect();
    format!("W/\"{hex}\"")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn new_cache() -> ResponseCache {
        // No meter provider in tests: the global meter is a no-op.
        ResponseCache::new(opentelemetry::global::meter("test").u64_counter("test").build())
    }

    fn build_counting<'a>(calls: &'a Cell<u32>, body: &'a str) -> impl FnOnce() -> AppResult<Vec<u8>> + 'a {
        move || {
            calls.set(calls.get() + 1);
            Ok(body.as_bytes().to_vec())
        }
    }

    #[test]
    fn hit_reuses_body_and_new_stamp_rebuilds() {
        let cache = new_cache();
        let calls = Cell::new(0);
        let a = cache.get_or_build("t", "k", 1, JSON, build_counting(&calls, "one")).unwrap();
        let b = cache.get_or_build("t", "k", 1, JSON, build_counting(&calls, "two")).unwrap();
        assert_eq!(calls.get(), 1);
        assert_eq!(b.body, "one");
        assert_eq!(a.etag, b.etag);

        let c = cache.get_or_build("t", "k", 2, JSON, build_counting(&calls, "two")).unwrap();
        assert_eq!(calls.get(), 2);
        assert_eq!(c.body, "two");
        assert_ne!(a.etag, c.etag);
    }

    #[test]
    fn build_error_is_not_cached() {
        let cache = new_cache();
        let err = cache.get_or_build("t", "k", 1, JSON, || Err(AppError::NotFound));
        assert!(err.is_err());
        let calls = Cell::new(0);
        cache.get_or_build("t", "k", 1, JSON, build_counting(&calls, "ok")).unwrap();
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn full_map_is_cleared_before_new_key() {
        let cache = new_cache();
        for i in 0..MAX_ENTRIES {
            cache.get_or_build("t", &format!("k{i}"), 0, JSON, || Ok(vec![])).unwrap();
        }
        // Updating an existing key keeps the map.
        cache.get_or_build("t", "k0", 1, JSON, || Ok(vec![])).unwrap();
        assert_eq!(cache.lock().len(), MAX_ENTRIES);
        cache.get_or_build("t", "new", 0, JSON, || Ok(vec![])).unwrap();
        assert_eq!(cache.lock().len(), 1);
    }

    #[test]
    fn if_none_match_weak_comparison() {
        let etag = r#"W/"abc""#;
        let check = |v: &str| {
            let mut h = HeaderMap::new();
            h.insert(IF_NONE_MATCH, v.parse().unwrap());
            if_none_match(&h, etag)
        };
        assert!(check(r#"W/"abc""#));
        assert!(check(r#""abc""#));
        assert!(check(r#""x", W/"abc""#));
        assert!(check("*"));
        assert!(!check(r#"W/"abd""#));
        assert!(!if_none_match(&HeaderMap::new(), etag));
    }

    #[test]
    fn respond_304_on_match_else_200() {
        let cache = new_cache();
        let c = cache.get_or_build("t", "k", 0, JSON, || Ok(b"{}".to_vec())).unwrap();

        let ok = respond(&c, &HeaderMap::new(), NO_CACHE);
        assert_eq!(ok.status(), StatusCode::OK);
        assert_eq!(ok.headers()[ETAG], c.etag.as_str());
        assert_eq!(ok.headers()[CONTENT_TYPE], JSON);
        assert_eq!(ok.headers()[CACHE_CONTROL], NO_CACHE);

        let mut h = HeaderMap::new();
        h.insert(IF_NONE_MATCH, c.etag.parse().unwrap());
        let not_modified = respond(&c, &h, NO_CACHE);
        assert_eq!(not_modified.status(), StatusCode::NOT_MODIFIED);
        assert!(not_modified.headers().get(CONTENT_TYPE).is_none());
    }
}
