//! The HTTP interfaces of agrafe as axum routers. [`private_router`] is
//! unsigned and trusted; [`public_router`] verifies a signature on every
//! route before touching the database. Every failure answers as an
//! [`ApiError`], a JSON body with a stable code.
//!
//! A host application builds an `agrafe::Agrafe` from a configuration and
//! a pool, nests [`public_router`] under a prefix of its own and calls the
//! private API in Rust, on the handle. It may also serve [`private_router`]
//! on an internal listener, or [`ops_router`] there when the process runs
//! workers only. Each router comes with its state applied, so nesting needs
//! no state plumbing. The [`host`] and [`origin`] checks are exported for
//! routes the host adds next to the private interface.

mod attachments;
mod body;
mod cors;
mod error;
mod extract;
pub mod host;
mod jobs;
mod json;
pub mod origin;
mod original;
mod private;
mod public;
mod representation;
mod signed;
pub mod trace;

use axum::Router;
use axum::extract::State;
use axum::http::header;
use axum::middleware;
use axum::response::{IntoResponse, Response};
use axum::routing::get;

use agrafe::Agrafe;
use agrafe::upload;

pub use error::{ApiError, ApiResult};

/// The private interface: creating, uploading, listing, deleting and
/// retrying blobs, minting their URLs and serving their bytes, attachments,
/// jobs, `/healthz` and `/metrics`. Unsigned, for trusted networks only:
/// a request must name one of `server.private_hosts` when it is set
/// (`/healthz` and `/metrics` aside), and an unsafe one a browser marks as
/// cross-site is refused. A request continues the trace its `traceparent`
/// header names, if any; see [`trace`]. The state is applied, so it can be
/// nested under a prefix as is.
pub fn private_router(state: Agrafe) -> Router {
    let router = Router::new()
        .route("/healthz", get(healthz))
        .route("/metrics", get(metrics))
        .merge(private::routes(upload::body_limit(&state.config().upload)))
        .merge(attachments::routes())
        .merge(jobs::routes())
        .fallback(not_found)
        .method_not_allowed_fallback(method_not_allowed)
        .layer(middleware::from_fn(origin::refuse_cross_site))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            host::refuse_unknown_host,
        ));
    trace::private(router).with_state(state)
}

/// The public interface: the upload, metadata, original, preview and
/// thumbnail of one blob under `/blobs/{token}/{id}`, each verifying its
/// signature before touching the database, and `/healthz`. With
/// `server.cors.allowed_origins` set, every response carries the CORS
/// headers and a preflight is answered without a signature. Request spans
/// record the path with the token redacted, and every request starts a
/// trace of its own; see [`trace`]. The state is applied, so it
/// can be nested under a prefix as is; minted URLs are
/// `server.public_base_url` followed by `/blobs/...`, so the base must end
/// with that prefix.
pub fn public_router(state: Agrafe) -> Router {
    // Public URLs carry bearer tokens; the span records a redacted path
    // instead of the full URI so tokens never reach log storage.
    let mut router = Router::new()
        .route("/healthz", get(healthz))
        .merge(public::routes(upload::body_limit(&state.config().upload)))
        .fallback(not_found)
        .method_not_allowed_fallback(method_not_allowed);
    if let Some(cors) = cors::layer(&state.config().server.cors) {
        router = router.layer(cors);
    }
    trace::public(router).with_state(state)
}

/// What a process without an API serves on the private address: enough
/// for liveness checks and scraping.
pub fn ops_router(state: Agrafe) -> Router {
    let router = Router::new()
        .route("/healthz", get(healthz))
        .route("/metrics", get(metrics))
        .fallback(not_found)
        .method_not_allowed_fallback(method_not_allowed);
    trace::private(router).with_state(state)
}

async fn healthz() -> &'static str {
    "ok"
}

async fn metrics(State(state): State<Agrafe>) -> ApiResult<Response> {
    let counts = state
        .job_counts()
        .await
        .inspect_err(|err| tracing::warn!(error = %err, "the queue census failed"))
        .ok();
    let body = state
        .metrics()
        .render(counts.as_deref())
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
