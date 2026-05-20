//! GitHub OAuth 2.0 sign-in. GitHub is not an OIDC provider for end-user
//! login (no `.well-known/openid-configuration`, no `id_token`), so we
//! can't reuse the `openidconnect` crate path. Instead, hand-build the
//! flow with hardcoded GitHub endpoints, then call `api.github.com/user`
//! to produce a `VerifiedIdentity` with the same shape the OIDC path
//! returns. Downstream code (session encoding, users.toml lookup,
//! INITIAL_ADMIN_SUB convenience, blocked/missing checks) treats the
//! two paths identically.
//!
//! Activated only when all of `RAWDB_GITHUB_CLIENT_ID`, `_CLIENT_SECRET`,
//! `_REDIRECT_URL` are set.

use anyhow::{anyhow, Context, Result};
use oauth2::basic::BasicClient;
use oauth2::{
    AuthUrl, AuthorizationCode, ClientId, ClientSecret, CsrfToken, PkceCodeChallenge,
    PkceCodeVerifier, RedirectUrl, Scope, TokenResponse, TokenUrl,
};
use serde::{Deserialize, Serialize};

use crate::config::Config;

const AUTHORIZE_URL: &str = "https://github.com/login/oauth/authorize";
const TOKEN_URL: &str = "https://github.com/login/oauth/access_token";
const USER_URL: &str = "https://api.github.com/user";
const EMAILS_URL: &str = "https://api.github.com/user/emails";

pub struct GithubClient {
    client: BasicClient,
    http: reqwest::Client,
}

/// Transient state stored in a signed short-lived cookie so the callback
/// can verify CSRF + complete PKCE across pods.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingFlow {
    pub csrf: String,
    pub pkce_verifier: String,
    pub exp: i64,
}

/// Mirrors [`crate::auth_oidc::VerifiedIdentity`] so the calling code can
/// stay agnostic about which IdP produced the session.
#[derive(Debug, Clone)]
pub struct VerifiedIdentity {
    /// GitHub login (the `@handle`); used as `preferred_username`.
    pub login: String,
    /// Stringified numeric GitHub user id.
    pub id: String,
    pub name: Option<String>,
    pub email: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GithubUser {
    login: String,
    id: u64,
    name: Option<String>,
    email: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GithubEmail {
    email: String,
    primary: bool,
    verified: bool,
}

impl GithubClient {
    pub fn from_config(cfg: &Config) -> Result<Option<Self>> {
        if !cfg.github_enabled() {
            return Ok(None);
        }
        let client_id = cfg
            .github_client_id
            .clone()
            .expect("checked by github_enabled");
        let client_secret = cfg
            .github_client_secret
            .clone()
            .expect("checked by github_enabled");
        let redirect = cfg
            .github_redirect_url
            .clone()
            .expect("checked by github_enabled");

        let client = BasicClient::new(
            ClientId::new(client_id),
            Some(ClientSecret::new(client_secret)),
            AuthUrl::new(AUTHORIZE_URL.into()).context("github authorize url")?,
            Some(TokenUrl::new(TOKEN_URL.into()).context("github token url")?),
        )
        .set_redirect_uri(RedirectUrl::new(redirect).context("github redirect url")?);

        // GitHub's API requires a User-Agent on every request.
        let http = reqwest::Client::builder()
            .user_agent(concat!("rawdb/", env!("CARGO_PKG_VERSION")))
            .build()
            .context("github http client")?;

        Ok(Some(Self { client, http }))
    }

    /// Begin the auth-code+PKCE flow. Returns the authorize URL to
    /// redirect the user to, plus the transient state to persist for
    /// later verification.
    pub fn start_flow(&self) -> (String, PendingFlow) {
        let (pkce_challenge, pkce_verifier) = PkceCodeChallenge::new_random_sha256();
        let (url, csrf) = self
            .client
            .authorize_url(CsrfToken::new_random)
            .add_scope(Scope::new("read:user".into()))
            .add_scope(Scope::new("user:email".into()))
            .set_pkce_challenge(pkce_challenge)
            .url();
        let pending = PendingFlow {
            csrf: csrf.secret().clone(),
            pkce_verifier: pkce_verifier.secret().clone(),
            exp: chrono::Utc::now().timestamp() + 600, // 10 min
        };
        (url.to_string(), pending)
    }

    /// Exchange the code, then call `api.github.com/user` (and
    /// `/user/emails` if the profile's primary email is hidden) to
    /// assemble a verified identity.
    pub async fn finish_flow(
        &self,
        pending: PendingFlow,
        received_state: &str,
        code: &str,
    ) -> Result<VerifiedIdentity> {
        if received_state != pending.csrf {
            return Err(anyhow!("github state mismatch"));
        }
        let token = self
            .client
            .exchange_code(AuthorizationCode::new(code.into()))
            .set_pkce_verifier(PkceCodeVerifier::new(pending.pkce_verifier))
            .request_async(oauth2::reqwest::async_http_client)
            .await
            .context("github code exchange")?;

        let access = token.access_token().secret();
        let user: GithubUser = self
            .http
            .get(USER_URL)
            .bearer_auth(access)
            .header("Accept", "application/vnd.github+json")
            .send()
            .await
            .context("github userinfo request")?
            .error_for_status()
            .context("github userinfo status")?
            .json()
            .await
            .context("github userinfo body")?;

        // If the user keeps their primary email private, the `/user`
        // response sets it to null; fall back to `/user/emails` (the
        // `user:email` scope grants this).
        let email = if user.email.is_some() {
            user.email.clone()
        } else {
            self.fetch_primary_email(access).await.unwrap_or(None)
        };

        Ok(VerifiedIdentity {
            login: user.login,
            id: user.id.to_string(),
            name: user.name,
            email,
        })
    }

    async fn fetch_primary_email(&self, access: &str) -> Result<Option<String>> {
        let emails: Vec<GithubEmail> = self
            .http
            .get(EMAILS_URL)
            .bearer_auth(access)
            .header("Accept", "application/vnd.github+json")
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(emails
            .into_iter()
            .find(|e| e.primary && e.verified)
            .map(|e| e.email))
    }
}
