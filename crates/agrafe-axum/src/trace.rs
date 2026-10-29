//! Request spans and the trace context every request runs in. See
//! DESIGN.md, "Tracing".
//!
//! Each interface wraps its router in a `request` span at `info` and runs
//! the request as a unit of a trace, an `agrafe::trace::TraceContext`:
//! the jobs it enqueues record that context and continue the trace. The
//! private interface continues the trace of a W3C `traceparent` header,
//! sent by the host application; the public one starts a trace of its
//! own every time, since its clients are browsers and anyone, and the
//! choice of a trace id is not theirs to make.

use std::fmt::Display;

use axum::Router;
use axum::body::Body;
use axum::extract::{OriginalUri, Request};
use axum::http::HeaderName;
use axum::middleware::{self, Next};
use axum::response::Response;
use tower_http::trace::TraceLayer;
use tracing::Span;
use tracing::field::{Empty, display};

use agrafe::trace::TraceContext;

use crate::signed;

/// The W3C trace context header.
pub const TRACEPARENT: HeaderName = HeaderName::from_static("traceparent");

/// `router` with the private interface's request span and trace context:
/// the path recorded as the client sent it, mount prefix included, the
/// query string left out, and a `traceparent` header continued. For routes a host serves next to the
/// private interface, the dashboard among them: a router's layers do not
/// reach what is nested into it afterwards.
pub fn private<S: Clone + Send + Sync + 'static>(router: Router<S>) -> Router<S> {
    router
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(|req: &Request<Body>| request_span(req, original_path(req))),
        )
        .layer(middleware::from_fn(continue_trace))
}

/// `router` with the public interface's request span and trace context:
/// the token segment of the path redacted, and a trace started afresh
/// whatever the request says.
pub(crate) fn public<S: Clone + Send + Sync + 'static>(router: Router<S>) -> Router<S> {
    router
        .layer(
            TraceLayer::new_for_http().make_span_with(|req: &Request<Body>| {
                // Redacted on the route's own path, where the token segment
                // is known to be, then put back behind the mount prefix.
                let path = req.uri().path();
                let prefix = original_path(req).strip_suffix(path).unwrap_or_default();
                request_span(req, format!("{prefix}{}", signed::redact_token(path)))
            }),
        )
        .layer(middleware::from_fn(start_trace))
}

/// The path as the client sent it: a router nested under a prefix sees its
/// own path in the URI, which would make `/dashboard/jobs` and `/jobs` one
/// line in the logs.
fn original_path(req: &Request<Body>) -> &str {
    req.extensions()
        .get::<OriginalUri>()
        .map_or(req.uri().path(), |original| original.0.path())
}

fn request_span(req: &Request<Body>, path: impl Display) -> Span {
    let span = tracing::info_span!(
        "request",
        method = %req.method(),
        path = %path,
        trace_id = Empty,
    );
    if let Some(context) = req.extensions().get::<TraceContext>() {
        span.record("trace_id", display(context.trace_id()));
    }
    span
}

/// The context of the caller's `traceparent`, continued, or a new trace
/// when there is none, more than one, or a malformed one, as W3C says.
async fn continue_trace(req: Request, next: Next) -> Response {
    let mut headers = req.headers().get_all(TRACEPARENT).iter();
    let context = match (headers.next(), headers.next()) {
        (Some(value), None) => value.to_str().ok().and_then(TraceContext::continue_from),
        _ => None,
    };
    run(context.unwrap_or_else(TraceContext::root), req, next).await
}

async fn start_trace(req: Request, next: Next) -> Response {
    run(TraceContext::root(), req, next).await
}

/// The context goes in the extensions for the span, which the trace layer
/// makes inside this middleware, and is current for the handler.
async fn run(context: TraceContext, mut req: Request, next: Next) -> Response {
    req.extensions_mut().insert(context);
    context.scope(next.run(req)).await
}
