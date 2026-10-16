# agrafe — design

A file storage service. Rust server, Postgres for metadata, an object store for
bytes. Two HTTP interfaces on two ports: a **private** one for the backend that
owns the files, and a **public** one exposed to the internet where every URL
carries a signature.

This document records the decisions taken so far and the reasoning behind them.
It is the reference when code and intuition disagree.

## Goals

- Store files uploaded by end users (through browsers) or by backends.
- Serve each file as the original plus two derived representations: a WebP
  preview and a WebP thumbnail.
- Encrypt everything at rest with keys the storage provider never sees.
- Scan everything with ClamAV unless the backend explicitly vouches for the
  source.
- Stream on both the write and the read path. Never buffer a whole file in
  memory.
- Generate tabular files (CSV, XLSX, ODS) and ZIP archives server side from a
  streamed description.

## Non-goals (for now)

- Multi-tenancy or per-client authentication on the private port. The private
  port is reachable only from trusted networks.
- Versioned history of mutable blobs. A mutable blob keeps only its current
  version.
- Range requests on previews and thumbnails. They are small and served whole.

## Interfaces

### Private port

No signature. Reachable only from trusted networks.

| Method | Path | Purpose |
|---|---|---|
| `POST` | `/blobs` | Create a blob (metadata only, no bytes yet) |
| `GET` | `/blobs` | List blobs, cursor paginated, filterable |
| `GET` | `/blobs/:id` | Full metadata plus ready-made signed public URLs |
| `PUT` | `/blobs/:id` | Upload bytes (all input types) |
| `DELETE` | `/blobs/:id` | Soft delete, bucket objects purged asynchronously |
| `POST` | `/blobs/:id/urls` | Mint signed public URLs with a chosen TTL and query parameters |
| `GET` | `/blobs/:id/original/:filename` | Original bytes, supports `Range` |
| `GET` | `/blobs/:id/preview/:filename` | WebP preview or 404 |
| `GET` | `/blobs/:id/thumbnail/:filename` | WebP thumbnail, always 200 |
| `GET` | `/healthz` | Liveness |

### Public port

Every route carries a signature token as the first path segment after
`/blobs`. The tail after the token has the same shape as the private routes so
handlers are shared.

| Method | Path | Purpose |
|---|---|---|
| `PUT` | `/blobs/:token/:id` | Upload bytes, only `raw` and `multipart` |
| `GET` | `/blobs/:token/:id` | Reduced metadata: state, scan state, representation states, content type, size. `404` once the blob is `expired`, `infected` or `deleted` |
| `GET` | `/blobs/:token/:id/original/:filename` | Original bytes, supports `Range` |
| `GET` | `/blobs/:token/:id/preview/:filename` | WebP preview or 404 |
| `GET` | `/blobs/:token/:id/thumbnail/:filename` | WebP thumbnail, always 200 |
| `GET` | `/healthz` | Liveness |

Blobs in a terminal state (`expired`, `infected`, `deleted`) do not exist as
far as the public port is concerned: every public route answers `404`, upload
included, even with a valid signature. A URL that leaks after the blob is gone
therefore reveals nothing, not even that it once existed. The private port
keeps reporting the real state.

The `:filename` segment of representation routes must match the blob's
filename (percent-decoded), otherwise `404`. It exists so browsers get a
sensible download name and so a URL cannot be made to look like it serves a
different file than it does.

### Errors

Every error response, on both ports, is JSON with one shape:

```json
{"error": {"code": "unprocessable", "message": "checksum is required for raw and multipart input"}}
```

`code` is stable and machine readable; `message` is for humans and may change.
This includes failures axum would otherwise answer in plain text: malformed
JSON, unknown query values, a non-UUID path segment, unknown routes (`404`)
and wrong methods (`405`). Handlers use wrapper extractors in `api/extract.rs`
that convert those rejections. Internal errors are logged in full and reported
as `internal` with no detail.

## Signed URLs

Token format, one path segment, no slashes:

```
v1.<key_id>.<expires_unix>.<base64url(hmac_sha256(key, message))>
```

```
message = METHOD "\n" expires_unix "\n" path_and_query_after_token
```

- `path_and_query_after_token` is the raw request target as received, starting
  at `/<id>`, including the query string if any. Nothing is decoded,
  reordered or re-encoded. The server is the only minter, so both sides use the
  same encoder.
