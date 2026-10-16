//! The upload pipeline for `raw` and `multipart` input.
//!
//! Bytes flow once through a size counter, a SHA-256 hasher, the magic-byte
//! sniffer, the streaming encryptor and a multipart upload to the object
//! store. Any failure aborts the multipart upload and hands the blob back to
//! the state it came from; nothing partial is ever visible.

use agrafe_crypto::Key;
use agrafe_crypto::streaming::{Encryptor, Layout};
use axum::http::HeaderMap;
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
use crate::blob::create;
use crate::blob::repo::{self, Completed, Transition};
use crate::blob::{Blob, BlobState};
use crate::config::UploadConfig;
use crate::error::ApiError;
use crate::sniff;

/// Private-port headers carrying the checksum and size of a re-upload.
pub const CHECKSUM_HEADER: &str = "x-agrafe-checksum";
pub const SIZE_HEADER: &str = "x-agrafe-size";

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

/// Read the override headers, if both or neither are present.
pub fn declared_from_headers(
    headers: &HeaderMap,
    config: &UploadConfig,
) -> Result<Option<Declared>, ApiError> {
    let checksum = headers.get(CHECKSUM_HEADER);
    let size = headers.get(SIZE_HEADER);
    let (checksum, size) = match (checksum, size) {
        (None, None) => return Ok(None),
        (Some(c), Some(s)) => (c, s),
        _ => {
            return Err(ApiError::Unprocessable(format!(
                "{CHECKSUM_HEADER} and {SIZE_HEADER} must be given together"
            )));
        }
    };
    let checksum = checksum
        .to_str()
        .map_err(|_| ApiError::Unprocessable(format!("{CHECKSUM_HEADER} is not valid")))?;
    let size: i64 = size
        .to_str()
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .ok_or_else(|| ApiError::Unprocessable(format!("{SIZE_HEADER} must be an integer")))?;
    Ok(Some(Declared {
        checksum: create::decode_checksum(checksum)?,
        size: create::validate_size(size, config)? as u64,
    }))
}

/// Decide what this upload must contain. A first upload uses what the blob
/// was created with; a re-upload of a mutable blob must say what is coming.
pub fn resolve(blob: &Blob, headers: Option<Declared>) -> Result<Declared, ApiError> {
    let own = match (&blob.checksum_sha256, blob.size) {
        (Some(c), Some(s)) => Some(Declared {
            checksum: c.clone(),
            size: s as u64,
        }),
        _ => None,
    };
    match (blob.state, headers) {
        (BlobState::Ready, Some(h)) => Ok(h),
        (BlobState::Ready, None) => Err(ApiError::Unprocessable(format!(
            "re-uploading requires {CHECKSUM_HEADER} and {SIZE_HEADER}"
        ))),
        (_, Some(h)) => match own {
            Some(own) if own == h => Ok(h),
            _ => Err(ApiError::Unprocessable(
                "checksum and size headers do not match the blob".into(),
            )),
        },
        (_, None) => own.ok_or_else(|| {
            ApiError::Unprocessable("blob has no declared checksum and size".into())
        }),
    }
}

/// Object key of version `version` of a blob's original.
pub fn object_key(blob_id: Uuid, version: i32) -> String {
    format!("{blob_id}/{version}/original")
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
            UploadError::Store(_) | UploadError::Crypto(_) => {
                ApiError::Internal(anyhow::Error::new(err))
            }
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
        .map_err(|e| ApiError::Internal(anyhow::anyhow!("upload task failed: {e}")))?
}

/// Encrypt and store `body` as the next version of `blob`, which is in
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
    let version = blob.version + 1;
    let storage_key = object_key(blob.id, version);
    let envelope = state.keks().new_envelope(blob.id);
    let layout = state.keks().layout();

    let header = match stream_to_store(
        &state,
        &blob,
        &declared,
        &storage_key,
        &envelope.key,
        layout,
        body,
    )
    .await
    {
        Ok(header) => header,
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
        version,
        kek_id: envelope.kek_id,
        wrapped_key: envelope.wrapped,
        encryption_header: header,
        chunk_size: layout.segment_size() as i32,
        checksum_sha256: declared.checksum,
        size: declared.size as i64,
    };
    match repo::complete_upload(state.pool(), &blob, &completed).await? {
        Transition::Done(blob) => Ok(blob),
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

async fn delete_quietly(state: &AppState, key: &str) {
    if let Err(err) = state.store().delete(&ObjectPath::from(key)).await {
        tracing::warn!(key, error = ?err, "could not delete orphaned object");
    }
}

/// Encrypt `body` into a new object. Returns the stream header on success;
/// on failure the multipart upload is aborted and nothing is left behind.
async fn stream_to_store<S>(
    state: &AppState,
    blob: &Blob,
    declared: &Declared,
    storage_key: &str,
    key: &Key,
    layout: Layout,
    body: S,
) -> Result<Vec<u8>, UploadError>
where
    S: Stream<Item = Result<Bytes, ApiError>> + Unpin,
{
    let path = ObjectPath::from(storage_key);
    let upload = state.store().put_multipart(&path).await?;
    let mut writer = WriteMultipart::new(upload);
    let encryptor = Encryptor::new(key, storage_key.as_bytes(), layout);
    let header = encryptor.header().as_bytes().to_vec();
    match pump(&mut writer, encryptor, declared, &blob.content_type, body).await {
        Ok(()) => {
            writer.finish().await?;
            Ok(header)
        }
        Err(err) => {
            if let Err(abort) = writer.abort().await {
                tracing::warn!(key = storage_key, error = ?abort, "could not abort multipart upload");
            }
            Err(err)
        }
    }
}

async fn pump<S>(
    writer: &mut WriteMultipart,
    mut encryptor: Encryptor,
    declared: &Declared,
    content_type: &str,
    mut body: S,
) -> Result<(), UploadError>
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
    Ok(())
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
