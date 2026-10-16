//! The upload pipeline for `raw` and `multipart` input.
//!
//! Bytes flow once through a size counter, a SHA-256 hasher, the magic-byte
//! sniffer, clamd when the file is small enough to scan inline, the streaming
//! encryptor and a multipart upload to the object store. Any failed check
//! aborts the multipart upload and hands the blob back to the state it came
//! from; nothing partial is ever visible. An infected file is not a failed
//! check: it is stored and the blob becomes `infected`, exactly as a deferred
//! scan would leave it.

use agrafe_crypto::Key;
use agrafe_crypto::streaming::{Encryptor, Layout};
use bytes::Bytes;
use futures::{Stream, StreamExt};
use object_store::ObjectStoreExt;
use object_store::WriteMultipart;
use object_store::path::Path as ObjectPath;
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use uuid::Uuid;

use crate::app::AppState;
use crate::blob::repo::{self, Completed, Transition};
use crate::blob::{Blob, ScanOutcome};
use crate::clamav::{self, Verdict};
use crate::config::UploadConfig;
use crate::error::ApiError;
use crate::sniff;

/// Parts of the object in flight at once while streaming.
const PART_CONCURRENCY: usize = 4;

/// Body chunks buffered between the request handler and the pipeline task.
const CHANNEL_DEPTH: usize = 8;

/// Slack allowed on top of `upload.max_size` for multipart framing.
const MULTIPART_OVERHEAD: u64 = 1024 * 1024;

/// Request body limit for the upload routes.
pub fn body_limit(config: &UploadConfig) -> usize {
    usize::try_from(config.max_size.as_u64().saturating_add(MULTIPART_OVERHEAD))
        .unwrap_or(usize::MAX)
}

/// What the bytes must turn out to be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Declared {
    pub checksum: Vec<u8>,
    pub size: u64,
}

/// What the blob was created with. `raw` and `multipart` blobs always have
/// both; a generated blob does not, and does not go through this pipeline.
pub fn declared(blob: &Blob) -> Result<Declared, ApiError> {
    match (&blob.checksum_sha256, blob.size) {
        (Some(c), Some(s)) => Ok(Declared {
            checksum: c.clone(),
            size: s as u64,
        }),
        _ => Err(ApiError::Unprocessable(
            "blob has no declared checksum and size".into(),
        )),
    }
}

/// Object key of a blob's original. Blobs are write-once, so there is one.
pub fn object_key(blob_id: Uuid) -> String {
    format!("{blob_id}/original")
}

