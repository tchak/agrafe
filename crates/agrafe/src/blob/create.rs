//! Creating a blob: validation of the request, then the insert.

use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use jiff::{SignedDuration, Timestamp};
use serde::Deserialize;
use uuid::Uuid;

use super::{Blob, InputType, repo};
use crate::app::Agrafe;
use crate::classify::{self, Category};
use crate::config::UploadConfig;
use crate::jobs::Priority;

/// Why a blob could not be created.
#[derive(Debug, thiserror::Error)]
pub enum CreateError {
    /// The request is well formed but not acceptable; the message says why.
    #[error("{0}")]
    Invalid(String),
    #[error("database: {0}")]
    Db(#[from] sqlx::Error),
}

impl Agrafe {
    /// Create a blob: metadata only, no bytes yet.
    pub async fn create_blob(&self, req: CreateBlobRequest) -> Result<Blob, CreateError> {
        let new = validate(req, &self.config().upload, Timestamp::now())?;
        Ok(repo::insert(self.pool(), &new).await?)
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateBlobRequest {
    pub filename: String,
    pub content_type: String,
    pub input_type: InputType,
    /// SHA-256 of the bytes, base64. Required for `raw` and `multipart`.
    pub checksum: Option<String>,
    /// Declared size in bytes. Required for `raw` and `multipart`.
    pub size: Option<i64>,
    #[serde(default)]
    pub skip_virus_scan: bool,
    pub expires_at: Option<Timestamp>,
    #[serde(default, with = "humantime_serde")]
    pub expires_in: Option<Duration>,
    /// Where the blob's jobs go in their kind's queue.
    #[serde(default)]
    pub priority: Priority,
}

/// A validated blob, ready to insert.
#[derive(Debug, Clone)]
pub struct NewBlob {
    pub id: Uuid,
    pub filename: String,
    pub content_type: String,
    pub category: Category,
    pub input_type: InputType,
    pub checksum_sha256: Option<Vec<u8>>,
    pub size: Option<i64>,
    pub skip_virus_scan: bool,
    pub expires_at: Option<Timestamp>,
    pub priority: Priority,
}

pub const MAX_FILENAME_LEN: usize = 255;

pub fn validate(
    req: CreateBlobRequest,
    upload: &UploadConfig,
    now: Timestamp,
) -> Result<NewBlob, CreateError> {
    let filename = validate_filename(&req.filename)?;
    let content_type = normalize_content_type(&req.content_type).map_err(unprocessable)?;
    let category = classify::classify(&content_type);

    let (checksum_sha256, size) = if req.input_type.is_direct() {
        let checksum = req
            .checksum
            .as_deref()
            .ok_or_else(|| unprocessable("checksum is required for raw and multipart input"))?;
        let size = req
            .size
            .ok_or_else(|| unprocessable("size is required for raw and multipart input"))?;
        (
            Some(decode_checksum(checksum)?),
            Some(validate_size(size, upload)?),
        )
    } else {
        if req.checksum.is_some() || req.size.is_some() {
            return Err(unprocessable(
                "checksum and size must be omitted for rows and archive input",
            ));
        }
        validate_generated_content_type(req.input_type, &content_type)?;
        (None, None)
    };

    let expires_at = match (req.expires_at, req.expires_in) {
        (Some(_), Some(_)) => {
            return Err(unprocessable(
                "expires_at and expires_in are mutually exclusive",
            ));
        }
        (Some(at), None) => Some(at),
        (None, Some(d)) => {
            let d = SignedDuration::try_from(d)
                .map_err(|_| unprocessable("expires_in is out of range"))?;
            Some(
                now.checked_add(d)
                    .map_err(|_| unprocessable("expires_in is out of range"))?,
            )
        }
        (None, None) => None,
    };
    if let Some(at) = expires_at
        && at <= now
    {
        return Err(unprocessable("expiration must be in the future"));
    }

    Ok(NewBlob {
        id: Uuid::now_v7(),
        filename,
        content_type,
        category,
        input_type: req.input_type,
        checksum_sha256,
        size,
        skip_virus_scan: req.skip_virus_scan,
        expires_at,
        priority: req.priority,
    })
}

pub(crate) fn validate_filename(filename: &str) -> Result<String, CreateError> {
    let filename = filename.trim();
    if filename.is_empty() {
        return Err(unprocessable("filename must not be empty"));
    }
    if filename.len() > MAX_FILENAME_LEN {
        return Err(unprocessable("filename is too long"));
    }
    if filename == "." || filename == ".." {
        return Err(unprocessable("filename is not valid"));
    }
    if filename
        .chars()
        .any(|c| c == '/' || c == '\\' || c.is_control() || is_bidi_control(c))
    {
        return Err(unprocessable(
            "filename must not contain path separators or control characters",
        ));
    }
    Ok(filename.to_string())
}

/// Bidirectional formatting characters. `char::is_control` covers only the
/// C0 and C1 controls, and these can make `invoice\u{202E}fdp.exe` display
/// as `invoiceexe.pdf` in a download prompt, a URL bar or a thumbnail.
fn is_bidi_control(c: char) -> bool {
    matches!(c, '\u{061C}' | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
}

/// Parse and reduce to `type/subtype`, lowercase, parameters dropped. The
/// error is a message; callers pick the status (422 on create, 400 as a
/// list filter).
pub fn normalize_content_type(raw: &str) -> Result<String, &'static str> {
    let mime: mime::Mime = raw
        .trim()
        .parse()
        .map_err(|_| "content_type is not a valid media type")?;
    if mime.type_() == "*" || mime.subtype() == "*" {
        return Err("content_type must not contain wildcards");
    }
    // The essence keeps a `+xml` or `+json` suffix, which `subtype()` drops.
    Ok(mime.essence_str().trim().to_ascii_lowercase())
}

pub(crate) fn decode_checksum(encoded: &str) -> Result<Vec<u8>, CreateError> {
    let bytes = STANDARD
        .decode(encoded.trim())
        .map_err(|_| unprocessable("checksum must be base64"))?;
    if bytes.len() != 32 {
        return Err(unprocessable(
            "checksum must be a base64 SHA-256 digest (32 bytes)",
        ));
    }
    Ok(bytes)
}

pub(crate) fn validate_size(size: i64, upload: &UploadConfig) -> Result<i64, CreateError> {
    if size < 0 {
        return Err(unprocessable("size must not be negative"));
    }
    if size as u64 > upload.max_size.as_u64() {
        return Err(unprocessable(format!(
            "size exceeds the maximum of {} bytes",
            upload.max_size.as_u64()
        )));
    }
    Ok(size)
}

fn validate_generated_content_type(
    input_type: InputType,
    content_type: &str,
) -> Result<(), CreateError> {
    let ok = match input_type {
        InputType::Rows => matches!(content_type, classify::CSV | classify::XLSX | classify::ODS),
        InputType::Archive => content_type == classify::ZIP,
        InputType::Raw | InputType::Multipart => true,
    };
    if ok {
        Ok(())
    } else {
        Err(unprocessable(format!(
            "content_type `{content_type}` cannot be produced from `{input_type}` input"
        )))
    }
}

fn unprocessable(msg: impl Into<String>) -> CreateError {
    CreateError::Invalid(msg.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_content_types() {
        assert_eq!(
            normalize_content_type(" Image/SVG+XML; charset=utf-8 ").unwrap(),
            "image/svg+xml"
        );
        assert_eq!(normalize_content_type("text/csv").unwrap(), "text/csv");
        assert!(normalize_content_type("image/*").is_err());
        assert!(normalize_content_type("not a type").is_err());
    }

    fn req(input_type: InputType, content_type: &str) -> CreateBlobRequest {
        CreateBlobRequest {
            filename: "photo.jpg".into(),
            content_type: content_type.into(),
            input_type,
            checksum: input_type.is_direct().then(|| STANDARD.encode([7u8; 32])),
            size: input_type.is_direct().then_some(1234),
            skip_virus_scan: false,
            expires_at: None,
            expires_in: None,
            priority: Priority::Normal,
        }
    }

    fn cfg() -> UploadConfig {
        UploadConfig::default()
    }

    #[test]
    fn raw_ok() {
        let b = validate(
            req(InputType::Raw, "image/jpeg; charset=binary"),
            &cfg(),
            Timestamp::now(),
        )
        .unwrap();
        assert_eq!(b.content_type, "image/jpeg");
        assert_eq!(b.category, Category::Image);
        assert_eq!(b.size, Some(1234));
        assert_eq!(b.checksum_sha256.as_deref(), Some(&[7u8; 32][..]));
    }

    #[test]
    fn direct_requires_checksum_and_size() {
        let mut r = req(InputType::Raw, "image/jpeg");
        r.checksum = None;
        assert!(validate(r, &cfg(), Timestamp::now()).is_err());
        let mut r = req(InputType::Multipart, "image/jpeg");
        r.size = None;
        assert!(validate(r, &cfg(), Timestamp::now()).is_err());
    }

    #[test]
    fn generated_rejects_checksum_and_checks_type() {
        let mut r = req(InputType::Rows, classify::CSV);
        r.checksum = Some(STANDARD.encode([0u8; 32]));
        assert!(validate(r, &cfg(), Timestamp::now()).is_err());
        assert!(
            validate(
                req(InputType::Rows, classify::CSV),
                &cfg(),
                Timestamp::now()
            )
            .is_ok()
        );
        assert!(
            validate(
                req(InputType::Rows, classify::XLSX),
                &cfg(),
                Timestamp::now()
            )
            .is_ok()
        );
        assert!(validate(req(InputType::Rows, "image/png"), &cfg(), Timestamp::now()).is_err());
        assert!(
            validate(
                req(InputType::Archive, classify::ZIP),
                &cfg(),
                Timestamp::now()
            )
            .is_ok()
        );
        assert!(
            validate(
                req(InputType::Archive, classify::CSV),
                &cfg(),
                Timestamp::now()
            )
            .is_err()
        );
    }

    #[test]
    fn filename_rules() {
        for bad in [
            "",
            "   ",
            "a/b",
            "a\\b",
            "..",
            "x\u{0}y",
            "invoice\u{202E}fdp.exe",
            "a\u{2066}b",
            "a\u{200F}b",
        ] {
            let mut r = req(InputType::Raw, "image/jpeg");
            r.filename = bad.into();
            assert!(validate(r, &cfg(), Timestamp::now()).is_err(), "{bad:?}");
        }
        // Scripts written right to left are fine; only the controls are not.
        let mut r = req(InputType::Raw, "image/jpeg");
        r.filename = "صورة.jpg".into();
        assert!(validate(r, &cfg(), Timestamp::now()).is_ok());
    }

    #[test]
    fn expiration() {
        let now = Timestamp::now();
        let mut r = req(InputType::Raw, "image/jpeg");
        r.expires_in = Some(Duration::from_secs(3600));
        let b = validate(r, &cfg(), now).unwrap();
        assert_eq!(b.expires_at, Some(now + SignedDuration::from_secs(3600)));

        let mut r = req(InputType::Raw, "image/jpeg");
        r.expires_at = Some(now - SignedDuration::from_secs(1));
        assert!(validate(r, &cfg(), now).is_err());

        let mut r = req(InputType::Raw, "image/jpeg");
        r.expires_at = Some(now + SignedDuration::from_secs(10));
        r.expires_in = Some(Duration::from_secs(10));
        assert!(validate(r, &cfg(), now).is_err());
    }

    #[test]
    fn huge_expires_in_is_rejected_not_panicking() {
        let mut r = req(InputType::Raw, "image/jpeg");
        r.expires_in = Some(Duration::from_secs(100_000_000 * 365 * 86_400));
        assert!(validate(r, &cfg(), Timestamp::now()).is_err());
    }

    #[test]
    fn size_limit() {
        let mut r = req(InputType::Raw, "image/jpeg");
        r.size = Some(i64::MAX);
        assert!(validate(r, &cfg(), Timestamp::now()).is_err());
        let mut r = req(InputType::Raw, "image/jpeg");
        r.size = Some(-1);
        assert!(validate(r, &cfg(), Timestamp::now()).is_err());
    }
}
