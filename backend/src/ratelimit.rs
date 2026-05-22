//! Per-instance, per-IP download rate limiter — a second tier behind any
//! edge rate limiting (e.g. the Traefik middleware). Each backend pod
//! keeps its own in-memory window; there is no shared store, which is
//! fine because the limit is deliberately "per running instance".
//!
//! Algorithm: a sliding window of hit timestamps per client IP. A repeat
//! request for the *same path* within a short grace period reuses the
//! prior hit instead of consuming a new token — this collapses the
//! frontend's preflight + navigation into a single logical download and
//! absorbs accidental double-clicks.

use std::collections::{HashMap, VecDeque};
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Same-path grace window: a second request for an identical path from the
/// same IP within this period is free. Sized to comfortably cover the
/// frontend's preflight→navigation gap.
const DEDUP_GRACE: Duration = Duration::from_secs(20);

struct Hit {
    at: Instant,
    path_hash: u64,
}

pub enum Decision {
    Allowed,
    Limited {
        /// Seconds until the oldest hit ages out and a slot frees up.
        retry_after: Duration,
    },
}

pub struct DownloadRateLimiter {
    /// Max distinct downloads per window. `0` disables the limiter.
    limit: u32,
    window: Duration,
    hits: Mutex<HashMap<IpAddr, VecDeque<Hit>>>,
}

impl DownloadRateLimiter {
    pub fn new(limit: u32, window: Duration) -> Self {
        Self {
            limit,
            window,
            hits: Mutex::new(HashMap::new()),
        }
    }

    pub fn enabled(&self) -> bool {
        self.limit > 0
    }

    /// Account for a download attempt by `ip` for `path`. Returns whether
    /// the request may proceed; on rejection, also how long to back off.
    pub fn check(&self, ip: IpAddr, path: &str) -> Decision {
        if self.limit == 0 {
            return Decision::Allowed;
        }
        let now = Instant::now();
        let path_hash = hash_path(path);
        let mut map = self.hits.lock().expect("ratelimit mutex");
        let dq = map.entry(ip).or_default();

        // Drop hits that have aged out of the window.
        while let Some(front) = dq.front() {
            if now.duration_since(front.at) >= self.window {
                dq.pop_front();
            } else {
                break;
            }
        }

        // Same path requested again very recently → reuse the token.
        if dq
            .iter()
            .any(|h| h.path_hash == path_hash && now.duration_since(h.at) < DEDUP_GRACE)
        {
            return Decision::Allowed;
        }

        if dq.len() as u32 >= self.limit {
            // The first hit to expire is the front of the deque.
            let oldest = dq.front().map(|h| h.at).unwrap_or(now);
            let retry_after = self
                .window
                .saturating_sub(now.duration_since(oldest))
                .max(Duration::from_secs(1));
            return Decision::Limited { retry_after };
        }

        dq.push_back(Hit { at: now, path_hash });
        Decision::Allowed
    }

    /// Evict IPs whose hits have all aged out. Called periodically so the
    /// map doesn't grow unboundedly with one-off visitors.
    pub fn sweep(&self) {
        let now = Instant::now();
        let mut map = self.hits.lock().expect("ratelimit mutex");
        map.retain(|_, dq| {
            while let Some(front) = dq.front() {
                if now.duration_since(front.at) >= self.window {
                    dq.pop_front();
                } else {
                    break;
                }
            }
            !dq.is_empty()
        });
    }
}

fn hash_path(s: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}
