//! Embedded HTML error pages for the non-API (SPA / static) branch.
//!
//! These are baked into the binary with `include_str!` so they render even
//! when `RAWDB_STATIC_DIR` is missing or empty. `/api` and `/auth` keep their
//! JSON error contract (see [`crate::error::AppError`]) and never reach here.

use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};

const PAGE_404: &str = include_str!("../../static/errors/404.html");
const PAGE_4XX: &str = include_str!("../../static/errors/4xx.html");
const PAGE_5XX: &str = include_str!("../../static/errors/5xx.html");

/// Pick the page for an error status: 404 is special-cased, otherwise the
/// generic page for the status class (4xx vs 5xx).
pub fn html_for_status(status: StatusCode) -> &'static str {
    if status == StatusCode::NOT_FOUND {
        PAGE_404
    } else if status.is_server_error() {
        PAGE_5XX
    } else {
        PAGE_4XX
    }
}

/// A full HTML error response (status + embedded page body).
pub fn error_response(status: StatusCode) -> Response {
    (status, Html(html_for_status(status))).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_status_to_expected_page() {
        assert_eq!(html_for_status(StatusCode::NOT_FOUND), PAGE_404);
        assert_eq!(html_for_status(StatusCode::FORBIDDEN), PAGE_4XX);
        assert_eq!(html_for_status(StatusCode::BAD_REQUEST), PAGE_4XX);
        assert_eq!(html_for_status(StatusCode::METHOD_NOT_ALLOWED), PAGE_4XX);
        assert_eq!(
            html_for_status(StatusCode::INTERNAL_SERVER_ERROR),
            PAGE_5XX
        );
        assert_eq!(html_for_status(StatusCode::BAD_GATEWAY), PAGE_5XX);
    }

    #[test]
    fn pages_are_non_empty_and_self_contained() {
        for page in [PAGE_404, PAGE_4XX, PAGE_5XX] {
            assert!(page.contains("<!DOCTYPE html>"));
            // No external asset references — must render with no static dir.
            assert!(!page.contains("src="));
            assert!(!page.contains("href=\"http"));
        }
    }
}
