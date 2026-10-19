//! Serving the original bytes of a blob, decrypted on the fly, with `Range`.
//! The decryption itself lives in `blob::read`; this module maps HTTP onto it.

use std::ops::Range;

use axum::body::Body;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::Response;
use bytes::Bytes;
use futures::StreamExt;
use jiff::Timestamp;
use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use serde::Deserialize;
use uuid::Uuid;

use agrafe::Agrafe;
use agrafe::blob::BlobHead;
use agrafe::blob::envelope::Envelope;
use agrafe::blob::read::{ReadError, Reader, Stored};
use agrafe::blob::urls::disposition_kind;

use crate::error::{ApiError, ApiResult};

/// Signed query parameters the minter may attach to an original URL.
#[derive(Debug, Default, Deserialize)]
pub struct OriginalQuery {
    /// `inline` (default) or `attachment`.
    pub disposition: Option<String>,
    /// Download name, instead of the blob's filename.
    pub filename: Option<String>,
    /// The envelope of a sealed URL, public port only.
    pub e: Option<String>,
}

/// Where the row came from: the database, or the URL's envelope. On the
/// sealed path a missing object means the blob is gone, since nothing
/// checked the row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    Row,
    Envelope,
}

/// What serving an original needs of a blob, from either origin.
pub struct Original {
    pub id: Uuid,
    pub filename: String,
    pub content_type: String,
    pub uploaded_at: Option<Timestamp>,
    pub stored: Stored,
    pub origin: Origin,
}

impl Original {
    /// From a row. `filename` is the path segment, which must match the
    /// blob's filename so a URL cannot pretend to serve another file; the
    /// blob must be `ready`.
    pub fn from_row(blob: BlobHead, filename: &str) -> ApiResult<Self> {
        if filename != blob.filename {
            return Err(ApiError::NotFound);
        }
        let now = Timestamp::now();
        if !blob.is_readable(now) {
            return Err(ApiError::Conflict(format!(
                "blob is {} and not readable",
                blob.state_at(now)
            )));
        }
        let stored = Stored::from_head(&blob).map_err(ApiError::internal)?;
        Ok(Self {
            id: blob.id,
            filename: blob.filename,
            content_type: blob.content_type,
            uploaded_at: blob.uploaded_at,
            stored,
            origin: Origin::Row,
        })
    }

    /// From the envelope of a sealed URL: the filename is the signed path
    /// segment, and the envelope only exists for a `ready` blob.
    pub fn from_envelope(id: Uuid, filename: String, envelope: Envelope) -> ApiResult<Self> {
        let stored = Stored::from_envelope(id, &envelope).map_err(ApiError::internal)?;
        Ok(Self {
            id,
            filename,
            content_type: envelope.content_type,
            uploaded_at: envelope.uploaded_at,
            stored,
            origin: Origin::Envelope,
        })
    }
}

