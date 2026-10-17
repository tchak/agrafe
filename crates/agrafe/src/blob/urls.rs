//! Minting signed public URLs for a blob, and the signed query parameters
//! they may carry. The server is the only minter, so a parameter is checked
//! here, when the backend asks for it, rather than on a correctly signed URL
//! later.

use std::collections::BTreeMap;
use std::time::{Duration, SystemTime};

use http::Method;
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{Blob, create, repo};
use crate::app::Agrafe;
use crate::signing::PERMANENT;

/// Longest a representation request may `wait` for a pending render.
pub const MAX_WAIT: Duration = Duration::from_secs(30);

/// Why URLs could not be minted.
#[derive(Debug, thiserror::Error)]
pub enum MintError {
    #[error("no such blob")]
    NotFound,
    /// The options are not acceptable; the message says why.
    #[error("{0}")]
    Invalid(String),
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
        if opts.permanent && !self.signer().allow_permanent() {
            return Err(MintError::Invalid(
                "permanent URLs are disabled (signing.allow_permanent)".into(),
            ));
        }
        if opts.permanent && opts.ttl.is_some() {
            return Err(MintError::Invalid(
                "ttl and permanent are mutually exclusive".into(),
            ));
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

pub fn encode_segment(s: &str) -> String {
    utf8_percent_encode(s, PATH_SEGMENT).to_string()
}

#[derive(Debug, Clone, Serialize)]
pub struct BlobUrls {
    /// Absent when the blob may not be uploaded through the public port.
    pub upload: Option<String>,
    pub metadata: String,
    pub original: String,
    pub preview: String,
    pub thumbnail: String,
    /// Unix timestamp the read URLs expire at, `0` for never.
    pub expires_at: u64,
}

/// What `POST /blobs/:id/urls` accepts, and what `Agrafe::mint_urls` takes.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MintOptions {
    /// Overrides the configured read TTL.
    #[serde(default, with = "humantime_serde")]
    pub ttl: Option<Duration>,
    /// Never expire. Requires `signing.allow_permanent`.
    #[serde(default)]
    pub permanent: bool,
    /// Signed query parameters appended to the representation URLs.
    #[serde(default)]
    pub query: BTreeMap<String, String>,
}

/// Check the query parameters a backend asks to sign into representation
/// URLs: a bad value is the backend's mistake and is refused now.
pub fn validate_query(query: &BTreeMap<String, String>) -> Result<(), MintError> {
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

/// Mint at `now`, for callers that want several blobs to share one expiry.
pub fn mint(state: &Agrafe, blob: &Blob, opts: &MintOptions, now: SystemTime) -> BlobUrls {
    let signer = state.signer();
    let ttl = &state.config().signing.ttl;
    let read_expires = if opts.permanent {
        PERMANENT
    } else {
        signer.expiry_for(opts.ttl.unwrap_or(ttl.read), now)
    };
    let metadata_expires = if opts.permanent {
        PERMANENT
    } else {
        signer.expiry_for(opts.ttl.unwrap_or(ttl.metadata), now)
    };

    let query = if opts.query.is_empty() {
        String::new()
    } else {
        let mut q = form_urlencoded::Serializer::new(String::new());
        for (k, v) in &opts.query {
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
