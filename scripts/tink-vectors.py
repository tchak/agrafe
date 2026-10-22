#!/usr/bin/env python3
"""The Tink side of the streaming AEAD known-answer tests in
`crates/agrafe-crypto/src/streaming.rs`: key `[7; 32]`, associated data
`aad`, 128-byte segments, HKDF-SHA256, a 32-byte derived key, plaintext
byte `i` being `i * 7 % 256`.

    python3 -m venv .venv && .venv/bin/pip install tink
    .venv/bin/python -I scripts/tink-vectors.py encrypt 0 184 300

prints one `<len> <hex>` line per plaintext length, for `TINK_VECTORS`.
The other direction, Tink reading what agrafe wrote:

    AGRAFE_CRYPTO_VECTOR_LEN=184 AGRAFE_CRYPTO_VECTOR_OUT=/tmp/v.bin \
      cargo test -p agrafe-crypto --lib -- --ignored write_vector_for_tink
    .venv/bin/python -I scripts/tink-vectors.py decrypt 184 /tmp/v.bin
"""

import io
import sys

from tink import cleartext_keyset_handle, streaming_aead
from tink.proto import aes_gcm_hkdf_streaming_pb2, common_pb2, tink_pb2

streaming_aead.register()
key = aes_gcm_hkdf_streaming_pb2.AesGcmHkdfStreamingKey()
key.version = 0
key.key_value = bytes([7] * 32)
key.params.ciphertext_segment_size = 128
key.params.derived_key_size = 32
key.params.hkdf_hash_type = common_pb2.SHA256
keyset = tink_pb2.Keyset()
k = keyset.key.add()
k.key_data.type_url = "type.googleapis.com/google.crypto.tink.AesGcmHkdfStreamingKey"
k.key_data.value = key.SerializeToString()
k.key_data.key_material_type = tink_pb2.KeyData.SYMMETRIC
k.status = tink_pb2.ENABLED
k.key_id = 1
k.output_prefix_type = tink_pb2.RAW
keyset.primary_key_id = 1
primitive = cleartext_keyset_handle.from_keyset(keyset).primitive(
    streaming_aead.StreamingAead
)


def plaintext(n):
    return bytes((i * 7) % 256 for i in range(n))


class Kept(io.BytesIO):
    """A buffer the encrypting stream's close leaves readable."""

    def close(self):
        pass


if sys.argv[1] == "encrypt":
    for n in map(int, sys.argv[2:]):
        out = Kept()
        with primitive.new_encrypting_stream(out, b"aad") as stream:
            stream.write(plaintext(n))
        print(n, out.getvalue().hex())
elif sys.argv[1] == "decrypt":
    n, path = int(sys.argv[2]), sys.argv[3]
    with open(path, "rb") as f:
        ciphertext = f.read()
    with primitive.new_decrypting_stream(io.BytesIO(ciphertext), b"aad") as stream:
        got = stream.read()
    assert got == plaintext(n), f"got {len(got)} bytes, not the {n} expected"
    print(f"tink decrypts {n} OK")
else:
    sys.exit(__doc__)
