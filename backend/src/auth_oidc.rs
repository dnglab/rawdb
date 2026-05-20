//! OIDC (auth-code + PKCE) wiring. Optional: constructed only when all of
//! `RAWDB_OIDC_ISSUER_URL`, `_CLIENT_ID`, `_CLIENT_SECRET`, `_REDIRECT_URL`
//! are set.
//!
//! The transient state required by the OIDC dance (CSRF token, nonce, PKCE
//! verifier) is stored in a short-lived signed JWT cookie so the flow
//! survives the round-trip across multiple pods.

#[cfg(feature = "oidc")]
pub use enabled::*;

#[cfg(not(feature = "oidc"))]
pub use disabled::*;

#[cfg(feature = "oidc")]
mod enabled {
    use anyhow::{anyhow, Context, Result};
    use openidconnect::core::{CoreAuthenticationFlow, CoreClient, CoreProviderMetadata};
    use openidconnect::reqwest::async_http_client;
    use openidconnect::{
        AuthorizationCode, ClientId, ClientSecret, CsrfToken, IssuerUrl, Nonce,
        PkceCodeChallenge, PkceCodeVerifier, RedirectUrl, Scope, TokenResponse,
    };
    use serde::{Deserialize, Serialize};

    use crate::config::Config;

    pub struct OidcClient {
        client: CoreClient,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct PendingFlow {
        pub csrf: String,
        pub nonce: String,
        pub pkce_verifier: String,
        pub exp: i64,
    }

    #[derive(Debug, Clone)]
    pub struct VerifiedIdentity {
        pub sub: String,
        pub preferred_username: Option<String>,
        pub email: Option<String>,
        pub name: Option<String>,
    }

    impl OidcClient {
        pub async fn from_config(cfg: &Config) -> Result<Option<Self>> {
            if !cfg.oidc_enabled() {
                return Ok(None);
            }
            let issuer = IssuerUrl::new(
                cfg.oidc_issuer_url.clone().expect("checked by oidc_enabled"),
            )
            .context("oidc issuer url")?;
            let meta = CoreProviderMetadata::discover_async(issuer, async_http_client)
                .await
                .context("oidc discovery")?;
            let client = CoreClient::from_provider_metadata(
                meta,
                ClientId::new(cfg.oidc_client_id.clone().unwrap()),
                Some(ClientSecret::new(cfg.oidc_client_secret.clone().unwrap())),
            )
            .set_redirect_uri(
                RedirectUrl::new(cfg.oidc_redirect_url.clone().unwrap())
                    .context("oidc redirect url")?,
            );
            Ok(Some(Self { client }))
        }

        /// Build an authorize URL + the transient state to be persisted in
        /// a cookie for later callback verification.
        pub fn start_flow(&self) -> (String, PendingFlow) {
            let (pkce_challenge, pkce_verifier) = PkceCodeChallenge::new_random_sha256();
            let (url, csrf, nonce) = self
                .client
                .authorize_url(
                    CoreAuthenticationFlow::AuthorizationCode,
                    CsrfToken::new_random,
                    Nonce::new_random,
                )
                .add_scope(Scope::new("openid".to_string()))
                .add_scope(Scope::new("email".to_string()))
                .add_scope(Scope::new("profile".to_string()))
                .set_pkce_challenge(pkce_challenge)
                .url();

            let pending = PendingFlow {
                csrf: csrf.secret().clone(),
                nonce: nonce.secret().clone(),
                pkce_verifier: pkce_verifier.secret().clone(),
                exp: chrono::Utc::now().timestamp() + 600, // 10 min
            };
            (url.to_string(), pending)
        }

        /// Exchange the code + nonce on callback and produce a verified
        /// user identity.
        pub async fn finish_flow(
            &self,
            pending: PendingFlow,
            received_state: &str,
            code: &str,
        ) -> Result<VerifiedIdentity> {
            if received_state != pending.csrf {
                return Err(anyhow!("OIDC state mismatch"));
            }
            let token_response = self
                .client
                .exchange_code(AuthorizationCode::new(code.to_string()))
                .set_pkce_verifier(PkceCodeVerifier::new(pending.pkce_verifier))
                .request_async(async_http_client)
                .await
                .context("exchange code")?;
            let id_token = token_response
                .id_token()
                .ok_or_else(|| anyhow!("no id_token returned"))?;
            let claims = id_token
                .claims(&self.client.id_token_verifier(), &Nonce::new(pending.nonce))
                .context("verify id_token")?;
            Ok(VerifiedIdentity {
                sub: claims.subject().as_str().to_string(),
                preferred_username: claims
                    .preferred_username()
                    .map(|p| p.as_str().to_string()),
                email: claims.email().map(|e| e.as_str().to_string()),
                name: claims
                    .name()
                    .and_then(|n| n.get(None))
                    .map(|n| n.as_str().to_string()),
            })
        }
    }
}

#[cfg(not(feature = "oidc"))]
mod disabled {
    use crate::config::Config;

    pub struct OidcClient;
    pub struct PendingFlow;
    pub struct VerifiedIdentity;

    impl OidcClient {
        pub async fn from_config(_cfg: &Config) -> anyhow::Result<Option<Self>> {
            Ok(None)
        }
    }
}
