//! What the two build jobs share: the generated file streamed into the
//! bucket as it is produced, through the same encryptor and multipart
//! upload as an uploaded file, to `<blob id>/original`, and the row moved
//! to `ready` once it is there. Nothing touches local disk and nothing is
//! held whole in memory. See DESIGN.md, "Build".
//!
//! A kind of build is an [`Assemble`]: `archive` writes a ZIP of members,
//! `rows` a CSV, XLSX or ODS of cells. Each gets a [`Sink`] to write into
//! and the blob to read its source from; everything around it, the key,
//! the multipart upload, the abort on failure, the last check that the
//! blob still waits for its build, is here.

pub mod zip;

use agrafe_crypto::Key;
use agrafe_crypto::streaming::{Encryptor, Layout};
use bytes::Bytes;
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
}

impl BuildError {
    /// Whether the next attempt would fail the same way. A source that does
    /// not parse, a row missing its key material, a key that does not
    /// unwrap and a package the writer refuses are the blob's own; the
    /// bucket and the database may come back.
    pub fn is_permanent(&self) -> bool {
        matches!(
            self,
            BuildError::Source { .. }
                | BuildError::Incomplete { .. }
                | BuildError::Key(_)
                | BuildError::Zip(_)
        )
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
    if blob.state != BlobState::Uploaded || blob.input_type != A::INPUT {
        tracing::info!(state = %blob.state, input_type = %blob.input_type, "blob is not waiting for a build");
        return Ok(());
    }
    let Some(built) = build::<A>(state, &blob).await? else {
        tracing::warn!("blob left `uploaded` during the build, nothing written");
        return Ok(());
    };
    match repo::complete_build(state.pool(), id, &built).await? {
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

/// Assemble the file into `<id>/original`. `None` when the blob was no
/// longer `uploaded` at the end: the object is not finished and nothing
/// is written.
async fn build<A: Assemble>(state: &Agrafe, blob: &Blob) -> Result<Option<Built>, BuildError> {
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

    let storage_key = upload::object_key(blob.id);
    let mut sink = Sink::open(state, &storage_key, &key, layout).await?;
    let counts = match A::assemble(state, blob, &mut sink).await {
        Ok(counts) => counts,
        Err(err) => {
            sink.abort().await;
            return Err(err);
        }
    };
    // One last look before the object becomes visible: the key is the
    // blob's, so only a build of a blob still waiting may finish it.
    let still_ours = repo::find(state.pool(), blob.id)
        .await?
        .is_some_and(|b| b.state == BlobState::Uploaded);
    if !still_ours {
        sink.abort().await;
        return Ok(None);
    }
    let (encryption_header, checksum_sha256, size) = sink.finish().await?;
    Ok(Some(Built {
        storage_key,
        encryption_header,
        checksum_sha256,
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
        let encryptor = Encryptor::new(key, storage_key.as_bytes(), layout);
        let header = encryptor.header().as_bytes().to_vec();
        writer.write(&header);
        Ok(Self {
            writer,
            encryptor: Some(encryptor),
            header,
            hasher: Sha256::new(),
            size: 0,
            out: Vec::new(),
            storage_key: storage_key.to_string(),
        })
    }

    pub(crate) async fn write(&mut self, bytes: &[u8]) -> Result<(), BuildError> {
        self.hasher.update(bytes);
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

    /// Seal the stream and complete the upload: the header, the SHA-256 of
    /// the plaintext and its length.
    async fn finish(mut self) -> Result<(Vec<u8>, Vec<u8>, u64), BuildError> {
        self.encryptor
            .take()
            .expect("open until finish")
            .finish(&mut self.out)?;
        self.flush().await?;
        self.writer.finish().await?;
        Ok((self.header, self.hasher.finalize().to_vec(), self.size))
    }

    async fn abort(self) {
        if let Err(err) = self.writer.abort().await {
            tracing::warn!(key = self.storage_key, error = ?err, "could not abort multipart upload");
        }
    }
}
