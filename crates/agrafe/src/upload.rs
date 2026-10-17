//! The upload pipeline for `raw` and `multipart` input.
//!
//! Bytes flow once through a size counter, a SHA-256 hasher, the magic-byte
//! sniffer, clamd when the file is small enough to scan inline, the streaming
//! encryptor and a multipart upload to the object store. Any failed check
//! aborts the multipart upload and hands the blob back to the state it came
//! from; nothing partial is ever visible. An infected file is not a failed
//! check: it is stored and the blob becomes `infected`, exactly as a deferred
//! scan would leave it.

use std::time::Duration;

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

use jiff::Timestamp;

use crate::app::Agrafe;
use crate::blob::repo::{self, Completed, Transition};
use crate::blob::{Blob, BlobState, ScanOutcome};
use crate::clamav::{self, Verdict};
use crate::config::UploadConfig;
use crate::sniff;

/// The error a body stream may yield. The HTTP layer puts its own error
/// type in the box and takes it back out of `UploadError::Body`.
pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

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

/// Check that `blob` can take an upload at `now`, and what its bytes must
/// turn out to be. `raw` and `multipart` blobs always declare both; a
/// generated blob does not, and does not go through this pipeline.
pub fn prepare(blob: &Blob, now: Timestamp) -> Result<Declared, UploadError> {
    if !blob.accepts_upload(now) {
        return Err(UploadError::Conflict(blob.state_at(now)));
    }
    if !blob.input_type.is_direct() {
        return Err(UploadError::Generated);
    }
    match (&blob.checksum_sha256, blob.size) {
        (Some(c), Some(s)) => Ok(Declared {
            checksum: c.clone(),
            size: s as u64,
        }),
        _ => Err(UploadError::Undeclared),
    }
}

impl Agrafe {
    /// Upload the bytes of `blob`, once. The stream is the file itself;
    /// framing (multipart, content length) is the caller's business, which
    /// is why the caller fetches the blob and hands it over rather than an
    /// id: what the body looks like depends on its input type. The checks
    /// of `prepare` run here regardless of whether the caller ran them.
    pub async fn upload<S>(&self, blob: Blob, body: S) -> Result<Blob, UploadError>
    where
        S: Stream<Item = Result<Bytes, BoxError>> + Unpin,
    {
        let declared = prepare(&blob, Timestamp::now())?;
        run(self, blob, declared, body).await
    }
}

/// Object key of a blob's original. Blobs are write-once, so there is one.
pub fn object_key(blob_id: Uuid) -> String {
    format!("{blob_id}/original")
}