- `HEAD` is verified as `GET`.
- `expires_unix = 0` means never expires, allowed only when
  `signing.allow_permanent = true`.
- Several keys live in config. One is current for minting, all are valid for
  verification. The key id in the token selects the key. This makes rotation
  possible without invalidating URLs in flight. Ids are restricted to
  `[A-Za-z0-9_-]` so they are safe path segments and cannot contain the token
  separator.
- Minted expiries are rounded: `expires = ceil(now / window) * window + ttl`.
  All URLs minted for the same blob inside one window are identical, so
  browsers and CDNs can cache them. Window and TTL are per operation in config.
- HMAC compared in constant time. Bad signature is `403`, expired is `410`, a
  valid signature on a missing blob is `404`.
- Because everything after the token is signed, query parameters are a safe
  way for the minter to control behaviour: `disposition=attachment`, a
  filename override, `wait=<ms>` for pending representations.

## Blobs

### Creation

`POST /blobs` takes:

| Field | Required | Notes |
|---|---|---|
| `filename` | yes | Stored as given, used in URLs and `Content-Disposition` |
| `content_type` | yes | Normalised to `type/subtype`, classified into a category |
| `input_type` | yes | `raw`, `multipart`, `rows`, `archive` |
| `checksum` | for `raw`, `multipart` | SHA-256 of the bytes, base64 |
| `size` | for `raw`, `multipart` | Declared size in bytes, enforced on upload |
| `write_once` | no, default `true` | Mutable blobs are private-only |
| `skip_virus_scan` | no, default `false` | Private-only, backend vouches for the source |
| `expires_at` or `expires_in` | no | Absolute timestamp or duration such as `7d` |

Checksum is SHA-256 because browsers compute it cheaply with
`crypto.subtle.digest`, and S3 understands it natively.

### Input types

The name describes the shape of the `PUT` body.

| Input type | Body | Produces |
|---|---|---|
| `raw` | The file bytes | The file |
| `multipart` | `multipart/form-data` with one part named `filename` holding a `File` whose name must match the blob filename | The file |
| `rows` | JSONL, one object per line | CSV, XLSX or ODS per the blob content type |
| `archive` | JSONL, one `{"path": "...", "blob_id": "..."}` per line | ZIP containing the referenced blobs |

`rows` and `archive` are **private-only**. An `archive` upload reads other
blobs, so letting a browser drive it would be a confused-deputy path into files
the uploader does not own.

### Public upload rule

```
public_upload_allowed = write_once && !skip_virus_scan && input_type in (raw, multipart)
```

Enforced twice: when minting (the private API refuses to mint a public upload
URL for such a blob) and in the public `PUT` handler. Returned in metadata as a
computed field.

Why: a public `PUT` URL is reusable until it expires, and will leak into
browser histories and proxy logs. On a write-once blob a leaked URL can only
earn a `409` after the first successful upload. Write-once also means
everything the public port serves is immutable and cacheable for its URL
lifetime.

### State machine

```
created ──► uploading ──► uploaded ──► ready
   ▲            │             │
   └────────────┘             └──► infected
   (failed upload, nothing stored)

ready ──► uploading ──► uploaded ──► ready        (mutable blobs only)
             │
             └──► ready
   (failed re-upload, previous object kept)

any non-terminal ──► expired
any ──► deleted
```

| State | Meaning |
|---|---|
| `created` | Row exists, no bytes. `PUT` allowed. |
| `uploading` | A `PUT` is in flight. Concurrent `PUT` gets `409`. Stale rows are reset by the reaper (see below). |
| `uploaded` | Bytes are in the bucket, encrypted. Scan pending. Not readable. |
| `ready` | Scan passed or skipped. Readable. |
| `infected` | Scan failed. Never readable. Object kept for forensics until purge. |
| `expired` | Past `expires_at`. Not readable. Object purged by reaper. |
| `deleted` | Soft deleted. Object purged by reaper. |

Transitions are conditional updates (`UPDATE ... WHERE state = $from`) so two
concurrent requests cannot both win. Write-once is enforced by refusing `PUT`
from any state other than `created`. For mutable blobs, `PUT` is also allowed
from `ready`, must carry a new checksum and size in headers, writes to a new
object key and swaps the pointer on the row. Readers mid-stream keep the old
object; the reaper deletes it.

