//! Reading a stored original back as plaintext.
//!
//! A range of plaintext maps to a range of whole segments, fetched from the
//! object store in one read, opened one by one and trimmed at both ends. The
//! header cached on the row provides the salt and nonce prefix, so no second
//! read is needed. Each byte is copied once, when the store's chunks are
//! gathered into a segment; the segment is opened where it lies and handed
//! on as a view of that same buffer.

use std::ops::Range;

use agrafe_crypto::streaming::{Decryptor, HEADER_LEN, Header, Layout};
use bytes::{Bytes, BytesMut};
use futures::StreamExt;
use futures::stream::BoxStream;
use object_store::path::Path as ObjectPath;
use object_store::{GetOptions, GetRange, ObjectStoreExt};
use uuid::Uuid;

use crate::storage::Store;

use super::envelope::Envelope;
use super::{Blob, BlobHead};
use crate::app::Agrafe;
use crate::encryption::UnwrapError;

/// Why a stored original could not be read back.
#[derive(Debug, thiserror::Error)]
pub enum ReadError {
    /// The row lacks a column a read needs, or its chunk size is not a
    /// valid segment size.
    #[error("blob {blob_id} is stored but has no {what}")]
    Incomplete {
        /// The blob whose row is incomplete.
        blob_id: Uuid,
        /// What is missing, such as `wrapped key`.
        what: &'static str,
    },
    /// The streaming AEAD header, from the row or the object, does not
    /// parse.
    #[error("blob {blob_id}: {source}")]
    Header {
        /// The blob being read.
        blob_id: Uuid,
        /// Why the header was refused.
        source: agrafe_crypto::Error,
    },
    /// The data key does not unwrap.
    #[error(transparent)]
    Key(#[from] UnwrapError),
    /// The object store failed, or has no such object.
    #[error("reading the object: {0}")]
    Store(#[from] object_store::Error),
    /// The object's bytes stopped before the given segment was complete.
    #[error("object ended before segment {0}")]
    Truncated(u64),
    /// The object's length is not one an encrypted stream can have.
    #[error("object {key} is {len} bytes, which no stream of this layout produces")]
    Length {
        /// The object's key.
        key: String,
        /// Its length in bytes.
        len: u64,
    },
    /// A segment failed to decrypt: a wrong key, or a tampered or
    /// truncated object.
    #[error("opening segment {segment}: {source}")]
    Segment {
        /// Index of the segment, from zero.
        segment: u64,
        /// Why it did not open.
        source: agrafe_crypto::Error,
    },
}

impl ReadError {
    /// Whether the object is not in the store.
    pub fn is_not_found(&self) -> bool {
        matches!(self, ReadError::Store(object_store::Error::NotFound { .. }))
    }

    /// Whether reading again would fail the same way: the row or the object
    /// is damaged. A store error, or an object ending short of what its
    /// length promised, may be the connection's.
    pub(crate) fn is_permanent(&self) -> bool {
        match self {
            ReadError::Incomplete { .. }
            | ReadError::Header { .. }
            | ReadError::Key(_)
            | ReadError::Length { .. }
            | ReadError::Segment { .. } => true,
            ReadError::Store(_) | ReadError::Truncated(_) => false,
        }
    }
}

/// Everything the row must carry once an object is stored.
pub struct Stored {
    /// The blob, which the wrapped key is bound to.
    pub blob_id: Uuid,
    /// Object key of the original, `key::original`; the stream's
    /// associated data is `key::associated_data` of it.
    pub storage_key: String,
    /// The key-encryption key that unwraps `wrapped_key`.
    pub kek_id: String,
    /// The blob's wrapped data key.
    pub wrapped_key: Vec<u8>,
    /// The streaming AEAD header, cached from the object.
    pub header: Header,
    /// The segment layout the chunk size gives.
    pub layout: Layout,
    /// SHA-256 of the plaintext.
    pub checksum: Vec<u8>,
    /// Plaintext size in bytes.
    pub size: u64,
}

/// The nullable columns `Stored` is assembled from, as either row carries
/// them.
pub(crate) struct Columns<'a> {
    pub(crate) blob_id: Uuid,
    pub(crate) storage_key: Option<&'a str>,
    pub(crate) kek_id: Option<&'a str>,
    pub(crate) wrapped_key: Option<&'a [u8]>,
    pub(crate) encryption_header: Option<&'a [u8]>,
    pub(crate) chunk_size: Option<i32>,
    pub(crate) checksum_sha256: Option<&'a [u8]>,
    pub(crate) size: Option<i64>,
}

impl Stored {
    pub(crate) fn from_blob(blob: &Blob) -> Result<Self, ReadError> {
        Self::from_columns(Columns {
            blob_id: blob.id,
            storage_key: blob.storage_key.as_deref(),
            kek_id: blob.kek_id.as_deref(),
            wrapped_key: blob.wrapped_key.as_deref(),
            encryption_header: blob.encryption_header.as_deref(),
            chunk_size: blob.chunk_size,
            checksum_sha256: blob.checksum_sha256.as_deref(),
            size: blob.size,
        })
    }

