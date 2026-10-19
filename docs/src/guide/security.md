# Security model

agrafe stores files uploaded by strangers and serves them back from your
origin. This chapter describes what protects what.

## Trust boundaries

| Party | Trusted with |
|---|---|
| The backend, on the private port | Everything. The private port has no authentication |
| An operator's browser, on the private port | Everything, which is why the dashboard has extra checks |
| A browser, on the public port | Exactly what a signed URL grants: one method, one path, one query, until an expiry |
| The object store | Ciphertext only |
| The renderers | One file at a time, inside a sandbox |

The private port must be reachable only from a trusted network.

## Encryption at rest

Every blob gets a random 256-bit data key when its upload starts. The key is
wrapped with a key-encryption key (KEK) from the configuration, with
AES-256-GCM and the blob id as associated data, so a wrapped key copied to
another row does not unwrap. The wrapped key and the KEK's id are stored on
the blob's row.

The bytes are encrypted with a streaming AEAD in [Tink's
AES-GCM-HKDF-STREAMING](https://developers.google.com/tink/streaming-aead/aes_gcm_hkdf_streaming)
format, in segments of `encryption.chunk_size` (256 KiB by default). The
object key is the stream's associated data, so a ciphertext copied to
another key does not decrypt, and the segment nonces prevent reordering,
truncation and swapping segments between blobs. Random access is arithmetic,
which is what makes `Range` requests cheap.

Previews and thumbnails are encrypted with the blob's own data key, each
with a fresh subkey. Server-side encryption on the bucket is not relied on:
the requirement is that the storage provider cannot read the data.

The implementation is a few hundred lines on RustCrypto's `aes-gcm` and
`hkdf`, in its own crate, `agrafe-crypto`, with tamper and truncation tests
and a known-answer test against Tink's Python implementation.

### Rotating KEKs

```toml
[encryption]
current_kek = "kek2"

[encryption.keks]
kek1 = "…"   # still unwraps the existing blobs
kek2 = "…"   # wraps new ones
```

New blobs are wrapped with `current_kek`; each row's `kek_id` picks the KEK
that unwraps it. A new KEK is therefore a configuration change. Retiring an
old one means rewrapping the data keys it wraps, a database update with no
bucket traffic, for which agrafe has no command yet. Keep every KEK that
still wraps a key.

## Virus scanning

Files are scanned by ClamAV through `clamd`'s `INSTREAM` command, inline
during the upload for files up to `upload.inline_scan_max_size` (25 MiB),
and by a job after storage for larger ones. Until the scan passes, the blob
is `uploaded` and nothing can read or render it. A file that cannot be
scanned is never treated as clean.

`clamd`'s own limits are sized for mail. A verdict is only as good as they
are: past `MaxScanSize` or `MaxFileSize`, clamd scans a prefix and answers
`OK`. See [Deployment](deployment.md#clamav) for the settings that must
follow `upload.max_size`.

`skip_virus_scan` is the backend's statement about where bytes come from.
Blobs created with it cannot be uploaded through the public port.

## Sandboxed rendering

Decoders are the most attackable code in the service, so none runs in the
server process. `vips`, `agrafe-extract`, `agrafe-render-text` and
`agrafe-render-document` run as subprocesses with a clean environment,
memory, CPU, output size and wall-clock limits. The file parsers they use,
`lopdf`, `zip`, `quick-xml`, `ab_glyph` and `typst`, are not even linked
into the server. See [Previews and thumbnails](representations.md#sandboxing).

Before vips sees a file, its magic bytes are checked again against the
formats agrafe means to render, so a file declared as a PNG that holds
something else never reaches ImageMagick or another loader.

## Signed URLs

- HMAC-SHA256 over the method, the expiry, the path and the query, compared
  in constant time.
- No permanent URLs: `signing.ttl.max` caps every lifetime, and the public
  port refuses any token that would outlive it, whoever minted it.
- Several keys verify at once, so keys rotate without breaking URLs in
  flight.
- Tokens are redacted from request logs: the public port logs
  `/blobs/<token>/…`.
- Public upload URLs work once, since blobs are write-once.

## Leak-proof public lookups

On the public port, a blob that is deleted, expired or infected answers
`404` on every route, even with a valid signature. A URL that leaks after
the blob is gone reveals nothing, not even that it existed. The private
port keeps reporting the real state.

The infected answer to a public upload does not name the signature, so an
anonymous uploader cannot learn what to evade.

## Serving user content safely

Originals carry `X-Content-Type-Options: nosniff` and
`Content-Security-Policy: sandbox`, so an uploaded HTML page or SVG never
runs scripts as your origin. Previews and thumbnails are always transcoded
to WebP, so decoder bombs and malformed containers never reach a client, and
all metadata is stripped from images, EXIF location included.

Uploaded filenames cannot contain path separators, control characters or
bidirectional formatting characters, which can disguise an extension
(`invoice‮fdp.exe`).

## Fetches

`url` blobs make agrafe connect where the URL points. Fetches are `https`
only, and refuse loopback, private, link-local and cloud metadata
addresses, checked on the resolved addresses that the connection then uses,
so DNS rebinding cannot slip past. See [Fetching from a
URL](fetch.md#network-policy). Presigned URLs are secrets: they appear in
private metadata until the fetch completes, and never in errors or logs.

## Browsers on the private port

The dashboard puts a browser on the private port, which breaks the "trusted
network" assumption in two ways, and agrafe guards against both.

- **Cross-site requests.** A page the operator visits could submit a form
  to the private port. Every unsafe request (anything but `GET`, `HEAD` and
  `OPTIONS`) is refused with `403` when the browser marks it as another
  site's: a `Sec-Fetch-Site` other than `same-origin` or `none`, or, without
  that header, an `Origin` that is not the `Host`. A request with neither
  header comes from a non-browser client, your backend, and goes through.
- **DNS rebinding.** A page can re-point its own name at the private
  address, then read the private API as same-origin. Set
  `server.private_hosts` to the names and ports the private port is reached
  under; every other `Host` is refused with `403`, except on `/healthz` and
  `/metrics`.

```toml
[server]
private_hosts = ["agrafe.internal:8080", "127.0.0.1:8080"]
```

agrafe warns at startup when the dashboard is enabled without
`private_hosts`. Neither check is authentication: a dashboard in daily use
belongs behind a proxy that has some.

## Secrets in configuration

The database URL, signing keys, KEKs, bucket credentials and webhook secret
are held in types whose debug output is redacted, so a dumped configuration
cannot leak them. In production, pass them through `AGRAFE_` environment
variables rather than the file.
