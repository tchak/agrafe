//! Encryption primitives for agrafe, kept in their own crate so they can be
//! reviewed in isolation.
//!
//! - [`streaming`]: a chunked streaming AEAD in Tink's
//!   `AES-GCM-HKDF-STREAMING` wire format, with random access by segment.
//! - [`wrap`]: envelope encryption of a blob's data key under a
//!   key-encryption key from configuration.

pub mod streaming;
pub mod wrap;

use zeroize::{Zeroize, ZeroizeOnDrop};

/// Length in bytes of a data key and of a key-encryption key.
pub const KEY_LEN: usize = 32;

/// A random 256-bit key. `Debug` never prints the bytes, and the bytes are
/// wiped when the key is dropped.
#[derive(Clone, PartialEq, Eq, Zeroize, ZeroizeOnDrop)]
pub struct Key([u8; KEY_LEN]);

impl Key {
    /// A fresh key from `rand`'s thread-local CSPRNG, which the operating
    /// system's random source seeds.
    pub fn generate() -> Self {
        let mut bytes = [0u8; KEY_LEN];
        rand::fill(&mut bytes);
        Self(bytes)
    }

    /// The key made of `bytes`.
    pub fn from_bytes(bytes: [u8; KEY_LEN]) -> Self {
        Self(bytes)
    }

    /// The key made of `bytes`, which must be exactly [`KEY_LEN`] long, or
    /// [`Error::InvalidKeyLength`].
    pub fn from_slice(bytes: &[u8]) -> Result<Self, Error> {
        let bytes: [u8; KEY_LEN] = bytes.try_into().map_err(|_| Error::InvalidKeyLength)?;
        Ok(Self(bytes))
    }

    /// The raw key material.
    pub fn as_bytes(&self) -> &[u8; KEY_LEN] {
        &self.0
    }
}

impl std::fmt::Debug for Key {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Key(…)")
    }
}

/// Why a key, a header or a ciphertext was refused. None of them says
/// which check failed inside AES-GCM, by design.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// Key material that is not [`KEY_LEN`] bytes.
    #[error("key must be {KEY_LEN} bytes")]
    InvalidKeyLength,
    /// A segment size below [`streaming::MIN_SEGMENT_SIZE`] or above
    /// [`streaming::MAX_SEGMENT_SIZE`].
    #[error(
        "segment size must be between {} and {} bytes",
        streaming::MIN_SEGMENT_SIZE,
        streaming::MAX_SEGMENT_SIZE
    )]
    InvalidSegmentSize,
    /// A stream header that is not [`streaming::HEADER_LEN`] bytes, or whose
    /// first byte, the header length, is not `HEADER_LEN`.
    #[error("malformed stream header")]
    InvalidHeader,
    /// Authentication failed, or the ciphertext has a length no plaintext
    /// produces: the bytes were altered, truncated or reordered, or the key,
    /// the associated data or the segment's position (its index, whether it
    /// is the last) is not the one it was sealed with. Also a wrapped key
    /// that does not unwrap.
    #[error("ciphertext is corrupt or truncated")]
    Corrupt,
    /// A segment index past the 32-bit counter of the nonce: a stream has
    /// at most 2^32 segments.
    #[error("plaintext is too long for the segment size")]
    TooManySegments,
}
