//! What the build jobs share: the generated file streamed into the
//! bucket as it is produced, through the same encryptor and multipart
//! upload as an uploaded file, to the original's key, and the row moved
//! to `ready` once it is there. Nothing is held whole in memory, and only
//! a `document` build, which hands its workspace to a compiler, touches
//! local disk. See [DESIGN.md, "Build"].
//!
//! A kind of build is an [`Assemble`]: `archive` writes a ZIP of members,
//! `rows` a CSV, XLSX or ODS of cells, `document` the PDF a sandboxed
//! compiler made of a workspace. Each gets a [`Sink`] to write into and
//! the blob to read its source from; everything around it, the key, the
//! multipart upload, the abort on failure, the last check that the blob
//! still waits for its build, is here.
//!
//! [DESIGN.md, "Build"]: https://github.com/tchak/agrafe/blob/main/DESIGN.md#build

pub mod zip;

use agrafe_crypto::Key;
use agrafe_crypto::streaming::{Encryptor, Layout};
use bytes::Bytes;
use jiff::Timestamp;
use md5::Md5;
use object_store::path::Path as ObjectPath;
use object_store::{ObjectStoreExt, WriteMultipart};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use self::zip::ZipError;
use super::{Job, JobError};
use crate::app::Agrafe;
use crate::blob::read::ReadError;
use crate::blob::repo::{self, Built, Transition};
use crate::blob::{Blob, BlobState, InputType};
use crate::encryption::UnwrapError;
use crate::source::SourceError;
use crate::upload::{self, PART_CONCURRENCY};

/// Parts of the multipart upload. S3 allows 10 000 per object; the default
/// 5 MiB would cap an archive at 50 GB. `ArchiveConfig::MAX_SIZE_LIMIT`
/// follows from the two.
pub(crate) const BUILD_PART_SIZE: usize = 32 * 1024 * 1024;

/// The object store's limit on parts per object.
pub(crate) const MAX_BUILD_PARTS: u64 = 10_000;