/// Serve the original `blob` describes.
pub async fn serve(
    state: &Agrafe,
    blob: Original,
    query: OriginalQuery,
    method: &Method,
    headers: &HeaderMap,
) -> ApiResult<Response> {
    let stored = &blob.stored;
    let origin = blob.origin;
    let etag = etag(&stored.checksum);

    let mut response = Response::new(Body::empty());
    let h = response.headers_mut();
    h.insert(header::ETAG, HeaderValue::from_str(&etag).expect("ascii"));
    h.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    // The URL is a capability with its own expiry; blobs are write-once, so
    // the bytes behind it never change.
    h.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, max-age=31536000, immutable"),
    );
    if let Some(at) = blob.uploaded_at {
        h.insert(header::LAST_MODIFIED, http_date(at));
    }

    if none_match(headers, &etag) {
        *response.status_mut() = StatusCode::NOT_MODIFIED;
        return Ok(response);
    }

    let h = response.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(&blob.content_type).map_err(|_| {
            ApiError::internal(format!("blob {} has an invalid content type", blob.id))
        })?,
    );
    h.insert(
        header::CONTENT_DISPOSITION,
        content_disposition(
            query.disposition.as_deref(),
            query.filename.as_deref().unwrap_or(&blob.filename),
        )?,
    );
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    // User content is served from this origin; never let it run scripts.
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("sandbox"),
    );

    // An `If-Range` naming another entity means the client's partial copy is
    // not of this file, so the range is ignored and the whole file is sent.
    let range_applies = headers
        .get(header::IF_RANGE)
        .and_then(|v| v.to_str().ok())
        .is_none_or(|v| v == etag);
    let range = match headers.get(header::RANGE).filter(|_| range_applies) {
        Some(value) => match parse_range(value.to_str().unwrap_or(""), stored.size) {
            RangeOutcome::Satisfiable(range) => Some(range),
            RangeOutcome::Unsatisfiable => {
                *response.status_mut() = StatusCode::RANGE_NOT_SATISFIABLE;
                response.headers_mut().insert(
                    header::CONTENT_RANGE,
                    HeaderValue::from_str(&format!("bytes */{}", stored.size)).expect("ascii"),
                );
                return Ok(response);
            }
            RangeOutcome::Ignored => None,
        },
        None => None,
    };
    let (status, span) = match range {
        Some(range) => (StatusCode::PARTIAL_CONTENT, range),
        None => (StatusCode::OK, 0..stored.size),
    };
    *response.status_mut() = status;
    let h = response.headers_mut();
    h.insert(
        header::CONTENT_LENGTH,
        HeaderValue::from(span.end - span.start),
    );
    if status == StatusCode::PARTIAL_CONTENT {
        h.insert(
            header::CONTENT_RANGE,
            HeaderValue::from_str(&format!(
                "bytes {}-{}/{}",
                span.start,
                span.end - 1,
                stored.size
            ))
            .expect("ascii"),
        );
    }
    if method == Method::HEAD || span.is_empty() {
        return Ok(response);
    }

    let mut reader = Reader::open(state, stored, span).await.map_err(|e| {
        if origin == Origin::Envelope && e.is_not_found() {
            ApiError::NotFound
        } else {
            ApiError::internal(format!("blob {}: opening the original: {e}", blob.id))
        }
    })?;
    // Open the first segment before the response starts: a wrong key, a
    // tampered object or a missing one is then a logged `500`, not a `200`
    // cut short.
    let first = reader
        .next_segment()
        .await
        .map_err(|e| {
            ApiError::internal(format!("blob {}: opening the first segment: {e}", blob.id))
        })?
        .ok_or_else(|| ApiError::internal(format!("blob {}: range is empty", blob.id)))?;
    *response.body_mut() = Body::from_stream(plaintext_stream(blob.id, first, reader));
    Ok(response)
}

/// `first`, then every further segment. A failure here happens after the
/// status line went out, so the connection is cut; it is logged, since the
/// client cannot tell a corrupt object from a dropped connection.
fn plaintext_stream(
    blob_id: Uuid,
    first: Bytes,
    reader: Reader,
) -> impl futures::Stream<Item = Result<Bytes, ReadError>> {
    let rest = futures::stream::try_unfold(reader, move |mut r| async move {
        match r.next_segment().await {
            Ok(Some(bytes)) => Ok(Some((bytes, r))),
            Ok(None) => Ok(None),
            Err(err) => {
                tracing::error!(blob = %blob_id, error = ?err, "serving the original failed mid-stream");
                Err(err)
            }
        }
    });
    futures::stream::once(async move { Ok(first) }).chain(rest)
}

enum RangeOutcome {
    Satisfiable(Range<u64>),
    Unsatisfiable,
    /// Malformed or multiple ranges: the whole representation is served.
    Ignored,
}

/// Parse a single `bytes=` range against a representation of `size` bytes.
fn parse_range(value: &str, size: u64) -> RangeOutcome {
    let Some(spec) = value.trim().strip_prefix("bytes=") else {
        return RangeOutcome::Ignored;
    };
    if spec.contains(',') {
        return RangeOutcome::Ignored;
    }
    let Some((start, end)) = spec.trim().split_once('-') else {
        return RangeOutcome::Ignored;
    };
    let (start, end) = match (start.trim(), end.trim()) {
        ("", "") => return RangeOutcome::Ignored,
        // Suffix: the last `n` bytes.
        ("", n) => {
            let Ok(n) = n.parse::<u64>() else {
                return RangeOutcome::Ignored;
            };
            if n == 0 || size == 0 {
                return RangeOutcome::Unsatisfiable;
            }
            (size.saturating_sub(n), size)
        }
        (s, "") => {
            let Ok(s) = s.parse::<u64>() else {
                return RangeOutcome::Ignored;
            };
            (s, size)
        }
        (s, e) => {
            let (Ok(s), Ok(e)) = (s.parse::<u64>(), e.parse::<u64>()) else {
                return RangeOutcome::Ignored;
            };
            if e < s {
                return RangeOutcome::Ignored;
            }
            (s, e.saturating_add(1).min(size))
        }
    };
    if start >= size {
        return RangeOutcome::Unsatisfiable;
    }
    RangeOutcome::Satisfiable(start..end)
}

