//! Serving thumbnails and previews. The thumbnail route always answers an
//! image, an icon when the real one is not there, because its consumer is
//! an `<img>` tag; the preview route answers `404` instead. Both carry
//! `X-Representation-State` so a client can tell a placeholder from the
//! final answer, and both can `wait` for a pending render.
//!
//! A deployment on JPEG XL with `media.webp_fallback` stores a WebP beside
//! each picture. Which is served is the signed `format` parameter's
//! choice, for a `<picture>` source or a download, where both are stored,
//! and without one the client's: the JPEG XL when its `Accept` lists
//! `image/jxl`, the WebP otherwise, with `Vary: Accept` so a cache keeps
//! them apart. The icon follows the same rule, being embedded in both
//! formats.
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
use agrafe::blob::urls::{MAX_WAIT, validate_format};
use agrafe::blob::{
    BlobHead, ImageFormat, Object, Rendered, Representation, RepresentationState, WebpFallback,
};
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
    /// `webp` or `jxl`: which of a representation's pictures to serve,
    /// where it is stored in both; `Accept` decides otherwise.
    pub format: Option<String>,
    /// The envelope of a sealed URL, public port only.
    pub e: Option<String>,
}

impl RepresentationQuery {
    /// The `format` parameter, parsed; `400` for a value that names no
    /// format, as a minter elsewhere could write.
    fn format(&self) -> ApiResult<Option<ImageFormat>> {
        self.format
            .as_deref()
            .map(|value| validate_format(value).map_err(ApiError::BadRequest))
            .transpose()
    }
}

/// Which port is asking: the public one does not see terminal blobs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Port {
    Private,
    Public,
}

/// What the request wants served, decided once from the query and the
/// `Accept` header.
#[derive(Debug, Clone, Copy)]
struct Wanted {
    /// The format asked for by name, when any.
    asked: Option<ImageFormat>,
    /// Whether the client takes JPEG XL, for when none is: its `Accept`
    /// lists `image/jxl`, or it sent none, a client that is not a browser
    /// and gets what is stored.
    accepts_jxl: bool,
}

impl Wanted {
    fn new(query: &RepresentationQuery, headers: &HeaderMap) -> ApiResult<Self> {
        Ok(Self {
            asked: query.format()?,
            accepts_jxl: accepts(headers, ImageFormat::Jxl).unwrap_or(true),
        })
    }

    /// Which of a rendered representation's objects to serve, and whether
    /// `Accept` decided, which the response then varies on. A format asked
    /// for by name is served where the representation is stored in it, and
    /// the picture otherwise, what is stored under its own `Content-Type`:
    /// browsers decode a picture by its bytes, so a `<picture>` source that
    /// promised another format still shows, where a `404` is a broken image
    /// that no later source repairs. Without one, a picture with a fallback
    /// goes to the clients that say they decode it, or say nothing, and the
    /// fallback to the rest; a picture without one goes to everybody.
    fn choose(self, rendered: Rendered, fallback: Option<WebpFallback>) -> Chosen {
        let picture = Chosen {
            rendered,
            object: Object::Picture,
            negotiated: false,
        };
        let fallback = fallback.map(|f| Chosen {
            rendered: f.rendered(),
            object: Object::Fallback,
            negotiated: false,
        });
        match (self.asked, fallback) {
            (Some(ImageFormat::Webp), Some(fallback)) => fallback,
            (Some(_), _) => picture,
            (None, Some(fallback)) => Chosen {
                negotiated: true,
                ..if self.accepts_jxl { picture } else { fallback }
            },
            (None, None) => picture,
        }
    }

    /// The icon's format: the one asked for, both being embedded, else
    /// the deployment's `format`, or WebP for a client that does not decode
    /// it where the deployment serves the `webp_fallback`. The second of
    /// the pair is whether `Accept` decided.
    fn icon(self, format: ImageFormat, webp_fallback: bool) -> (ImageFormat, bool) {
        match self.asked {
            Some(format) => (format, false),
            None if webp_fallback && !self.accepts_jxl => (ImageFormat::Webp, true),
            None => (format, webp_fallback),
        }
    }
}

/// The object to serve, as the row records it.
#[derive(Debug, Clone, Copy)]
struct Chosen {
    rendered: Rendered,
    object: Object,
    /// `Accept` chose, so the response carries `Vary: Accept`.
    negotiated: bool,
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
    let wanted = Wanted::new(&query, headers)?;
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

