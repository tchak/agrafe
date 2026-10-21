//! CORS on the public interface, from `server.cors`. See DESIGN.md, "CORS".
//!
//! The layer answers a preflight itself, before routing, so an `OPTIONS`
//! never reaches the `Signed` extractor: it carries no signature, since a
//! URL is signed for the method it is minted for, and it touches nothing.
//! Every other response gets the headers, errors included, so a page can
//! read the `code` of a `403` or a `410` rather than a bare network error.

use axum::http::{HeaderName, HeaderValue, Method, header};
use tower_http::cors::{AllowOrigin, CorsLayer};

use agrafe::config::CorsConfig;

/// What a page sends: the upload's type, and the conditional and range
/// requests of a download.
const ALLOW_HEADERS: [HeaderName; 5] = [
    header::CONTENT_TYPE,
    header::RANGE,
    header::IF_RANGE,
    header::IF_NONE_MATCH,
    header::IF_MODIFIED_SINCE,
];

/// What a page may read beyond the safelisted response headers: the
/// download's name, the partial responses and the validators.
const EXPOSE_HEADERS: [HeaderName; 5] = [
    header::CONTENT_DISPOSITION,
    header::CONTENT_RANGE,
    header::ACCEPT_RANGES,
    header::ETAG,
    header::LAST_MODIFIED,
];

/// The layer `config` asks for, or `None` when it allows no origin.
/// Credentials are never allowed: public URLs carry their authorisation.
pub(crate) fn layer(config: &CorsConfig) -> Option<CorsLayer> {
    if config.allowed_origins.is_empty() {
        return None;
    }
    let origins = if config.allows_any_origin() {
        AllowOrigin::any()
    } else {
        // Validated at load: each is its own serialization, ASCII.
        AllowOrigin::list(
            config
                .allowed_origins
                .iter()
                .filter_map(|o| HeaderValue::from_str(o).ok()),
        )
    };
    Some(
        CorsLayer::new()
            .allow_origin(origins)
            .allow_methods([Method::GET, Method::HEAD, Method::PUT])
            .allow_headers(ALLOW_HEADERS)
            .expose_headers(EXPOSE_HEADERS)
            .max_age(config.max_age),
    )
}
