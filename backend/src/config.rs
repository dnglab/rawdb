use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{bail, Result};
use clap::Parser;

/// RawDB — RAW sample sharing site backend.
#[derive(Parser, Debug, Clone)]
#[command(version, about, long_about = None)]
pub struct Config {
    // ---- HTTP / paths ---------------------------------------------------------
    #[arg(long, env = "RAWDB_BIND", default_value = "0.0.0.0:8080")]
    pub bind: SocketAddr,

    /// Listen address for the metrics + healthcheck endpoints (`/metrics`,
    /// `/live`, `/ready`, `/healthz`). Exposed on its own port so it can be
    /// scraped publicly without going through the main API/SPA listener.
    #[arg(long, env = "RAWDB_METRICS_BIND", default_value = "0.0.0.0:9090")]
    pub metrics_bind: SocketAddr,

    #[arg(long, env = "RAWDB_STATIC_DIR", default_value = "/app/static")]
    pub static_dir: PathBuf,

    #[arg(long, env = "RAWDB_CACHE_DIR", default_value = "/tmp/rawdb-cache")]
    pub cache_dir: PathBuf,

    // ---- S3 -------------------------------------------------------------------
    #[arg(long, env = "RAWDB_S3_BUCKET")]
    pub s3_bucket: String,

    #[arg(long, env = "RAWDB_S3_ENDPOINT")]
    pub s3_endpoint: Option<String>,

    #[arg(long, env = "RAWDB_S3_REGION", default_value = "us-east-1")]
    pub s3_region: String,

    #[arg(long, env = "RAWDB_S3_ACCESS_KEY")]
    pub s3_access_key: String,

    #[arg(long, env = "RAWDB_S3_SECRET_KEY")]
    pub s3_secret_key: String,

    #[arg(long, env = "RAWDB_S3_PATH_STYLE", default_value_t = true)]
    pub s3_path_style: bool,

    /// Whether the S3 backend honors `If-Match` on `PutObject`. RawDB uses
    /// it for optimistic-concurrency writes to `_system/users.toml` so
    /// peer pods can't clobber each other's edits. Ceph RGW–based stores
    /// (e.g. Hetzner Object Storage) reject conditional PUTs with 412 —
    /// set this to `false` for those, and user-management writes fall back
    /// to last-writer-wins (fine for the rare, human-paced edits involved).
    #[arg(long, env = "RAWDB_S3_CONDITIONAL_WRITES", default_value_t = true)]
    pub s3_conditional_writes: bool,

    // ---- Scanner --------------------------------------------------------------
    #[arg(long, env = "RAWDB_RESCAN_SECS", default_value_t = 300)]
    pub rescan_secs: u64,

    #[arg(long, env = "RAWDB_SCAN_CONCURRENCY", default_value_t = 16)]
    pub scan_concurrency: usize,

    // ---- Upload ---------------------------------------------------------------
    #[arg(long, env = "RAWDB_UPLOAD_MODE", default_value = "either")]
    pub upload_mode: UploadMode,

    /// How file downloads are served:
    /// - `presigned` (default): 302 redirect to a presigned S3 GET URL.
    /// - `stream`: backend proxies the bytes from S3 to the client.
    /// - `either`: presigned by default; client can request streaming via
    ///   `?stream=1` on the download URL.
    #[arg(long, env = "RAWDB_DOWNLOAD_MODE", default_value = "presigned")]
    pub download_mode: DownloadMode,

    #[arg(long, env = "RAWDB_PRESIGN_TTL_SECS", default_value_t = 3600)]
    pub presign_ttl_secs: u64,

    #[arg(long, env = "RAWDB_PENDING_TTL_DAYS", default_value_t = 14)]
    pub pending_ttl_days: u64,

    /// Per-instance, per-IP download rate limit: max distinct sample-file
    /// downloads one client IP may start within
    /// `RAWDB_DOWNLOAD_RATE_WINDOW_SECS`. This is a second tier behind any
    /// edge rate limiting. `0` disables it. Default 50.
    #[arg(long, env = "RAWDB_DOWNLOAD_RATE_LIMIT", default_value_t = 50)]
    pub download_rate_limit: u32,

    /// Sliding-window length (seconds) for `RAWDB_DOWNLOAD_RATE_LIMIT`.
    /// Default 300 (5 minutes).
    #[arg(long, env = "RAWDB_DOWNLOAD_RATE_WINDOW_SECS", default_value_t = 300)]
    pub download_rate_window_secs: u64,