    // The download name takes the extension of what is served: the
    // picture's recorded format, or the icon's.
    let filename = query.filename.as_deref().unwrap_or(&blob.filename);
    let disposition = |format: ImageFormat| {
        original::content_disposition(
            query.disposition.as_deref(),
            &picture_name(filename, format),
        )
    };
    let render = &state.config().media.render;
    let (icon_format, icon_negotiated) = wanted.icon(render.format, render.webp_fallback);
    let placeholder = |rep_state| {
        placeholder(
            kind,
            &blob,
            rep_state,
            icon_format,
            icon_negotiated,
            disposition(icon_format)?,
            method,
        )
    };
    match served {
        Resolved::Ready(rendered, fallback) => {
            let chosen = wanted.choose(rendered, fallback);
            let served = image(
                state,
                &blob,
                kind,
                chosen,
                disposition(chosen.rendered.format)?,
                method,
                headers,
                Origin::Row,
            )
            .await?;
            match served {
                Some(response) => Ok(response),
                // The object is gone and the repair is queued.
                None => placeholder(RepresentationState::Pending),
            }
        }
        Resolved::Placeholder(rep_state) => placeholder(rep_state),
        Resolved::NotReady(blob_state) => match kind {
            Representation::Thumbnail => placeholder(if blob_state.is_terminal() {
                RepresentationState::Unavailable
            } else {
                RepresentationState::Pending
            }),
            Representation::Preview => Err(ApiError::Conflict(format!(
                "blob is {blob_state} and not readable"
            ))),
        },
    }
}

/// Serve `kind` from the URL's envelope, with no row to look up, when
/// the envelope has it rendered with the current variant version, and
/// with the WebP fallback where the deployment stores one. `None`
/// otherwise: a render was pending, the version has moved or the fallback
/// was turned on since the envelope was minted, or the object is gone,
/// and the row knows better.
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
    // A kind the envelope does not list has no row, `disabled`, which the
    // row-backed path answers as it does today.
    let Some(sealed) = envelope.representation(kind) else {
        return Ok(None);
    };
    let current = state.config().media.variant_version;
    let (Some(rendered), RepresentationState::Ready, Some(version)) =
        (sealed.rendered, sealed.state, sealed.version)
    else {
        return Ok(None);
    };
    if version != current
        || representation::lacks_fallback(&state.config().media.render, rendered, sealed.fallback)
    {
        return Ok(None);
    }
    let chosen = Wanted::new(query, headers)?.choose(rendered, sealed.fallback);
    let blob = envelope.head(id, filename);
    let name = picture_name(
        query.filename.as_deref().unwrap_or(filename),
        chosen.rendered.format,
    );
    let disposition = original::content_disposition(query.disposition.as_deref(), &name)?;
    image(
        state,
        &blob,
        kind,
        chosen,
        disposition,
        method,
        headers,
        Origin::Envelope,
    )
    .await
}

async fn refetch(state: &Agrafe, id: Uuid, port: Port) -> ApiResult<BlobHead> {
    state
        .find_blob_head(id)
        .await?
        .filter(|blob| port == Port::Private || blob.visible_on_public(Timestamp::now()))
        .ok_or(ApiError::NotFound)
}

/// Whether `Accept` lists `format`'s media type by name, with a non-zero
/// quality; `None` without an `Accept` header at all. A wildcard does not
/// count: the browsers the fallback is for send `*/*`, and the ones that
/// decode JPEG XL say so. Every browser sends the header, so a request
/// without one is not a browser's, and the caller serves it what is
/// stored.
fn accepts(headers: &HeaderMap, format: ImageFormat) -> Option<bool> {
    let mut values = headers.get_all(header::ACCEPT).iter().peekable();
    values.peek()?;
    Some(
        values
            .filter_map(|value| value.to_str().ok())
            .flat_map(|value| value.split(','))
            .any(|entry| {
                let mut parts = entry.split(';').map(str::trim);
                let media_type = parts.next().unwrap_or_default();
                media_type.eq_ignore_ascii_case(format.content_type())
                    && !parts.any(|param| {
                        param
                            .strip_prefix("q=")
                            .is_some_and(|q| q.trim().parse::<f32>().is_ok_and(|q| q <= 0.0))
                    })
            }),
    )
}

