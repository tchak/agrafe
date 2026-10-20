//! Tink `AES-GCM-HKDF-STREAMING` with a 32-byte key, HKDF-SHA256 and a
//! 32-byte derived key.
//!
//! Wire format:
//!
//! ```text
//! header   = header_len (1 byte, = 40) || salt (32 bytes) || nonce_prefix (7 bytes)
//! stream   = header || segment_0 || segment_1 || ... || segment_{n-1}
//! segment_i = AES-256-GCM(derived_key, nonce_i, plaintext_i), tag appended
//! nonce_i  = nonce_prefix || i (u32 big endian) || last (1 byte, 0 or 1)
//! derived_key = HKDF-SHA256(ikm = key, salt = salt, info = associated_data)
//! ```
//!
//! Every ciphertext segment is `segment_size` bytes, except that the first one
//! is `segment_size - header_len` bytes (so `header || segment_0` is exactly
//! `segment_size`) and the last one holds whatever remains. The last segment
//! is sealed with `last = 1`, which is what prevents truncation; the counter in
//! the nonce prevents reordering, and the random salt and prefix prevent
//! reuse across streams. A stream is split so that a full final segment stays
//! full: no empty trailing segment is produced unless the plaintext is empty.
//!
//! A [`Layout`] turns plaintext offsets into segment indices and segment
//! indices into ciphertext byte ranges, so a byte range of plaintext can be
//! served from one contiguous ciphertext read.

use std::ops::Range;

use aes_gcm::aead::inout::InOutBuf;
use aes_gcm::aead::{Aead, AeadInOut};
use aes_gcm::{Aes256Gcm, KeyInit, Nonce, Tag};
use hkdf::Hkdf;
use sha2::Sha256;
use zeroize::Zeroizing;

use crate::{Error, KEY_LEN, Key};

/// Length of the HKDF salt in the header, the size of the derived key as
/// in Tink.
pub const SALT_LEN: usize = KEY_LEN;
/// Length of the random nonce prefix in the header. With the 4-byte
/// segment counter and the 1-byte last flag it makes the 12-byte AES-GCM
/// nonce.
pub const NONCE_PREFIX_LEN: usize = 7;
/// Length of the stream header: the length byte, the salt and the nonce
/// prefix.
pub const HEADER_LEN: usize = 1 + SALT_LEN + NONCE_PREFIX_LEN;
/// Length of the AES-GCM tag appended to every segment.
pub const TAG_LEN: usize = 16;
/// The first segment must be able to hold at least one plaintext byte.
pub const MIN_SEGMENT_SIZE: usize = HEADER_LEN + TAG_LEN + 1;
/// Segment sizes are stored in a signed 32-bit column.
pub const MAX_SEGMENT_SIZE: usize = i32::MAX as usize;

/// The 40-byte stream header: salt and nonce prefix.
#[derive(Clone, PartialEq, Eq)]
pub struct Header([u8; HEADER_LEN]);

impl Header {
    fn new(salt: [u8; SALT_LEN], nonce_prefix: [u8; NONCE_PREFIX_LEN]) -> Self {
        let mut bytes = [0u8; HEADER_LEN];
        bytes[0] = HEADER_LEN as u8;
        bytes[1..1 + SALT_LEN].copy_from_slice(&salt);
        bytes[1 + SALT_LEN..].copy_from_slice(&nonce_prefix);
        Self(bytes)
    }

    /// The header in `bytes`, which must be exactly [`HEADER_LEN`] long and
    /// start with that length, or [`Error::InvalidHeader`]. The salt and
    /// prefix are not checked: a wrong one fails authentication later.
    pub fn parse(bytes: &[u8]) -> Result<Self, Error> {
        let bytes: [u8; HEADER_LEN] = bytes.try_into().map_err(|_| Error::InvalidHeader)?;
        if bytes[0] as usize != HEADER_LEN {
            return Err(Error::InvalidHeader);
        }
        Ok(Self(bytes))
    }

    /// The header as it starts the stream.
    pub fn as_bytes(&self) -> &[u8; HEADER_LEN] {
        &self.0
    }

    /// The HKDF salt.
    pub fn salt(&self) -> &[u8] {
        &self.0[1..1 + SALT_LEN]
    }

    /// The nonce prefix shared by every segment of the stream.
    pub fn nonce_prefix(&self) -> [u8; NONCE_PREFIX_LEN] {
        self.0[1 + SALT_LEN..].try_into().expect("fixed width")
    }
}

