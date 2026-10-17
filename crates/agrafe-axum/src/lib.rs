//! The HTTP interfaces of agrafe as axum routers. `private` is unsigned and
//! trusted; `public` verifies a signature on every route before touching
//! the database. Each router carries its state, so a host application can
//! nest one under a prefix and keep the private API in Rust.

pub mod body;
pub mod error;
pub mod extract;
pub mod jobs;
pub mod json;
pub mod original;
pub mod private;
pub mod public;
pub mod representation;
pub mod signed;

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use tower_http::trace::TraceLayer;

use agrafe::Agrafe;
use agrafe::upload;

pub use error::{ApiError, ApiResult};

pub fn private_router(state: Agrafe) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/metrics", get(metrics))
        .merge(private::routes(upload::body_limit(&state.config().upload)))
        .merge(jobs::routes())
        .fallback(not_found)
        .method_not_allowed_fallback(method_not_allowed)
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

pub fn public_router(state: Agrafe) -> Router {
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
        .merge(public::routes(upload::body_limit(&state.config().upload)))
        .fallback(not_found)
        .method_not_allowed_fallback(method_not_allowed)
        .layer(trace)
        .with_state(state)
}

/// What a process without an API serves on the private address: enough
/// for liveness checks and scraping.
pub fn ops_router(state: Agrafe) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/metrics", get(metrics))
        .fallback(not_found)
        .method_not_allowed_fallback(method_not_allowed)
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

async fn healthz() -> &'static str {
    "ok"
}

async fn metrics(State(state): State<Agrafe>) -> ApiResult<Response> {
    let lease = state.config().jobs.lease_timeout;
    let body = state
        .metrics()
        .render(state.pool(), lease)
        .await
        .map_err(ApiError::internal)?;
    Ok((
        [(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        body,
    )
        .into_response())
}

async fn not_found() -> ApiError {
    ApiError::NotFound
}

async fn method_not_allowed() -> ApiError {
    ApiError::MethodNotAllowed
}
