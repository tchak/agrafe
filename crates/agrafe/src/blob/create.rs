//! Creating a blob: validation of the request, then the insert.

use std::collections::HashMap;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use jiff::{SignedDuration, Timestamp};
use serde::Deserialize;
use uuid::Uuid;

use super::{Blob, InputType, Representation, repo};
use crate::app::Agrafe;
use crate::attachment::{self, AttachmentRequest, NewAttachment};
use crate::classify::{self, Category};
use crate::config::{BlobsConfig, FetchConfig, UploadConfig};
use crate::jobs::{self, JobKind, Priority, fetch};
use crate::source::document;
use crate::source::rows::{self, Column, Sheet};
use crate::text::is_bidi_control;

/// Why a blob could not be created.
#[derive(Debug, thiserror::Error)]
pub enum CreateError {
    /// The request is well formed but not acceptable; the message says why.
    #[error("{0}")]
    Invalid(String),
    /// The query or the transaction failed.
    #[error("database: {0}")]
    Db(#[from] sqlx::Error),
}

impl Agrafe {
    /// Create a blob: metadata only, no bytes yet. With an `attachment`,
    /// the blob is attached in the same transaction and carries no expiry.
    /// A `url` blob gets its fetch job in the same transaction too.
    pub async fn create_blob(&self, req: CreateBlobRequest) -> Result<Blob, CreateError> {
        let config = self.config();
        let new = validate(
            req,
            &config.upload,
            &config.blobs,
            &config.fetch,
            Timestamp::now(),
        )?;
        if new.attachment.is_none() && new.url.is_none() {
            return Ok(repo::insert(self.pool(), &new).await?);
        }
        let mut tx = self.pool().begin().await?;
        let blob = repo::insert(&mut *tx, &new).await?;
        if let Some(attachment) = &new.attachment {
            attachment::insert_with_blob(&mut tx, blob.id, attachment).await?;
        }
        if blob.input_type.is_fetched() {
            jobs::enqueue(
                &mut *tx,
                JobKind::Fetch,
                Some(blob.id),
                blob.priority,
                serde_json::json!({}),
                Timestamp::now(),
            )
            .await?;
        }
        tx.commit().await?;
        Ok(blob)
    }
}

/// The body of `POST /blobs`, as [`Agrafe::create_blob`] takes it. See
/// [DESIGN.md, "Creation"].
///
/// [DESIGN.md, "Creation"]: https://github.com/tchak/agrafe/blob/main/DESIGN.md#creation
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateBlobRequest {
    /// The file's name, trimmed: at most `MAX_FILENAME_LEN` bytes, not `.`
    /// or `..`, no path separators, control or bidirectional formatting
    /// characters.
    pub filename: String,
    /// The media type, normalised to `type/subtype` and classified into a
    /// category. A generated blob takes one its build can produce.
    pub content_type: String,
    /// How the bytes will arrive.
    pub input_type: InputType,
    /// SHA-256 of the bytes, base64. Required for `raw` and `multipart`,
    /// optional for `url`, where it is checked when given and computed
    /// otherwise.
    pub checksum: Option<String>,
    /// MD5 of the bytes, base64: the `checksum` of an Active Storage blob.
    /// Optional for `raw`, `multipart` and `url`; checked when given.
    pub checksum_md5: Option<String>,
    /// Declared size in bytes. Required for `raw` and `multipart`,
    /// optional for `url`, where `upload.max_size` is the cap otherwise.
    pub size: Option<i64>,
    /// Where a `url` blob is fetched from: `https`, no credentials, not a
    /// private address. Required for `url`, refused otherwise.
    pub url: Option<String>,
    /// The columns of the one sheet, in order. A `rows` blob takes this or
    /// `sheets`.
    pub columns: Option<Vec<Column>>,
    /// The sheets of the workbook, in order, each with its columns. For
    /// `rows` to XLSX or ODS; a CSV holds one sheet.
    pub sheets: Option<Vec<Sheet>>,
    /// The entry file of a `document` workspace, a path relative to its
    /// root; `main.typ` by default. Refused on any other input type.
    pub main: Option<String>,
    /// The backend vouches for the bytes: no scan, and no public upload.
    /// Implied for a generated blob.
    #[serde(default)]
    pub skip_virus_scan: bool,
    /// When the blob expires, in the future. `blobs.default_expires_in`
    /// from now when neither this nor `expires_in` is given.
    pub expires_at: Option<Timestamp>,
    /// How long until the blob expires, such as `7d`; the alternative to
    /// `expires_at`.
    #[serde(default, with = "humantime_serde")]
    pub expires_in: Option<Duration>,
    /// Where the blob's jobs go in their kind's queue.
    #[serde(default)]
    pub priority: Priority,
    /// The derived representations to render, both by default. An empty
    /// list renders nothing, for files nobody will look at through the
    /// service.
    pub representations: Option<Vec<Representation>>,
    /// Attach the blob to a record as it is created. The blob then has no
    /// expiry, so `expires_at` and `expires_in` must be omitted.
    pub attachment: Option<AttachmentRequest>,
}

/// A validated blob, ready to insert.
#[derive(Debug, Clone)]
pub(crate) struct NewBlob {
    pub id: Uuid,
    pub filename: String,
    pub content_type: String,
    pub category: Category,
    pub input_type: InputType,
    pub checksum_sha256: Option<Vec<u8>>,
    pub checksum_md5: Option<Vec<u8>>,
    pub size: Option<i64>,
    /// Where a `url` blob is fetched from, as given.
    pub url: Option<String>,
    /// The sheets of a `rows` blob.
    pub source_columns: Option<Vec<Sheet>>,
    /// The entry file of a `document` blob.
    pub source_main: Option<String>,
    pub skip_virus_scan: bool,
    /// `None` only for a blob created attached.
    pub expires_at: Option<Timestamp>,
    pub priority: Priority,
    /// The representations to render; the others start `disabled`.
    pub representations: Vec<Representation>,
    pub attachment: Option<NewAttachment>,
}

/// Longest `filename` accepted, in bytes, once trimmed.
pub const MAX_FILENAME_LEN: usize = 255;

/// Longest `content_type` accepted, in bytes.
pub const MAX_CONTENT_TYPE_LEN: usize = 255;

pub(crate) fn validate(
    req: CreateBlobRequest,
    upload: &UploadConfig,
    blobs: &BlobsConfig,
    fetch: &FetchConfig,
    now: Timestamp,
) -> Result<NewBlob, CreateError> {
    let filename = validate_filename(&req.filename)?;
    let content_type = normalize_content_type(&req.content_type).map_err(unprocessable)?;
    let category = classify::classify(&content_type);

    let (checksum_sha256, size) = match req.input_type {
        InputType::Raw | InputType::Multipart => {
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
        }
        // Checked when given, computed otherwise: the backend may not
        // have them for a file it only holds a URL to.
        InputType::Url => (
            req.checksum.as_deref().map(decode_checksum).transpose()?,
            req.size.map(|s| validate_size(s, upload)).transpose()?,
        ),
        InputType::Rows | InputType::Archive | InputType::Document => {
            if req.checksum.is_some() || req.checksum_md5.is_some() || req.size.is_some() {
                return Err(unprocessable(
                    "checksum, checksum_md5 and size must be omitted for generated input \
                     (rows, archive, document)",
                ));
            }
            validate_generated_content_type(req.input_type, &content_type)?;
            (None, None)
        }
    };
    let checksum_md5 = req
        .checksum_md5
        .as_deref()
        .map(decode_checksum_md5)
        .transpose()?;
    let url = match (req.input_type, req.url) {
        (InputType::Url, Some(url)) => {
            fetch::check_url(&url, fetch).map_err(|e| unprocessable(format!("url: {e}")))?;
            Some(url.trim().to_string())
        }
        (InputType::Url, None) => return Err(unprocessable("url is required for url input")),
        (_, Some(_)) => return Err(unprocessable("url applies to url input only")),
        (_, None) => None,
    };
    let source_columns = match (req.input_type, req.columns, req.sheets) {
        (InputType::Rows, Some(_), Some(_)) => {
            return Err(unprocessable("columns and sheets are mutually exclusive"));
        }
        (InputType::Rows, Some(columns), None) => Some(vec![Sheet {
            name: rows::sheet_name(&filename),
            columns: validate_columns(columns, &content_type)?,
        }]),
        (InputType::Rows, None, Some(sheets)) => Some(validate_sheets(sheets, &content_type)?),
        (InputType::Rows, None, None) => {
            return Err(unprocessable("rows input requires columns or sheets"));
        }
        (_, Some(_), _) => {
            return Err(unprocessable("columns apply to rows input only"));
        }
        (_, None, Some(_)) => {
            return Err(unprocessable("sheets apply to rows input only"));
        }
        (_, None, None) => None,
    };
    let source_main = match (req.input_type, req.main) {
        (InputType::Document, Some(main)) => Some(
            document::normalize_path(&main).map_err(|msg| unprocessable(format!("main: {msg}")))?,
        ),
        (InputType::Document, None) => Some(document::DEFAULT_MAIN.to_string()),
        (_, Some(_)) => return Err(unprocessable("main applies to document input only")),
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
        checksum_md5,
        size,
        url,
        source_columns,
        source_main,
        // A generated file holds only what the service wrote, from scanned
        // members or from scalar values: skipped by construction.
        skip_virus_scan: req.skip_virus_scan || req.input_type.is_generated(),
        expires_at,
        priority: req.priority,
        representations: req
            .representations
            .unwrap_or_else(|| vec![Representation::Thumbnail, Representation::Preview]),
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
pub(crate) fn normalize_content_type(raw: &str) -> Result<String, &'static str> {
    // Registered types are far shorter; the bound keeps the value fit for
    // a header and for the envelope's length-prefixed fields.
    if raw.len() > MAX_CONTENT_TYPE_LEN {
        return Err("content_type is too long");
    }
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

pub(crate) fn decode_checksum_md5(encoded: &str) -> Result<Vec<u8>, CreateError> {
    let bytes = STANDARD
        .decode(encoded.trim())
        .map_err(|_| unprocessable("checksum_md5 must be base64"))?;
    if bytes.len() != 16 {
        return Err(unprocessable(
            "checksum_md5 must be a base64 MD5 digest (16 bytes)",
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

/// The sheets of a workbook: at least one and no more than `MAX_SHEETS`,
/// each named as Excel accepts, no two alike ignoring case, each with
/// columns as `validate_columns` wants them. Not for CSV, which holds one
/// sheet.
fn validate_sheets(sheets: Vec<Sheet>, content_type: &str) -> Result<Vec<Sheet>, CreateError> {
    if !rows::is_sheet(content_type) {
        return Err(unprocessable(
            "sheets apply to XLSX and ODS output; a CSV holds one sheet, declared with columns",
        ));
    }
    if sheets.is_empty() {
        return Err(unprocessable("sheets must declare at least one sheet"));
    }
    if sheets.len() > rows::MAX_SHEETS {
        return Err(unprocessable(format!(
            "a workbook holds at most {} sheets",
            rows::MAX_SHEETS
        )));
    }
    // The folded name of every sheet so far, with its number.
    let mut seen: HashMap<String, usize> = HashMap::with_capacity(sheets.len());
    sheets
        .into_iter()
        .enumerate()
        .map(|(i, sheet)| {
            let n = i + 1;
            rows::check_sheet_name(&sheet.name)
                .map_err(|why| unprocessable(format!("sheet {n} has a name that {why}")))?;
            if let Some(j) = seen.insert(sheet.name.to_lowercase(), n) {
                return Err(unprocessable(format!(
                    "sheet {n} has the same name as sheet {j}, ignoring case"
                )));
            }
            let columns = validate_columns(sheet.columns, content_type)
                .map_err(|e| unprocessable(format!("sheet {n}: {e}")))?;
            Ok(Sheet {
                name: sheet.name,
                columns,
            })
        })
        .collect()
}

fn validate_generated_content_type(
    input_type: InputType,
    content_type: &str,
) -> Result<(), CreateError> {
    let ok = match input_type {
        InputType::Rows => matches!(content_type, classify::CSV | classify::XLSX | classify::ODS),
        InputType::Archive => content_type == classify::ZIP,
        InputType::Document => content_type == classify::PDF,
        InputType::Raw | InputType::Multipart | InputType::Url => true,
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
        let long = format!("text/{}", "x".repeat(MAX_CONTENT_TYPE_LEN));
        assert!(normalize_content_type(&long).is_err());
    }

    fn req(input_type: InputType, content_type: &str) -> CreateBlobRequest {
        CreateBlobRequest {
            filename: "photo.jpg".into(),
            content_type: content_type.into(),
            input_type,
            checksum: input_type.is_direct().then(|| STANDARD.encode([7u8; 32])),
            checksum_md5: None,
            size: input_type.is_direct().then_some(1234),
            url: input_type
                .is_fetched()
                .then(|| "https://files.example/photo.jpg".into()),
            columns: (input_type == InputType::Rows).then(|| {
                vec![Column {
                    name: "a".into(),
                    kind: None,
                }]
            }),
            sheets: None,
            main: None,
            skip_virus_scan: false,
            expires_at: None,
            expires_in: None,
            priority: Priority::Normal,
            representations: None,
            attachment: None,
        }
    }

    fn cfg() -> UploadConfig {
        UploadConfig::default()
    }

    fn blobs() -> BlobsConfig {
        BlobsConfig::default()
    }

    fn fetch() -> FetchConfig {
        FetchConfig::default()
    }

    #[test]
    fn raw_ok() {
        let b = validate(
            req(InputType::Raw, "image/jpeg; charset=binary"),
            &cfg(),
            &blobs(),
            &fetch(),
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
        assert!(validate(r, &cfg(), &blobs(), &fetch(), Timestamp::now()).is_err());
        let mut r = req(InputType::Multipart, "image/jpeg");
        r.size = None;
        assert!(validate(r, &cfg(), &blobs(), &fetch(), Timestamp::now()).is_err());
    }

    #[test]
    fn generated_rejects_checksum_and_checks_type() {
        let mut r = req(InputType::Rows, classify::CSV);
        r.checksum = Some(STANDARD.encode([0u8; 32]));
        assert!(validate(r, &cfg(), &blobs(), &fetch(), Timestamp::now()).is_err());
        let b = validate(
            req(InputType::Rows, classify::CSV),
            &cfg(),
            &blobs(),
            &fetch(),
            Timestamp::now(),
        )
        .unwrap();
        assert!(b.skip_virus_scan, "generated blobs are never scanned");
        assert!(
            validate(
                req(InputType::Rows, classify::XLSX),
                &cfg(),
                &blobs(),
                &fetch(),
                Timestamp::now()
            )
            .is_ok()
        );
        assert!(
            validate(
                req(InputType::Rows, "image/png"),
                &cfg(),
                &blobs(),
                &fetch(),
                Timestamp::now()
            )
            .is_err()
        );
        assert!(
            validate(
                req(InputType::Archive, classify::ZIP),
                &cfg(),
                &blobs(),
                &fetch(),
                Timestamp::now()
            )
            .is_ok()
        );
        assert!(
            validate(
                req(InputType::Archive, classify::CSV),
                &cfg(),
                &blobs(),
                &fetch(),
                Timestamp::now()
            )
            .is_err()
        );
    }

    #[test]
    fn documents_are_pdfs_with_a_main_file() {
        let now = Timestamp::now();
        let b = validate(
            req(InputType::Document, classify::PDF),
            &cfg(),
            &blobs(),
            &fetch(),
            now,
        )
        .unwrap();
        assert_eq!(b.source_main.as_deref(), Some("main.typ"));
        assert!(b.skip_virus_scan);
        let mut r = req(InputType::Document, classify::PDF);
        r.main = Some("src/facture.typ".into());
        let b = validate(r, &cfg(), &blobs(), &fetch(), now).unwrap();
        assert_eq!(b.source_main.as_deref(), Some("src/facture.typ"));
        for bad in ["", "/main.typ", "../main.typ", "a\\b.typ"] {
            let mut r = req(InputType::Document, classify::PDF);
            r.main = Some(bad.into());
            let err = validate(r, &cfg(), &blobs(), &fetch(), now).unwrap_err();
            assert!(err.to_string().starts_with("main:"), "{bad:?}: {err}");
        }
        for other in ["image/png", classify::ZIP, "text/plain"] {
            assert!(
                validate(
                    req(InputType::Document, other),
                    &cfg(),
                    &blobs(),
                    &fetch(),
                    now
                )
                .is_err(),
                "{other}"
            );
        }
        let mut r = req(InputType::Archive, classify::ZIP);
        r.main = Some("main.typ".into());
        let err = validate(r, &cfg(), &blobs(), &fetch(), now).unwrap_err();
        assert!(err.to_string().contains("document input only"), "{err}");
        let mut r = req(InputType::Raw, "application/pdf");
        r.main = Some("main.typ".into());
        assert!(validate(r, &cfg(), &blobs(), &fetch(), now).is_err());
        let b = validate(
            req(InputType::Raw, "application/pdf"),
            &cfg(),
            &blobs(),
            &fetch(),
            now,
        )
        .unwrap();
        assert_eq!(b.source_main, None);
    }

    #[test]
    fn rows_declare_their_columns() {
        let now = Timestamp::now();
        let mut r = req(InputType::Rows, classify::XLSX);
        r.filename = "export: 2026.xlsx".into();
        let b = validate(r, &cfg(), &blobs(), &fetch(), now).unwrap();
        let sheets = b.source_columns.unwrap();
        assert_eq!(sheets.len(), 1);
        assert_eq!(sheets[0].name, "export_ 2026");
        assert_eq!(sheets[0].columns.len(), 1);
        assert!(
            validate(
                req(InputType::Raw, "image/jpeg"),
                &cfg(),
                &blobs(),
                &fetch(),
                now
            )
            .unwrap()
            .source_columns
            .is_none()
        );

        let mut r = req(InputType::Rows, classify::CSV);
        r.columns = None;
        assert!(
            validate(r, &cfg(), &blobs(), &fetch(), now).is_err(),
            "required"
        );
        let mut r = req(InputType::Rows, classify::CSV);
        r.columns = Some(vec![]);
        assert!(
            validate(r, &cfg(), &blobs(), &fetch(), now).is_err(),
            "at least one"
        );
        let mut r = req(InputType::Rows, classify::CSV);
        r.columns = Some(vec![Column {
            name: "".into(),
            kind: None,
        }]);
        assert!(
            validate(r, &cfg(), &blobs(), &fetch(), now).is_err(),
            "named"
        );
        let mut r = req(InputType::Archive, classify::ZIP);
        r.columns = Some(vec![Column {
            name: "a".into(),
            kind: None,
        }]);
        assert!(
            validate(r, &cfg(), &blobs(), &fetch(), now).is_err(),
            "rows only"
        );

        let many: Vec<Column> = (0..=rows::MAX_SHEET_COLUMNS)
            .map(|i| Column {
                name: i.to_string(),
                kind: None,
            })
            .collect();
        let mut r = req(InputType::Rows, classify::ODS);
        r.columns = Some(many.clone());
        assert!(
            validate(r, &cfg(), &blobs(), &fetch(), now).is_err(),
            "a sheet's limit"
        );
        let mut r = req(InputType::Rows, classify::CSV);
        r.columns = Some(many);
        assert!(
            validate(r, &cfg(), &blobs(), &fetch(), now).is_ok(),
            "CSV has no such limit"
        );

        for bad in ["a\nb", "x\u{0}y", "total\u{202E}", "a\u{2066}b", "\t"] {
            let mut r = req(InputType::Rows, classify::CSV);
            r.columns = Some(vec![Column {
                name: bad.into(),
                kind: None,
            }]);
            assert!(
                validate(r, &cfg(), &blobs(), &fetch(), now).is_err(),
                "{bad:?}"
            );
        }
        let mut r = req(InputType::Rows, classify::CSV);
        r.columns = Some(vec![Column {
            name: "Nom / Prénom: été".into(),
            kind: None,
        }]);
        assert!(
            validate(r, &cfg(), &blobs(), &fetch(), now).is_ok(),
            "separators are fine"
        );

        let long = vec![Column {
            name: "n".repeat(rows::MAX_SHEET_CELL_LEN + 1),
            kind: None,
        }];
        let mut r = req(InputType::Rows, classify::XLSX);
        r.columns = Some(long.clone());
        assert!(
            validate(r, &cfg(), &blobs(), &fetch(), now).is_err(),
            "a cell's limit"
        );
        let mut r = req(InputType::Rows, classify::CSV);
        r.columns = Some(long);
        assert!(
            validate(r, &cfg(), &blobs(), &fetch(), now).is_ok(),
            "CSV has no such limit"
        );
    }

    fn sheet(name: &str, columns: &[&str]) -> Sheet {
        Sheet {
            name: name.into(),
            columns: columns
                .iter()
                .map(|c| Column {
                    name: c.to_string(),
                    kind: None,
                })
                .collect(),
        }
    }

    #[test]
    fn rows_declare_their_sheets() {
        let now = Timestamp::now();
        let workbook = |content_type: &str, sheets: Vec<Sheet>| {
            let mut r = req(InputType::Rows, content_type);
            r.columns = None;
            r.sheets = Some(sheets);
            r
        };
        let b = validate(
            workbook(
                classify::XLSX,
                vec![sheet("Dossiers", &["a", "b"]), sheet("Avis", &["c"])],
            ),
            &cfg(),
            &blobs(),
            &fetch(),
            now,
        )
        .unwrap();
        let sheets = b.source_columns.unwrap();
        assert_eq!(sheets.len(), 2);
        assert_eq!(sheets[0].name, "Dossiers");
        assert_eq!(sheets[0].columns.len(), 2);
        assert_eq!(sheets[1].name, "Avis");
        assert!(
            validate(
                workbook(classify::ODS, vec![sheet("Une", &["a"])]),
                &cfg(),
                &blobs(),
                &fetch(),
                now
            )
            .is_ok(),
            "one named sheet"
        );

        let cases: Vec<(&str, CreateBlobRequest, &str)> = vec![
            (
                "csv",
                workbook(classify::CSV, vec![sheet("a", &["a"])]),
                "a CSV holds one sheet",
            ),
            (
                "none",
                workbook(classify::XLSX, vec![]),
                "at least one sheet",
            ),
            (
                "too many",
                workbook(
                    classify::XLSX,
                    (0..=rows::MAX_SHEETS)
                        .map(|i| sheet(&format!("s{i}"), &["a"]))
                        .collect(),
                ),
                "at most 1000 sheets",
            ),
            (
                "both",
                {
                    let mut r = req(InputType::Rows, classify::XLSX);
                    r.sheets = Some(vec![sheet("a", &["a"])]);
                    r
                },
                "mutually exclusive",
            ),
            (
                "neither",
                {
                    let mut r = req(InputType::Rows, classify::XLSX);
                    r.columns = None;
                    r
                },
                "requires columns or sheets",
            ),
            (
                "not for archives",
                {
                    let mut r = req(InputType::Archive, classify::ZIP);
                    r.sheets = Some(vec![sheet("a", &["a"])]);
                    r
                },
                "sheets apply to rows input only",
            ),
            (
                "bad name",
                workbook(
                    classify::XLSX,
                    vec![sheet("a", &["a"]), sheet("b:c", &["a"])],
                ),
                "sheet 2 has a name that must not contain",
            ),
            (
                "reserved name",
                workbook(classify::XLSX, vec![sheet("History", &["a"])]),
                "sheet 1 has a name that is reserved",
            ),
            (
                "duplicate ignoring case",
                workbook(
                    classify::XLSX,
                    vec![
                        sheet("Été", &["a"]),
                        sheet("b", &["a"]),
                        sheet("été", &["a"]),
                    ],
                ),
                "sheet 3 has the same name as sheet 1",
            ),
            (
                "no columns",
                workbook(classify::XLSX, vec![sheet("a", &["a"]), sheet("b", &[])]),
                "sheet 2: columns must name at least one column",
            ),
            (
                "bad column",
                workbook(classify::XLSX, vec![sheet("a", &["x", ""])]),
                "sheet 1: column 2 has an empty name",
            ),
        ];
        for (name, r, says) in cases {
            let err = validate(r, &cfg(), &blobs(), &fetch(), now).unwrap_err();
            assert!(err.to_string().contains(says), "{name}: {err}");
        }
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
                validate(r, &cfg(), &blobs(), &fetch(), Timestamp::now()).is_err(),
                "{bad:?}"
            );
        }
        // Scripts written right to left are fine; only the controls are not.
        let mut r = req(InputType::Raw, "image/jpeg");
        r.filename = "صورة.jpg".into();
        assert!(validate(r, &cfg(), &blobs(), &fetch(), Timestamp::now()).is_ok());
    }

    #[test]
    fn expiration() {
        let now = Timestamp::now();
        let mut r = req(InputType::Raw, "image/jpeg");
        r.expires_in = Some(Duration::from_secs(3600));
        let b = validate(r, &cfg(), &blobs(), &fetch(), now).unwrap();
        assert_eq!(b.expires_at, Some(now + SignedDuration::from_secs(3600)));

        // Left out, the default applies: a blob is never without an expiry
        // unless it is attached.
        let b = validate(
            req(InputType::Raw, "image/jpeg"),
            &cfg(),
            &blobs(),
            &fetch(),
            now,
        )
        .unwrap();
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
        let b = validate(r, &cfg(), &blobs(), &fetch(), now).unwrap();
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
            validate(r, &cfg(), &blobs(), &fetch(), now).is_err(),
            "attached blobs do not expire"
        );
        let mut r = req(InputType::Raw, "image/jpeg");
        r.attachment = Some(AttachmentRequest {
            record_type: "".into(),
            record_id: "1".into(),
            name: "piece".into(),
        });
        assert!(
            validate(r, &cfg(), &blobs(), &fetch(), now).is_err(),
            "validated like filenames"
        );

        let mut r = req(InputType::Raw, "image/jpeg");
        r.expires_at = Some(now - SignedDuration::from_secs(1));
        assert!(validate(r, &cfg(), &blobs(), &fetch(), now).is_err());

        let mut r = req(InputType::Raw, "image/jpeg");
        r.expires_at = Some(now + SignedDuration::from_secs(10));
        r.expires_in = Some(Duration::from_secs(10));
        assert!(validate(r, &cfg(), &blobs(), &fetch(), now).is_err());
    }

    #[test]
    fn url_blobs_declare_a_url_and_may_declare_the_rest() {
        let now = Timestamp::now();
        let b = validate(
            req(InputType::Url, "image/jpeg"),
            &cfg(),
            &blobs(),
            &fetch(),
            now,
        )
        .unwrap();
        assert_eq!(b.url.as_deref(), Some("https://files.example/photo.jpg"));
        assert_eq!(b.checksum_sha256, None);
        assert_eq!(b.checksum_md5, None);
        assert_eq!(b.size, None);
        assert!(!b.skip_virus_scan, "fetched files are scanned like uploads");

        let mut r = req(InputType::Url, "image/jpeg");
        r.checksum = Some(STANDARD.encode([7u8; 32]));
        r.checksum_md5 = Some(STANDARD.encode([8u8; 16]));
        r.size = Some(99);
        let b = validate(r, &cfg(), &blobs(), &fetch(), now).unwrap();
        assert_eq!(b.checksum_sha256.as_deref(), Some(&[7u8; 32][..]));
        assert_eq!(b.checksum_md5.as_deref(), Some(&[8u8; 16][..]));
        assert_eq!(b.size, Some(99));

        let mut r = req(InputType::Url, "image/jpeg");
        r.url = None;
        let err = validate(r, &cfg(), &blobs(), &fetch(), now).unwrap_err();
        assert!(err.to_string().contains("url is required"), "{err}");
        for bad in [
            "http://files.example/a",
            "https://user:pw@files.example/a",
            "https://127.0.0.1/a",
            "nope",
        ] {
            let mut r = req(InputType::Url, "image/jpeg");
            r.url = Some(bad.into());
            let err = validate(r, &cfg(), &blobs(), &fetch(), now).unwrap_err();
            assert!(err.to_string().starts_with("url: "), "{bad}: {err}");
        }
        let mut r = req(InputType::Url, "image/jpeg");
        r.url = Some("http://127.0.0.1:18080/a".into());
        let lax = FetchConfig {
            allow_private_networks: true,
            ..FetchConfig::default()
        };
        assert!(validate(r, &cfg(), &blobs(), &lax, now).is_ok());
        let mut r = req(InputType::Url, "image/jpeg");
        r.url = Some("  https://files.example/a  ".into());
        let b = validate(r, &cfg(), &blobs(), &fetch(), now).unwrap();
        assert_eq!(b.url.as_deref(), Some("https://files.example/a"));

        let mut r = req(InputType::Raw, "image/jpeg");
        r.url = Some("https://files.example/a".into());
        assert!(
            validate(r, &cfg(), &blobs(), &fetch(), now).is_err(),
            "url only"
        );
        let mut r = req(InputType::Url, "image/jpeg");
        r.checksum_md5 = Some(STANDARD.encode([8u8; 15]));
        assert!(
            validate(r, &cfg(), &blobs(), &fetch(), now).is_err(),
            "16 bytes"
        );
        let mut r = req(InputType::Url, "image/jpeg");
        r.size = Some(-1);
        assert!(validate(r, &cfg(), &blobs(), &fetch(), now).is_err());
    }

    #[test]
    fn direct_blobs_may_declare_an_md5_and_generated_ones_may_not() {
        let now = Timestamp::now();
        let mut r = req(InputType::Raw, "image/jpeg");
        r.checksum_md5 = Some(STANDARD.encode([8u8; 16]));
        let b = validate(r, &cfg(), &blobs(), &fetch(), now).unwrap();
        assert_eq!(b.checksum_md5.as_deref(), Some(&[8u8; 16][..]));
        let mut r = req(InputType::Rows, classify::CSV);
        r.checksum_md5 = Some(STANDARD.encode([8u8; 16]));
        assert!(validate(r, &cfg(), &blobs(), &fetch(), now).is_err());
    }

    #[test]
    fn huge_expires_in_is_rejected_not_panicking() {
        let mut r = req(InputType::Raw, "image/jpeg");
        r.expires_in = Some(Duration::from_secs(100_000_000 * 365 * 86_400));
        assert!(validate(r, &cfg(), &blobs(), &fetch(), Timestamp::now()).is_err());
    }

    #[test]
    fn size_limit() {
        let mut r = req(InputType::Raw, "image/jpeg");
        r.size = Some(i64::MAX);
        assert!(validate(r, &cfg(), &blobs(), &fetch(), Timestamp::now()).is_err());
        let mut r = req(InputType::Raw, "image/jpeg");
        r.size = Some(-1);
        assert!(validate(r, &cfg(), &blobs(), &fetch(), Timestamp::now()).is_err());
    }
}