/// The rendered representation, with the conditional request handling a
/// cacheable image deserves. The `ETag` and the length come from the row,
/// so a `304` and a `HEAD` never read the object; only a `GET` with a body
/// to send does. `None` when the object is not in the bucket: from the row
/// the repair is then queued, from the envelope nothing is, since the sealed
/// path looks nothing up and the row decides. On the sealed path a data key
/// that does not unwrap under a known KEK is another blob's envelope, the
/// minter's mistake, so it is `400` rather than the `500` a row that does
/// not unwrap earns.
#[allow(clippy::too_many_arguments)]
async fn image(
    state: &Agrafe,
    blob: &BlobHead,
    kind: Representation,
    chosen: Chosen,
    disposition: HeaderValue,
    method: &Method,
    headers: &HeaderMap,
    origin: Origin,
) -> ApiResult<Option<Response>> {
    let rendered = chosen.rendered;
    let etag = original::etag(&rendered.sha256);
    let mut response = Response::new(Body::empty());
    let h = response.headers_mut();
    h.insert(header::ETAG, HeaderValue::from_str(&etag).expect("ascii"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static(CACHE_READY));
    h.insert(STATE_HEADER, state_value(RepresentationState::Ready));
    if chosen.negotiated {
        h.insert(header::VARY, HeaderValue::from_static("accept"));
    }
    if original::none_match(headers, &etag) {
        *response.status_mut() = StatusCode::NOT_MODIFIED;
        return Ok(Some(response));
    }
    if method == Method::HEAD {
        picture_headers(
            response.headers_mut(),
            rendered.format,
            rendered.size,
            disposition,
        );
        return Ok(Some(response));
    }
    let loaded = match origin {
        Origin::Row => state.load_representation(blob, kind, chosen.object).await,
        Origin::Envelope => state.read_representation(blob, kind, chosen.object).await,
    };
    let loaded = loaded.map_err(|e| match (origin, &e) {
        (
            Origin::Envelope,
            representation::Error::Read(ReadError::Key(UnwrapError::Crypto { .. })),
        ) => ApiError::BadRequest(format!("envelope: {e}")),
        _ => ApiError::from(e),
    })?;
    let Some(bytes) = loaded else {
        return Ok(None);
    };
    picture_headers(
        response.headers_mut(),
        rendered.format,
        bytes.len() as u64,
        disposition,
    );
    *response.body_mut() = Body::from(bytes);
    Ok(Some(response))
}

/// What stands in for a representation that is not there: the icon for a
/// thumbnail, in `format`, a `404` for a preview. A pending one must not
/// be cached, the client is expected back; one `Accept` chose the format
/// of varies on it.
fn placeholder(
    kind: Representation,
    blob: &BlobHead,
    rep_state: RepresentationState,
    format: ImageFormat,
    negotiated: bool,
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
            let icon = icons::icon_for(format, blob.category, &blob.content_type);
            let mut response = Response::new(Body::empty());
            picture_headers(
                response.headers_mut(),
                format,
                icon.len() as u64,
                disposition,
            );
            if negotiated {
                response
                    .headers_mut()
                    .insert(header::VARY, HeaderValue::from_static("accept"));
            }
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

fn picture_headers(h: &mut HeaderMap, format: ImageFormat, len: u64, disposition: HeaderValue) {
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(format.content_type()),
    );
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

/// The download name of a representation: the file's name with the
/// picture format's extension in place of its own.
fn picture_name(filename: &str, format: ImageFormat) -> String {
    let (stem, _) = split_extension(filename);
    format!("{stem}.{}", format.extension())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picture_names() {
        let webp = |name| picture_name(name, ImageFormat::Webp);
        assert_eq!(webp("photo été.jpg"), "photo été.webp");
        assert_eq!(webp("archive.tar.gz"), "archive.webp");
        assert_eq!(webp("my.report.pdf"), "my.report.webp");
        assert_eq!(webp("README"), "README.webp");
        assert_eq!(webp(".bashrc"), ".bashrc.webp");
        assert_eq!(picture_name("photo.jpg", ImageFormat::Jxl), "photo.jxl");
    }

    /// What browsers send: Safari, Chrome and Firefox name `image/jxl`
    /// once they decode it; Edge and the rest offer `*/*`, which is not a
    /// claim to. No header at all is no browser, and no answer.
    #[test]
    fn accept_names_the_format_or_does_not() {
        let accept = |value: &str| {
            let mut h = HeaderMap::new();
            h.insert(header::ACCEPT, HeaderValue::from_str(value).unwrap());
            accepts(&h, ImageFormat::Jxl)
        };
        assert_eq!(
            accept(
                "image/webp,image/avif,image/jxl,image/heic,image/heic-sequence,video/*;q=0.8,\
                 image/png,image/svg+xml,image/*;q=0.8,*/*;q=0.5"
            ),
            Some(true)
        );
        assert_eq!(
            accept(
                "image/avif,image/jxl,image/webp,image/png,image/svg+xml,image/*;q=0.8,*/*;q=0.5"
            ),
            Some(true)
        );
        assert_eq!(accept("image/JXL"), Some(true));
        assert_eq!(accept("image/jxl; q=0.9"), Some(true));
        assert_eq!(
            accept("image/avif,image/webp,image/apng,image/svg+xml,image/*,*/*;q=0.8"),
            Some(false)
        );
        assert_eq!(accept("*/*"), Some(false));
        assert_eq!(accept("image/*"), Some(false));
        assert_eq!(accept("image/jxl;q=0"), Some(false));
        assert_eq!(accept(""), Some(false));
        assert_eq!(
            accepts(&HeaderMap::new(), ImageFormat::Jxl),
            None,
            "no header"
        );
        let mut two = HeaderMap::new();
        two.append(header::ACCEPT, HeaderValue::from_static("image/webp"));
        two.append(header::ACCEPT, HeaderValue::from_static("image/jxl"));
        assert_eq!(accepts(&two, ImageFormat::Jxl), Some(true));
    }

    #[test]
    fn the_choice_is_the_query_s_then_the_accept_s() {
        let jxl = Rendered {
            format: ImageFormat::Jxl,
            sha256: [1; 32],
            size: 10,
        };
        let webp = WebpFallback {
            sha256: [2; 32],
            size: 20,
        };
        let wanted = |asked, accepts_jxl| Wanted { asked, accepts_jxl };

        // Without a fallback: the picture, whatever Accept says and
        // whatever format is asked for, which gets what is stored.
        let chosen = wanted(None, false).choose(jxl, None);
        assert_eq!((chosen.object, chosen.negotiated), (Object::Picture, false));
        let chosen = wanted(Some(ImageFormat::Jxl), false).choose(jxl, None);
        assert_eq!(chosen.object, Object::Picture);
        let chosen = wanted(Some(ImageFormat::Webp), true).choose(jxl, None);
        assert_eq!((chosen.object, chosen.negotiated), (Object::Picture, false));
        assert_eq!(chosen.rendered, jxl);

        // With one: Accept decides, and the response says so.
        let chosen = wanted(None, true).choose(jxl, Some(webp));
        assert_eq!((chosen.object, chosen.negotiated), (Object::Picture, true));
        let chosen = wanted(None, false).choose(jxl, Some(webp));
        assert_eq!((chosen.object, chosen.negotiated), (Object::Fallback, true));
        assert_eq!(chosen.rendered, webp.rendered());
        // Unless the query did.
        let chosen = wanted(Some(ImageFormat::Webp), true).choose(jxl, Some(webp));
        assert_eq!(
            (chosen.object, chosen.negotiated),
            (Object::Fallback, false)
        );
        let chosen = wanted(Some(ImageFormat::Jxl), false).choose(jxl, Some(webp));
        assert_eq!((chosen.object, chosen.negotiated), (Object::Picture, false));
    }

    #[test]
    fn the_icon_follows_the_deployment_then_the_client() {
        use ImageFormat::{Jxl, Webp};
        let wanted = |asked, accepts_jxl| Wanted { asked, accepts_jxl };
        assert_eq!(wanted(None, false).icon(Webp, false), (Webp, false));
        assert_eq!(
            wanted(Some(Jxl), false).icon(Webp, false),
            (Jxl, false),
            "both are embedded"
        );
        assert_eq!(wanted(None, false).icon(Jxl, false), (Jxl, false));
        assert_eq!(wanted(None, false).icon(Jxl, true), (Webp, true));
        assert_eq!(wanted(None, true).icon(Jxl, true), (Jxl, true));
        assert_eq!(wanted(Some(Jxl), false).icon(Jxl, true), (Jxl, false));
    }
}
