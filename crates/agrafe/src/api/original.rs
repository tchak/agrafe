//! Serving the original bytes of a blob, decrypted on the fly, with `Range`.
//!
//! A range of plaintext maps to a range of whole segments, fetched from the
//! object store in one read, opened one by one and trimmed at both ends. The
//! header cached on the row provides the salt and nonce prefix, so no second
//! read is needed.

use std::ops::Range;

use agrafe_crypto::streaming::{Decryptor, Header, Layout};
use anyhow::Context as _;
use axum::body::Body;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::Response;
use bytes::Bytes;
use futures::StreamExt;
use futures::stream::BoxStream;
use jiff::Timestamp;
use object_store::path::Path as ObjectPath;
use object_store::{GetOptions, GetRange};
use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use serde::Deserialize;
use uuid::Uuid;

use crate::app::AppState;
use crate::blob::Blob;
use crate::error::{ApiError, ApiResult};

/// Signed query parameters the minter may attach to an original URL.
#[derive(Debug, Default, Deserialize)]
pub struct OriginalQuery {
    /// `inline` (default) or `attachment`.
    pub disposition: Option<String>,
    /// Download name, instead of the blob's filename.
    pub filename: Option<String>,
}

/// Serve `blob`'s original. `filename` is the path segment, which must match
/// the blob's filename so a URL cannot pretend to serve another file.
pub async fn serve(
    state: &AppState,
    blob: Blob,
    filename: &str,
    query: OriginalQuery,
    method: &Method,
    headers: &HeaderMap,
) -> ApiResult<Response> {
    if filename != blob.filename {
        return Err(ApiError::NotFound);
    }
    if !blob.is_readable() {
        return Err(ApiError::Conflict(format!(
            "blob is {} and not readable",
            blob.state
        )));
    }
    let stored = Stored::from_blob(&blob)?;
    let etag = etag(&stored.checksum);

    let mut response = Response::new(Body::empty());
    let h = response.headers_mut();
    h.insert(header::ETAG, HeaderValue::from_str(&etag).expect("ascii"));
    h.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    h.insert(header::CACHE_CONTROL, cache_control(&blob));
    if let Some(at) = blob.uploaded_at {
        h.insert(header::LAST_MODIFIED, http_date(at));
    }

    if headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v == "*" || v.split(',').any(|t| t.trim() == etag))
    {
        *response.status_mut() = StatusCode::NOT_MODIFIED;
        return Ok(response);
    }

    let h = response.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(&blob.content_type).map_err(|_| {
            ApiError::Internal(anyhow::anyhow!(
                "blob {} has an invalid content type",
                blob.id
            ))
        })?,
    );
    h.insert(
        header::CONTENT_DISPOSITION,
        content_disposition(&query, &blob.filename)?,
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

    // A stale `If-Range` means the client's partial copy is from another
    // version, so the range is ignored and the whole file is sent.
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

    let key = state
        .keks()
        .unwrap(blob.id, &stored.kek_id, &stored.wrapped_key)?;
    let decryptor = Decryptor::new(
        &key,
        stored.storage_key.as_bytes(),
        &stored.header,
        stored.layout,
    );
    let (first, bytes) = stored.layout.ciphertext_span(span.clone(), stored.size);
    let source = state
        .store()
        .get_opts(
            &ObjectPath::from(stored.storage_key.as_str()),
            GetOptions {
                range: Some(GetRange::Bounded(bytes)),
                ..Default::default()
            },
        )
        .await
        .map_err(|e| ApiError::Internal(anyhow::Error::new(e).context("reading the object")))?
        .into_stream()
        .boxed();

    let mut reader = Reader {
        source,
        buffer: Vec::new(),
        decryptor,
        layout: stored.layout,
        size: stored.size,
        segment: first,
        skip: (span.start - stored.layout.plaintext_start(first)) as usize,
        remaining: span.end - span.start,
    };
    // Open the first segment before the response starts: a wrong key, a
    // tampered object or a missing one is then a logged `500`, not a `200`
    // cut short.
    let first = reader
        .next_segment()
        .await
        .with_context(|| format!("blob {}: opening the first segment", blob.id))?
        .ok_or_else(|| anyhow::anyhow!("blob {}: range is empty", blob.id))?;
    *response.body_mut() = Body::from_stream(plaintext_stream(blob.id, first, reader));
    Ok(response)
}

/// Everything the row must carry once an object is stored.
struct Stored {
    storage_key: String,
    kek_id: String,
    wrapped_key: Vec<u8>,
    header: Header,
    layout: Layout,
    checksum: Vec<u8>,
    size: u64,
}

