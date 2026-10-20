//! Minting signed public URLs for a blob, and the signed query parameters
//! they may carry. The server is the only minter, so a parameter is checked
//! here, when the backend asks for it, rather than on a correctly signed URL
//! later.

use std::collections::BTreeMap;
use std::time::{Duration, SystemTime};

use http::Method;
use jiff::Timestamp;
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::envelope::Envelope;
use super::{Blob, create, repo};
use crate::app::Agrafe;

/// Longest a representation request may `wait` for a pending render.
pub const MAX_WAIT: Duration = Duration::from_secs(30);

/// Why URLs could not be minted.
#[derive(Debug, thiserror::Error)]
pub enum MintError {
    /// No blob has this id.
    #[error("no such blob")]
    NotFound,
    /// The options are not acceptable; the message says why.
    #[error("{0}")]
    Invalid(String),
    /// The query failed.
    #[error("database: {0}")]
    Db(#[from] sqlx::Error),
}

impl Agrafe {
    /// The URLs for `blob` with the default options. Never fails.
    pub fn urls(&self, blob: &Blob) -> BlobUrls {
        mint(self, blob, &MintOptions::default(), SystemTime::now())
    }

    /// URLs with `opts`, checked against the configuration.
    pub async fn mint_urls(&self, id: Uuid, opts: &MintOptions) -> Result<BlobUrls, MintError> {
        let max = self.signer().max_ttl();
        if opts.ttl.is_some_and(|ttl| ttl > max) {
            return Err(MintError::Invalid(format!(
                "ttl must be at most {}s (signing.ttl.max)",
                max.as_secs()
            )));
        }
        validate_query(&opts.query)?;
        let blob = repo::find(self.pool(), id)
            .await?
            .ok_or(MintError::NotFound)?;
        Ok(mint(self, &blob, opts, SystemTime::now()))
    }
}

/// Everything but RFC 3986 unreserved characters is encoded, so the path we
/// mint is byte-for-byte what browsers send back.
const PATH_SEGMENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

/// Percent-encode `s` as one path segment, everything but RFC 3986
/// unreserved characters encoded, as the minted URLs carry the filename.
pub fn encode_segment(s: &str) -> String {
    utf8_percent_encode(s, PATH_SEGMENT).to_string()
}

/// The signed public URLs of a blob. The read URLs of a `ready` blob are
/// sealed: they carry its envelope, and expire no later than the blob.
#[derive(Debug, Clone, Serialize)]
pub struct BlobUrls {
    /// The `PUT` of the bytes, signed for `signing.ttl.upload`. Absent when
    /// the blob may not be uploaded through the public port.
    pub upload: Option<String>,
    /// The public metadata, signed for `signing.ttl.metadata`.
    pub metadata: String,
    /// The original, under the blob's filename.
    pub original: String,
    /// The preview, under the blob's filename.
    pub preview: String,
    /// The thumbnail, under the blob's filename.
    pub thumbnail: String,

    /// Unix timestamp the read URLs expire at.
    pub expires_at: u64,
}

/// What `POST /blobs/:id/urls` accepts, and what `Agrafe::mint_urls` takes.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MintOptions {
    /// Overrides the configured read and metadata TTLs, up to
    /// `signing.ttl.max`.
    #[serde(default, with = "humantime_serde")]
    pub ttl: Option<Duration>,
    /// Signed query parameters appended to the representation URLs.
    #[serde(default)]
    pub query: BTreeMap<String, String>,
}

/// Check the query parameters a backend asks to sign into representation
/// URLs: a bad value is the backend's mistake and is refused now.
pub(crate) fn validate_query(query: &BTreeMap<String, String>) -> Result<(), MintError> {
    for (name, value) in query {
        match name.as_str() {
            "disposition" => {
                disposition_kind(Some(value)).map_err(MintError::Invalid)?;
            }
            "filename" => {
                create::validate_filename(value)
                    .map_err(|e| MintError::Invalid(format!("query filename: {e}")))?;
            }
            "wait" => {
                validate_wait(value).map_err(MintError::Invalid)?;
            }
            other => {
                // `e` among them: the envelope is the server's to add.
                return Err(MintError::Invalid(format!(
                    "unknown query parameter `{other}`; allowed: disposition, filename, wait"
                )));
            }
        }
    }
    Ok(())
}

