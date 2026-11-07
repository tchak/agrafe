# agrafe-crypto

[![crates.io](https://img.shields.io/crates/v/agrafe-crypto.svg)](https://crates.io/crates/agrafe-crypto)
[![docs.rs](https://img.shields.io/docsrs/agrafe-crypto)](https://docs.rs/agrafe-crypto)

The encryption of [agrafe](https://github.com/tchak/agrafe), the file
storage service, in a crate of its own so that it can be reviewed in
isolation:

- `streaming`: a chunked streaming AEAD in Tink's `AES-GCM-HKDF-STREAMING`
  wire format, with random access by segment, so that a `Range` read
  decrypts only the segments it covers;
- `wrap`: envelope encryption of a file's data key under a key-encryption
  key.

The stream is Tink's format, checked against test vectors produced by
Tink's Python implementation: given its data key, any Tink implementation
decrypts what agrafe wrote. It depends on `aes-gcm`, `hkdf` and `sha2`,
and on nothing of agrafe's. See
[Encryption at rest](https://tchak.github.io/agrafe/guide/security.html#encryption-at-rest)
for how agrafe uses it.

## Part of agrafe

A building block of [agrafe](https://github.com/tchak/agrafe), the file
storage service. It can be used on its own, but its API serves agrafe's
and moves with its releases: every agrafe crate is released together, at
one version, and before 1.0 a minor version may break it, as the
[changelog](https://github.com/tchak/agrafe/blob/main/CHANGELOG.md) says.
To run agrafe, see [agrafe-server](https://crates.io/crates/agrafe-server)
and the [documentation](https://tchak.github.io/agrafe/).

## License

Licensed under either of the [Apache License, Version 2.0](https://github.com/tchak/agrafe/blob/main/LICENSE-APACHE)
or the [MIT license](https://github.com/tchak/agrafe/blob/main/LICENSE-MIT),
at your option.
