//! The upload pipeline for `raw` and `multipart` input, which the fetch of
//! a `url` blob (see `jobs::fetch`) runs too, and the core the source
//! parts of a generated blob (see `source`) share with it.
//!
//! Bytes flow once through a size counter, SHA-256 and MD5 hashers, the
//! magic-byte sniffer, clamd when the file is small enough to scan inline,
//! the streaming encryptor and a multipart upload to the object store. Any
//! failed check aborts the multipart upload and hands the blob back to the
//! state it came from; nothing partial is ever visible. An infected file is
//! not a failed check: it is stored and the blob becomes `infected`, exactly
//! as a deferred scan would leave it.
//!
//! The checks are a [`Tap`] on the plaintext; the encryptor, the multipart
//! upload, the heartbeat on the row and the abort on failure are the same
//! whatever the tap, which is how a manifest part is stored by the same code
//! as a file.

use std::time::Duration;

use agrafe_crypto::Key;
use agrafe_crypto::streaming::{Encryptor, Layout};
use bytes::Bytes;
use futures::{Stream, StreamExt};
use md5::Md5;
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
use crate::blob::{Blob, BlobState, InputType, ScanOutcome};
use crate::clamav::{self, Verdict};
use crate::config::UploadConfig;
use crate::sniff;

/// The error a body stream may yield. The HTTP layer puts its own error
/// type in the box and takes it back out of `UploadError::Body`.
pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Parts of the object in flight at once while streaming.
pub(crate) const PART_CONCURRENCY: usize = 4;

/// Body chunks buffered between the request handler and the pipeline task.
pub(crate) const CHANNEL_DEPTH: usize = 8;

/// Slack allowed on top of `upload.max_size` for multipart framing.
const MULTIPART_OVERHEAD: u64 = 1024 * 1024;

/// Request body limit for the upload routes.
pub fn body_limit(config: &UploadConfig) -> usize {
    usize::try_from(config.max_size.as_u64().saturating_add(MULTIPART_OVERHEAD))
        .unwrap_or(usize::MAX)
}

/// What the bytes must turn out to be. A `raw` or `multipart` blob
/// declares its SHA-256 and size; a `url` blob may declare either, and an
/// MD5 besides, or nothing at all. What is declared is checked; what is
/// not is computed and written to the row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Declared {
    pub checksum: Option<Vec<u8>>,
    pub checksum_md5: Option<Vec<u8>>,
    /// The exact size, when declared.
    pub size: Option<u64>,
    /// The most bytes the body may hold: the declared size, or
    /// `upload.max_size` when none was.
    pub cap: u64,
}

impl Declared {
    /// What a `url` blob declared, with `upload.max_size` as the cap when
    /// it gave no size.
    pub(crate) fn fetched(blob: &Blob, upload: &UploadConfig) -> Self {
        let size = blob.size.map(|s| s as u64);
        Self {
            checksum: blob.checksum_sha256.clone(),
            checksum_md5: blob.checksum_md5.clone(),
            size,
            cap: size.unwrap_or(upload.max_size.as_u64()),
        }
    }
}

