//! Creating a blob: validation of the request, then the insert.

use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use jiff::{SignedDuration, Timestamp};
use serde::Deserialize;
use uuid::Uuid;

use super::{Blob, InputType, repo};
use crate::app::Agrafe;
use crate::attachment::{self, AttachmentRequest, NewAttachment};
use crate::classify::{self, Category};
use crate::config::{BlobsConfig, UploadConfig};
use crate::jobs::Priority;
use crate::source::rows::{self, Column, Sheet};
use crate::text::is_bidi_control;

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
    /// Create a blob: metadata only, no bytes yet. With an `attachment`,
    /// the blob is attached in the same transaction and carries no expiry.
    pub async fn create_blob(&self, req: CreateBlobRequest) -> Result<Blob, CreateError> {
        let config = self.config();
        let new = validate(req, &config.upload, &config.blobs, Timestamp::now())?;
        let Some(attachment) = &new.attachment else {
            return Ok(repo::insert(self.pool(), &new).await?);
        };
        let mut tx = self.pool().begin().await?;
        let blob = repo::insert(&mut *tx, &new).await?;
        attachment::insert_with_blob(&mut tx, blob.id, attachment).await?;
        tx.commit().await?;
        Ok(blob)
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
    /// The columns of the sheet, in order. Required for `rows`.
    pub columns: Option<Vec<Column>>,
    #[serde(default)]
    pub skip_virus_scan: bool,
    pub expires_at: Option<Timestamp>,
    #[serde(default, with = "humantime_serde")]
    pub expires_in: Option<Duration>,
    /// Where the blob's jobs go in their kind's queue.
    #[serde(default)]
    pub priority: Priority,
    /// Attach the blob to a record as it is created. The blob then has no
    /// expiry, so `expires_at` and `expires_in` must be omitted.
    pub attachment: Option<AttachmentRequest>,
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
    /// The sheets of a `rows` blob.
    pub source_columns: Option<Vec<Sheet>>,
    pub skip_virus_scan: bool,
    /// `None` only for a blob created attached.
    pub expires_at: Option<Timestamp>,
    pub priority: Priority,
    pub attachment: Option<NewAttachment>,
}

pub const MAX_FILENAME_LEN: usize = 255;