/// Why an upload was refused or failed. The first group is the caller's
/// or the file's fault, the last the service's.
#[derive(Debug, thiserror::Error)]
pub enum UploadError {
    #[error("no such blob")]
    NotFound,
    #[error("blob is {0} and cannot be uploaded")]
    Conflict(BlobState),
    /// `rows` and `archive` blobs are built server side, not uploaded.
    #[error("rows and archive blobs are built server side, not uploaded")]
    Generated,
    #[error("blob has no declared checksum and size")]
    Undeclared,
    /// The body stream failed; the boxed error is the caller's own.
    #[error(transparent)]
    Body(BoxError),
    #[error("body size does not match the declared size")]
    SizeMismatch,
    #[error("body checksum does not match the declared checksum")]
    ChecksumMismatch,
    #[error("content looks like `{sniffed}`, not `{declared}`")]
    TypeMismatch {
        declared: String,
        sniffed: &'static str,
    },
    /// The bytes were stored and the blob is `infected`, by this signature.
    #[error("file is infected: {0}")]
    Infected(String),
    #[error("object store: {0}")]
    Store(#[from] object_store::Error),
    #[error("encryption: {0}")]
    Crypto(#[from] agrafe_crypto::Error),
    #[error("database: {0}")]
    Db(#[from] sqlx::Error),
    #[error("upload task failed: {0}")]
    Task(#[from] tokio::task::JoinError),
}

/// Run the pipeline on `body`, the file bytes. The caller has checked that
/// the blob accepts an upload; the transition here is what makes it atomic.
///
/// Everything after the transition runs in its own task, fed through a
/// channel. The handler future can be dropped at any point (an HTTP/2 stream
/// reset, a closed connection), and that only closes the channel: the task
/// sees the body end early, fails the size check, aborts the multipart
/// upload and hands the blob back. Nothing is ever left half done.
async fn run<S>(
    state: &Agrafe,
    blob: Blob,
    declared: Declared,
    mut body: S,
) -> Result<Blob, UploadError>
where
    S: Stream<Item = Result<Bytes, BoxError>> + Unpin,
{
    let blob = {
        let mut conn = state.pool().acquire().await?;
        match repo::begin_upload(&mut conn, &blob).await? {
            Transition::Done(blob) => blob,
            Transition::Conflict(blob) => return Err(UploadError::Conflict(blob.state)),
            Transition::Missing => return Err(UploadError::NotFound),
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
    task.await?
}

/// Encrypt and store `body` as the original of `blob`, which is in
/// `uploading`, then complete or abort the transition. `blob` is the row as
/// `begin_upload` returned it: its `upload_started_at` is this upload's
/// claim on the row, checked by every write to it from here on. An upload
/// the reaper gave up on, whose blob a later `PUT` may own by now, stops
/// rather than finishing on top of its successor, and leaves the row to
/// whoever has it.
async fn store_and_complete<S>(
    state: Agrafe,
    blob: Blob,
    declared: Declared,
    body: S,
) -> Result<Blob, UploadError>
where
    S: Stream<Item = Result<Bytes, BoxError>> + Unpin,
{
    let started = blob
        .upload_started_at
        .expect("begin_upload sets upload_started_at");
    let storage_key = object_key(blob.id);
    let envelope = state.keks().new_envelope(blob.id);
    let layout = state.keks().layout();
    let scanner = inline_scanner(&state, &blob, &declared).await;

    let stored = stream_to_store(
        &state,
        &blob,
        started,
        &declared,
        &storage_key,
        &envelope.key,
        layout,
        scanner,
        body,
    )
    .await;
    let (header, scan) = match stored {
        Ok(stored) => stored,
        Err(err) => {
            // Hands the row back if it is still this upload's; one the
            // reaper reset, or another upload owns by now, is left alone.
            match state.pool().acquire().await {
                Ok(mut conn) => {
                    if let Err(db) = repo::abort_upload(&mut conn, blob.id, started).await {
                        tracing::error!(blob = %blob.id, error = ?db, "could not reset blob after failed upload");
                    }
                }
                Err(db) => {
                    tracing::error!(blob = %blob.id, error = ?db, "could not reset blob after failed upload");
                }
            }
            return Err(err);
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
    match repo::complete_upload(state.pool(), &blob, started, &completed).await? {
        Transition::Done(blob) => match completed.scan {
            ScanOutcome::Infected(name) => {
                tracing::warn!(blob = %blob.id, signature = name, "inline scan found a virus");
                Err(UploadError::Infected(name))
            }
            _ => Ok(blob),
        },
        Transition::Conflict(current) => {
            // The row changed in the moment since the last ownership check.
            // Reset, expired or deleted, the object just written is
            // nobody's. Under another upload, or finished by one, the
            // object may be its: leave it.
            if matches!(
                current.state,
                BlobState::Created | BlobState::Expired | BlobState::Deleted
            ) {
                delete_quietly(&state, &storage_key).await;
            }
            Err(UploadError::Conflict(current.state))
        }
        Transition::Missing => {
            delete_quietly(&state, &storage_key).await;
            Err(UploadError::NotFound)
        }
    }
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
async fn inline_scanner(state: &Agrafe, blob: &Blob, declared: &Declared) -> Scanner {
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

/// Touch the row every third of `upload.stale_after`, for as long as this
/// future is polled, and resolve the moment the row is no longer this
/// upload's. A touch that fails for other reasons is logged and tried
/// again at the next tick: a database blip is not a lost row.
async fn heartbeat(state: &Agrafe, id: Uuid, started: Timestamp) -> UploadError {
    let period = (state.config().upload.stale_after / 3).max(Duration::from_secs(1));
    let mut ticks = tokio::time::interval(period);
    ticks.tick().await; // the first tick is immediate; the transition just set it
    loop {
        ticks.tick().await;
        match repo::touch_upload(state.pool(), id, started).await {
            Ok(true) => {}
            Ok(false) => {
                tracing::warn!(blob = %id, "upload lost its blob, stopping");
                return lost(state, id).await;
            }
            Err(err) => {
                tracing::warn!(blob = %id, error = ?err, "could not touch the uploading blob")
            }
        }
    }
}

/// What to answer once the row is no longer this upload's: whatever state
/// it is in now (`deleted`, `expired`, `created` after the reaper, or
/// `uploading` under the `PUT` that took it since), or gone.
async fn lost(state: &Agrafe, id: Uuid) -> UploadError {
    match repo::find(state.pool(), id).await {
        Ok(Some(blob)) => UploadError::Conflict(blob.state),
        Ok(None) => UploadError::NotFound,
        Err(err) => err.into(),
    }
}

async fn delete_quietly(state: &Agrafe, key: &str) {
    if let Err(err) = state.store().delete(&ObjectPath::from(key)).await {
        tracing::warn!(key, error = ?err, "could not delete orphaned object");
    }
}

/// Encrypt `body` into a new object. Returns the stream header and the scan
/// outcome on success; on failure, or once the row is no longer this
/// upload's, the multipart upload is aborted and nothing is left behind.
#[allow(clippy::too_many_arguments)]
async fn stream_to_store<S>(
    state: &Agrafe,
    blob: &Blob,
    started: Timestamp,
    declared: &Declared,
    storage_key: &str,
    key: &Key,
    layout: Layout,
    scanner: Scanner,
    body: S,
) -> Result<(Vec<u8>, ScanOutcome), UploadError>
where
    S: Stream<Item = Result<Bytes, BoxError>> + Unpin,
{
    let path = ObjectPath::from(storage_key);
    let upload = state.store().put_multipart(&path).await?;
    let mut writer = WriteMultipart::new(upload);
    let encryptor = Encryptor::new(key, storage_key.as_bytes(), layout);
    let header = encryptor.header().as_bytes().to_vec();
    // The row is touched while the bytes flow, so the reaper only resets
    // an upload whose process died, never a slow one; and an upload whose
    // row was taken away stops here.
    let pumped = tokio::select! {
        scan = pump(
            &mut writer,
            encryptor,
            declared,
            &blob.content_type,
            scanner,
            body,
        ) => scan,
        lost = heartbeat(state, blob.id, started) => Err(lost),
    };
    // One last look before the object becomes visible: every key is the
    // blob's, so the object must only ever be finished by the upload that
    // holds the row.
    let stored = match pumped {
        Ok(scan) => match repo::touch_upload(state.pool(), blob.id, started).await? {
            true => Ok(scan),
            false => Err(lost(state, blob.id).await),
        },
        Err(err) => Err(err),
    };
    match stored {
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
    S: Stream<Item = Result<Bytes, BoxError>> + Unpin,
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
