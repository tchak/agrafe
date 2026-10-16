//! Reading a stored original back as plaintext.
//!
//! A range of plaintext maps to a range of whole segments, fetched from the
//! object store in one read, opened one by one and trimmed at both ends. The
//! header cached on the row provides the salt and nonce prefix, so no second
//! read is needed. Each byte is copied once on the way in (store chunks are
//! gathered into a segment) and once on the way out (the decryption).

use std::ops::Range;

use agrafe_crypto::streaming::{Decryptor, Header, Layout};
use anyhow::Context as _;
use bytes::{Bytes, BytesMut};
use futures::StreamExt;
use futures::stream::BoxStream;
use object_store::path::Path as ObjectPath;
use object_store::{GetOptions, GetRange};

use super::Blob;
use crate::app::AppState;

/// Everything the row must carry once an object is stored.
pub struct Stored {
    pub storage_key: String,
    pub kek_id: String,
    pub wrapped_key: Vec<u8>,
    pub header: Header,
    pub layout: Layout,
    pub checksum: Vec<u8>,
    pub size: u64,
}

impl Stored {
    pub fn from_blob(blob: &Blob) -> anyhow::Result<Self> {
        let missing = |what: &str| anyhow::anyhow!("blob {} is stored but has no {what}", blob.id);
        let header = Header::parse(
            blob.encryption_header
                .as_deref()
                .ok_or_else(|| missing("encryption header"))?,
        )
        .map_err(|e| anyhow::anyhow!("blob {}: {e}", blob.id))?;
        let chunk_size = blob.chunk_size.ok_or_else(|| missing("chunk size"))?;
        let layout = usize::try_from(chunk_size)
            .ok()
            .and_then(|s| Layout::new(s).ok())
            .ok_or_else(|| missing("valid chunk size"))?;
        Ok(Self {
            storage_key: blob
                .storage_key
                .clone()
                .ok_or_else(|| missing("storage key"))?,
            kek_id: blob.kek_id.clone().ok_or_else(|| missing("KEK id"))?,
            wrapped_key: blob
                .wrapped_key
                .clone()
                .ok_or_else(|| missing("wrapped key"))?,
            header,
            layout,
            checksum: blob
                .checksum_sha256
                .clone()
                .ok_or_else(|| missing("checksum"))?,
            size: blob.size.ok_or_else(|| missing("size"))? as u64,
        })
    }
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
    /// Start reading the plaintext `span` of `blob`'s original. Nothing is
    /// decrypted yet; the first `next_segment` is where a wrong key or a
    /// tampered object shows up.
    pub async fn open(
        state: &AppState,
        blob: &Blob,
        stored: &Stored,
        span: Range<u64>,
    ) -> anyhow::Result<Self> {
        let key = state
            .keks()
            .unwrap(blob.id, &stored.kek_id, &stored.wrapped_key)?;
        let decryptor = Decryptor::new(
            &key,
            stored.storage_key.as_bytes(),
            &stored.header,
            stored.layout,
        );
        if span.is_empty() {
            // An empty file, or an empty range of one: nothing to fetch, and
            // `ciphertext_span` has no segment to point at.
            return Ok(Self {
                source: futures::stream::empty().boxed(),
                buffer: BytesMut::new(),
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
            .await
            .context("reading the object")?
            .into_stream()
            .boxed();
        Ok(Self {
            source,
            buffer: BytesMut::new(),
            decryptor,
            layout: stored.layout,
            size: stored.size,
            segment: first,
            skip: (span.start - stored.layout.plaintext_start(first)) as usize,
            remaining: span.end - span.start,
        })
    }

    /// The next piece of plaintext, or `None` once the range is served.
    pub async fn next_segment(&mut self) -> anyhow::Result<Option<Bytes>> {
        if self.remaining == 0 {
            return Ok(None);
        }
        let need = {
            let range = self.layout.ciphertext_range(self.segment, self.size);
            (range.end - range.start) as usize
        };
        while self.buffer.len() < need {
            match self.source.next().await {
                Some(chunk) => self
                    .buffer
                    .extend_from_slice(&chunk.context("reading the object")?),
                None => anyhow::bail!("object ended before segment {}", self.segment),
            }
        }
        let sealed = self.buffer.split_to(need);
        let last = self.layout.is_last(self.segment, self.size);
        let plain = self
            .decryptor
            .open(self.segment, &sealed, last)
            .with_context(|| format!("opening segment {}", self.segment))?;
        let skip = std::mem::take(&mut self.skip).min(plain.len());
        let take = ((plain.len() - skip) as u64).min(self.remaining) as usize;
        self.remaining -= take as u64;
        self.segment += 1;
        // Both conversions are free: the Vec becomes the backing buffer and
        // the slice is a view into it.
        Ok(Some(Bytes::from(plain).slice(skip..skip + take)))
    }
}
