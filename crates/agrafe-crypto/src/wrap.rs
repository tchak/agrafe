//! Envelope encryption: a blob's data key wrapped under a key-encryption key.
//!
//! ```text
//! wrap_key = HKDF-SHA256(ikm = kek, salt = none, info = associated_data)
//! wrapped  = nonce (12 bytes) || AES-256-GCM(wrap_key, nonce, data_key, associated_data)
//! ```
//!
//! The associated data, the blob id, binds the wrapped key to its blob
//! twice over: it picks the wrapping key and it is authenticated with the
//! ciphertext, so a wrapped key copied onto another row does not unwrap.
//! Deriving a key per blob also keeps every AES-GCM key to one message:
//! NIST SP 800-38D allows 2^32 random nonces under one key, and a KEK
//! wrapping the keys of billions of blobs directly would run into it.
//! Rotating a KEK means unwrapping with the old one and wrapping with the
//! new one, a database-only operation.

use aes_gcm::Nonce;
use aes_gcm::aead::{Aead, Payload};
use zeroize::Zeroizing;

use crate::streaming::TAG_LEN;
use crate::{Error, KEY_LEN, Key, derive};

const NONCE_LEN: usize = 12;
/// Length of a wrapped key.
pub const WRAPPED_LEN: usize = NONCE_LEN + KEY_LEN + TAG_LEN;

/// A key-encryption key.
pub struct Kek {
    key: Key,
}

impl Kek {
    /// A key-encryption key: `key` is the HKDF input from which every
    /// wrapping key is derived.
    pub fn new(key: &Key) -> Self {
        Self { key: key.clone() }
    }

    /// `data_key` sealed under a fresh random nonce, bound to
    /// `associated_data`: [`WRAPPED_LEN`] bytes.
    pub fn wrap(&self, data_key: &Key, associated_data: &[u8]) -> Vec<u8> {
        let mut nonce = [0u8; NONCE_LEN];
        rand::fill(&mut nonce);
        self.wrap_with_nonce(data_key, associated_data, nonce)
    }

    /// [`Kek::wrap`] under `nonce`. The nonce need not be unique, since
    /// the wrapping key is derived per `associated_data`, but a random one
    /// costs nothing and keeps two wraps of one key apart.
    pub(crate) fn wrap_with_nonce(
        &self,
        data_key: &Key,
        associated_data: &[u8],
        nonce: [u8; NONCE_LEN],
    ) -> Vec<u8> {
        let nonce = Nonce::from(nonce);
        let sealed = derive(&self.key, None, associated_data)
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

    /// The data key in `wrapped`, sealed by this KEK with the same
    /// `associated_data`, or [`Error::Corrupt`] when the length, the KEK,
    /// the associated data or the bytes do not match.
    pub fn unwrap(&self, wrapped: &[u8], associated_data: &[u8]) -> Result<Key, Error> {
        if wrapped.len() != WRAPPED_LEN {
            return Err(Error::Corrupt);
        }
        let (nonce, sealed) = wrapped.split_at(NONCE_LEN);
        let nonce =
            Nonce::<aes_gcm::aead::consts::U12>::try_from(nonce).map_err(|_| Error::Corrupt)?;
        let bytes = Zeroizing::new(
            derive(&self.key, None, associated_data)
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

    /// The format, pinned: KEK `[1; 32]`, data key `[5; 32]`, associated
    /// data `blob-1`, nonce `[9; 12]`. A change here is a change to every
    /// wrapped key in every database.
    #[test]
    fn known_answer() {
        let kek = Kek::new(&Key::from_bytes([1u8; KEY_LEN]));
        let dk = Key::from_bytes([5u8; KEY_LEN]);
        let wrapped = kek.wrap_with_nonce(&dk, b"blob-1", [9u8; NONCE_LEN]);
        assert_eq!(hex::encode(&wrapped), WRAPPED_HEX);
        assert_eq!(
            kek.unwrap(&hex::decode(WRAPPED_HEX).unwrap(), b"blob-1")
                .unwrap(),
            dk
        );
    }

    const WRAPPED_HEX: &str = "0909090909090909090909099196443e9a28b107b5d8d0432caf6b60c29537af166f503a4364b12e8df12e7a14b28265494ece0b1b8359c236547205";
}