pub fn validate(
    req: CreateBlobRequest,
    upload: &UploadConfig,
    blobs: &BlobsConfig,
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
    let source_columns = match (req.input_type, req.columns) {
        (InputType::Rows, Some(columns)) => Some(vec![Sheet {
            name: rows::sheet_name(&filename),
            columns: validate_columns(columns, &content_type)?,
        }]),
        (InputType::Rows, None) => {
            return Err(unprocessable("columns are required for rows input"));
        }
        (_, Some(_)) => {
            return Err(unprocessable("columns apply to rows input only"));
        }
        (_, None) => None,
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
    // A blob is either expiring or attached: created attached, it has no
    // expiry, and one asked for would be cleared on the spot.
    let attachment = match req.attachment {
        Some(_) if expires_at.is_some() => {
            return Err(unprocessable(
                "expires_at and expires_in do not apply to a blob created attached",
            ));
        }
        Some(attachment) => Some(
            attachment::validate(attachment)
                .map_err(|msg| unprocessable(format!("attachment: {msg}")))?,
        ),
        None => None,
    };
    let expires_at = match (&attachment, expires_at) {
        (Some(_), _) => None,
        (None, Some(at)) => Some(at),
        (None, None) => Some(blobs.default_expires_at(now)),
    };

    Ok(NewBlob {
        id: Uuid::now_v7(),
        filename,
        content_type,
        category,
        input_type: req.input_type,
        checksum_sha256,
        size,
        source_columns,
        // A generated file holds only what the service wrote, from scanned
        // members or from scalar values: skipped by construction.
        skip_virus_scan: req.skip_virus_scan || !req.input_type.is_direct(),
        expires_at,
        priority: req.priority,
        attachment,
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

/// The columns of a `rows` blob: at least one, each with a header free of
/// control characters, as the filename is, no more than a sheet holds when
/// the output is one, and no header longer than a sheet cell holds.
fn validate_columns(columns: Vec<Column>, content_type: &str) -> Result<Vec<Column>, CreateError> {
    if columns.is_empty() {
        return Err(unprocessable("columns must name at least one column"));
    }
    let sheet = rows::is_sheet(content_type);
    if sheet && columns.len() > rows::MAX_SHEET_COLUMNS {
        return Err(unprocessable(format!(
            "a sheet holds at most {} columns",
            rows::MAX_SHEET_COLUMNS
        )));
    }
    if let Some(i) = columns.iter().position(|c| c.name.is_empty()) {
        return Err(unprocessable(format!("column {} has an empty name", i + 1)));
    }
    if let Some(i) = columns.iter().position(|c| {
        c.name
            .chars()
            .any(|ch| ch.is_control() || is_bidi_control(ch))
    }) {
        return Err(unprocessable(format!(
            "column {} has a name with control characters",
            i + 1
        )));
    }
    if sheet && let Some(i) = columns.iter().position(|c| !rows::fits_sheet_cell(&c.name)) {
        return Err(unprocessable(format!(
            "column {} has a name longer than the {} characters a sheet cell holds",
            i + 1,
            rows::MAX_SHEET_CELL_LEN
        )));
    }
    Ok(columns)
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
            columns: (input_type == InputType::Rows).then(|| {
                vec![Column {
                    name: "a".into(),
                    kind: None,
                }]
            }),
            skip_virus_scan: false,
            expires_at: None,
            expires_in: None,
            priority: Priority::Normal,
            attachment: None,
        }
    }

    fn cfg() -> UploadConfig {
        UploadConfig::default()
    }

    fn blobs() -> BlobsConfig {
        BlobsConfig::default()
    }

    #[test]
    fn raw_ok() {
        let b = validate(
            req(InputType::Raw, "image/jpeg; charset=binary"),
            &cfg(),
            &blobs(),
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
        assert!(validate(r, &cfg(), &blobs(), Timestamp::now()).is_err());
        let mut r = req(InputType::Multipart, "image/jpeg");
        r.size = None;
        assert!(validate(r, &cfg(), &blobs(), Timestamp::now()).is_err());
    }

    #[test]
    fn generated_rejects_checksum_and_checks_type() {
        let mut r = req(InputType::Rows, classify::CSV);
        r.checksum = Some(STANDARD.encode([0u8; 32]));
        assert!(validate(r, &cfg(), &blobs(), Timestamp::now()).is_err());
        let b = validate(
            req(InputType::Rows, classify::CSV),
            &cfg(),
            &blobs(),
            Timestamp::now(),
        )
        .unwrap();
        assert!(b.skip_virus_scan, "generated blobs are never scanned");
        assert!(
            validate(
                req(InputType::Rows, classify::XLSX),
                &cfg(),
                &blobs(),
                Timestamp::now()
            )
            .is_ok()
        );
        assert!(
            validate(
                req(InputType::Rows, "image/png"),
                &cfg(),
                &blobs(),
                Timestamp::now()
            )
            .is_err()
        );
        assert!(
            validate(
                req(InputType::Archive, classify::ZIP),
                &cfg(),
                &blobs(),
                Timestamp::now()
            )
            .is_ok()
        );
        assert!(
            validate(
                req(InputType::Archive, classify::CSV),
                &cfg(),
                &blobs(),
                Timestamp::now()
            )
            .is_err()
        );
    }

    #[test]
    fn rows_declare_their_columns() {
        let now = Timestamp::now();
        let mut r = req(InputType::Rows, classify::XLSX);
        r.filename = "export: 2026.xlsx".into();
        let b = validate(r, &cfg(), &blobs(), now).unwrap();
        let sheets = b.source_columns.unwrap();
        assert_eq!(sheets.len(), 1);
        assert_eq!(sheets[0].name, "export_ 2026");
        assert_eq!(sheets[0].columns.len(), 1);
        assert!(
            validate(req(InputType::Raw, "image/jpeg"), &cfg(), &blobs(), now)
                .unwrap()
                .source_columns
                .is_none()
        );

        let mut r = req(InputType::Rows, classify::CSV);
        r.columns = None;
        assert!(validate(r, &cfg(), &blobs(), now).is_err(), "required");
        let mut r = req(InputType::Rows, classify::CSV);
        r.columns = Some(vec![]);
        assert!(validate(r, &cfg(), &blobs(), now).is_err(), "at least one");
        let mut r = req(InputType::Rows, classify::CSV);
        r.columns = Some(vec![Column {
            name: "".into(),
            kind: None,
        }]);
        assert!(validate(r, &cfg(), &blobs(), now).is_err(), "named");
        let mut r = req(InputType::Archive, classify::ZIP);
        r.columns = Some(vec![Column {
            name: "a".into(),
            kind: None,
        }]);
        assert!(validate(r, &cfg(), &blobs(), now).is_err(), "rows only");

        let many: Vec<Column> = (0..=rows::MAX_SHEET_COLUMNS)
            .map(|i| Column {
                name: i.to_string(),
                kind: None,
            })
            .collect();
        let mut r = req(InputType::Rows, classify::ODS);
        r.columns = Some(many.clone());
        assert!(
            validate(r, &cfg(), &blobs(), now).is_err(),
            "a sheet's limit"
        );
        let mut r = req(InputType::Rows, classify::CSV);
        r.columns = Some(many);
        assert!(
            validate(r, &cfg(), &blobs(), now).is_ok(),
            "CSV has no such limit"
        );

        for bad in ["a\nb", "x\u{0}y", "total\u{202E}", "a\u{2066}b", "\t"] {
            let mut r = req(InputType::Rows, classify::CSV);
            r.columns = Some(vec![Column {
                name: bad.into(),
                kind: None,
            }]);
            assert!(validate(r, &cfg(), &blobs(), now).is_err(), "{bad:?}");
        }
        let mut r = req(InputType::Rows, classify::CSV);
        r.columns = Some(vec![Column {
            name: "Nom / Prénom: été".into(),
            kind: None,
        }]);
        assert!(
            validate(r, &cfg(), &blobs(), now).is_ok(),
            "separators are fine"
        );

        let long = vec![Column {
            name: "n".repeat(rows::MAX_SHEET_CELL_LEN + 1),
            kind: None,
        }];
        let mut r = req(InputType::Rows, classify::XLSX);
        r.columns = Some(long.clone());
        assert!(
            validate(r, &cfg(), &blobs(), now).is_err(),
            "a cell's limit"
        );
        let mut r = req(InputType::Rows, classify::CSV);
        r.columns = Some(long);
        assert!(
            validate(r, &cfg(), &blobs(), now).is_ok(),
            "CSV has no such limit"
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
            assert!(
                validate(r, &cfg(), &blobs(), Timestamp::now()).is_err(),
                "{bad:?}"
            );
        }
        // Scripts written right to left are fine; only the controls are not.
        let mut r = req(InputType::Raw, "image/jpeg");
        r.filename = "صورة.jpg".into();
        assert!(validate(r, &cfg(), &blobs(), Timestamp::now()).is_ok());
    }

    #[test]
    fn expiration() {
        let now = Timestamp::now();
        let mut r = req(InputType::Raw, "image/jpeg");
        r.expires_in = Some(Duration::from_secs(3600));
        let b = validate(r, &cfg(), &blobs(), now).unwrap();
        assert_eq!(b.expires_at, Some(now + SignedDuration::from_secs(3600)));

        // Left out, the default applies: a blob is never without an expiry
        // unless it is attached.
        let b = validate(req(InputType::Raw, "image/jpeg"), &cfg(), &blobs(), now).unwrap();
        assert_eq!(
            b.expires_at,
            Some(now + SignedDuration::from_secs(24 * 3600))
        );
        let mut r = req(InputType::Raw, "image/jpeg");
        r.attachment = Some(AttachmentRequest {
            record_type: "Dossier".into(),
            record_id: "1".into(),
            name: "piece".into(),
        });
        let b = validate(r, &cfg(), &blobs(), now).unwrap();
        assert_eq!(b.expires_at, None);
        assert_eq!(b.attachment.as_ref().unwrap().record_id, "1");
        let mut r = req(InputType::Raw, "image/jpeg");
        r.attachment = Some(AttachmentRequest {
            record_type: "Dossier".into(),
            record_id: "1".into(),
            name: "piece".into(),
        });
        r.expires_in = Some(Duration::from_secs(3600));
        assert!(
            validate(r, &cfg(), &blobs(), now).is_err(),
            "attached blobs do not expire"
        );
        let mut r = req(InputType::Raw, "image/jpeg");
        r.attachment = Some(AttachmentRequest {
            record_type: "".into(),
            record_id: "1".into(),
            name: "piece".into(),
        });
        assert!(
            validate(r, &cfg(), &blobs(), now).is_err(),
            "validated like filenames"
        );

        let mut r = req(InputType::Raw, "image/jpeg");
        r.expires_at = Some(now - SignedDuration::from_secs(1));
        assert!(validate(r, &cfg(), &blobs(), now).is_err());

        let mut r = req(InputType::Raw, "image/jpeg");
        r.expires_at = Some(now + SignedDuration::from_secs(10));
        r.expires_in = Some(Duration::from_secs(10));
        assert!(validate(r, &cfg(), &blobs(), now).is_err());
    }

    #[test]
    fn huge_expires_in_is_rejected_not_panicking() {
        let mut r = req(InputType::Raw, "image/jpeg");
        r.expires_in = Some(Duration::from_secs(100_000_000 * 365 * 86_400));
        assert!(validate(r, &cfg(), &blobs(), Timestamp::now()).is_err());
    }

    #[test]
    fn size_limit() {
        let mut r = req(InputType::Raw, "image/jpeg");
        r.size = Some(i64::MAX);
        assert!(validate(r, &cfg(), &blobs(), Timestamp::now()).is_err());
        let mut r = req(InputType::Raw, "image/jpeg");
        r.size = Some(-1);
        assert!(validate(r, &cfg(), &blobs(), Timestamp::now()).is_err());
    }
}
