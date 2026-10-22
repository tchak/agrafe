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
use uuid::Uuid;

use agrafe::Agrafe;
use agrafe::blob::envelope::Envelope;
use agrafe::blob::read::ReadError;
use agrafe::blob::representation::{self, Resolved};
use agrafe::blob::urls::MAX_WAIT;
use agrafe::blob::{BlobHead, Rendered, Representation, RepresentationState};
use agrafe::encryption::UnwrapError;
use agrafe::filename::split_extension;
use agrafe::icons;

use super::original::{self, Origin};
use crate::error::{ApiError, ApiResult};

pub const STATE_HEADER: &str = "x-representation-state";

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
    /// The envelope of a sealed URL, public port only.
    pub e: Option<String>,
}

/// Which port is asking: the public one does not see terminal blobs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Port {
    Private,
    Public,
}

#[allow(clippy::too_many_arguments)]
pub async fn serve(
    state: &Agrafe,
    blob: BlobHead,
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
        let served = state.representation(&blob, kind).await?;
        let pending = match &served {
            Resolved::Placeholder(RepresentationState::Pending) => true,
            Resolved::NotReady(state) => !state.is_terminal(),
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
        Resolved::Ready(rendered) => {
            image(
                state,
                &blob,
                kind,
                rendered,
                disposition,
                method,
                headers,
                Origin::Row,
            )
            .await
        }
        Resolved::Placeholder(rep_state) => {
            placeholder(kind, &blob, rep_state, disposition, method)
        }
        Resolved::NotReady(blob_state) => match kind {
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

/// Serve `kind` from the URL's envelope, with no row to look up, when
/// the envelope has it rendered with the current variant version. `None`
/// otherwise: a render was pending or the version has moved since the
/// envelope was minted, and the row knows better.
#[allow(clippy::too_many_arguments)]
pub async fn serve_sealed(
    state: &Agrafe,
    id: Uuid,
    envelope: &Envelope,
    kind: Representation,
    filename: &str,
    query: &RepresentationQuery,
    method: &Method,
    headers: &HeaderMap,
) -> ApiResult<Option<Response>> {
    let sealed = envelope.representation(kind);
    let current = state.config().media.variant_version;
    let (Some(rendered), RepresentationState::Ready, Some(version)) =
        (sealed.rendered, sealed.state, sealed.variant_version)
    else {
        return Ok(None);
    };
    if version != current {
        return Ok(None);
    }
    let blob = envelope.head(id, filename);
    let name = webp_name(query.filename.as_deref().unwrap_or(filename));
    let disposition = original::content_disposition(query.disposition.as_deref(), &name)?;
    image(
        state,
        &blob,
        kind,
        rendered,
        disposition,
        method,
        headers,
        Origin::Envelope,
    )
    .await
    .map(Some)
}

async fn refetch(state: &Agrafe, id: Uuid, port: Port) -> ApiResult<BlobHead> {
    state
        .find_blob_head(id)
        .await?
        .filter(|blob| port == Port::Private || blob.visible_on_public(Timestamp::now()))
        .ok_or(ApiError::NotFound)
}

/// The rendered representation, with the conditional request handling a
/// cacheable image deserves. The `ETag` and the length come from the row,
/// so a `304` and a `HEAD` never read the object; only a `GET` with a body
/// to send does. On the sealed path a data key that does not unwrap under
/// a known KEK is another blob's envelope, the minter's mistake, so it is
/// `400` rather than the `500` a row that does not unwrap earns.
#[allow(clippy::too_many_arguments)]
async fn image(
    state: &Agrafe,
    blob: &BlobHead,
    kind: Representation,
    rendered: Rendered,
    disposition: HeaderValue,
    method: &Method,
    headers: &HeaderMap,
    origin: Origin,
) -> ApiResult<Response> {
    let etag = original::etag(&rendered.sha256);
    let mut response = Response::new(Body::empty());
    let h = response.headers_mut();
    h.insert(header::ETAG, HeaderValue::from_str(&etag).expect("ascii"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static(CACHE_READY));
    h.insert(STATE_HEADER, state_value(RepresentationState::Ready));
    if original::none_match(headers, &etag) {
        *response.status_mut() = StatusCode::NOT_MODIFIED;
        return Ok(response);
    }
    if method == Method::HEAD {
        webp_headers(response.headers_mut(), rendered.size, disposition);
        return Ok(response);
    }
    let loaded = state
        .load_representation(blob, kind)
        .await
        .map_err(|e| match (origin, &e) {
            (
                Origin::Envelope,
                representation::Error::Read(ReadError::Key(UnwrapError::Crypto { .. })),
            ) => ApiError::BadRequest(format!("envelope: {e}")),
            _ => ApiError::from(e),
        })?;
    let Some(bytes) = loaded else {
        return placeholder(
            kind,
            blob,
            RepresentationState::Pending,
            disposition,
            method,
        );
    };
    webp_headers(response.headers_mut(), bytes.len() as u64, disposition);
    *response.body_mut() = Body::from(bytes);
    Ok(response)
}

/// What stands in for a representation that is not there: the icon for a
/// thumbnail, a `404` for a preview. A pending one must not be cached, the
/// client is expected back.
fn placeholder(
    kind: Representation,
    blob: &BlobHead,
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
            webp_headers(response.headers_mut(), icon.len() as u64, disposition);
            if method != Method::HEAD {
                *response.body_mut() = Body::from(icon);
            }
            response
        }
        Representation::Preview => ApiError::Missing(match rep_state {
            RepresentationState::Pending => "preview is pending".to_string(),
            RepresentationState::Failed => "preview could not be generated".to_string(),
            RepresentationState::Unavailable => "no preview for this file".to_string(),
            RepresentationState::Disabled => "preview disabled for this blob".to_string(),
            RepresentationState::Ready => unreachable!("a ready preview is served, not placed"),
        })
        .into_response(),
    };
    let h = response.headers_mut();
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    h.insert(STATE_HEADER, state_value(rep_state));
    Ok(response)
}

fn webp_headers(h: &mut HeaderMap, len: u64, disposition: HeaderValue) {
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
    let (stem, _) = split_extension(filename);
    format!("{stem}.webp")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn webp_names() {
        assert_eq!(webp_name("photo été.jpg"), "photo été.webp");
        assert_eq!(webp_name("archive.tar.gz"), "archive.webp");
        assert_eq!(webp_name("my.report.pdf"), "my.report.webp");
        assert_eq!(webp_name("README"), "README.webp");
        assert_eq!(webp_name(".bashrc"), ".bashrc.webp");
    }
}
