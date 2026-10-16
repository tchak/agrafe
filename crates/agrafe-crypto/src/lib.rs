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
    pub fn generate() -> Self {
        let mut bytes = [0u8; KEY_LEN];
        rand::fill(&mut bytes);
        Self(bytes)
    }

    pub fn from_bytes(bytes: [u8; KEY_LEN]) -> Self {
        Self(bytes)
    }

    pub fn from_slice(bytes: &[u8]) -> Result<Self, Error> {
        let bytes: [u8; KEY_LEN] = bytes.try_into().map_err(|_| Error::InvalidKeyLength)?;
        Ok(Self(bytes))
    }

    pub fn as_bytes(&self) -> &[u8; KEY_LEN] {
        &self.0
    }
}

impl std::fmt::Debug for Key {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Key(…)")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("key must be {KEY_LEN} bytes")]
    InvalidKeyLength,
    #[error(
        "segment size must be between {} and {} bytes",
        streaming::MIN_SEGMENT_SIZE,
        streaming::MAX_SEGMENT_SIZE
    )]
    InvalidSegmentSize,
    #[error("malformed stream header")]
    InvalidHeader,
    #[error("ciphertext is corrupt or truncated")]
    Corrupt,
    #[error("plaintext is too long for the segment size")]
    TooManySegments,
    #[error("the encryptor was already finished")]
    Finished,
}