impl std::fmt::Debug for Header {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Header(…)")
    }
}

/// Segment arithmetic for a given ciphertext segment size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Layout {
    segment_size: usize,
}

impl Layout {
    /// The layout of ciphertext segments of `segment_size` bytes, tag
    /// included, or [`Error::InvalidSegmentSize`] outside
    /// [`MIN_SEGMENT_SIZE`]`..=`[`MAX_SEGMENT_SIZE`].
    pub fn new(segment_size: usize) -> Result<Self, Error> {
        if !(MIN_SEGMENT_SIZE..=MAX_SEGMENT_SIZE).contains(&segment_size) {
            return Err(Error::InvalidSegmentSize);
        }
        Ok(Self { segment_size })
    }

    /// Ciphertext bytes per full segment, tag included.
    pub fn segment_size(&self) -> usize {
        self.segment_size
    }

    /// Plaintext bytes in a full segment other than the first.
    pub fn plaintext_per_segment(&self) -> usize {
        self.segment_size - TAG_LEN
    }

    /// Plaintext bytes in the first segment, which also carries the header.
    pub fn first_segment_plaintext(&self) -> usize {
        self.plaintext_per_segment() - HEADER_LEN
    }

    fn pps(&self) -> u64 {
        self.plaintext_per_segment() as u64
    }

    /// Number of segments a plaintext of `plaintext_len` bytes produces.
    /// At least one, since the header occupies the first.
    pub fn segment_count(&self, plaintext_len: u64) -> u64 {
        (plaintext_len + HEADER_LEN as u64).div_ceil(self.pps())
    }

    /// Total ciphertext length, header included.
    pub fn ciphertext_len(&self, plaintext_len: u64) -> u64 {
        let shifted = plaintext_len + HEADER_LEN as u64;
        let full = shifted / self.pps();
        let rem = shifted % self.pps();
        let mut len = full * self.segment_size as u64;
        if rem > 0 {
            len += rem + TAG_LEN as u64;
        }
        len
    }

    /// The plaintext length of a ciphertext of `ciphertext_len` bytes,
    /// header included: the inverse of [`Layout::ciphertext_len`]. An
    /// `Err` means no plaintext produces that length, so the object is
    /// truncated or not a stream of this layout.
    pub fn plaintext_len(&self, ciphertext_len: u64) -> Result<u64, Error> {
        let full = ciphertext_len / self.segment_size as u64;
        let rem = ciphertext_len % self.segment_size as u64;
        let shifted = if rem == 0 {
            full * self.pps()
        } else if rem > TAG_LEN as u64 {
            full * self.pps() + rem - TAG_LEN as u64
        } else {
            return Err(Error::Corrupt);
        };
        shifted.checked_sub(HEADER_LEN as u64).ok_or(Error::Corrupt)
    }

    /// The segment holding plaintext byte `offset`.
    pub fn segment_of(&self, offset: u64) -> u64 {
        (offset + HEADER_LEN as u64) / self.pps()
    }

    /// Plaintext offset at which `segment` starts.
    pub fn plaintext_start(&self, segment: u64) -> u64 {
        if segment == 0 {
            0
        } else {
            segment * self.pps() - HEADER_LEN as u64
        }
    }

    /// Byte range of sealed `segment` inside the ciphertext (header excluded).
    pub fn ciphertext_range(&self, segment: u64, plaintext_len: u64) -> Range<u64> {
        let total = self.ciphertext_len(plaintext_len);
        let start = if segment == 0 {
            HEADER_LEN as u64
        } else {
            segment * self.segment_size as u64
        };
        let end = ((segment + 1) * self.segment_size as u64).min(total);
        start..end
    }

    /// Byte range of the ciphertext covering plaintext `range`, header
    /// excluded, and the index of the first segment in it.
    pub fn ciphertext_span(&self, range: Range<u64>, plaintext_len: u64) -> (u64, Range<u64>) {
        debug_assert!(range.start < range.end && range.end <= plaintext_len);
        let first = self.segment_of(range.start);
        let last = self.segment_of(range.end - 1);
        let start = self.ciphertext_range(first, plaintext_len).start;
        let end = self.ciphertext_range(last, plaintext_len).end;
        (first, start..end)
    }

