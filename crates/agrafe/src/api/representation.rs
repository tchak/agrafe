//! Serving thumbnails and previews. The thumbnail route always answers an
//! image, an icon when the real one is not there, because its consumer is
//! an `<img>` tag; the preview route answers `404` instead. Both carry
//! `X-Representation-State` so a client can tell a placeholder from the
//! final answer, and both can `wait` for a pending render.
//!
//! The lazy path lives here too: a representation whose object is gone, or
//! that was built with another variant version, is set back to `pending`
//! and queued again on first request.

use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use jiff::Timestamp;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::original;
use agrafe_render::icons;

use crate::app::AppState;
use crate::blob::{Blob, BlobState, Representation, RepresentationState, repo, representation};
use crate::error::{ApiError, ApiResult};

pub const STATE_HEADER: &str = "x-representation-state";

/// Longest a request may wait for a pending representation.
pub const MAX_WAIT: Duration = Duration::from_secs(30);
const WAIT_POLL: Duration = Duration::from_millis(250);

/// Ready representations change only on a variant version bump; the URL's
/// own expiry bounds how long it works.
const CACHE_READY: &str = "private, max-age=86400";

/// Signed query parameters the minter may attach to a representation URL.
#[derive(Debug, Default, Deserialize)]
pub struct RepresentationQuery {
    pub disposition: Option<String>,
    pub filename: Option<String>,
    /// Milliseconds to wait for a pending representation before answering.
    pub wait: Option<u64>,
}

/// The `wait` parameter, checked when minting.
pub fn validate_wait(value: &str) -> Result<Duration, String> {
    let ms: u64 = value
        .parse()
        .map_err(|_| format!("wait must be a number of milliseconds, not `{value}`"))?;
    let wait = Duration::from_millis(ms);
    if wait > MAX_WAIT {
        return Err(format!("wait must be at most {} ms", MAX_WAIT.as_millis()));
    }
    Ok(wait)
}

/// Which port is asking: the public one does not see terminal blobs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Port {
    Private,
    Public,
}

/// What the request resolves to.
enum Served {
    Bytes(Vec<u8>),
    Placeholder(RepresentationState),
    /// The blob itself is not readable (yet, or ever).
    NotReady(BlobState),
}

#[allow(clippy::too_many_arguments)]
pub async fn serve(
    state: &AppState,
    blob: Blob,
    kind: Representation,
    filename: &str,
    query: RepresentationQuery,
    method: &Method,
    headers: &HeaderMap,
    port: Port,
) -> ApiResult<Response> {
    if filename != blob.filename {
        return Err(ApiError::NotFound);
    }
    let wait = query
        .wait
        .map(Duration::from_millis)
        .unwrap_or_default()
        .min(MAX_WAIT);
    let deadline = Instant::now() + wait;
    let mut blob = blob;
    let served = loop {
        let served = resolve(state, &blob, kind).await?;
        let pending = match &served {
            Served::Placeholder(RepresentationState::Pending) => true,
            Served::NotReady(state) => !state.is_terminal(),
            _ => false,
        };
        if !pending || Instant::now() >= deadline {
            break served;
        }
        tokio::time::sleep(WAIT_POLL.min(deadline.saturating_duration_since(Instant::now()))).await;
        blob = refetch(state, blob.id, port).await?;
    };

    let name = webp_name(query.filename.as_deref().unwrap_or(&blob.filename));
    let disposition = original::content_disposition(query.disposition.as_deref(), &name)?;
    match served {
        Served::Bytes(bytes) => Ok(image(bytes, disposition, method, headers)),
        Served::Placeholder(rep_state) => placeholder(kind, &blob, rep_state, disposition, method),
        Served::NotReady(blob_state) => match kind {
            Representation::Thumbnail => {
                let rep_state = if blob_state.is_terminal() {
                    RepresentationState::Unavailable
                } else {
                    RepresentationState::Pending
                };
                placeholder(kind, &blob, rep_state, disposition, method)
            }
            Representation::Preview => Err(ApiError::Conflict(format!(
                "blob is {blob_state} and not readable"
            ))),
        },
    }
}