#[derive(Debug, thiserror::Error)]
enum UploadError {
    #[error(transparent)]
    Body(ApiError),
    #[error("body size does not match the declared size")]
    SizeMismatch,
    #[error("body checksum does not match the declared checksum")]
    ChecksumMismatch,
    #[error("content looks like `{sniffed}`, not `{declared}`")]
    TypeMismatch {
        declared: String,
        sniffed: &'static str,
    },
    #[error("object store: {0}")]
    Store(#[from] object_store::Error),
    #[error("encryption: {0}")]
    Crypto(#[from] agrafe_crypto::Error),
}

impl From<UploadError> for ApiError {
    fn from(err: UploadError) -> Self {
        match err {
            UploadError::Body(e) => e,
            UploadError::SizeMismatch
            | UploadError::ChecksumMismatch
            | UploadError::TypeMismatch { .. } => ApiError::Unprocessable(err.to_string()),
            UploadError::Store(_) | UploadError::Crypto(_) => ApiError::internal(err),
        }
    }
}

/// Run the pipeline on `body`, the file bytes. The caller has checked that
/// the blob accepts an upload; the transition here is what makes it atomic.
///
/// Everything after the transition runs in its own task, fed through a
/// channel. The handler future can be dropped at any point (an HTTP/2 stream
/// reset, a closed connection), and that only closes the channel: the task
/// sees the body end early, fails the size check, aborts the multipart
/// upload and hands the blob back. Nothing is ever left half done.
pub async fn run<S>(
    state: &AppState,
    blob: Blob,
    declared: Declared,
    mut body: S,
) -> Result<Blob, ApiError>
where
    S: Stream<Item = Result<Bytes, ApiError>> + Unpin,
{
    let blob = {
        let mut conn = state.pool().acquire().await?;
        match repo::begin_upload(&mut conn, &blob).await? {
            Transition::Done(blob) => blob,
            Transition::Conflict(blob) => return Err(conflict(&blob)),
            Transition::Missing => return Err(ApiError::NotFound),
        }
    };

    let (tx, rx) = mpsc::channel(CHANNEL_DEPTH);
    let task = tokio::spawn(store_and_complete(
        state.clone(),
        blob,
        declared,
        ReceiverStream::new(rx),
    ));
    while let Some(chunk) = body.next().await {
        // A closed receiver means the pipeline already failed; its error is
        // what the task returns.
        if tx.send(chunk).await.is_err() {
            break;
        }
    }
    drop(tx);
    task.await
        .map_err(|e| ApiError::internal(format!("upload task failed: {e}")))?
}

/// Encrypt and store `body` as the original of `blob`, which is in
/// `uploading`, then complete or abort the transition.
async fn store_and_complete<S>(
    state: AppState,
    blob: Blob,
    declared: Declared,
    body: S,
) -> Result<Blob, ApiError>
where
    S: Stream<Item = Result<Bytes, ApiError>> + Unpin,
{
    let storage_key = object_key(blob.id);
    let envelope = state.keks().new_envelope(blob.id);
    let layout = state.keks().layout();
    let scanner = inline_scanner(&state, &blob, &declared).await;

    let (header, scan) = match stream_to_store(
        &state,
        &blob,
        &declared,
        &storage_key,
        &envelope.key,
        layout,
        scanner,
        body,
    )
    .await
    {
        Ok(stored) => stored,
        Err(err) => {
            match state.pool().acquire().await {
                Ok(mut conn) => {
                    if let Err(db) = repo::abort_upload(&mut conn, blob.id).await {
                        tracing::error!(blob = %blob.id, error = ?db, "could not reset blob after failed upload");
                    }
                }
                Err(db) => {
                    tracing::error!(blob = %blob.id, error = ?db, "could not reset blob after failed upload");
                }
            }
            return Err(err.into());
        }
    };

    let completed = Completed {
        storage_key: storage_key.clone(),
        kek_id: envelope.kek_id,
        wrapped_key: envelope.wrapped,
        encryption_header: header,
        chunk_size: layout.segment_size() as i32,
        checksum_sha256: declared.checksum,
        size: declared.size as i64,
        scan,
    };
    match repo::complete_upload(state.pool(), &blob, &completed).await? {
        Transition::Done(blob) => match completed.scan {
            ScanOutcome::Infected(name) => {
                tracing::warn!(blob = %blob.id, signature = name, "inline scan found a virus");
                Err(ApiError::Infected(Some(name)))
            }
            _ => Ok(blob),
        },
        Transition::Conflict(current) => {
            // The row changed under us (reaper or delete); the object we just
            // wrote belongs to nobody.
            delete_quietly(&state, &storage_key).await;
            Err(conflict(&current))
        }
        Transition::Missing => {
            delete_quietly(&state, &storage_key).await;
            Err(ApiError::NotFound)
        }
    }
}

fn conflict(blob: &Blob) -> ApiError {
    ApiError::Conflict(format!("blob is {} and cannot be uploaded", blob.state))
}

/// Where the bytes go for scanning while they stream.
enum Scanner {
    /// `skip_virus_scan`: nobody looks at them.
    Skipped,
    /// Too large for `INSTREAM`, or clamd could not be reached: a job will
    /// scan the stored object.
    Deferred,
    Inline(clamav::Session),
}

/// Open a clamd session when the file is small enough to scan inline. clamd
/// being down must not refuse uploads, and uploaders must not be able to
/// tie up every clamd thread by trickling bytes, so both cases defer the
/// scan to a job instead.
async fn inline_scanner(state: &AppState, blob: &Blob, declared: &Declared) -> Scanner {
    if blob.skip_virus_scan {
        return Scanner::Skipped;
    }
    if declared.size > state.config().upload.inline_scan_max_size.as_u64() {
        return Scanner::Deferred;
    }
    match state.clamd().begin_inline().await {
        Ok(session) => Scanner::Inline(session),
        Err(err) => {
            tracing::warn!(blob = %blob.id, error = %err, "no inline scan, deferring it to a job");
            Scanner::Deferred
        }
    }
}

async fn delete_quietly(state: &AppState, key: &str) {
    if let Err(err) = state.store().delete(&ObjectPath::from(key)).await {
        tracing::warn!(key, error = ?err, "could not delete orphaned object");
    }
}

/// Encrypt `body` into a new object. Returns the stream header and the scan
/// outcome on success; on failure the multipart upload is aborted and
/// nothing is left behind.
#[allow(clippy::too_many_arguments)]
async fn stream_to_store<S>(
    state: &AppState,
    blob: &Blob,
    declared: &Declared,
    storage_key: &str,
    key: &Key,
    layout: Layout,
    scanner: Scanner,
    body: S,
) -> Result<(Vec<u8>, ScanOutcome), UploadError>
where
    S: Stream<Item = Result<Bytes, ApiError>> + Unpin,
{
    let path = ObjectPath::from(storage_key);
    let upload = state.store().put_multipart(&path).await?;
    let mut writer = WriteMultipart::new(upload);
    let encryptor = Encryptor::new(key, storage_key.as_bytes(), layout);
    let header = encryptor.header().as_bytes().to_vec();
    match pump(
        &mut writer,
        encryptor,
        declared,
        &blob.content_type,
        scanner,
        body,
    )
    .await
    {
        Ok(scan) => {
            writer.finish().await?;
            Ok((header, scan))
        }
        Err(err) => {
            if let Err(abort) = writer.abort().await {
                tracing::warn!(key = storage_key, error = ?abort, "could not abort multipart upload");
            }
            Err(err)
        }
    }
}

/// Run the checks and the encryption over `body`. The scan verdict is read
/// last, after the cheap checks have passed: a mismatched checksum is a
/// `422` whatever clamd would have said.
async fn pump<S>(
    writer: &mut WriteMultipart,
    mut encryptor: Encryptor,
    declared: &Declared,
    content_type: &str,
    mut scanner: Scanner,
    mut body: S,
) -> Result<ScanOutcome, UploadError>
where
    S: Stream<Item = Result<Bytes, ApiError>> + Unpin,
{
    writer.write(encryptor.header().as_bytes());
    let mut hasher = Sha256::new();
    let mut received: u64 = 0;
    let mut head: Vec<u8> = Vec::with_capacity(sniff::SNIFF_LEN);
    let mut sniffed = false;
    let mut out = Vec::new();

    while let Some(chunk) = body.next().await {
        let chunk = chunk.map_err(UploadError::Body)?;
        received += chunk.len() as u64;
        if received > declared.size {
            return Err(UploadError::SizeMismatch);
        }
        hasher.update(&chunk);
        if let Scanner::Inline(session) = &mut scanner {
            // A write failure is remembered by the session and surfaces
            // from `finish`, where it defers the scan; the upload itself
            // carries on regardless.
            session.write(&chunk).await;
        }
        if !sniffed {
            let take = (sniff::SNIFF_LEN - head.len()).min(chunk.len());
            head.extend_from_slice(&chunk[..take]);
            if head.len() == sniff::SNIFF_LEN {
                check_type(content_type, &head)?;
                sniffed = true;
            }
        }
        encryptor.update(&chunk, &mut out)?;
        if !out.is_empty() {
            writer.wait_for_capacity(PART_CONCURRENCY).await?;
            writer.put(Bytes::from(std::mem::take(&mut out)));
        }
    }

    if received != declared.size {
        return Err(UploadError::SizeMismatch);
    }
    if !sniffed {
        check_type(content_type, &head)?;
    }
    if hasher.finalize().as_slice() != declared.checksum.as_slice() {
        return Err(UploadError::ChecksumMismatch);
    }
    encryptor.finish(&mut out)?;
    writer.put(Bytes::from(out));

    Ok(match scanner {
        Scanner::Skipped => ScanOutcome::Skipped,
        Scanner::Deferred => ScanOutcome::Deferred,
        Scanner::Inline(session) => match session.finish().await {
            Ok(Verdict::Clean) => ScanOutcome::Clean,
            Ok(Verdict::Infected(name)) => ScanOutcome::Infected(name),
            Err(err) => {
                tracing::warn!(error = %err, "inline scan failed, deferring to a job");
                ScanOutcome::Deferred
            }
        },
    })
}

fn check_type(declared: &str, head: &[u8]) -> Result<(), UploadError> {
    match sniff::sniff(head) {
        Some(sniffed) if !sniff::compatible(declared, sniffed) => Err(UploadError::TypeMismatch {
            declared: declared.to_string(),
            sniffed,
        }),
        _ => Ok(()),
    }
}
