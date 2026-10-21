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

### Keeping the KEKs off the host

A KEK in `agrafe.toml` or in the environment is readable by anyone who can
read the file, the unit file or `/proc/<pid>/environ`, and it travels with
every disk image and backup of the host. Some deployments require that
nothing at rest on the host can decrypt the data. `encryption.keks_file`
is for them: a path agrafe reads exactly once, at startup, holding
`id = "…"` lines like the `[encryption.keks]` table. What sits at that path
decides how strict the deployment is; agrafe does not know or care.

- **A regular file** is no stronger than the inline table. Fine for
  development.
- **A systemd credential** keeps the KEKs on a tmpfs only the service can
  see, decrypted at start from a blob sealed to the machine's TPM, so no
  human is needed at boot and a copy of the disk is useless elsewhere:

  ```sh
  printf 'kek1 = "%s"\n' "$(openssl rand -base64 32)" \
    | systemd-creds encrypt --name=keks --with-key=tpm2 - /etc/agrafe/keks.cred
  ```

  ```ini
  [Service]
  LoadCredentialEncrypted=keks:/etc/agrafe/keks.cred
  Environment=AGRAFE_ENCRYPTION__KEKS_FILE=%d/keks
  ```

- **A FIFO fed by a human** is the strictest: the KEKs exist only in the
  memory of the running process and in a password manager. Every start
  waits for someone to type them, and a stolen disk or backup holds
  nothing. agrafe opens the FIFO for reading and blocks until the writer
  closes it, so the FIFO must exist and belong to the service's user
  before the service starts, and the service starts before the writer:

  ```sh
  mkfifo -m 0600 /run/agrafe/keks && chown agrafe: /run/agrafe/keks
  systemctl start agrafe        # returns at once; agrafe waits in the read
  systemd-ask-password 'KEK for agrafe (id = "…")' > /run/agrafe/keks
  rm -f /run/agrafe/keks
  ```

  with `AGRAFE_ENCRYPTION__KEKS_FILE=/run/agrafe/keks` and
  `RuntimeDirectory=agrafe` in the unit. `systemd-ask-password` returns
  one line and the file wants one `id = "…"` per line, so one KEK fits
  per prompt. For several, prompt once per KEK inside one redirection,
  `{ systemd-ask-password …; systemd-ask-password …; } > /run/agrafe/keks`,
  or type them into `cat > /run/agrafe/keks` and end with Ctrl-D.

  The redirection itself blocks until agrafe opens the other end. If the
  prompt never appears, agrafe has not reached the read: it connects to
  the database first and exits before opening the FIFO when that fails,
  or it could not open the FIFO at all. Interrupt with Ctrl-C and read
  `journalctl -u agrafe`. A mistyped line fails the start with its line
  number, never its content, which would be a key in the journal; just
  start again.

Whichever rung, add `LimitCORE=0` to the unit so a crash does not write
the KEKs to a core file. None of this keeps the KEKs out of the process's
memory while it runs: a running agrafe must hold them to wrap and unwrap
data keys. Only an external key service, which agrafe does not support
yet, would do that.

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
server process. `vips`, `agrafe-extract`, `agrafe-render-text`,
`agrafe-render-document` and `agrafe-render-geo` run as subprocesses with
a clean environment, memory, CPU, output size and wall-clock limits. The
file parsers they use, `lopdf`, `zip`, `quick-xml`, `ab_glyph`, `typst`
and `geojson`, are not even linked into the server. The one network step of a
render, fetching the background of a map from a configured WMS, is the
worker's own and sends that provider the extent of the user's geometry;
no layer is configured by default. See [Previews and thumbnails](representations.md#sandboxing).

On Linux the kernel confines each subprocess as well, with no helper
binary, no user namespaces and nothing to install:

- **Landlock** gives the process a filesystem of the system libraries,
  the fonts and their cache, the file it reads and the one directory it
  writes. The configuration file, with the master key and the database
  URL, the other jobs' work directories and the rest of the disk do not
  exist for it. TCP `bind` and `connect` are refused on kernels 6.7 and
  newer, and on 6.12 and newer the process cannot signal a process
  outside the sandbox, such as the worker itself.
- **seccomp** refuses, with `EPERM`, every socket call (which also closes
  path-based unix sockets such as Postgres's), namespaces, `ptrace`,
  mounts, reaching another process's descriptors or limits, and the
  kernel-facing calls a decoder has no use for: `bpf`, `keyctl`,
  `userfaultfd`, `io_uring_setup`, module loading. It is a
  denylist, not an allowlist, so a glibc or libvips upgrade does not break
  it silently. The one exception is the optional
  [LibreOffice](deployment.md#libreoffice), which cannot start without a
  unix socket in `/tmp`: it may have unix sockets, and only those, and
  Landlock lets it create and remove socket files there. Since Landlock is
  all that keeps a document's links inside the work directory, a worker
  refuses to run the converter without it.

`media.sandbox.isolation` is `"auto"` by default: a development VM, CI and
a default Docker container all get both layers with nothing to configure,
and the worker logs what it got at startup. Production should set
`"required"`, so a kernel downgrade or a seccomp profile that blocks the
`landlock_*` syscalls fails the deployment instead of quietly turning
confinement off. `"required"` checks that both layers install; what
Landlock enforces still depends on the kernel's ABI, which the `sandbox:`
boot line names (`abi 4` adds the TCP refusal, `abi 6` the signal scope). Before any tool runs, the worker checks that the sandbox
refuses a read it was not granted. macOS keeps the limits and the timeout
only.

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
variables rather than the file, and the KEKs through `encryption.keks_file`
where they must not rest on the host at all (see [Keeping the KEKs off the
host](#keeping-the-keks-off-the-host)).