    /// Multipart-upload part size, in bytes, for the presigned upload
    /// path. Files larger than this are uploaded with S3 multipart (each
    /// part presigned separately) rather than a single PUT — some S3
    /// backends (e.g. Hetzner) fail large single PUTs. Also the dividing
    /// line: a file at or below this size still uses one presigned PUT.
    /// Minimum 5 MiB (the S3 part-size floor). Default 16 MiB.
    #[arg(long, env = "RAWDB_MULTIPART_PART_SIZE", default_value_t = 16 * 1024 * 1024)]
    pub multipart_part_size: u64,

    /// Hard ceiling on a single uploaded file, in bytes. Enforced
    /// server-side on the streaming upload path (rejected with 413) and
    /// when finalizing the upload (every declared file's stored size is
    /// HEADed and rejected if it exceeds this); also published via
    /// `/api/stats` so the client can refuse oversized files before the
    /// PUT. Default 2 GiB.
    #[arg(long, env = "RAWDB_MAX_UPLOAD_BYTES", default_value_t = 2 * 1024 * 1024 * 1024)]
    pub max_upload_bytes: u64,

    /// Denylist of file extensions that may NOT be uploaded. Everything else
    /// is accepted (RAW formats are too numerous to allowlist). Compared
    /// case-insensitively against the final path extension; `gz`/`tgz` cover
    /// `.tar.gz`/`.tgz` archives.
    #[arg(
        long,
        env = "RAWDB_BLOCKED_EXTENSIONS",
        value_delimiter = ',',
        default_value = "zip,tar,gz,tgz,txt,exe,pdf,rar,7z,bz2,xz"
    )]
    pub blocked_extensions: Vec<String>,

    /// Operator-curated tags that are always present in `/api/tags` with
    /// `suggested = true`, even if no current set uses them. The frontend
    /// merges these with the top-N most-used tags to build its suggestion
    /// chips.
    #[arg(
        long,
        env = "RAWDB_TAGS_SUGGESTED",
        value_delimiter = ',',
        default_value = "lossy,lossless,uncompressed"
    )]
    pub tags_suggested: Vec<String>,

    // ---- Auth: password (bootstrap admin) -------------------------------------
    /// Enable the bootstrap-admin password login. When `false`, OIDC must
    /// be fully configured (the only remaining login path), `/auth/login`
    /// returns 403, and `RAWDB_ADMIN_PASSWORD[_HASH]` are no longer
    /// required. This is the right mode for production once you have an
    /// IdP wired up; defaults to `true` so first-run bootstrap still works.
    #[arg(long, env = "RAWDB_PASSWORD_AUTH_ENABLED", default_value_t = true)]
    pub password_auth_enabled: bool,

    #[arg(long, env = "RAWDB_ADMIN_PASSWORD")]
    pub admin_password: Option<String>,

    #[arg(long, env = "RAWDB_ADMIN_PASSWORD_HASH")]
    pub admin_password_hash: Option<String>,

    #[arg(long, env = "RAWDB_SESSION_KEY")]
    pub session_key: String,

    #[arg(long, env = "RAWDB_SESSION_TTL_SECS", default_value_t = 86_400)]
    pub session_ttl_secs: u64,

    // ---- Auth: OIDC (optional, all-or-nothing) --------------------------------
    #[arg(long, env = "RAWDB_OIDC_ISSUER_URL")]
    pub oidc_issuer_url: Option<String>,

    #[arg(long, env = "RAWDB_OIDC_CLIENT_ID")]
    pub oidc_client_id: Option<String>,

    #[arg(long, env = "RAWDB_OIDC_CLIENT_SECRET")]
    pub oidc_client_secret: Option<String>,

    #[arg(long, env = "RAWDB_OIDC_REDIRECT_URL")]
    pub oidc_redirect_url: Option<String>,

    #[arg(long, env = "RAWDB_OIDC_SUB_FORMAT", default_value = "raw")]
    pub oidc_sub_format: OidcSubFormat,

    #[arg(long, env = "RAWDB_OIDC_INITIAL_ADMIN_SUB")]
    pub oidc_initial_admin_sub: Option<String>,

    // ---- Auth: GitHub OAuth (optional, all-or-nothing) -----------------------
    // GitHub isn't an OIDC provider for user login (no id_token, no
    // discovery doc), so it gets its own code path next to OIDC. Set the
    // three vars below to enable; the synthetic `sub` for these users is
    // always `github:<login>`.
    #[arg(long, env = "RAWDB_GITHUB_CLIENT_ID")]
    pub github_client_id: Option<String>,

    #[arg(long, env = "RAWDB_GITHUB_CLIENT_SECRET")]
    pub github_client_secret: Option<String>,

    #[arg(long, env = "RAWDB_GITHUB_REDIRECT_URL")]
    pub github_redirect_url: Option<String>,

    /// One-time bootstrap convenience: when no users file exists yet and a
    /// successful GitHub login resolves to this sub (`github:<login>`),
    /// the user is auto-provisioned with the `admin` role. Mirrors
    /// `RAWDB_OIDC_INITIAL_ADMIN_SUB`.
    #[arg(long, env = "RAWDB_GITHUB_INITIAL_ADMIN_SUB")]
    pub github_initial_admin_sub: Option<String>,

    // ---- OpenAPI docs ---------------------------------------------------------
    /// Serve interactive OpenAPI docs at `/docs` and the spec at
    /// `/openapi.json`. Defaults to enabled — disable in locked-down
    /// production environments where the docs surface shouldn't be public.
    #[arg(long, env = "RAWDB_DOCS_ENABLED", default_value_t = true)]
    pub docs_enabled: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum UploadMode {
    Presigned,
    Stream,
    Either,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum DownloadMode {
    Presigned,
    Stream,
    Either,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum OidcSubFormat {
    Raw,
    Github,
}

impl Config {
    /// Parse from process env / argv and validate invariants the type system can't enforce.
    pub fn parse_and_validate() -> Result<Self> {
        let cfg = Self::parse();
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> Result<()> {
        if self.password_auth_enabled
            && self.admin_password.is_none()
            && self.admin_password_hash.is_none()
        {
            bail!(
                "password auth is enabled but neither RAWDB_ADMIN_PASSWORD \
                 nor RAWDB_ADMIN_PASSWORD_HASH is set — set one, or disable \
                 with RAWDB_PASSWORD_AUTH_ENABLED=false (requires OIDC)"
            );
        }
        if self.session_key.len() < 32 {
            bail!("RAWDB_SESSION_KEY must be at least 32 bytes (hex-encoded)");
        }
        // S3 requires every multipart part except the last to be ≥ 5 MiB.
        if self.multipart_part_size < 5 * 1024 * 1024 {
            bail!("RAWDB_MULTIPART_PART_SIZE must be at least 5 MiB (S3 part-size floor)");
        }
        // OIDC must be all-or-nothing.
        let oidc_fields = [
            self.oidc_issuer_url.is_some(),
            self.oidc_client_id.is_some(),
            self.oidc_client_secret.is_some(),
            self.oidc_redirect_url.is_some(),
        ];
        let set_count = oidc_fields.iter().filter(|x| **x).count();
        if set_count != 0 && set_count != 4 {
            bail!(
                "OIDC must be fully configured or fully absent; \
                 set all of RAWDB_OIDC_ISSUER_URL, _CLIENT_ID, _CLIENT_SECRET, _REDIRECT_URL — or none"
            );
        }
        // GitHub OAuth must also be all-or-nothing.
        let gh_fields = [
            self.github_client_id.is_some(),
            self.github_client_secret.is_some(),
            self.github_redirect_url.is_some(),
        ];
        let gh_count = gh_fields.iter().filter(|x| **x).count();
        if gh_count != 0 && gh_count != 3 {
            bail!(
                "GitHub OAuth must be fully configured or fully absent; \
                 set all of RAWDB_GITHUB_CLIENT_ID, _CLIENT_SECRET, _REDIRECT_URL — or none"
            );
        }
        // Avoid locking everyone out: at least one login path must work.
        if !self.password_auth_enabled && !self.oidc_enabled() && !self.github_enabled() {
            bail!(
                "RAWDB_PASSWORD_AUTH_ENABLED=false requires OIDC or GitHub OAuth \
                 to be configured (otherwise nobody can log in)"
            );
        }
        Ok(())
    }

    pub fn github_enabled(&self) -> bool {
        self.github_client_id.is_some()
            && self.github_client_secret.is_some()
            && self.github_redirect_url.is_some()
    }

    pub fn oidc_enabled(&self) -> bool {
        self.oidc_issuer_url.is_some()
            && self.oidc_client_id.is_some()
            && self.oidc_client_secret.is_some()
            && self.oidc_redirect_url.is_some()
    }
}
