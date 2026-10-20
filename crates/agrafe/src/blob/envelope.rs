//! The envelope: what serving a `ready` blob needs of its row, as one
//! opaque string a signed URL can carry, so that the read path has no row
//! to look up. See DESIGN.md, "Sealed URLs".
//!
//! Wire format, version 1, base64url without padding:
//!
//! ```text
//! u8 version = 1
//! str kek_id, bytes wrapped_key, bytes header     (u16 length each)
//! u32 chunk_size, u64 size, bytes checksum (32)
//! str content_type, str category, str scan_state
//! u8 has uploaded_at, i64 uploaded_at (unix seconds, when present)
//! thumbnail, preview:
//!   str state, i32 variant_version (0 when none),
//!   u8 rendered, bytes sha256 (32), u32 size  (when rendered)
//! ```
//!
//! Integers are big endian. The envelope is signed with the rest of the
//! URL, and the wrapped key is bound to the blob id, so nothing in it
//! needs authenticating on its own.

use agrafe_crypto::streaming::{Header, Layout};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use jiff::Timestamp;
use uuid::Uuid;

use super::{BlobHead, BlobState, Rendered, Representation, RepresentationState, ScanState};
use crate::classify::Category;

const VERSION: u8 = 1;

/// Why an envelope could not be read back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("invalid envelope")]
pub struct EnvelopeError;

/// A representation as the envelope recorded it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SealedRepresentation {
    /// The representation's state when the URL was minted.
    pub state: RepresentationState,
    /// The variant version it was rendered with; `None` before its first
    /// render, carried as `0` on the wire.
    pub variant_version: Option<i32>,
    /// The digest and size of the rendered WebP, when the row recorded
    /// both.
    pub rendered: Option<Rendered>,
}

/// What serving a `ready` blob needs of its row, sealed into a signed URL
/// so the read path needs no database lookup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Envelope {
    /// The key-encryption key that unwraps `wrapped_key`.
    pub kek_id: String,
    /// The blob's data key, wrapped and bound to the blob id.
    pub wrapped_key: Vec<u8>,
    /// The streaming AEAD header of the original.
    pub header: Header,
    /// Segment size of the streaming AEAD in bytes.
    pub chunk_size: u32,
    /// Plaintext size of the original in bytes.
    pub size: u64,
    /// SHA-256 of the original.
    pub checksum: [u8; 32],
    /// The blob's normalised media type.
    pub content_type: String,
    /// What the content type classifies as.
    pub category: Category,
    /// How the scan ended: `clean` or `skipped` for a `ready` blob.
    pub scan_state: ScanState,
    /// When the bytes landed, to the second.
    pub uploaded_at: Option<Timestamp>,
    /// The thumbnail as the row recorded it.
    pub thumbnail: SealedRepresentation,
    /// The preview as the row recorded it.
    pub preview: SealedRepresentation,
}

impl Envelope {
    /// The envelope of a blob `ready` at `now`; `None` for any other, or
    /// for a row missing what a read needs.
    pub fn from_head(head: &BlobHead, now: Timestamp) -> Option<Self> {
        if !head.is_readable(now) {
            return None;
        }
        let sealed = |kind| SealedRepresentation {
            state: head.representation_state(kind),
            variant_version: head.representation_version(kind),
            rendered: head.rendered(kind),
        };
        Some(Self {
            kek_id: head.kek_id.clone()?,
            wrapped_key: head.wrapped_key.clone()?,
            header: Header::parse(head.encryption_header.as_deref()?).ok()?,
            chunk_size: u32::try_from(head.chunk_size?).ok()?,
            size: u64::try_from(head.size?).ok()?,
            checksum: head.checksum_sha256.as_deref()?.try_into().ok()?,
            content_type: head.content_type.clone(),
            category: head.category,
            scan_state: head.scan_state,
            uploaded_at: head.uploaded_at,
            thumbnail: sealed(Representation::Thumbnail),
            preview: sealed(Representation::Preview),
        })
    }

    /// The `kind` representation as the envelope recorded it.
    pub fn representation(&self, kind: Representation) -> SealedRepresentation {
        match kind {
            Representation::Thumbnail => self.thumbnail,
            Representation::Preview => self.preview,
        }
    }

