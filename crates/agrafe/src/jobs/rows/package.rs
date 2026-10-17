//! A ZIP package streamed into the sink: small entries given whole, and
//! one large entry, the sheet's XML, deflated as the writer produces it.

use std::io::Write;

use flate2::Compression;
use flate2::write::DeflateEncoder;
use jiff::Timestamp;

use super::FLUSH_AT;
use crate::jobs::build::zip::{DosTime, ZipWriter};
use crate::jobs::build::{BuildError, Sink};

pub struct Package<'a> {
    sink: &'a mut Sink,
    zip: ZipWriter,
    /// The compressor of the open deflated entry.
    encoder: Option<DeflateEncoder<Vec<u8>>>,
    /// Plain bytes of the open entry not yet compressed.
    buf: Vec<u8>,
    time: DosTime,
}

impl<'a> Package<'a> {
    pub fn new(sink: &'a mut Sink) -> Self {
        Self {
            sink,
            zip: ZipWriter::new(),
            encoder: None,
            buf: Vec::with_capacity(2 * FLUSH_AT),
            time: DosTime::from_timestamp(Timestamp::now()),
        }
    }

    /// A stored entry given whole, with its CRC in the header and no
    /// descriptor: what an ODS `mimetype` has to be.
    pub async fn stored(&mut self, name: &str, bytes: &[u8]) -> Result<(), BuildError> {
        let out = self.zip.whole_entry(name, bytes, self.time)?;
        self.sink.write(&out).await
    }

    /// A deflated entry given whole.
    pub async fn file(&mut self, name: &str, bytes: &[u8]) -> Result<(), BuildError> {
        self.begin(name).await?;
        self.buf.extend_from_slice(bytes);
        self.end().await
    }

    /// Open a deflated entry; its bytes go through [`Self::buf`] and
    /// [`Self::flush`] until [`Self::end`].
    pub async fn begin(&mut self, name: &str) -> Result<(), BuildError> {
        let out = self.zip.begin_deflated(name, self.time)?;
        self.sink.write(&out).await?;
        self.encoder = Some(DeflateEncoder::new(Vec::new(), Compression::default()));
        Ok(())
    }

    /// Where the writer of the open entry puts its bytes.
    pub fn buf(&mut self) -> &mut Vec<u8> {
        &mut self.buf
    }

    /// Compress what has gathered once there is enough of it, or whatever
    /// there is when `force`.
    pub async fn flush(&mut self, force: bool) -> Result<(), BuildError> {
        if self.buf.is_empty() || (!force && self.buf.len() < FLUSH_AT) {
            return Ok(());
        }
        let encoder = self.encoder.as_mut().expect("an entry is open");
        self.zip.data(&self.buf)?;
        encoder.write_all(&self.buf)?;
        self.buf.clear();
        let compressed = std::mem::take(encoder.get_mut());
        self.zip.compressed(compressed.len() as u64)?;
        self.sink.write(&compressed).await
    }

    /// Close the open entry: the rest of its bytes, the end of the deflate
    /// stream and the descriptor.
    pub async fn end(&mut self) -> Result<(), BuildError> {
        self.flush(true).await?;
        let encoder = self.encoder.take().expect("an entry is open");
        let compressed = encoder.finish()?;
        self.zip.compressed(compressed.len() as u64)?;
        self.sink.write(&compressed).await?;
        let out = self.zip.end_entry()?;
        self.sink.write(&out).await
    }

    /// The central directory: the end of the package.
    pub async fn finish(self) -> Result<(), BuildError> {
        let out = self.zip.finish()?;
        self.sink.write(&out).await
    }
}
