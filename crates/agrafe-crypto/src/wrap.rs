//! Envelope encryption: a blob's data key wrapped under a key-encryption key.
//!
//! ```text
//! wrapped = nonce (12 bytes) || AES-256-GCM(kek, nonce, data_key, aad)
//! ```
//!
//! The associated data binds the wrapped key to its blob, so a wrapped key
//! copied onto another row does not unwrap. Rotating a KEK means unwrapping
//! with the old one and wrapping with the new one, a database-only operation.

use aes_gcm::aead::{Aead, Payload};
use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
use zeroize::Zeroizing;

use crate::{Error, KEY_LEN, Key};

const NONCE_LEN: usize = 12;
const TAG_LEN: usize = 16;
/// Length of a wrapped key.
pub const WRAPPED_LEN: usize = NONCE_LEN + KEY_LEN + TAG_LEN;

/// A key-encryption key.
pub struct Kek {
    cipher: Aes256Gcm,
}

impl Kek {
    pub fn new(key: &Key) -> Self {
        Self {
            cipher: Aes256Gcm::new(key.as_bytes().into()),
        }
    }

    pub fn wrap(&self, data_key: &Key, associated_data: &[u8]) -> Vec<u8> {
        let mut nonce = [0u8; NONCE_LEN];
        rand::fill(&mut nonce);
        let nonce = Nonce::from(nonce);
        let sealed = self
            .cipher
            .encrypt(
                &nonce,
                Payload {
                    msg: data_key.as_bytes(),
                    aad: associated_data,
                },
            )
            .expect("AES-GCM accepts a 32-byte message");
        let mut out = Vec::with_capacity(WRAPPED_LEN);
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&sealed);
        out
    }

    pub fn unwrap(&self, wrapped: &[u8], associated_data: &[u8]) -> Result<Key, Error> {
        if wrapped.len() != WRAPPED_LEN {
            return Err(Error::Corrupt);
        }
        let (nonce, sealed) = wrapped.split_at(NONCE_LEN);
        let nonce =
            Nonce::<aes_gcm::aead::consts::U12>::try_from(nonce).map_err(|_| Error::Corrupt)?;
        let bytes = Zeroizing::new(
            self.cipher
                .decrypt(
                    &nonce,
                    Payload {
                        msg: sealed,
                        aad: associated_data,
                    },
                )
                .map_err(|_| Error::Corrupt)?,
        );
        Key::from_slice(&bytes)
    }
}

impl std::fmt::Debug for Kek {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Kek(…)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_binding() {
        let kek = Kek::new(&Key::from_bytes([1u8; KEY_LEN]));
        let dk = Key::generate();
        let wrapped = kek.wrap(&dk, b"blob-1");
        assert_eq!(wrapped.len(), WRAPPED_LEN);
        assert_eq!(kek.unwrap(&wrapped, b"blob-1").unwrap(), dk);

        assert_eq!(kek.unwrap(&wrapped, b"blob-2"), Err(Error::Corrupt));
        let other = Kek::new(&Key::from_bytes([2u8; KEY_LEN]));
        assert_eq!(other.unwrap(&wrapped, b"blob-1"), Err(Error::Corrupt));
        let mut bad = wrapped.clone();
        bad[20] ^= 1;
        assert_eq!(kek.unwrap(&bad, b"blob-1"), Err(Error::Corrupt));
        assert_eq!(
            kek.unwrap(&wrapped[..WRAPPED_LEN - 1], b"blob-1"),
            Err(Error::Corrupt)
        );

        // Two wraps of the same key differ (random nonce).
        assert_ne!(kek.wrap(&dk, b"blob-1"), wrapped);
    }
}
