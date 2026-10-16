//! HTTP interfaces. `private` is unsigned and trusted; `public` verifies a
//! signature on every route before touching the database.

pub mod extract;
pub mod json;
pub mod private;
pub mod public;
pub mod signed;
pub mod urls;

use axum::Router;
use axum::body::Body;
use axum::http::Request;
use axum::routing::get;
use tower_http::trace::TraceLayer;

use crate::app::AppState;
use crate::error::ApiError;

pub fn private_router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .merge(private::routes())
        .fallback(not_found)
        .method_not_allowed_fallback(method_not_allowed)
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

pub fn public_router(state: AppState) -> Router {
    // Public URLs carry bearer tokens; the span records a redacted path
    // instead of the full URI so tokens never reach log storage.
    let trace = TraceLayer::new_for_http().make_span_with(|req: &Request<Body>| {
        tracing::debug_span!(
            "request",
            method = %req.method(),
            path = %signed::redact_token(req.uri().path()),
            version = ?req.version(),
        )
    });
    Router::new()
        .route("/healthz", get(healthz))
        .merge(public::routes())
        .fallback(not_found)
        .method_not_allowed_fallback(method_not_allowed)
        .layer(trace)
        .with_state(state)
}

async fn healthz() -> &'static str {
    "ok"
}

async fn not_found() -> ApiError {
    ApiError::NotFound
}

async fn method_not_allowed() -> ApiError {
    ApiError::MethodNotAllowed
}
