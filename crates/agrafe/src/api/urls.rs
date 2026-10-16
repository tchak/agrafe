//! Minting signed public URLs for a blob.

use std::collections::BTreeMap;
use std::time::{Duration, SystemTime};

use http::Method;
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use serde::Serialize;

use super::original;
use crate::app::AppState;
use crate::blob::Blob;
use crate::blob::create;
use crate::error::ApiError;
use crate::signing::PERMANENT;

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

#[derive(Debug, Clone, Default)]
pub struct MintOptions {
    /// Overrides the configured read TTL.
    pub ttl: Option<Duration>,
    /// Never expire. Requires `signing.allow_permanent`.
    pub permanent: bool,
    /// Signed query parameters appended to the representation URLs.
    pub query: BTreeMap<String, String>,
}

/// Check the query parameters a backend asks to sign into representation
/// URLs. The server is the only minter, so a bad value here is the backend's
/// mistake and answers `422` now, rather than a client error on a correctly
/// signed URL later.
pub fn validate_query(query: &BTreeMap<String, String>) -> Result<(), ApiError> {
    for (name, value) in query {
        match name.as_str() {
            "disposition" => {
                original::disposition_kind(Some(value)).map_err(ApiError::Unprocessable)?;
            }
            "filename" => {
                create::validate_filename(value)
                    .map_err(|e| ApiError::Unprocessable(format!("query filename: {e}")))?;
            }
            other => {
                return Err(ApiError::Unprocessable(format!(
                    "unknown query parameter `{other}`; allowed: disposition, filename"
                )));
            }
        }
    }
    Ok(())
}

pub fn mint(state: &AppState, blob: &Blob, opts: &MintOptions, now: SystemTime) -> BlobUrls {
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

fn url(state: &AppState, method: &Method, expires: u64, tail: &str) -> String {
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
    fn encodes_segments() {
        assert_eq!(encode_segment("photo.jpg"), "photo.jpg");
        assert_eq!(encode_segment("a b/c?d#e.png"), "a%20b%2Fc%3Fd%23e.png");
        assert_eq!(encode_segment("été.pdf"), "%C3%A9t%C3%A9.pdf");
    }
}