A failed or stale upload leaves `uploading` for the state it came from, decided
by whether an object exists: `created` when `storage_key` is null, `ready`
otherwise. A mutable blob whose re-upload fails therefore keeps serving its
previous version instead of becoming unreadable. Both the abort path and the
reaper apply this one rule.

Separate from the blob state, each blob tracks `scan_state`
(`pending`, `clean`, `infected`, `skipped`) and a state per derived
representation (`pending`, `ready`, `failed`, `unsupported`).

### Listing

`GET /blobs` is cursor paginated on the blob id, which is a UUID v7 so ids
sort by creation time. Filters: `content_type`, `category`, `state`,
`scan_state`. Response carries `next_cursor` or null.

### Classification

Content type is mapped to a category at creation and re-checked against sniffed
magic bytes during upload. A mismatch between declared and sniffed type is
rejected with `422`. Categories:

`image`, `document`, `spreadsheet`, `presentation`, `archive`, `audio`,
`video`, `text`, `other`.

The mapping table lives in code with a config override. PDF is a `document`.

## Upload pipeline

Bytes flow once through a chain of consumers:

1. Size counter, rejects on exceeding declared size or `upload.max_size`.
2. SHA-256 hasher, compared against the declared checksum at the end.
3. Magic-byte sniffer on the first chunk, compared against the declared
   content type.
4. ClamAV `INSTREAM` when the declared size is at or below
   `upload.inline_scan_max_size`. Above that, the scan runs as a job after
   storage and the blob waits in `uploaded`.
5. Chunked AEAD encryption (see below).
6. Multipart upload to the object store.

A failed check aborts the multipart upload, resets the blob to `created` and
returns `422` (checksum, size, type) or `409` (state). On success the blob
moves to `uploaded`, jobs are enqueued in the same transaction, and the blob
moves to `ready` immediately if the scan was inline or skipped.

## Encryption at rest

age was considered and rejected. Its recipient header exists for file exchange
between humans and buys nothing when the service is the only reader; its key
rotation would require rewriting every object header; and its fixed 64 KiB
chunks and sync-only seek make range requests awkward.

Instead: **envelope encryption with a chunked streaming AEAD.**

- Each blob gets a random 256-bit data key when its upload starts, wrapped
  with a key-encryption key (KEK) from config. The wrapped key and the `kek_id` are
  stored on the blob row. Rotating a KEK is a database update that rewraps keys,
  with zero bucket traffic. Moving KEKs to a KMS later changes nothing in the
  object format.
- Payload follows **Tink's AES-GCM-HKDF-STREAMING** wire format. It is a
  published spec with reference implementations in Go and Java, so our
  implementation can be validated against cross-language test vectors.
- Chunk (segment) size is `encryption.chunk_size`, default 256 KiB. Larger
  chunks mean fewer tags; smaller chunks mean finer range granularity. Stored
  on the row so it can change without migrating objects.
- Per-chunk nonce is `nonce_prefix || counter || last_flag`, which prevents
  reordering, truncation and chunk swapping between blobs.
- Random access is arithmetic: chunk `i` starts at `header + i * (chunk + 16)`.
  A `Range` request maps to a chunk range, one byte-range `GET` to the bucket,
  decrypt, trim, stream. `Content-Length` is exact.

The crypto code is small (a few hundred lines on RustCrypto `aes-gcm` and
`hkdf`) and owned by us. It gets known-answer tests from Tink, tamper tests and
truncation tests, and lives in its own crate so it can be reviewed in
isolation.

Server-side encryption on the bucket is deliberately not relied on: the
requirement is that the provider cannot read the data, which SSE does not give.

## Virus scanning

ClamAV via `clamd` `INSTREAM` over TCP. Inline during upload for files up to
`upload.inline_scan_max_size` (default 25 MiB, matching clamd's default stream
limit). Larger files are stored first and scanned by a job that streams the
decrypted object to clamd. Until the scan passes the blob is `uploaded` and
unreadable. Infected blobs become `infected`, permanently unreadable.

Skipped only when the blob was created with `skip_virus_scan = true`, which is
a statement by the backend about where the bytes come from. Such blobs cannot
be uploaded through the public port.

## Derived representations

Generated **eagerly on upload** by workers, not lazily on first request. The
read path stays a plain read, the transcoder runs only in a sandboxed worker,
listing pages with many thumbnails do not stampede, and the common "upload then
show it" flow gets its thumbnail within seconds.