    /// Whether `segment` is the last one of a plaintext of `plaintext_len`.
    pub fn is_last(&self, segment: u64, plaintext_len: u64) -> bool {
        segment + 1 == self.segment_count(plaintext_len)
    }
}

fn derive(key: &Key, salt: &[u8], associated_data: &[u8]) -> Aes256Gcm {
    let hk = Hkdf::<Sha256>::new(Some(salt), key.as_bytes());
    let mut derived = Zeroizing::new([0u8; KEY_LEN]);
    hk.expand(associated_data, &mut *derived)
        .expect("32 bytes is a valid HKDF output length");
    Aes256Gcm::new((&*derived).into())
}

fn nonce(
    prefix: &[u8; NONCE_PREFIX_LEN],
    segment: u32,
    last: bool,
) -> Nonce<aes_gcm::aead::consts::U12> {
    let mut n = [0u8; 12];
    n[..NONCE_PREFIX_LEN].copy_from_slice(prefix);
    n[NONCE_PREFIX_LEN..11].copy_from_slice(&segment.to_be_bytes());
    n[11] = last as u8;
    n.into()
}

/// Incremental encryption. Feed plaintext with [`Encryptor::update`], which
/// emits every segment it can seal, then [`Encryptor::finish`] for the last.
pub struct Encryptor {
    cipher: Aes256Gcm,
    header: Header,
    nonce_prefix: [u8; NONCE_PREFIX_LEN],
    layout: Layout,
    buffer: Vec<u8>,
    segment: u32,
}

impl Encryptor {
    /// Start a stream with a random salt and nonce prefix.
    pub fn new(key: &Key, associated_data: &[u8], layout: Layout) -> Self {
        let mut salt = [0u8; SALT_LEN];
        rand::fill(&mut salt);
        let mut prefix = [0u8; NONCE_PREFIX_LEN];
        rand::fill(&mut prefix);
        Self::with_params(key, associated_data, layout, salt, prefix)
    }

    /// Start a stream with explicit parameters. Reusing a (salt, prefix) pair
    /// under one key breaks the security of the mode; only tests should pick
    /// them.
    pub fn with_params(
        key: &Key,
        associated_data: &[u8],
        layout: Layout,
        salt: [u8; SALT_LEN],
        nonce_prefix: [u8; NONCE_PREFIX_LEN],
    ) -> Self {
        Self {
            cipher: derive(key, &salt, associated_data),
            header: Header::new(salt, nonce_prefix),
            nonce_prefix,
            layout,
            buffer: Vec::with_capacity(layout.plaintext_per_segment()),
            segment: 0,
        }
    }

    /// The header. Write it before the first segment.
    pub fn header(&self) -> &Header {
        &self.header
    }

    fn capacity(&self) -> usize {
        if self.segment == 0 {
            self.layout.first_segment_plaintext()
        } else {
            self.layout.plaintext_per_segment()
        }
    }

    fn seal(&mut self, last: bool, out: &mut Vec<u8>) -> Result<(), Error> {
        let nonce = nonce(&self.nonce_prefix, self.segment, last);
        let sealed = self
            .cipher
            .encrypt(&nonce, self.buffer.as_slice())
            .map_err(|_| Error::Corrupt)?;
        out.extend_from_slice(&sealed);
        self.buffer.clear();
        if !last {
            self.segment = self.segment.checked_add(1).ok_or(Error::TooManySegments)?;
        }
        Ok(())
    }

    /// Absorb `plaintext`, appending every segment that became sealable to
    /// `out`. A full buffer is only sealed once more plaintext arrives, so the
    /// final segment is always sealed by [`Encryptor::finish`].
    pub fn update(&mut self, mut plaintext: &[u8], out: &mut Vec<u8>) -> Result<(), Error> {
        while !plaintext.is_empty() {
            let room = self.capacity() - self.buffer.len();
            if room == 0 {
                self.seal(false, out)?;
                continue;
            }
            let n = room.min(plaintext.len());
            self.buffer.extend_from_slice(&plaintext[..n]);
            plaintext = &plaintext[n..];
        }
        Ok(())
    }

    /// Seal the last segment, appending it to `out`.
    pub fn finish(mut self, out: &mut Vec<u8>) -> Result<(), Error> {
        self.seal(true, out)
    }
}

/// Decryption of individual segments, for sequential or random access.
pub struct Decryptor {
    cipher: Aes256Gcm,
    nonce_prefix: [u8; NONCE_PREFIX_LEN],
    layout: Layout,
}