/// Check that `blob` can take a `PUT` at `now`, and what its bytes must
/// turn out to be. `raw` and `multipart` blobs always declare a checksum
/// and a size; a generated blob is accepted as a source instead, and a
/// `url` blob is fetched by a job: neither takes a file here.
pub fn prepare(blob: &Blob, now: Timestamp) -> Result<Declared, UploadError> {
    if !blob.accepts_upload(now) {
        return Err(UploadError::Conflict(blob.state_at(now)));
    }
    if blob.input_type.is_generated() {
        return Err(UploadError::Generated(blob.input_type));
    }
    if blob.input_type.is_fetched() {
        return Err(UploadError::Fetched(blob.input_type));
    }
    match (&blob.checksum_sha256, blob.size) {
        (Some(c), Some(s)) => Ok(Declared {
            checksum: Some(c.clone()),
            checksum_md5: blob.checksum_md5.clone(),
            size: Some(s as u64),
            cap: s as u64,
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
    /// A `rows`, `archive` or `document` blob was handed to the file
    /// pipeline: its `PUT` body is a source, which goes through `source`
    /// instead.
    #[error("a {0} blob is built from a source, not uploaded as a file")]
    Generated(InputType),
    /// A `url` blob was handed a `PUT`: its bytes are fetched by a job
    /// from the URL given at creation.
    #[error("a {0} blob is fetched by the service from its url, not uploaded")]
    Fetched(InputType),
    /// Parts and finish apply to generated blobs only.
    #[error("parts and finish apply to rows, archive and document blobs, not to a {0} blob")]
    NotGenerated(InputType),
    #[error("blob has no declared checksum and size")]
    Undeclared,
    /// A `rows` blob without its columns: creation never lets one through.
    #[error("rows blob has no declared columns")]
    NoColumns,
    /// Parts are numbered from one and consecutive; this is the one the
    /// blob is waiting for.
    #[error("expected part {expected}")]
    PartSequence { expected: u32 },
    #[error("a source has at most {0} parts")]
    TooManyParts(u32),
    #[error("part exceeds the maximum of {0} bytes")]
    PartTooLarge(u64),
    /// A line of a source failed its type's rules; the number is within the
    /// part, counted from one.
    #[error("line {line}: {reason}")]
    Line { line: u64, reason: String },
    /// Nothing to build: no part was accepted.
    #[error("the source has no parts")]
    NoParts,
    #[error("the archive would exceed the maximum of {0} bytes (archive.max_size)")]
    ArchiveTooLarge(u64),
    #[error("the workspace would exceed the maximum of {0} bytes (document.max_workspace_size)")]
    WorkspaceTooLarge(u64),
    /// The body stream failed; the boxed error is the caller's own.
    #[error(transparent)]
    Body(BoxError),
    #[error("body size does not match the declared size")]
    SizeMismatch,
    /// No size was declared and the body ran past `upload.max_size`.
    #[error("body exceeds the maximum of {0} bytes")]
    BodyTooLarge(u64),
    #[error("body checksum does not match the declared checksum")]
    ChecksumMismatch,
    #[error("body MD5 does not match the declared checksum_md5")]
    Md5Mismatch,
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
    #[error(transparent)]
    Key(#[from] crate::encryption::UnwrapError),
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
    body: S,
) -> Result<Blob, UploadError>
where
    S: Stream<Item = Result<Bytes, BoxError>> + Unpin,
{
    let blob = match begin(state, &blob).await? {
        Transition::Done(blob) => blob,
        Transition::Conflict(blob) => return Err(UploadError::Conflict(blob.state)),
        Transition::Missing => return Err(UploadError::NotFound),
    };
    let state = state.clone();
    feed(body, move |fed| {
        store_and_complete(state, blob, declared, fed)
    })
    .await
}

/// Claim the row: `created` to `uploading`. The one way in for a `PUT`
/// and for a fetch alike.
pub(crate) async fn begin(state: &Agrafe, blob: &Blob) -> sqlx::Result<Transition> {
    let mut conn = state.pool().acquire().await?;
    repo::begin_upload(&mut conn, blob).await
}

/// Pump `body` into the channel `task` reads from, in this future, while
/// the pipeline runs in a task of its own; see `run`. The `Fed` side is
/// what `task` is given to read.
pub(crate) async fn feed<S, F, Fut, T>(mut body: S, task: F) -> Result<T, UploadError>
where
    S: Stream<Item = Result<Bytes, BoxError>> + Unpin,
    F: FnOnce(Fed) -> Fut,
    Fut: Future<Output = Result<T, UploadError>> + Send + 'static,
    T: Send + 'static,
{
    let (tx, rx) = mpsc::channel(CHANNEL_DEPTH);
    let task = tokio::spawn(task(ReceiverStream::new(rx)));
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

/// The body as the pipeline task receives it.
pub(crate) type Fed = ReceiverStream<Result<Bytes, BoxError>>;

/// Encrypt and store `body` as the original of `blob`, which is in
/// `uploading`, then complete or abort the transition. `blob` is the row as
/// `begin_upload` returned it: its `upload_started_at` is this upload's
/// claim on the row, checked by every write to it from here on. An upload
/// the reaper gave up on, whose blob a later `PUT` may own by now, stops
/// rather than finishing on top of its successor, and leaves the row to
/// whoever has it.
pub(crate) async fn store_and_complete<S>(
    state: Agrafe,
    blob: Blob,
    declared: Declared,
    body: S,
) -> Result<Blob, UploadError>
where
    S: Stream<Item = Result<Bytes, BoxError>> + Unpin + Send,
{
    let started = blob
        .upload_started_at
        .expect("begin_upload sets upload_started_at");
    let storage_key = object_key(blob.id);
    let envelope = state.keks().new_envelope(blob.id);
    let layout = state.keks().layout();
    let scanner = inline_scanner(&state, &blob, &declared).await;
    let checks = Checks::new(&declared, &blob.content_type, scanner);

    let stored = stream_to_store(
        &state,
        blob.id,
        started,
        &storage_key,
        &envelope.key,
        layout,
        checks,
        body,
    )
    .await;
    let (header, checked) = match stored {
        Ok(stored) => stored,
        Err(err) => {
            hand_back(&state, blob.id, started).await;
            return Err(err);
        }
    };

    let completed = Completed {
        storage_key: storage_key.clone(),
        kek_id: envelope.kek_id,
        wrapped_key: envelope.wrapped,
        encryption_header: header,
        chunk_size: layout.segment_size() as i32,
        checksum_sha256: checked.sha256,
        checksum_md5: checked.md5,
        size: checked.received as i64,
        scan: checked.scan,
    };
    match repo::complete_upload(
        state.pool(),
        &state.config().webhook,
        &blob,
        started,
        &completed,
    )
    .await?
    {
        Transition::Done(blob) => match completed.scan {
            ScanOutcome::Infected(name) => {
                tracing::warn!(blob = %blob.id, signature = name, "inline scan found a virus");
                Err(UploadError::Infected(name))
            }
            _ => Ok(blob),
        },
        Transition::Conflict(current) => {
            disown(&state, &current, &storage_key).await;
            Err(UploadError::Conflict(current.state))
        }
        Transition::Missing => {
            delete_quietly(&state, &storage_key).await;
            Err(UploadError::NotFound)
        }
    }
}

/// Hand the row back to `created` after a failed upload, if it is still
/// this upload's; one the reaper reset, or another upload owns by now, is
/// left alone.
pub(crate) async fn hand_back(state: &Agrafe, blob_id: Uuid, started: Timestamp) {
    match state.pool().acquire().await {
        Ok(mut conn) => {
            if let Err(db) = repo::abort_upload(&mut conn, blob_id, started).await {
                tracing::error!(blob = %blob_id, error = ?db, "could not reset blob after failed upload");
            }
        }
        Err(db) => {
            tracing::error!(blob = %blob_id, error = ?db, "could not reset blob after failed upload");
        }
    }
}

/// The row changed in the moment since the last ownership check, and the
/// object just written at `storage_key` is complete. Reset, expired or
/// deleted, it is nobody's and goes. Under another upload, or finished by
/// one, it may be its: leave it.
pub(crate) async fn disown(state: &Agrafe, current: &Blob, storage_key: &str) {
    if matches!(
        current.state,
        BlobState::Created | BlobState::Expired | BlobState::Deleted
    ) {
        delete_quietly(state, storage_key).await;
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
    if declared.cap > state.config().upload.inline_scan_max_size.as_u64() {
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

pub(crate) async fn delete_quietly(state: &Agrafe, key: &str) {
    if let Err(err) = state.store().delete(&ObjectPath::from(key)).await {
        tracing::warn!(key, error = ?err, "could not delete orphaned object");
    }
}

/// What a pipeline watches the plaintext for as it streams by, and what it
/// has to say once the body has ended. The file pipeline checks size,
/// checksum, type and virus; a source part parses its lines.
pub(crate) trait Tap: Send {
    type Output: Send;

    /// Sees every chunk before it is encrypted. An error stops the upload.
    fn chunk(&mut self, chunk: &Bytes) -> impl Future<Output = Result<(), UploadError>> + Send;

    /// The body ended; the checks that need the whole of it run here.
    fn end(self) -> impl Future<Output = Result<Self::Output, UploadError>> + Send;
}

/// Encrypt `body` into a new object. Returns the stream header and the
/// tap's verdict on success; on failure, or once the row is no longer this
/// upload's, the multipart upload is aborted and nothing is left behind.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn stream_to_store<T, S>(
    state: &Agrafe,
    blob_id: Uuid,
    started: Timestamp,
    storage_key: &str,
    key: &Key,
    layout: Layout,
    tap: T,
    body: S,
) -> Result<(Vec<u8>, T::Output), UploadError>
where
    T: Tap,
    S: Stream<Item = Result<Bytes, BoxError>> + Unpin + Send,
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
        out = pump(&mut writer, encryptor, tap, body) => out,
        lost = heartbeat(state, blob_id, started) => Err(lost),
    };
    // One last look before the object becomes visible: every key is the
    // blob's, so the object must only ever be finished by the upload that
    // holds the row.
    let stored = match pumped {
        Ok(out) => match repo::touch_upload(state.pool(), blob_id, started).await? {
            true => Ok(out),
            false => Err(lost(state, blob_id).await),
        },
        Err(err) => Err(err),
    };
    match stored {
        Ok(out) => {
            writer.finish().await?;
            Ok((header, out))
        }
        Err(err) => {
            if let Err(abort) = writer.abort().await {
                tracing::warn!(key = storage_key, error = ?abort, "could not abort multipart upload");
            }
            Err(err)
        }
    }
}

/// Run the tap and the encryption over `body`.
async fn pump<T, S>(
    writer: &mut WriteMultipart,
    mut encryptor: Encryptor,
    mut tap: T,
    mut body: S,
) -> Result<T::Output, UploadError>
where
    T: Tap,
    S: Stream<Item = Result<Bytes, BoxError>> + Unpin,
{
    writer.write(encryptor.header().as_bytes());
    let mut out = Vec::new();
    while let Some(chunk) = body.next().await {
        let chunk = chunk.map_err(UploadError::Body)?;
        tap.chunk(&chunk).await?;
        encryptor.update(&chunk, &mut out)?;
        if !out.is_empty() {
            writer.wait_for_capacity(PART_CONCURRENCY).await?;
            writer.put(Bytes::from(std::mem::take(&mut out)));
        }
    }
    let verdict = tap.end().await?;
    encryptor.finish(&mut out)?;
    writer.put(Bytes::from(out));
    Ok(verdict)
}

/// The checks on a file: size against the declaration or the cap, SHA-256
/// and MD5 against the checksums when declared, magic bytes against the
/// content type, and the inline scan. What was not declared is computed
/// for the row.
struct Checks {
    declared: Declared,
    content_type: String,
    hasher: Sha256,
    md5: Md5,
    received: u64,
    head: Vec<u8>,
    sniffed: bool,
    scanner: Scanner,
}

impl Checks {
    fn new(declared: &Declared, content_type: &str, scanner: Scanner) -> Self {
        Self {
            declared: declared.clone(),
            content_type: content_type.to_string(),
            hasher: Sha256::new(),
            md5: Md5::new(),
            received: 0,
            head: Vec::with_capacity(sniff::SNIFF_LEN),
            sniffed: false,
            scanner,
        }
    }
}

/// What the checks hand over once the whole body passed them.
struct Checked {
    scan: ScanOutcome,
    sha256: Vec<u8>,
    md5: Vec<u8>,
    received: u64,
}

impl Tap for Checks {
    type Output = Checked;

    async fn chunk(&mut self, chunk: &Bytes) -> Result<(), UploadError> {
        self.received += chunk.len() as u64;
        if self.received > self.declared.cap {
            return Err(match self.declared.size {
                Some(_) => UploadError::SizeMismatch,
                None => UploadError::BodyTooLarge(self.declared.cap),
            });
        }
        self.hasher.update(chunk);
        self.md5.update(chunk);
        if let Scanner::Inline(session) = &mut self.scanner {
            // A write failure is remembered by the session and surfaces
            // from `finish`, where it defers the scan; the upload itself
            // carries on regardless.
            session.write(chunk).await;
        }
        if !self.sniffed {
            let take = (sniff::SNIFF_LEN - self.head.len()).min(chunk.len());
            self.head.extend_from_slice(&chunk[..take]);
            if self.head.len() == sniff::SNIFF_LEN {
                check_type(&self.content_type, &self.head)?;
                self.sniffed = true;
            }
        }
        Ok(())
    }

    /// The scan verdict is read last, after the cheap checks have passed: a
    /// mismatched checksum is a `422` whatever clamd would have said.
    async fn end(self) -> Result<Checked, UploadError> {
        if self.declared.size.is_some_and(|size| self.received != size) {
            return Err(UploadError::SizeMismatch);
        }
        if !self.sniffed {
            check_type(&self.content_type, &self.head)?;
        }
        let sha256 = self.hasher.finalize().to_vec();
        if self
            .declared
            .checksum
            .as_ref()
            .is_some_and(|c| *c != sha256)
        {
            return Err(UploadError::ChecksumMismatch);
        }
        let md5 = self.md5.finalize().to_vec();
        if self
            .declared
            .checksum_md5
            .as_ref()
            .is_some_and(|c| *c != md5)
        {
            return Err(UploadError::Md5Mismatch);
        }
        let scan = match self.scanner {
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
        };
        Ok(Checked {
            scan,
            sha256,
            md5,
            received: self.received,
        })
    }
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