/// Why a build attempt failed.
#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    /// The stored source does not parse: accept let something through, or
    /// the object is not what accept stored.
    #[error("part {part}, line {line}: {reason}")]
    Source {
        part: u32,
        line: u64,
        reason: String,
    },
    #[error("blob {blob_id} is stored but has no {what}")]
    Incomplete { blob_id: Uuid, what: &'static str },
    #[error(transparent)]
    Key(#[from] UnwrapError),
    #[error("encryption: {0}")]
    Crypto(#[from] agrafe_crypto::Error),
    #[error("reading a source: {0}")]
    Read(#[from] ReadError),
    #[error("object store: {0}")]
    Store(#[from] object_store::Error),
    #[error("writing the package: {0}")]
    Zip(#[from] ZipError),
    #[error("compressing: {0}")]
    Compress(#[from] std::io::Error),
    #[error("database: {0}")]
    Db(#[from] sqlx::Error),
    /// A workspace file whose blob was `ready` at accept time is not any
    /// more. A document cannot do without a file, so this is not skipped
    /// the way an archive skips an entry.
    #[error("workspace file {path}: blob {blob_id} is {state}, not ready")]
    Member {
        path: String,
        blob_id: Uuid,
        state: BlobState,
    },
    /// The workspace cannot be written as the manifest describes it: a
    /// path twice, a path both file and directory, more bytes than
    /// `document.max_workspace_size`.
    #[error("workspace: {0}")]
    Workspace(String),
    /// The compiler refused the document, or ran out of time on it; the
    /// diagnostics follow.
    #[error("the document does not compile: {0}")]
    Compile(String),
    /// The compiler helper failed on its own account, not the document's:
    /// a full disk, a crash. Retried.
    #[error("the compiler helper failed: {0}")]
    Helper(agrafe_render::Rejection),
    /// pandoc refused the HTML the document compiled to, or the
    /// workspace's reference document, ran out of time on it or wrote past
    /// the output limit; what it said follows.
    #[error("the document does not convert: {0}")]
    Convert(String),
    /// pandoc failed on its own account: its heap, a signal. Retried.
    #[error("pandoc failed: {0}")]
    Pandoc(agrafe_render::Rejection),
    /// The sandbox could not run the compiler.
    #[error(transparent)]
    Render(#[from] agrafe_render::Error),
    #[error("temporary files: {0}")]
    Io(std::io::Error),
}

impl BuildError {
    /// Whether the next attempt would fail the same way. A source that does
    /// not parse, a row missing its key material, a key that does not
    /// unwrap, a stored object that is damaged, a package the writer
    /// refuses, a workspace file gone or a document that does not compile
    /// or convert are the blob's own; the bucket, the database, the
    /// sandbox, the compiler helper, pandoc and the temporary directory
    /// may come back.
    pub fn is_permanent(&self) -> bool {
        match self {
            BuildError::Source { .. }
            | BuildError::Incomplete { .. }
            | BuildError::Key(_)
            | BuildError::Zip(_)
            | BuildError::Member { .. }
            | BuildError::Workspace(_)
            | BuildError::Compile(_)
            | BuildError::Convert(_) => true,
            BuildError::Read(err) => err.is_permanent(),
            _ => false,
        }
    }
}

impl From<SourceError> for BuildError {
    fn from(err: SourceError) -> Self {
        match err {
            SourceError::Malformed { part, line, reason } => {
                BuildError::Source { part, line, reason }
            }
            SourceError::Read(err) => BuildError::Read(err),
        }
    }
}

/// What a build writes beyond the file: for an archive, the members
/// written and the members skipped.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    pub archive: Option<(u64, u64)>,
}

/// One kind of generated file: how to produce it from a blob's source.
pub(crate) trait Assemble {
    /// The input type whose blobs this builds.
    const INPUT: InputType;

    /// Write the whole file into `sink`. The source is read through
    /// `crate::source::Source`; a line that does not parse is a
    /// `BuildError::Source`.
    fn assemble(
        state: &Agrafe,
        blob: &Blob,
        sink: &mut Sink,
    ) -> impl Future<Output = Result<Counts, BuildError>> + Send;
}

/// Run the build job for `A`: find the blob, build it if it still waits
/// for one, record the result.
pub(crate) async fn run<A: Assemble>(state: &Agrafe, job: &Job) -> Result<(), JobError> {
    let id = job.blob_id.ok_or(JobError::NoBlob(job.kind))?;
    let Some(blob) = repo::find(state.pool(), id).await? else {
        tracing::info!("blob is gone, nothing to build");
        return Ok(());
    };
    // As of now: a blob past its expiry while it waited in the queue is
    // expired, reaper or not, and not built.
    let current = blob.state_at(Timestamp::now());
    if current != BlobState::Uploaded || blob.input_type != A::INPUT {
        tracing::info!(state = %current, input_type = %blob.input_type, "blob is not waiting for a build");
        return Ok(());
    }
    let Some(built) = build::<A>(state, job, &blob).await? else {
        tracing::warn!(
            "blob left `uploaded`, or the job its lease, during the build: nothing written"
        );
        return Ok(());
    };
    match repo::complete_build(state.pool(), &state.config().webhook, id, &built).await? {
        Transition::Done(blob) => tracing::info!(
            size = built.size,
            entries = built.archive.map(|a| a.0),
            skipped = built.archive.map(|a| a.1),
            state = %blob.state,
            "built"
        ),
        // The blob left `uploaded` in the moment since the last check.
        // Expired or deleted, the object is nobody's and goes. `ready`
        // means another build of the same blob finished first, which a
        // lapsed job lease allows: the object is its, and deleting it here
        // would leave a `ready` blob with nothing behind it.
        Transition::Conflict(current) => {
            tracing::warn!(state = %current.state, "blob changed state as the build finished");
            upload::disown(state, &current, &built.storage_key).await;
        }
        Transition::Missing => upload::delete_quietly(state, &built.storage_key).await,
    }
    Ok(())
}

/// Whether the build may still finish the blob's key: the job's lease
/// still this worker's, and the blob still waiting for its build.
async fn last_look(state: &Agrafe, job: &Job, blob: &Blob) -> sqlx::Result<bool> {
    let held = match job.locked_by.as_deref() {
        Some(worker) => crate::jobs::touch(state.pool(), job.id, worker).await?,
        None => true,
    };
    Ok(held
        && repo::find(state.pool(), blob.id)
            .await?
            .is_some_and(|b| b.state_at(Timestamp::now()) == BlobState::Uploaded))
}

/// Assemble the file into the original's key. `None` when the blob was no
/// longer `uploaded` at the end, or the job no longer this worker's: the
/// object is not finished and nothing is written.
async fn build<A: Assemble>(
    state: &Agrafe,
    job: &Job,
    blob: &Blob,
) -> Result<Option<Built>, BuildError> {
    let missing = |what| BuildError::Incomplete {
        blob_id: blob.id,
        what,
    };
    let kek_id = blob.kek_id.as_deref().ok_or_else(|| missing("KEK id"))?;
    let wrapped = blob
        .wrapped_key
        .as_deref()
        .ok_or_else(|| missing("wrapped key"))?;
    let layout = blob
        .chunk_size
        .and_then(|s| usize::try_from(s).ok())
        .and_then(|s| Layout::new(s).ok())
        .ok_or_else(|| missing("valid chunk size"))?;
    let key = state.keks().unwrap(blob.id, kek_id, wrapped)?;

    let storage_key = crate::blob::key::original(blob.id);
    let mut sink = Sink::open(state, &storage_key, &key, layout).await?;
    let counts = match A::assemble(state, blob, &mut sink).await {
        Ok(counts) => counts,
        Err(err) => {
            sink.abort().await;
            return Err(err);
        }
    };
    // One last look before the object becomes visible: the key is the
    // blob's, so only a build of a blob still waiting may finish it, and
    // only the build whose job still holds its lease. A build whose lease
    // lapsed, a worker stalled for longer, is running beside the one that
    // reclaimed it, and the two would complete the same key, the row
    // taking one header and the object the other's bytes.
    // A database error here aborts the upload too: dropped, its parts would
    // stay in the bucket until the lifecycle rule, and the retry would
    // open another beside them.
    match last_look(state, job, blob).await {
        Ok(true) => {}
        Ok(false) => {
            sink.abort().await;
            return Ok(None);
        }
        Err(err) => {
            sink.abort().await;
            return Err(err.into());
        }
    }
    let (encryption_header, checksum_sha256, checksum_md5, size) = sink.finish().await?;
    Ok(Some(Built {
        storage_key,
        encryption_header,
        checksum_sha256,
        checksum_md5,
        size: size as i64,
        archive: counts
            .archive
            .map(|(entries, skipped)| (entries as i64, skipped as i64)),
    }))
}

/// The file's bytes on their way to the bucket: hashed, encrypted,
/// uploaded in `BUILD_PART_SIZE` parts with a few in flight.
pub(crate) struct Sink {
    writer: WriteMultipart,
    encryptor: Option<Encryptor>,
    header: Vec<u8>,
    hasher: Sha256,
    md5: Md5,
    size: u64,
    out: Vec<u8>,
    storage_key: String,
}

impl Sink {
    async fn open(
        state: &Agrafe,
        storage_key: &str,
        key: &Key,
        layout: Layout,
    ) -> Result<Self, BuildError> {
        let upload = state
            .store()
            .put_multipart(&ObjectPath::from(storage_key))
            .await?;
        let mut writer = WriteMultipart::new_with_chunk_size(upload, BUILD_PART_SIZE);
        let encryptor = Encryptor::new(
            key,
            crate::blob::key::associated_data(storage_key).as_bytes(),
            layout,
        );
        let header = encryptor.header().as_bytes().to_vec();
        writer.write(&header);
        Ok(Self {
            writer,
            encryptor: Some(encryptor),
            header,
            hasher: Sha256::new(),
            md5: Md5::new(),
            size: 0,
            out: Vec::new(),
            storage_key: storage_key.to_string(),
        })
    }

    pub(crate) async fn write(&mut self, bytes: &[u8]) -> Result<(), BuildError> {
        self.hasher.update(bytes);
        self.md5.update(bytes);
        self.size += bytes.len() as u64;
        self.encryptor
            .as_mut()
            .expect("open until finish")
            .update(bytes, &mut self.out)?;
        self.flush().await
    }

    async fn flush(&mut self) -> Result<(), BuildError> {
        if !self.out.is_empty() {
            self.writer.wait_for_capacity(PART_CONCURRENCY).await?;
            self.writer.put(Bytes::from(std::mem::take(&mut self.out)));
        }
        Ok(())
    }

    /// Seal the stream and complete the upload: the header, the SHA-256 and
    /// MD5 of the plaintext and its length.
    async fn finish(mut self) -> Result<(Vec<u8>, Vec<u8>, Vec<u8>, u64), BuildError> {
        self.encryptor
            .take()
            .expect("open until finish")
            .finish(&mut self.out)?;
        self.flush().await?;
        self.writer.finish().await?;
        Ok((
            self.header,
            self.hasher.finalize().to_vec(),
            self.md5.finalize().to_vec(),
            self.size,
        ))
    }

    async fn abort(self) {
        if let Err(err) = self.writer.abort().await {
            tracing::warn!(key = self.storage_key, error = ?err, "could not abort multipart upload");
        }
    }
}