impl Decryptor {
    /// A decryptor for the stream `header` starts, under `key` and the
    /// associated data it was encrypted with. Nothing is checked here: a
    /// wrong key or associated data fails on the first segment opened.
    pub fn new(key: &Key, associated_data: &[u8], header: &Header, layout: Layout) -> Self {
        Self {
            cipher: derive(key, header.salt(), associated_data),
            nonce_prefix: header.nonce_prefix(),
            layout,
        }
    }

    /// The layout the segments are cut to.
    pub fn layout(&self) -> Layout {
        self.layout
    }

    /// Open sealed segment `index` where it lies: the plaintext takes the
    /// front of `sealed`, and its length comes back. `last` must say
    /// whether it is the final segment of the stream; a mismatch fails
    /// authentication, and the buffer is then not to be read.
    pub fn open_in_place(&self, index: u64, sealed: &mut [u8], last: bool) -> Result<usize, Error> {
        let index = u32::try_from(index).map_err(|_| Error::TooManySegments)?;
        let max = if index == 0 {
            self.layout.segment_size() - HEADER_LEN
        } else {
            self.layout.segment_size()
        };
        if sealed.len() < TAG_LEN || sealed.len() > max {
            return Err(Error::Corrupt);
        }
        let nonce = nonce(&self.nonce_prefix, index, last);
        let len = sealed.len() - TAG_LEN;
        let (msg, tag) = sealed.split_at_mut(len);
        let tag = Tag::try_from(&*tag).map_err(|_| Error::Corrupt)?;
        self.cipher
            .decrypt_inout_detached(&nonce, b"", InOutBuf::from(msg), &tag)
            .map_err(|_| Error::Corrupt)?;
        Ok(len)
    }

    /// [`Decryptor::open_in_place`] into a fresh buffer.
    pub fn open(&self, index: u64, sealed: &[u8], last: bool) -> Result<Vec<u8>, Error> {
        let mut buffer = sealed.to_vec();
        let len = self.open_in_place(index, &mut buffer, last)?;
        buffer.truncate(len);
        Ok(buffer)
    }
}

/// Encrypt a whole buffer: header followed by every segment.
pub fn encrypt(
    key: &Key,
    associated_data: &[u8],
    layout: Layout,
    plaintext: &[u8],
) -> Result<Vec<u8>, Error> {
    let mut enc = Encryptor::new(key, associated_data, layout);
    let mut out = Vec::with_capacity(HEADER_LEN + plaintext.len() + TAG_LEN);
    out.extend_from_slice(enc.header().as_bytes());
    enc.update(plaintext, &mut out)?;
    enc.finish(&mut out)?;
    Ok(out)
}

/// Decrypt a whole buffer produced by [`encrypt`] or by any Tink
/// implementation with the same parameters, where it lies: the plaintext
/// takes the front of `buffer`, and its length comes back. On an error
/// the buffer is not to be read.
pub fn decrypt_in_place(
    key: &Key,
    associated_data: &[u8],
    layout: Layout,
    buffer: &mut [u8],
) -> Result<usize, Error> {
    if buffer.len() < HEADER_LEN + TAG_LEN {
        return Err(Error::Corrupt);
    }
    let header = Header::parse(&buffer[..HEADER_LEN])?;
    let dec = Decryptor::new(key, associated_data, &header, layout);
    // Each segment is opened where it is, then its plaintext moved down
    // over the header and the tags before it.
    let mut read = HEADER_LEN;
    let mut written = 0;
    let mut index = 0u64;
    loop {
        let full = if index == 0 {
            layout.segment_size() - HEADER_LEN
        } else {
            layout.segment_size()
        };
        // A segment is the last one when nothing follows it, which includes
        // a final segment that happens to be full.
        let rest = buffer.len() - read;
        let last = rest <= full;
        let take = rest.min(full);
        let len = dec.open_in_place(index, &mut buffer[read..read + take], last)?;
        buffer.copy_within(read..read + len, written);
        written += len;
        read += take;
        if last {
            return Ok(written);
        }
        index += 1;
    }
}