A lazy path exists only for repair: if the object is missing, the `GET` handler
enqueues a generation job (if none is pending) and serves the placeholder. Each
representation records the `variant_version` it was built with; bumping the
version in config regenerates everything on demand through this path.

### Thumbnail

Always exists. WebP, autorotated from EXIF, fixed box in config. For files that
cannot be rendered, a pre-rendered category icon with a type badge, built at
compile time, served with the same URL shape. While pending, the icon is served
with `Cache-Control: no-store` and `X-Representation-State: pending`. Never
`404` or `202`, because the consumer is an `<img>` tag.

### Preview

WebP, not JPEG: smaller, lossless-capable, alpha, universally supported, and it
shares one encoder with thumbnails. Bounded to `media.preview.max_dimension`,
autorotated, always transcoded so decoder bombs and malformed containers never
reach a client.

Preview exists for every file we can render, not only images. The UI must key
on "has preview", never on category. Sources, in order:

1. **Images**: decode and transcode.
2. **Embedded image extraction**: a one-page PDF whose single image XObject
   covers most of the page with little text, or a DOCX/ODT whose body is almost
   empty with exactly one media file. The embedded image is pulled out at full
   resolution and goes through the image pipeline. `preview_source` is set to
   `embedded_image`. Thresholds live in config. This addresses users who answer
   "send a screenshot" with a Word file containing a screenshot.
3. **First-page render**: PDF through pdfium (`pdfium-render`), which is the
   most hardened renderer available. Office formats would need LibreOffice
   headless, which is deferred until real need; until then they fall back to
   the icon.

Files with no source return `404` from the preview endpoint and
`preview_state = unavailable`.

### Sandboxing

Decoders and renderers are the most attackable code in the service. They run in
a subprocess with memory and wall-clock limits, never in the API process. Image
decoding uses libvips for format breadth.

## Jobs

No external queue. A `jobs` table in Postgres, workers in the same binary,
`SELECT ... FOR UPDATE SKIP LOCKED` polling woken by `LISTEN/NOTIFY`.

Why: a handful of job kinds at modest volume, every job already touches the
blob row, and enqueueing in the same transaction as the state change removes
the dual-write failure where a crash between commit and publish loses the job.

Job kinds: `scan`, `thumbnail`, `preview`, `purge` (delete bucket objects),
`reap` (periodic: expire blobs, reset stale `uploading`, find orphans).

Each row: `kind`, `blob_id`, `payload`, `run_at`, `attempts`, `max_attempts`,
`locked_at`, `locked_by`, `last_error`. Exponential backoff on failure. A lease
timeout makes jobs locked by a dead worker eligible again. Concurrency is per
kind in config: rendering is CPU bound, scanning waits on clamd.

The binary takes `--role api|worker|all` so workers can move to separate
machines without code changes. If volume ever outgrows Postgres polling, the
enqueue function is swapped for a broker and the table becomes the outbox.

## Configuration

One TOML file, path from `--config` or `AGRAFE_CONFIG`, every key overridable
by environment variables prefixed `AGRAFE_` with `__` as the nesting separator.
Secrets (database URL, KEKs, signing keys, bucket credentials) are expected
from the environment in production, and are held in types whose `Debug` output
is redacted so a dumped config cannot leak them. See `config/example.toml`.

## Stack

| Concern | Choice |
|---|---|
| HTTP | `axum` on `tokio`, two routers sharing state, two listeners |
| Database | Postgres via `sqlx`, migrations in `migrations/` |
| Object store | `object_store` (local filesystem, S3, GCS, Azure behind one trait) |
| Config | `figment` with TOML and env providers |
| Signing | `hmac` + `sha2` |
| Encryption | `aes-gcm` + `hkdf`, Tink streaming format, own crate |
| Scanning | clamd `INSTREAM` client |
| Images | libvips subprocess, `image` for pure-Rust cases |
| PDF | `pdfium-render` |
| Ids | UUID v7 |

## Build order

1. Config, two listeners, schema, blob state machine, `POST/GET/DELETE /blobs`,
   listing, signed URL minting and verification. **← current**
2. `raw` and `multipart` upload with hashing, size enforcement, sniffing,
   encryption, bucket write. Streaming `GET /original` with ranges.
3. Job runner. ClamAV scanning inline and deferred.
4. Thumbnail and preview workers, icon fallback, autorotate, PDF render,
   embedded image extraction.
5. Expiration, reaper, purge.
6. `rows` to CSV/XLSX/ODS, then `archive` to ZIP.