impl Stored {
    fn from_blob(blob: &Blob) -> ApiResult<Self> {
        let missing = |what: &str| {
            ApiError::Internal(anyhow::anyhow!(
                "blob {} is ready but has no {what}",
                blob.id
            ))
        };
        let header = Header::parse(
            blob.encryption_header
                .as_deref()
                .ok_or_else(|| missing("encryption header"))?,
        )
        .map_err(|e| ApiError::Internal(anyhow::anyhow!("blob {}: {e}", blob.id)))?;
        let chunk_size = blob.chunk_size.ok_or_else(|| missing("chunk size"))?;
        let layout = usize::try_from(chunk_size)
            .ok()
            .and_then(|s| Layout::new(s).ok())
            .ok_or_else(|| missing("valid chunk size"))?;
        Ok(Self {
            storage_key: blob
                .storage_key
                .clone()
                .ok_or_else(|| missing("storage key"))?,
            kek_id: blob.kek_id.clone().ok_or_else(|| missing("KEK id"))?,
            wrapped_key: blob
                .wrapped_key
                .clone()
                .ok_or_else(|| missing("wrapped key"))?,
            header,
            layout,
            checksum: blob
                .checksum_sha256
                .clone()
                .ok_or_else(|| missing("checksum"))?,
            size: blob.size.ok_or_else(|| missing("size"))? as u64,
        })
    }
}

/// Opens segments as their ciphertext arrives, trimming to the requested
/// plaintext range.
struct Reader {
    source: BoxStream<'static, object_store::Result<Bytes>>,
    buffer: Vec<u8>,
    decryptor: Decryptor,
    layout: Layout,
    size: u64,
    /// Next segment to open.
    segment: u64,
    /// Plaintext bytes to drop from the front of the next opened segment.
    skip: usize,
    /// Plaintext bytes still to send.
    remaining: u64,
}

impl Reader {
    /// The next piece of plaintext, or `None` once the range is served.
    async fn next_segment(&mut self) -> anyhow::Result<Option<Bytes>> {
        if self.remaining == 0 {
            return Ok(None);
        }
        let need = {
            let range = self.layout.ciphertext_range(self.segment, self.size);
            (range.end - range.start) as usize
        };
        while self.buffer.len() < need {
            match self.source.next().await {
                Some(chunk) => self
                    .buffer
                    .extend_from_slice(&chunk.context("reading the object")?),
                None => anyhow::bail!("object ended before segment {}", self.segment),
            }
        }
        let sealed: Vec<u8> = self.buffer.drain(..need).collect();
        let last = self.layout.is_last(self.segment, self.size);
        let mut plain = self
            .decryptor
            .open(self.segment, &sealed, last)
            .with_context(|| format!("opening segment {}", self.segment))?;
        let skip = std::mem::take(&mut self.skip).min(plain.len());
        let take = ((plain.len() - skip) as u64).min(self.remaining) as usize;
        plain.truncate(skip + take);
        plain.drain(..skip);
        self.remaining -= take as u64;
        self.segment += 1;
        Ok(Some(Bytes::from(plain)))
    }
}

/// `first`, then every further segment. A failure here happens after the
/// status line went out, so the connection is cut; it is logged, since the
/// client cannot tell a corrupt object from a dropped connection.
fn plaintext_stream(
    blob_id: Uuid,
    first: Bytes,
    reader: Reader,
) -> impl futures::Stream<Item = anyhow::Result<Bytes>> {
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

fn etag(checksum: &[u8]) -> String {
    let hex: String = checksum.iter().map(|b| format!("{b:02x}")).collect();
    format!("\"{hex}\"")
}

fn cache_control(blob: &Blob) -> HeaderValue {
    if blob.write_once {
        // The URL is a capability with its own expiry; the bytes behind it
        // never change.
        HeaderValue::from_static("private, max-age=31536000, immutable")
    } else {
        HeaderValue::from_static("private, no-cache")
    }
}

fn http_date(at: Timestamp) -> HeaderValue {
    HeaderValue::from_str(&at.strftime("%a, %d %b %Y %H:%M:%S GMT").to_string()).expect("ascii")
}

/// RFC 6266: an ASCII fallback plus the UTF-8 name.
fn content_disposition(query: &OriginalQuery, filename: &str) -> ApiResult<HeaderValue> {
    let kind = match query.disposition.as_deref() {
        None | Some("inline") => "inline",
        Some("attachment") => "attachment",
        Some(other) => {
            return Err(ApiError::BadRequest(format!(
                "disposition must be inline or attachment, not `{other}`"
            )));
        }
    };
    let name = query.filename.as_deref().unwrap_or(filename);
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
        let q = OriginalQuery {
            disposition: Some("attachment".into()),
            filename: None,
        };
        assert_eq!(
            content_disposition(&q, "été \"x\".pdf").unwrap(),
            "attachment; filename=\"_t_ _x_.pdf\"; filename*=UTF-8''%C3%A9t%C3%A9%20%22x%22%2Epdf"
        );
        let q = OriginalQuery {
            disposition: Some("other".into()),
            filename: None,
        };
        assert!(content_disposition(&q, "a").is_err());
        let q = OriginalQuery {
            disposition: None,
            filename: Some("renamed.pdf".into()),
        };
        assert_eq!(
            content_disposition(&q, "a.pdf").unwrap(),
            "inline; filename=\"renamed.pdf\"; filename*=UTF-8''renamed%2Epdf"
        );
    }

    #[test]
    fn dates() {
        let at: Timestamp = "2026-10-16T08:44:26Z".parse().unwrap();
        assert_eq!(http_date(at), "Fri, 16 Oct 2026 08:44:26 GMT");
    }
}