/// [`decrypt_in_place`] into a fresh buffer.
pub fn decrypt(
    key: &Key,
    associated_data: &[u8],
    layout: Layout,
    ciphertext: &[u8],
) -> Result<Vec<u8>, Error> {
    let mut buffer = ciphertext.to_vec();
    let len = decrypt_in_place(key, associated_data, layout, &mut buffer)?;
    buffer.truncate(len);
    Ok(buffer)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> Key {
        Key::from_bytes([7u8; KEY_LEN])
    }

    fn layout() -> Layout {
        Layout::new(128).unwrap()
    }

    #[test]
    fn plaintext_len_inverts_ciphertext_len() {
        let l = Layout::new(128).unwrap();
        for len in (0..600).chain([4000, 1 << 20, (1 << 32) + 7]) {
            assert_eq!(l.plaintext_len(l.ciphertext_len(len)), Ok(len), "{len}");
        }
        // Below the header plus one tag, inside a tag, or one byte past a
        // full segment: no plaintext produces these.
        for bad in [
            0,
            HEADER_LEN as u64,
            HEADER_LEN as u64 + TAG_LEN as u64 - 1,
            129,
            140,
        ] {
            assert_eq!(l.plaintext_len(bad), Err(Error::Corrupt), "{bad}");
        }
        assert_eq!(l.plaintext_len(HEADER_LEN as u64 + TAG_LEN as u64), Ok(0));
    }

    #[test]
    fn layout_arithmetic() {
        let l = Layout::new(128).unwrap();
        assert_eq!(l.plaintext_per_segment(), 112);
        assert_eq!(l.first_segment_plaintext(), 72);

        assert_eq!(l.segment_count(0), 1);
        assert_eq!(l.ciphertext_len(0), HEADER_LEN as u64 + 16);
        assert_eq!(l.segment_count(72), 1);
        assert_eq!(l.ciphertext_len(72), 128);
        assert_eq!(l.segment_count(73), 2);
        assert_eq!(l.ciphertext_len(73), 128 + 1 + 16);
        assert_eq!(l.segment_count(72 + 112), 2);
        assert_eq!(l.ciphertext_len(72 + 112), 256);

        assert_eq!(l.segment_of(0), 0);
        assert_eq!(l.segment_of(71), 0);
        assert_eq!(l.segment_of(72), 1);
        assert_eq!(l.segment_of(72 + 112 - 1), 1);
        assert_eq!(l.segment_of(72 + 112), 2);
        assert_eq!(l.plaintext_start(0), 0);
        assert_eq!(l.plaintext_start(1), 72);
        assert_eq!(l.plaintext_start(2), 184);

        let total = 72 + 112 + 5;
        assert_eq!(l.ciphertext_range(0, total), 40..128);
        assert_eq!(l.ciphertext_range(1, total), 128..256);
        assert_eq!(l.ciphertext_range(2, total), 256..256 + 5 + 16);
        assert!(l.is_last(2, total));
        assert!(!l.is_last(1, total));
        assert_eq!(l.ciphertext_span(10..20, total), (0, 40..128));
        assert_eq!(l.ciphertext_span(70..100, total), (0, 40..256));
        assert_eq!(l.ciphertext_span(184..189, total), (2, 256..277));
    }

    /// Produced by Tink's Python implementation (`tink` 1.x) with key
    /// `[7; 32]`, associated data `aad`, a 128-byte ciphertext segment size,
    /// HKDF-SHA256 and a 32-byte derived key. Decrypting it proves the header
    /// layout, key derivation, nonce construction and segment split match
    /// the reference implementation.
    const TINK_PLAINTEXT_LEN: usize = 300;
    const TINK_CIPHERTEXT: &str = "2812348b599d535bbbab20455c71e2e288ce44c2ec3e4a627a40bd5dcfacc32ed0b58d853472a7165ee495a6f76b46b1e61e144b4c13cf82213dcc5f57ba6a56dee5684b57a7021e8e625b1643a207f879c89fc1c5921770eedfd52274dd8f7ad62d7cbf20d7e34a885afeb62b74e9e03913c49e816e2d5a524d1fee8f6b8bab71ba14c2a55c5f2d3327ba7c214dddcf2f7b0b682515b51f1a113a87a0372b66c5cbdb10c49db9e1abd8a5d7fb5a2ea3c26566edf3bd29a22e13c80384b4887a9f124b3382a47644a54e21f01ed2500aada1ec8a32cc4d4907783f31d6ba91a5d9cc30a699aca8f72d81772dc2a66b61941141de6b161e82c03e60a62d424f06bdbb3ea163f7468868d29565eac44ef85175ac8a5e3e50f5194df236fd22d4179b29dc7b7104bf6123a0f04e8a3f998176ebf46de5e00ffab8a9738ee2b6ae5da1ffe0b28a7659ae0c70ad1e4390364b6d93810c3d76feaf20ed2fd8c7629a6545ec62bc4f9ac7c7ed741911a585d4b61fe69daac5fc06b6460e9d8ed3854cda7ad3fab9833c17cb51cd51d1cd54e38370d48a61";

    fn tink_plaintext() -> Vec<u8> {
        (0..TINK_PLAINTEXT_LEN)
            .map(|i| (i * 7 % 256) as u8)
            .collect()
    }

    #[test]
    fn decrypts_tink_reference_ciphertext() {
        let ct = hex::decode(TINK_CIPHERTEXT).unwrap();
        let l = layout();
        assert_eq!(ct.len() as u64, l.ciphertext_len(TINK_PLAINTEXT_LEN as u64));
        assert_eq!(decrypt(&key(), b"aad", l, &ct).unwrap(), tink_plaintext());
    }

    /// Writes a ciphertext with the reference parameters to the path in
    /// `AGRAFE_CRYPTO_VECTOR_OUT`, for checking that Tink can decrypt it.
    #[test]
    #[ignore]
    fn write_vector_for_tink() {
        let path = std::env::var("AGRAFE_CRYPTO_VECTOR_OUT").unwrap();
        let ct = encrypt(&key(), b"aad", layout(), &tink_plaintext()).unwrap();
        std::fs::write(path, ct).unwrap();
    }

    #[test]
    fn round_trips_every_boundary() {
        let l = layout();
        for len in [0usize, 1, 71, 72, 73, 183, 184, 185, 300, 1000] {
            let pt: Vec<u8> = (0..len).map(|i| i as u8).collect();
            let ct = encrypt(&key(), b"aad", l, &pt).unwrap();
            assert_eq!(ct.len() as u64, l.ciphertext_len(len as u64), "len {len}");
            assert_eq!(decrypt(&key(), b"aad", l, &ct).unwrap(), pt, "len {len}");
        }
    }

    #[test]
    fn in_place_compacts_the_plaintext_to_the_front() {
        let l = layout();
        let pt: Vec<u8> = (0..300u32).map(|i| (i % 251) as u8).collect();
        let mut buffer = encrypt(&key(), b"aad", l, &pt).unwrap();
        let len = decrypt_in_place(&key(), b"aad", l, &mut buffer).unwrap();
        assert_eq!(len, pt.len());
        assert_eq!(&buffer[..len], &pt[..]);
        // The last segment can also be opened on its own, where it lies.
        let mut sealed = encrypt(&key(), b"aad", l, &pt).unwrap();
        let header = Header::parse(&sealed[..HEADER_LEN]).unwrap();
        let dec = Decryptor::new(&key(), b"aad", &header, l);
        let last = l.segment_count(pt.len() as u64) - 1;
        let range = l.ciphertext_range(last, pt.len() as u64);
        let range = range.start as usize..range.end as usize;
        let len = dec
            .open_in_place(last, &mut sealed[range.clone()], true)
            .unwrap();
        let start = l.plaintext_start(last) as usize;
        assert_eq!(&sealed[range.start..range.start + len], &pt[start..]);
    }

    #[test]
    fn streaming_is_chunking_independent() {
        let l = layout();
        let pt: Vec<u8> = (0..1000u32).map(|i| (i % 251) as u8).collect();
        let salt = [1u8; SALT_LEN];
        let prefix = [2u8; NONCE_PREFIX_LEN];
        let mut expected = Vec::new();
        let mut enc = Encryptor::with_params(&key(), b"x", l, salt, prefix);
        expected.extend_from_slice(enc.header().as_bytes());
        enc.update(&pt, &mut expected).unwrap();
        enc.finish(&mut expected).unwrap();

        for chunk in [1usize, 7, 72, 112, 113, 999] {
            let mut out = Vec::new();
            let mut enc = Encryptor::with_params(&key(), b"x", l, salt, prefix);
            out.extend_from_slice(enc.header().as_bytes());
            for piece in pt.chunks(chunk) {
                enc.update(piece, &mut out).unwrap();
            }
            enc.finish(&mut out).unwrap();
            assert_eq!(out, expected, "chunk {chunk}");
        }
    }

    #[test]
    fn random_access_matches_sequential() {
        let l = layout();
        let pt: Vec<u8> = (0..500u32).map(|i| (i * 7 % 256) as u8).collect();
        let ct = encrypt(&key(), b"aad", l, &pt).unwrap();
        let header = Header::parse(&ct[..HEADER_LEN]).unwrap();
        let dec = Decryptor::new(&key(), b"aad", &header, l);
        let total = pt.len() as u64;
        for range in [0..1u64, 0..72, 71..73, 100..300, 184..500, 499..500, 0..500] {
            let (first, span) = l.ciphertext_span(range.clone(), total);
            let bytes = &ct[span.start as usize..span.end as usize];
            let mut got = Vec::new();
            let mut index = first;
            let mut rest = bytes;
            while !rest.is_empty() {
                let r = l.ciphertext_range(index, total);
                let n = (r.end - r.start) as usize;
                got.extend_from_slice(
                    &dec.open(index, &rest[..n], l.is_last(index, total))
                        .unwrap(),
                );
                rest = &rest[n..];
                index += 1;
            }
            let skip = (range.start - l.plaintext_start(first)) as usize;
            let want = &pt[range.start as usize..range.end as usize];
            assert_eq!(&got[skip..skip + want.len()], want, "{range:?}");
        }
    }

    #[test]
    fn tampering_is_detected() {
        let l = layout();
        let pt = vec![9u8; 300];
        let ct = encrypt(&key(), b"aad", l, &pt).unwrap();

        // Flip a byte in each region: header salt, first segment, last tag.
        for pos in [5, HEADER_LEN + 3, ct.len() - 1] {
            let mut bad = ct.clone();
            bad[pos] ^= 1;
            assert_eq!(
                decrypt(&key(), b"aad", l, &bad),
                Err(Error::Corrupt),
                "pos {pos}"
            );
        }
        // Wrong key, wrong associated data, wrong segment size.
        assert!(decrypt(&Key::from_bytes([8u8; KEY_LEN]), b"aad", l, &ct).is_err());
        assert!(decrypt(&key(), b"other", l, &ct).is_err());
        assert!(decrypt(&key(), b"aad", Layout::new(129).unwrap(), &ct).is_err());
    }

    #[test]
    fn truncation_and_reordering_are_detected() {
        let l = layout();
        let pt = vec![1u8; 72 + 112 + 112 + 5];
        let ct = encrypt(&key(), b"aad", l, &pt).unwrap();

        // Drop the last segment: the previous one is not marked last.
        let cut = 3 * 128;
        assert_eq!(decrypt(&key(), b"aad", l, &ct[..cut]), Err(Error::Corrupt));
        // Drop a few bytes.
        assert_eq!(
            decrypt(&key(), b"aad", l, &ct[..ct.len() - 3]),
            Err(Error::Corrupt)
        );
        // Header only, or less.
        assert!(decrypt(&key(), b"aad", l, &ct[..HEADER_LEN]).is_err());
        assert!(decrypt(&key(), b"aad", l, &ct[..10]).is_err());

        // Swap segments 1 and 2.
        let mut swapped = ct.clone();
        let (a, b) = (128..256, 256..384);
        let seg1 = ct[a.clone()].to_vec();
        swapped[a].copy_from_slice(&ct[b.clone()]);
        swapped[b].copy_from_slice(&seg1);
        assert_eq!(decrypt(&key(), b"aad", l, &swapped), Err(Error::Corrupt));

        // Extra bytes after the last segment.
        let mut longer = ct.clone();
        longer.push(0);
        assert_eq!(decrypt(&key(), b"aad", l, &longer), Err(Error::Corrupt));
    }

    #[test]
    fn header_validation() {
        assert!(Header::parse(&[0u8; HEADER_LEN]).is_err());
        assert!(Header::parse(&[40u8; HEADER_LEN - 1]).is_err());
        assert!(Header::parse(&[40u8; HEADER_LEN]).is_ok());
        assert!(Layout::new(MIN_SEGMENT_SIZE - 1).is_err());
        assert!(Layout::new(MIN_SEGMENT_SIZE).is_ok());
    }

    #[test]
    fn empty_stream_is_one_tag() {
        let mut out = Vec::new();
        let enc = Encryptor::new(&key(), b"", layout());
        enc.finish(&mut out).unwrap();
        assert_eq!(out.len(), TAG_LEN);
    }
}