/// A strong `ETag` from a content digest.
pub(crate) fn etag(digest: &[u8]) -> String {
    use std::fmt::Write;
    let mut tag = String::with_capacity(digest.len() * 2 + 2);
    tag.push('"');
    for b in digest {
        write!(tag, "{b:02x}").expect("writing to a string");
    }
    tag.push('"');
    tag
}

/// Whether `If-None-Match` names `etag`, so the client's copy is current.
pub(crate) fn none_match(headers: &HeaderMap, etag: &str) -> bool {
    headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v == "*" || v.split(',').any(|t| t.trim() == etag))
}

fn http_date(at: Timestamp) -> HeaderValue {
    HeaderValue::from_str(&at.strftime("%a, %d %b %Y %H:%M:%S GMT").to_string()).expect("ascii")
}

/// RFC 6266: an ASCII fallback plus the UTF-8 name.
pub(crate) fn content_disposition(disposition: Option<&str>, name: &str) -> ApiResult<HeaderValue> {
    let kind = disposition_kind(disposition).map_err(ApiError::BadRequest)?;
    let fallback: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_graphic() && c != '"' && c != '\\' || c == ' ' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let encoded = utf8_percent_encode(name, NON_ALPHANUMERIC).to_string();
    HeaderValue::from_str(&format!(
        "{kind}; filename=\"{fallback}\"; filename*=UTF-8''{encoded}"
    ))
    .map_err(|_| ApiError::BadRequest("filename is not valid".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(v: &str, size: u64) -> Option<Range<u64>> {
        match parse_range(v, size) {
            RangeOutcome::Satisfiable(r) => Some(r),
            RangeOutcome::Unsatisfiable => Some(u64::MAX..u64::MAX),
            RangeOutcome::Ignored => None,
        }
    }

    #[test]
    fn ranges() {
        assert_eq!(r("bytes=0-9", 100), Some(0..10));
        assert_eq!(r("bytes=90-", 100), Some(90..100));
        assert_eq!(r("bytes=-10", 100), Some(90..100));
        assert_eq!(r("bytes=-200", 100), Some(0..100));
        assert_eq!(r("bytes=0-200", 100), Some(0..100));
        assert_eq!(r("bytes=99-99", 100), Some(99..100));
        assert_eq!(r("bytes=100-", 100), Some(u64::MAX..u64::MAX));
        assert_eq!(r("bytes=-0", 100), Some(u64::MAX..u64::MAX));
        assert_eq!(r("bytes=0-", 0), Some(u64::MAX..u64::MAX));
        assert_eq!(r("bytes=0-1,5-6", 100), None);
        assert_eq!(r("bytes=5-1", 100), None);
        assert_eq!(r("items=0-1", 100), None);
        assert_eq!(r("bytes=a-b", 100), None);
    }

    #[test]
    fn disposition_header() {
        assert_eq!(
            content_disposition(Some("attachment"), "été \"x\".pdf").unwrap(),
            "attachment; filename=\"_t_ _x_.pdf\"; filename*=UTF-8''%C3%A9t%C3%A9%20%22x%22%2Epdf"
        );
        assert!(content_disposition(Some("other"), "a").is_err());
        assert_eq!(
            content_disposition(None, "renamed.pdf").unwrap(),
            "inline; filename=\"renamed.pdf\"; filename*=UTF-8''renamed%2Epdf"
        );
    }

    #[test]
    fn dates() {
        let at: Timestamp = "2026-10-16T08:44:26Z".parse().unwrap();
        assert_eq!(http_date(at), "Fri, 16 Oct 2026 08:44:26 GMT");
    }
}