    /// The row as the envelope recorded it: `ready`, with no expiry, since
    /// the URL's own expiry bounds it.
    pub fn head(&self, id: Uuid, filename: &str) -> BlobHead {
        BlobHead {
            id,
            state: BlobState::Ready,
            filename: filename.to_owned(),
            content_type: self.content_type.clone(),
            category: self.category,
            checksum_sha256: Some(self.checksum.to_vec()),
            size: Some(self.size as i64),
            expires_at: None,
            scan_state: self.scan_state,
            storage_key: Some(format!("{id}/original")),
            kek_id: Some(self.kek_id.clone()),
            wrapped_key: Some(self.wrapped_key.clone()),
            encryption_header: Some(self.header.as_bytes().to_vec()),
            chunk_size: Some(self.chunk_size as i32),
            thumbnail_state: self.thumbnail.state,
            preview_state: self.preview.state,
            thumbnail_variant_version: self.thumbnail.variant_version,
            preview_variant_version: self.preview.variant_version,
            thumbnail_sha256: self.thumbnail.rendered.map(|r| r.sha256.to_vec()),
            thumbnail_size: self.thumbnail.rendered.map(|r| r.size as i32),
            preview_sha256: self.preview.rendered.map(|r| r.sha256.to_vec()),
            preview_size: self.preview.rendered.map(|r| r.size as i32),
            uploaded_at: self.uploaded_at,
        }
    }

    /// The envelope in its wire format, base64url without padding.
    pub fn encode(&self) -> String {
        let mut w = Writer(Vec::with_capacity(320));
        w.u8(VERSION);
        w.str(&self.kek_id);
        w.bytes(&self.wrapped_key);
        w.bytes(self.header.as_bytes());
        w.u32(self.chunk_size);
        w.u64(self.size);
        w.bytes(&self.checksum);
        w.str(&self.content_type);
        w.str(self.category.as_str());
        w.str(self.scan_state.as_str());
        match self.uploaded_at {
            Some(at) => {
                w.u8(1);
                w.i64(at.as_second());
            }
            None => w.u8(0),
        }
        for rep in [&self.thumbnail, &self.preview] {
            w.str(rep.state.as_str());
            w.i32(rep.variant_version.unwrap_or(0));
            match rep.rendered {
                Some(r) => {
                    w.u8(1);
                    w.bytes(&r.sha256);
                    w.u32(r.size as u32);
                }
                None => w.u8(0),
            }
        }
        URL_SAFE_NO_PAD.encode(w.0)
    }

    /// Read back what [`Envelope::encode`] wrote. Fails on another version,
    /// a truncated or overlong string, an unknown enum value, or an
    /// invalid header or segment size.
    pub fn decode(encoded: &str) -> Result<Self, EnvelopeError> {
        let bytes = URL_SAFE_NO_PAD.decode(encoded).map_err(|_| EnvelopeError)?;
        let mut r = Reader(&bytes);
        if r.u8()? != VERSION {
            return Err(EnvelopeError);
        }
        let kek_id = r.str()?;
        let wrapped_key = r.bytes()?.to_vec();
        let header = Header::parse(r.bytes()?).map_err(|_| EnvelopeError)?;
        let chunk_size = r.u32()?;
        Layout::new(chunk_size as usize).map_err(|_| EnvelopeError)?;
        let size = r.u64()?;
        let checksum: [u8; 32] = r.bytes()?.try_into().map_err(|_| EnvelopeError)?;
        let content_type = r.str()?;
        let category = r.str()?.parse().map_err(|_| EnvelopeError)?;
        let scan_state = r.str()?.parse().map_err(|_| EnvelopeError)?;
        let uploaded_at = match r.u8()? {
            0 => None,
            1 => Some(Timestamp::from_second(r.i64()?).map_err(|_| EnvelopeError)?),
            _ => return Err(EnvelopeError),
        };
        let mut rep = || -> Result<SealedRepresentation, EnvelopeError> {
            let state = r.str()?.parse().map_err(|_| EnvelopeError)?;
            let variant_version = Some(r.i32()?).filter(|v| *v != 0);
            let rendered = match r.u8()? {
                0 => None,
                1 => Some(Rendered {
                    sha256: r.bytes()?.try_into().map_err(|_| EnvelopeError)?,
                    size: r.u32()? as u64,
                }),
                _ => return Err(EnvelopeError),
            };
            Ok(SealedRepresentation {
                state,
                variant_version,
                rendered,
            })
        };
        let thumbnail = rep()?;
        let preview = rep()?;
        if !r.0.is_empty() {
            return Err(EnvelopeError);
        }
        Ok(Self {
            kek_id,
            wrapped_key,
            header,
            chunk_size,
            size,
            checksum,
            content_type,
            category,
            scan_state,
            uploaded_at,
            thumbnail,
            preview,
        })
    }
}

struct Writer(Vec<u8>);