/// Decide what to serve from the row and the store, queueing a render when
/// what is there is missing or stale.
async fn resolve(state: &AppState, blob: &Blob, kind: Representation) -> ApiResult<Served> {
    let now = Timestamp::now();
    if !blob.is_readable(now) {
        return Ok(Served::NotReady(blob.state_at(now)));
    }
    let version = state.config().media.variant_version;
    let rep_state = blob.representation_state(kind);
    if rep_state != RepresentationState::Pending
        && blob.representation_version(kind) != Some(version)
    {
        tracing::info!(blob = %blob.id, kind = %kind, "built with another variant version, regenerating");
        repo::request_representation(state.pool(), blob.id, kind).await?;
        return Ok(Served::Placeholder(RepresentationState::Pending));
    }
    match rep_state {
        RepresentationState::Ready => {
            match representation::load(state, blob, kind)
                .await
                .map_err(ApiError::internal)?
            {
                Some(bytes) => Ok(Served::Bytes(bytes)),
                None => {
                    tracing::warn!(blob = %blob.id, kind = %kind, "object is missing, regenerating");
                    repo::request_representation(state.pool(), blob.id, kind).await?;
                    Ok(Served::Placeholder(RepresentationState::Pending))
                }
            }
        }
        other => Ok(Served::Placeholder(other)),
    }
}

async fn refetch(state: &AppState, id: Uuid, port: Port) -> ApiResult<Blob> {
    repo::find(state.pool(), id)
        .await?
        .filter(|blob| port == Port::Private || blob.visible_on_public(Timestamp::now()))
        .ok_or(ApiError::NotFound)
}

/// The rendered representation, with the conditional request handling a
/// cacheable image deserves.
fn image(
    bytes: Vec<u8>,
    disposition: HeaderValue,
    method: &Method,
    headers: &HeaderMap,
) -> Response {
    let digest: String = Sha256::digest(&bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let etag = format!("\"{digest}\"");
    let mut response = Response::new(Body::empty());
    let h = response.headers_mut();
    h.insert(header::ETAG, HeaderValue::from_str(&etag).expect("ascii"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static(CACHE_READY));
    h.insert(STATE_HEADER, state_value(RepresentationState::Ready));
    if headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v == "*" || v.split(',').any(|t| t.trim() == etag))
    {
        *response.status_mut() = StatusCode::NOT_MODIFIED;
        return response;
    }
    webp_headers(response.headers_mut(), bytes.len(), disposition);
    if method != Method::HEAD {
        *response.body_mut() = Body::from(bytes);
    }
    response
}

/// What stands in for a representation that is not there: the icon for a
/// thumbnail, a `404` for a preview. A pending one must not be cached, the
/// client is expected back.
fn placeholder(
    kind: Representation,
    blob: &Blob,
    rep_state: RepresentationState,
    disposition: HeaderValue,
    method: &Method,
) -> ApiResult<Response> {
    let cache = if rep_state == RepresentationState::Pending {
        "no-store"
    } else {
        CACHE_READY
    };
    let mut response = match kind {
        Representation::Thumbnail => {
            let icon = icons::icon_for(blob.category, &blob.content_type);
            let mut response = Response::new(Body::empty());
            webp_headers(response.headers_mut(), icon.len(), disposition);
            if method != Method::HEAD {
                *response.body_mut() = Body::from(icon);
            }
            response
        }
        Representation::Preview => ApiError::Missing(match rep_state {
            RepresentationState::Pending => "preview is pending".to_string(),
            RepresentationState::Failed => "preview could not be generated".to_string(),
            RepresentationState::Unavailable => "no preview for this file".to_string(),
            RepresentationState::Ready => unreachable!("a ready preview is served, not placed"),
        })
        .into_response(),
    };
    let h = response.headers_mut();
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    h.insert(STATE_HEADER, state_value(rep_state));
    Ok(response)
}

fn webp_headers(h: &mut HeaderMap, len: usize, disposition: HeaderValue) {
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("image/webp"));
    h.insert(header::CONTENT_LENGTH, HeaderValue::from(len));
    h.insert(header::CONTENT_DISPOSITION, disposition);
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
}

fn state_value(state: RepresentationState) -> HeaderValue {
    HeaderValue::from_static(state.as_str())
}

/// The download name of a representation: the file's name with a `.webp`
/// extension in place of its own.
fn webp_name(filename: &str) -> String {
    match filename.rsplit_once('.') {
        Some((stem, _)) if !stem.is_empty() => format!("{stem}.webp"),
        _ => format!("{filename}.webp"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn webp_names() {
        assert_eq!(webp_name("photo été.jpg"), "photo été.webp");
        assert_eq!(webp_name("archive.tar.gz"), "archive.tar.webp");
        assert_eq!(webp_name("README"), "README.webp");
        assert_eq!(webp_name(".bashrc"), ".bashrc.webp");
    }

    #[test]
    fn wait_bounds() {
        assert_eq!(validate_wait("0").unwrap(), Duration::ZERO);
        assert_eq!(validate_wait("30000").unwrap(), MAX_WAIT);
        assert!(validate_wait("30001").is_err());
        assert!(validate_wait("-1").is_err());
        assert!(validate_wait("soon").is_err());
    }
}
