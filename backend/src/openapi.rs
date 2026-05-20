//! OpenAPI 3.1 specification, generated at compile time by `utoipa`.
//!
//! Exposed at `/openapi.json` and rendered by Swagger UI at `/docs`; both
//! routes are gated by `RAWDB_DOCS_ENABLED`.

use utoipa::openapi::security::{ApiKey, ApiKeyValue, SecurityScheme};
use utoipa::{Modify, OpenApi};

use crate::routes::{self, admin, auth, public, upload};

/// Registers the cookie-based session scheme so endpoints can declare
/// `security(("session_cookie" = []))`. The session is the JWT issued by
/// `/auth/login` (or the OIDC callback) and transported in the
/// `rawdb_session` cookie — there is no `Authorization: Bearer` path today.
pub struct SecurityAddon;

impl Modify for SecurityAddon {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        let components = openapi
            .components
            .get_or_insert_with(utoipa::openapi::Components::new);
        components.add_security_scheme(
            "session_cookie",
            SecurityScheme::ApiKey(ApiKey::Cookie(ApiKeyValue::new("rawdb_session"))),
        );
    }
}

#[derive(OpenApi)]
#[openapi(
    info(
        title = "RawDB API",
        description = "RAW camera sample sharing site — public, upload, auth, admin and observability HTTP API.",
        license(name = "MIT OR Apache-2.0"),
        version = env!("CARGO_PKG_VERSION"),
    ),
    paths(
        // Public read API
        public::list_sets,
        public::set_detail,
        public::search,
        public::tags,
        public::makers,
        public::models,
        public::stats,
        public::download,
        // Upload flow
        upload::begin,
        upload::stream,
        upload::complete,
        // Auth
        auth::login,
        auth::logout,
        auth::me,
        auth::methods,
        auth::oidc_enabled,
        auth::oidc_start_stub,
        auth::oidc_callback_stub,
        auth::github_start,
        auth::github_callback,
        // Admin — pending review
        admin::list_pending,
        admin::get_pending,
        admin::download_pending,
        admin::edit_pending,
        admin::approve_pending,
        admin::reject_pending,
        admin::edit_set,
        // Admin — user management
        admin::list_users,
        admin::add_user,
        admin::patch_user,
        admin::delete_user,
        // Observability (served on the dedicated metrics listener)
        routes::healthz,
        routes::live,
        routes::ready,
        routes::metrics::metrics,
    ),
    components(schemas(
        // Public
        public::SetEnvelope,
        public::ListResponse,
        public::FileEnvelope,
        public::SetDetailResponse,
        public::StatsResponse,
        public::TagsResponse,
        public::TagCount,
        public::MakersResponse,
        public::ModelsResponse,
        public::MakerModel,
        // Upload
        upload::BeginRequest,
        upload::BeginFile,
        upload::BeginResponse,
        upload::CompleteRequest,
        // Auth
        auth::LoginRequest,
        auth::LoginResponse,
        auth::OkResponse,
        auth::MeResponse,
        auth::OidcEnabledResponse,
        auth::AuthMethodsResponse,
        // Admin
        admin::PendingRow,
        admin::PendingFile,
        admin::PendingDetail,
        admin::EditRequest,
        admin::EditFile,
        admin::EditSetRequest,
        admin::ApproveRequest,
        admin::UserView,
        admin::AddUserRequest,
        admin::PatchUserRequest,
    )),
    tags(
        (name = "public", description = "Unauthenticated browse, search and download."),
        (name = "upload", description = "Three-step upload flow: begin → PUT files (presigned or stream) → complete."),
        (name = "auth", description = "Password + OIDC login, session probe, logout."),
        (name = "admin", description = "Reviewer- and admin-gated: pending review, set editing, user management."),
        (name = "observability", description = "Health probes and Prometheus metrics — served on `RAWDB_METRICS_BIND` (default :9090)."),
    ),
    servers(
        (url = "/", description = "This server"),
    ),
    modifiers(&SecurityAddon),
)]
pub struct ApiDoc;