impl Writer {
    fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_be_bytes());
    }
    fn i32(&mut self, v: i32) {
        self.0.extend_from_slice(&v.to_be_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_be_bytes());
    }
    fn i64(&mut self, v: i64) {
        self.0.extend_from_slice(&v.to_be_bytes());
    }
    fn bytes(&mut self, v: &[u8]) {
        let len = u16::try_from(v.len()).expect("envelope fields are short");
        self.0.extend_from_slice(&len.to_be_bytes());
        self.0.extend_from_slice(v);
    }
    fn str(&mut self, v: &str) {
        self.bytes(v.as_bytes());
    }
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], EnvelopeError> {
        if self.0.len() < n {
            return Err(EnvelopeError);
        }
        let (head, rest) = self.0.split_at(n);
        self.0 = rest;
        Ok(head)
    }
    fn u8(&mut self) -> Result<u8, EnvelopeError> {
        Ok(self.take(1)?[0])
    }
    fn u32(&mut self) -> Result<u32, EnvelopeError> {
        Ok(u32::from_be_bytes(
            self.take(4)?.try_into().expect("4 bytes"),
        ))
    }
    fn i32(&mut self) -> Result<i32, EnvelopeError> {
        Ok(i32::from_be_bytes(
            self.take(4)?.try_into().expect("4 bytes"),
        ))
    }
    fn u64(&mut self) -> Result<u64, EnvelopeError> {
        Ok(u64::from_be_bytes(
            self.take(8)?.try_into().expect("8 bytes"),
        ))
    }
    fn i64(&mut self) -> Result<i64, EnvelopeError> {
        Ok(i64::from_be_bytes(
            self.take(8)?.try_into().expect("8 bytes"),
        ))
    }
    fn bytes(&mut self) -> Result<&'a [u8], EnvelopeError> {
        let len = u16::from_be_bytes(self.take(2)?.try_into().expect("2 bytes"));
        self.take(len as usize)
    }
    fn str(&mut self) -> Result<String, EnvelopeError> {
        std::str::from_utf8(self.bytes()?)
            .map(str::to_owned)
            .map_err(|_| EnvelopeError)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agrafe_crypto::streaming::HEADER_LEN;

    fn sample() -> Envelope {
        Envelope {
            kek_id: "kek1".into(),
            wrapped_key: vec![7u8; 60],
            header: Header::parse(&{
                let mut h = [1u8; HEADER_LEN];
                h[0] = HEADER_LEN as u8;
                h
            })
            .unwrap(),
            chunk_size: 256 * 1024,
            size: 1 << 33,
            checksum: [9u8; 32],
            content_type: "image/jpeg".into(),
            category: Category::Image,
            scan_state: ScanState::Clean,
            uploaded_at: Some("2026-10-16T08:44:26Z".parse().unwrap()),
            thumbnail: SealedRepresentation {
                state: RepresentationState::Ready,
                variant_version: Some(3),
                rendered: Some(Rendered {
                    sha256: [4u8; 32],
                    size: 12_345,
                }),
            },
            preview: SealedRepresentation {
                state: RepresentationState::Pending,
                variant_version: None,
                rendered: None,
            },
        }
    }

    #[test]
    fn round_trips() {
        let env = sample();
        let encoded = env.encode();
        assert!(encoded.len() < 400, "{}", encoded.len());
        assert_eq!(Envelope::decode(&encoded).unwrap(), env);
        let id = Uuid::now_v7();
        let head = env.head(id, "photo.jpg");
        assert_eq!(Envelope::from_head(&head, Timestamp::now()), Some(env));
        assert_eq!(
            head.storage_key.as_deref(),
            Some(format!("{id}/original").as_str())
        );
    }

    #[test]
    fn rejects_damage() {
        let encoded = sample().encode();
        assert!(Envelope::decode("").is_err());
        assert!(Envelope::decode("not base64!").is_err());
        assert!(Envelope::decode(&encoded[..encoded.len() - 4]).is_err());
        assert!(Envelope::decode(&format!("{encoded}AAAA")).is_err());
        let mut other = URL_SAFE_NO_PAD.decode(&encoded).unwrap();
        other[0] = 2;
        assert!(Envelope::decode(&URL_SAFE_NO_PAD.encode(other)).is_err());
    }

    #[test]
    fn only_a_ready_blob_has_one() {
        let now = Timestamp::now();
        let mut head = sample().head(Uuid::now_v7(), "a");
        head.state = BlobState::Uploaded;
        assert!(Envelope::from_head(&head, now).is_none());
        head.state = BlobState::Ready;
        head.expires_at = Some(now);
        assert!(Envelope::from_head(&head, now).is_none(), "expired");
        head.expires_at = None;
        head.chunk_size = None;
        assert!(Envelope::from_head(&head, now).is_none());
    }
}