/// The `disposition` parameter: `inline` (default) or `attachment`.
pub fn disposition_kind(value: Option<&str>) -> Result<&'static str, String> {
    match value {
        None | Some("inline") => Ok("inline"),
        Some("attachment") => Ok("attachment"),
        Some(other) => Err(format!(
            "disposition must be inline or attachment, not `{other}`"
        )),
    }
}

/// The `wait` parameter: milliseconds, at most `MAX_WAIT`.
pub(crate) fn validate_wait(value: &str) -> Result<Duration, String> {
    let ms: u64 = value
        .parse()
        .map_err(|_| format!("wait must be a number of milliseconds, not `{value}`"))?;
    let wait = Duration::from_millis(ms);
    if wait > MAX_WAIT {
        return Err(format!("wait must be at most {} ms", MAX_WAIT.as_millis()));
    }
    Ok(wait)
}

/// The signed query parameter carrying the envelope of a sealed URL.
pub(crate) const ENVELOPE_PARAM: &str = "e";

/// Mint at `now`, for callers that want several blobs to share one expiry.
/// The read URLs of a `ready` blob are sealed: they carry its envelope,
/// and expire no later than the blob does.
pub(crate) fn mint(state: &Agrafe, blob: &Blob, opts: &MintOptions, now: SystemTime) -> BlobUrls {
    let signer = state.signer();
    let ttl = &state.config().signing.ttl;
    let mut read_expires = signer.expiry_for(opts.ttl.unwrap_or(ttl.read), now);
    let metadata_expires = signer.expiry_for(opts.ttl.unwrap_or(ttl.metadata), now);

    let envelope = Timestamp::try_from(now)
        .ok()
        .and_then(|now| Envelope::from_head(&blob.head(), now))
        .map(|e| e.encode());
    if envelope.is_some()
        && let Some(at) = blob.expires_at
    {
        // Nothing on the sealed path checks the row, so the URL carries
        // the expiry.
        read_expires = read_expires.min(at.as_second().max(0) as u64);
    }

    // Sorted by name, the envelope among the others, so that a minter
    // elsewhere builds the same string.
    let mut query = opts.query.clone();
    if let Some(e) = envelope {
        query.insert(ENVELOPE_PARAM.to_owned(), e);
    }
    let query = if query.is_empty() {
        String::new()
    } else {
        let mut q = form_urlencoded::Serializer::new(String::new());
        for (k, v) in &query {
            q.append_pair(k, v);
        }
        format!("?{}", q.finish())
    };

    let filename = encode_segment(&blob.filename);
    let representation = |kind: &str| {
        let tail = format!("/{}/{kind}/{filename}{query}", blob.id);
        url(state, &Method::GET, read_expires, &tail)
    };

    let upload = blob.public_upload_allowed().then(|| {
        let expires = signer.expiry_for(ttl.upload, now);
        url(state, &Method::PUT, expires, &format!("/{}", blob.id))
    });

    BlobUrls {
        upload,
        metadata: url(
            state,
            &Method::GET,
            metadata_expires,
            &format!("/{}", blob.id),
        ),
        original: representation("original"),
        preview: representation("preview"),
        thumbnail: representation("thumbnail"),
        expires_at: read_expires,
    }
}

fn url(state: &Agrafe, method: &Method, expires: u64, tail: &str) -> String {
    let token = state.signer().sign(method, expires, tail);
    format!(
        "{}/blobs/{token}{tail}",
        state.config().server.public_base_url
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wait_bounds() {
        assert_eq!(validate_wait("0").unwrap(), Duration::ZERO);
        assert_eq!(validate_wait("30000").unwrap(), MAX_WAIT);
        assert!(validate_wait("30001").is_err());
        assert!(validate_wait("-1").is_err());
        assert!(validate_wait("soon").is_err());
    }

    #[test]
    fn encodes_segments() {
        assert_eq!(encode_segment("photo.jpg"), "photo.jpg");
        assert_eq!(encode_segment("a b/c?d#e.png"), "a%20b%2Fc%3Fd%23e.png");
        assert_eq!(encode_segment("été.pdf"), "%C3%A9t%C3%A9.pdf");
    }
}
