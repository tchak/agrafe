//! The HTML dashboard of agrafe: the queue, the blobs and the attachments
//! as pages on the private interface, a second presentation of the
//! `Agrafe` handle. Read-only but for the retry and discard actions, which
//! the configuration enables. See [DESIGN.md, "Dashboard"].
//!
//! Compile-time templates (askama) and one vendored stylesheet, no
//! JavaScript: the binary carries everything it serves. A host nests
//! [`dashboard_router`] wherever it likes, behind its own authentication;
//! `agrafe-server` puts it under `/dashboard` on the private listener.
//!
//! [DESIGN.md, "Dashboard"]: https://github.com/tchak/agrafe/blob/main/DESIGN.md#dashboard

mod attachments;
mod blobs;
mod error;
mod filters;
// Public for the census test in `tests/`, not part of the API.
#[doc(hidden)]
pub mod jobs;
mod layout;
mod origin;

use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::LazyLock;

use axum::Router;
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware;
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;

use agrafe::Agrafe;

use error::DashboardError;
use layout::Base;

/// daisyUI's reset, themes and the components the templates use, MIT,
/// assembled by `scripts/daisyui.sh`; see `assets/daisyui.css.LICENSE`.
/// The layout utilities the pages add on top are in `base.html`.
const STYLESHEET: &str = include_str!("../assets/daisyui.css");

/// The dashboard with its state applied, to nest under a prefix (it has a
/// fallback, so it cannot be merged into a router that has one). Its links
/// are built from the prefix it finds itself under, and the thumbnails it
/// embeds are the private API's, assumed one segment up (`/dashboard`
/// under `/`, `/internal/dashboard` under `/internal`). It traces its own
/// requests with the private interface's span and trace context, since a
/// host's layers applied before the nest do not reach routes nested after
/// it; a host does not wrap it again.
pub fn dashboard_router(state: Agrafe) -> Router {
    let router = Router::new()
        .route("/", get(home))
        .route("/style.css", get(stylesheet))
        .merge(jobs::routes())
        .merge(blobs::routes())
        .merge(attachments::routes())
        .fallback(not_found)
        .layer(middleware::from_fn(error::render))
        .with_state(state);
    agrafe_axum::trace::private(router)
}

async fn home(base: Base) -> Redirect {
    Redirect::to(&format!("{}/jobs", base.0))
}

/// Tagged with a hash of its content, so a browser revalidates on every
/// page (a `304`, no body) and a changed sheet is never stuck behind the
/// old one. `DefaultHasher::new()` is keyed with constants, so every
/// process of a build agrees on the tag.
static STYLESHEET_ETAG: LazyLock<String> = LazyLock::new(|| {
    let mut hasher = DefaultHasher::new();
    STYLESHEET.hash(&mut hasher);
    format!("\"{:016x}\"", hasher.finish())
});

async fn stylesheet(headers: HeaderMap) -> Response {
    let etag = STYLESHEET_ETAG.as_str();
    let fresh = headers
        .get(header::IF_NONE_MATCH)
        .is_some_and(|tag| tag == etag);
    let headers = [
        (header::CONTENT_TYPE, "text/css; charset=utf-8"),
        (header::CACHE_CONTROL, "no-cache"),
        (header::ETAG, etag),
    ];
    if fresh {
        (StatusCode::NOT_MODIFIED, headers).into_response()
    } else {
        (headers, STYLESHEET).into_response()
    }
}

async fn not_found() -> DashboardError {
    DashboardError::NotFound
}