    /// From the envelope of a sealed URL, which already carries each field
    /// parsed; nothing is looked up and nothing is re-validated.
    pub fn from_envelope(blob_id: Uuid, envelope: &Envelope) -> Result<Self, ReadError> {
        let layout =
            Layout::new(envelope.chunk_size as usize).map_err(|_| ReadError::Incomplete {
                blob_id,
                what: "valid chunk size",
            })?;
        Ok(Self {
            blob_id,
            storage_key: super::key::original(blob_id),
            kek_id: envelope.kek_id.clone(),
            wrapped_key: envelope.wrapped_key.clone(),
            header: envelope.header.clone(),
            layout,
            checksum: envelope.checksum.to_vec(),
            size: envelope.size,
        })
    }

    /// From the row as the read routes fetch it. [`ReadError::Incomplete`]
    /// or [`ReadError::Header`] when it lacks what a read needs.
    pub fn from_head(blob: &BlobHead) -> Result<Self, ReadError> {
        Self::from_columns(Columns {
            blob_id: blob.id,
            storage_key: blob.storage_key.as_deref(),
            kek_id: blob.kek_id.as_deref(),
            wrapped_key: blob.wrapped_key.as_deref(),
            encryption_header: blob.encryption_header.as_deref(),
            chunk_size: blob.chunk_size,
            checksum_sha256: blob.checksum_sha256.as_deref(),
            size: blob.size,
        })
    }

    pub(crate) fn from_columns(c: Columns<'_>) -> Result<Self, ReadError> {
        let blob_id = c.blob_id;
        let missing = |what| ReadError::Incomplete { blob_id, what };
        let header = Header::parse(
            c.encryption_header
                .ok_or_else(|| missing("encryption header"))?,
        )
        .map_err(|source| ReadError::Header { blob_id, source })?;
        let chunk_size = c.chunk_size.ok_or_else(|| missing("chunk size"))?;
        let layout = usize::try_from(chunk_size)
            .ok()
            .and_then(|s| Layout::new(s).ok())
            .ok_or_else(|| missing("valid chunk size"))?;
        Ok(Self {
            blob_id,
            storage_key: c
                .storage_key
                .ok_or_else(|| missing("storage key"))?
                .to_owned(),
            kek_id: c.kek_id.ok_or_else(|| missing("KEK id"))?.to_owned(),
            wrapped_key: c
                .wrapped_key
                .ok_or_else(|| missing("wrapped key"))?
                .to_owned(),
            header,
            layout,
            checksum: c
                .checksum_sha256
                .ok_or_else(|| missing("checksum"))?
                .to_owned(),
            size: c.size.ok_or_else(|| missing("size"))? as u64,
        })
    }
}

/// How much of a source part one request reads. A part is read a window at
/// a time, each fetched whole: the build consumes its lines in batches with
/// long member copies between them, and a response held open and idle that
/// long is one an object store cuts off, failing the attempt every time.
const PART_WINDOW: u64 = 8 * 1024 * 1024;

/// The bytes of `path`, `len` long, as a stream of ranged reads of at most
/// `window` bytes, each requested when the one before is used up.
fn windows(
    store: Store,
    path: ObjectPath,
    len: u64,
    window: u64,
) -> BoxStream<'static, object_store::Result<Bytes>> {
    futures::stream::try_unfold(0u64, move |offset| {
        let (store, path) = (store.clone(), path.clone());
        async move {
            if offset >= len {
                return Ok(None);
            }
            let end = (offset + window).min(len);
            let bytes = store.get_range(&path, offset..end).await?;
            Ok(Some((bytes, end)))
        }
    })
    .boxed()
}

/// Opens segments as their ciphertext arrives, trimming to the requested
/// plaintext range.
pub struct Reader {
    source: BoxStream<'static, object_store::Result<Bytes>>,
    buffer: BytesMut,
    decryptor: Decryptor,
    layout: Layout,
    size: u64,
    /// Next segment to open.
    segment: u64,
    /// Plaintext bytes to drop from the front of the next opened segment.
    skip: usize,
    /// Plaintext bytes still to send.
    remaining: u64,
}

impl Reader {
    /// Start reading the plaintext `span` of the original `stored`
    /// describes. Nothing is decrypted yet; the first `next_segment` is
    /// where a wrong key or a tampered object shows up.
    pub async fn open(
        state: &Agrafe,
        stored: &Stored,
        span: Range<u64>,
    ) -> Result<Self, ReadError> {
        let key = state
            .keks()
            .unwrap(stored.blob_id, &stored.kek_id, &stored.wrapped_key)?;
        let decryptor = Decryptor::new(
            &key,
            super::key::associated_data(&stored.storage_key).as_bytes(),
            &stored.header,
            stored.layout,
        );
        if span.is_empty() {
            // An empty file, or an empty range of one: nothing to fetch, and
            // `ciphertext_span` has no segment to point at.
            return Ok(Self {
                source: futures::stream::empty().boxed(),
                buffer: BytesMut::with_capacity(stored.layout.segment_size()),
                decryptor,
                layout: stored.layout,
                size: stored.size,
                segment: 0,
                skip: 0,
                remaining: 0,
            });
        }
        let (first, bytes) = stored.layout.ciphertext_span(span.clone(), stored.size);
        let source = state
            .store()
            .get_opts(
                &ObjectPath::from(stored.storage_key.as_str()),
                GetOptions {
                    range: Some(GetRange::Bounded(bytes)),
                    ..Default::default()
                },
            )
            .await?
            .into_stream()
            .boxed();
        Ok(Self {
            source,
            buffer: BytesMut::with_capacity(stored.layout.segment_size()),
            decryptor,
            layout: stored.layout,
            size: stored.size,
            segment: first,
            skip: (span.start - stored.layout.plaintext_start(first)) as usize,
            remaining: span.end - span.start,
        })
    }

    /// Start reading the whole of source part `n` of a generated blob. The
    /// row carries the data key and the layout, fixed by the first part;
    /// the part's own header is read off the object, and its length comes
    /// from the object's.
    pub(crate) async fn open_part(state: &Agrafe, blob: &Blob, n: u32) -> Result<Self, ReadError> {
        let missing = |what| ReadError::Incomplete {
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

        let storage_key = super::key::source(blob.id, n);
        let path = ObjectPath::from(storage_key.as_str());
        let len = state.store().head(&path).await?.size;
        let size = layout.plaintext_len(len).map_err(|_| ReadError::Length {
            key: storage_key.clone(),
            len,
        })?;
        let mut source = windows(state.store().clone(), path, len, PART_WINDOW);
        let mut buffer = BytesMut::with_capacity(layout.segment_size());
        while buffer.len() < HEADER_LEN {
            match source.next().await {
                Some(chunk) => buffer.extend_from_slice(&chunk?),
                None => return Err(ReadError::Truncated(0)),
            }
        }
        let header =
            Header::parse(&buffer.split_to(HEADER_LEN)).map_err(|source| ReadError::Header {
                blob_id: blob.id,
                source,
            })?;
        let decryptor = Decryptor::new(
            &key,
            super::key::associated_data(&storage_key).as_bytes(),
            &header,
            layout,
        );
        Ok(Self {
            source,
            buffer,
            decryptor,
            layout,
            size,
            segment: 0,
            skip: 0,
            remaining: size,
        })
    }

    /// The next piece of plaintext, or `None` once the range is served.
    pub async fn next_segment(&mut self) -> Result<Option<Bytes>, ReadError> {
        if self.remaining == 0 {
            return Ok(None);
        }
        let need = {
            let range = self.layout.ciphertext_range(self.segment, self.size);
            (range.end - range.start) as usize
        };
        // The previous segment is still held downstream, so this is one
        // allocation per segment, rather than one per chunk gathered.
        self.buffer.reserve(need);
        while self.buffer.len() < need {
            match self.source.next().await {
                Some(chunk) => self.buffer.extend_from_slice(&chunk?),
                None => return Err(ReadError::Truncated(self.segment)),
            }
        }
        let mut sealed = self.buffer.split_to(need);
        let last = self.layout.is_last(self.segment, self.size);
        let len = self
            .decryptor
            .open_in_place(self.segment, &mut sealed, last)
            .map_err(|source| ReadError::Segment {
                segment: self.segment,
                source,
            })?;
        sealed.truncate(len);
        let skip = std::mem::take(&mut self.skip).min(len);
        let take = ((len - skip) as u64).min(self.remaining) as usize;
        self.remaining -= take as u64;
        self.segment += 1;
        // Neither step copies: the buffer is frozen and the slice is a view
        // into it.
        Ok(Some(sealed.freeze().slice(skip..skip + take)))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use futures::TryStreamExt as _;
    use object_store::memory::InMemory;

    use super::*;

    #[tokio::test]
    async fn a_part_is_read_in_windows_to_its_end() {
        let store: Store = Arc::new(InMemory::new());
        let path = ObjectPath::from("part");
        let bytes: Vec<u8> = (0..10u8).collect();
        store.put(&path, bytes.clone().into()).await.unwrap();
        for window in [1, 3, 10, 64] {
            let read: Vec<Bytes> = windows(store.clone(), path.clone(), 10, window)
                .try_collect()
                .await
                .unwrap();
            assert_eq!(read.len() as u64, 10_u64.div_ceil(window), "{window}");
            assert!(read.iter().all(|w| w.len() as u64 <= window), "{window}");
            assert_eq!(read.concat(), bytes, "{window}");
        }
        let empty: Vec<Bytes> = windows(store, path, 0, 3).try_collect().await.unwrap();
        assert!(empty.is_empty());
    }
}
