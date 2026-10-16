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
- Replacing a file in place. Blobs are write-once (see below); a backend that
  wants new bytes creates a new blob and swaps its own reference.
- Range requests on previews and thumbnails. They are small and served whole.

## Interfaces

### Private port

No signature. Reachable only from trusted networks.

| Method | Path | Purpose |
|---|---|---|
| `POST` | `/blobs` | Create a blob (metadata only, no bytes yet) |
| `GET` | `/blobs` | List blobs, cursor paginated, filterable |
| `GET` | `/blobs/:id` | Full metadata plus ready-made signed public URLs |
| `PUT` | `/blobs/:id` | Upload bytes (all input types), once |
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
keeps reporting the real state. Public request spans record the path with the
token segment redacted, so a debug log level never writes capabilities to log
storage.

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
  filename override, `wait=<ms>` for pending representations. They are
  validated when minted, unknown names included, so a backend mistake is a
  `422` on the private port and never a client error on a correctly signed
  URL.

## Blobs

### Creation

`POST /blobs` takes:

| Field | Required | Notes |
|---|---|---|
| `filename` | yes | Stored as given, used in URLs and `Content-Disposition`. No path separators, control characters or bidirectional formatting characters (which can disguise an extension) |
| `content_type` | yes | Normalised to `type/subtype`, classified into a category |
| `input_type` | yes | `raw`, `multipart`, `rows`, `archive` |
| `checksum` | for `raw`, `multipart` | SHA-256 of the bytes, base64 |
| `size` | for `raw`, `multipart` | Declared size in bytes, enforced on upload |
| `skip_virus_scan` | no, default `false` | Private-only, backend vouches for the source |
| `expires_at` or `expires_in` | no | Absolute timestamp or duration such as `7d` |

Checksum is SHA-256 because browsers compute it cheaply with
`crypto.subtle.digest`, and S3 understands it natively.

### Input types

The name describes the shape of the `PUT` body.

| Input type | Body | Produces |
|---|---|---|
| `raw` | The file bytes | The file |
| `multipart` | `multipart/form-data` with one part named `file` holding the bytes; the part's own filename, if any, is ignored | The file |
| `rows` | JSONL, one object per line | CSV, XLSX or ODS per the blob content type |
| `archive` | JSONL, one `{"path": "...", "blob_id": "..."}` per line | ZIP containing the referenced blobs |

`rows` and `archive` are **private-only**. An `archive` upload reads other
blobs, so letting a browser drive it would be a confused-deputy path into files
the uploader does not own.

### Write-once

A blob receives bytes exactly once. `PUT` is accepted from `created` only; a
failed upload goes back to `created`, a successful one never does. There is no
re-upload, no per-blob switch, and no object version: the original lives at
`<blob id>/original`. A backend that needs different bytes creates a new blob,
points its own record at it and deletes the old one, the way Active Storage
treats blobs as immutable and attachments as the thing that changes.

Why: a public `PUT` URL is reusable until it expires, and will leak into
browser histories and proxy logs. Write-once means a leaked URL can only earn
a `409` after the first successful upload. It also means a signed URL for the
original is truthful for its whole lifetime, so originals can be served
`immutable`, and the server never has to reason about readers of a previous
version, re-scanning, or regenerating representations after a swap. The
`purge_delay` grace period exists only for deletes.

### Public upload rule

```
public_upload_allowed = !skip_virus_scan && input_type in (raw, multipart)
```

Enforced twice: when minting (the private API refuses to mint a public upload
URL for such a blob) and in the public `PUT` handler. Returned in metadata as a
computed field.

### State machine

```
created ──► uploading ──► uploaded ──► ready
   ▲            │   │         │
   └────────────┘   │         └──► infected
   (failed upload,  └──► ready | infected   (scanned inline, or skipped)
    nothing stored)

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

A blob is `expired` from the instant `expires_at` passes, not from whenever
the reaper gets to it: every read, upload and public lookup compares
`expires_at` with the current time and treats a blob past it as `expired`.
The reaper only records the state and purges the object, so expiry never
depends on its schedule. Until it runs, private metadata still reports the
stored state alongside the past `expires_at`.

Transitions are conditional updates (`UPDATE ... WHERE state = $from`) so two
concurrent requests cannot both win. Write-once is enforced by refusing `PUT`
from any state other than `created`. The upload's checksum and size are the
ones the blob was created with; `PUT` carries no override headers on either
port.

A failed or stale upload leaves `uploading` for `created`: nothing was stored,
so there is nothing to keep. Both the abort path and the reaper apply this one
rule.

A soft delete schedules a purge of the object `upload.purge_delay` (default one
hour) later, long enough for a download that started before the delete to
finish.

Separate from the blob state, each blob tracks `scan_state`
(`pending`, `clean`, `infected`, `skipped`), `scan_signature` (what clamd
found, for `infected`, whichever path scanned the file; private metadata
only) and a state per derived representation (`pending`, `ready`, `failed`,
`unsupported`).

### Listing

`GET /blobs` is cursor paginated on the blob id, which is a UUID v7 so ids
sort by creation time. Filters: `content_type`, `category`, `state`,
`scan_state`. Response carries `next_cursor` or null.

### Classification

Content type is mapped to a category at creation and re-checked against sniffed
magic bytes during upload (`infer`, on the first 8 KiB). A mismatch between
declared and sniffed type is rejected with `422`. The check is lenient where
rejecting would be wrong: aliases (`image/jpg`), a declared container of the
sniffed type in either direction (a DOCX is a ZIP), text sniffers (HTML, XML,
shell) against any `text/*` declaration, and `application/octet-stream`,
which declares nothing. Siblings such as DOCX against XLSX are rejected. The
rules live in `sniff.rs`. Categories:

`image`, `document`, `spreadsheet`, `presentation`, `archive`, `audio`,
`video`, `text`, `other`.

The mapping table lives in code (`classify.rs`). PDF is a `document`.

## Upload pipeline

Bytes flow once through a chain of consumers:

1. Size counter, rejects on exceeding the declared size, itself capped at
   `upload.max_size` when declared. The HTTP body limit on the upload routes
   is `max_size` plus slack for multipart framing, a backstop that the
   counter normally reaches first. A `raw` body whose `Content-Length`
   disagrees with the declared size is refused before it is read.
2. SHA-256 hasher, compared against the declared checksum at the end.
3. Magic-byte sniffer on the first chunk, compared against the declared
   content type.
4. ClamAV `INSTREAM` when the blob is not `skip_virus_scan` and the declared
   size is at or below `upload.inline_scan_max_size`. Above that, or when
   clamd cannot be reached, the scan runs as a job after storage and the
   blob waits in `uploaded`.
5. Chunked AEAD encryption (see below).
6. Multipart upload to the object store.

A failed check aborts the multipart upload, hands the blob back to the state
it came from and returns `422` (checksum, size, type) or `409` (state). On
success the blob moves to `uploaded`, or straight to `ready` when the scan was
inline or skipped, and the next jobs are enqueued in the same transaction: a
`scan` for a blob waiting in `uploaded`, `thumbnail` and `preview` for one
that is `ready`. The scan job enqueues the representations once the file is
clean, so nothing renders a file that has not been scanned.

An infected verdict from the inline scan is not a failed check. The object is
stored and the blob becomes `infected`, the same terminal state a deferred
scan produces, so the backend learns of it from the metadata whatever the
file size. The `PUT` itself answers `422` with code `infected`, so the
uploader learns at once rather than from a `200` that leads nowhere. The
signature name is in the message on the private port only: a backend wants
it, an anonymous uploader would use it to learn which signature to evade.
The verdict is read after the other checks: a wrong checksum
on an infected file is a plain `422` and nothing is stored.

The pipeline runs in its own task, fed by the request handler through a
channel. A handler future can be dropped at any moment (hyper cancels it on an
HTTP/2 stream reset, and a closed connection ends it on HTTP/1), and nothing
in that future is allowed to matter: dropping it closes the channel, the task
sees the body end before the declared size, aborts the multipart upload and
hands the blob back exactly as any other failed check does. Only a process
crash leaves work behind, which is what the stale-upload reaper is for. On S3
the bucket also needs an `AbortIncompleteMultipartUpload` lifecycle rule, since
a crashed process cannot abort the multipart uploads it had open.

Objects are keyed `<blob id>/original`. Blobs are write-once, so the key is
written exactly once; a failed upload aborts its multipart upload and leaves
no object behind. Representations will live under the same `<blob id>/`
prefix, which is what a purge deletes.

## Encryption at rest

age was considered and rejected. Its recipient header exists for file exchange
between humans and buys nothing when the service is the only reader; its key
rotation would require rewriting every object header; and its fixed 64 KiB
chunks and sync-only seek make range requests awkward.

Instead: **envelope encryption with a chunked streaming AEAD.**

- Each blob gets a random 256-bit data key when its upload starts, wrapped
  with a key-encryption key (KEK) from config: AES-256-GCM with a random
  nonce and the blob id as associated data, so a wrapped key moved to another
  row does not unwrap. The wrapped key and the `kek_id` are stored on the blob
  row. Rotating a KEK is a database update that rewraps keys, with zero bucket
  traffic. Moving KEKs to a KMS later changes nothing in the object format.
- The stream's associated data is the object key, so a ciphertext copied to
  another key does not decrypt.
- Payload follows **Tink's AES-GCM-HKDF-STREAMING** wire format. It is a
  published spec with reference implementations in Go and Java, so our
  implementation can be validated against cross-language test vectors.
- Chunk (segment) size is `encryption.chunk_size`, default 256 KiB, at most
  16 MiB because a segment is held in memory whole on both paths, once per
  in-flight request. Larger chunks mean fewer tags; smaller chunks mean
  finer range granularity. Stored on the row so it can change without
  migrating objects.
- Per-chunk nonce is `nonce_prefix || counter || last_flag`, which prevents
  reordering, truncation and chunk swapping between blobs.
- Random access is arithmetic. The 40-byte header (salt and nonce prefix)
  shares the first chunk with the plaintext, so chunk `0` holds
  `chunk - 16 - 40` plaintext bytes and every later chunk `chunk - 16`; chunk
  `i > 0` starts at ciphertext offset `i * chunk`. The header is also cached
  on the blob row (`encryption_header`), so a `Range` request maps to a chunk
  range, one byte-range `GET` to the bucket, decrypt, trim, stream.
  `Content-Length` is exact. The first segment is opened before the response
  starts, so a wrong key, a tampered object or a missing one is a logged
  `500`. A segment that fails later cannot change the status any more: the
  connection is cut, and the failure is logged, because the client cannot
  tell a corrupt object from a dropped connection.

The crypto code is small (a few hundred lines on RustCrypto `aes-gcm` and
`hkdf`) and owned by us, in the `agrafe-crypto` crate so it can be reviewed in
isolation. Key material is wiped on drop. It has tamper and truncation tests,
and a known-answer test against a ciphertext produced by Tink's Python
implementation (the reverse direction, Tink decrypting ours, was checked by
hand when the vector was made).

Server-side encryption on the bucket is deliberately not relied on: the
requirement is that the provider cannot read the data, which SSE does not give.

## Virus scanning

ClamAV via `clamd` `INSTREAM` over TCP. Inline during upload for files up to
`upload.inline_scan_max_size` (default 25 MiB, matching clamd's default stream
limit). Larger files are stored first and scanned by a job that decrypts the
object segment by segment and streams it to clamd. Until the scan passes the
blob is `uploaded` and unreadable. Infected blobs become `infected`,
permanently unreadable.

An inline session lasts as long as the upload it rides on, and on the public
port the uploader sets that pace, so an anonymous client trickling bytes
could hold a clamd thread indefinitely, and a handful of them every thread
clamd has. `clamav.max_inline_scans` (default 4) caps the inline sessions
open at once; an upload that finds them all taken is stored and scanned by a
job, without waiting. clamd's `MaxThreads` must stay above that cap plus the
scan workers, which hold a connection each while they run.

clamd being down must not stop uploads, and must not make anything readable
either. When it cannot be reached at the start of an upload, or fails midway,
the upload completes and the scan is deferred to a job, which is retried with
backoff until clamd answers. In the job, a connection failure or an `ERROR`
reply (the stream limit exceeded, for instance) is a job failure and the blob
stays `uploaded`; once the job is out of attempts the blob is stuck there,
unreadable, until it expires or is deleted. clamd scans only once the stream
is complete, but hangs up before that at its stream limit or when it dies,
so the client treats a failed write as "stop sending and read the reply",
not as an error.

A verdict is only as good as the clamd behind it, and clamd's defaults are
sized for mail, not for files of `upload.max_size`. Three limits matter:

- `StreamMaxLength`: above it clamd hangs up with an `ERROR`, the scan fails
  and the blob stays `uploaded`. Loud, so merely inconvenient.
- `MaxScanSize` and `MaxFileSize`: above these clamd scans a prefix of the
  file and still answers `OK`. Silent, so dangerous: a large file passes
  without having been looked at.

The clamd the workers talk to must set all three above `upload.max_size`,
and `AlertExceedsMax yes` so that any limit hit anyway becomes a `FOUND`
(`Heuristics.Limits.Exceeded.*`) rather than a pass. A file that cannot be
scanned is treated as infected, never as clean. `MaxThreads` must stay above
`jobs.concurrency.scan` plus `clamav.max_inline_scans`.

Skipped only when the blob was created with `skip_virus_scan = true`, which is
a statement by the backend about where the bytes come from. Such blobs cannot
be uploaded through the public port.

## Derived representations

Generated **eagerly on upload** by workers, not lazily on first request. The
read path stays a plain read, the renderer runs only in a sandboxed
subprocess, listing pages with many thumbnails do not stampede, and the
common "upload then show it" flow gets its thumbnail within seconds.

A lazy path exists only for repair: if the object is missing, or the stored
representation was built with another `media.variant_version`, the `GET`
handler sets it back to `pending`, enqueues a generation job (deduplicated
with any pending one) and serves the placeholder. Bumping the version in
config therefore regenerates everything on demand, `failed` and
`unavailable` ones included, since a new renderer may do better. Each
representation records the version it was built with
(`thumbnail_variant_version`, `preview_variant_version`).

### Rendering

Everything goes through one operation, `vips thumbnail`, run as a subprocess
(see "Sandboxing"): shrink-on-load where the format allows it, EXIF
orientation applied, colour converted to sRGB, every piece of metadata
dropped, lossy WebP out. The image is scaled to fit the box, never cropped.
Pixels are never enlarged, a small image stays small; a PDF page or an SVG
is vector with a nominal size, so it is rendered at whatever scale fills
the box. `--fail-on error` makes a truncated or malformed file a rejection
rather than a partial render. Each vips process runs two threads
(`VIPS_CONCURRENCY`): parallelism comes from the workers, and a narrow
process keeps the memory limit meaningful on any core count. Its `TMPDIR`
is the job's work directory, so anything it spills is cleaned up with it.

Only files with a known source are attempted:

- image types libvips loads natively: JPEG, PNG, GIF, WebP, TIFF, HEIF/AVIF,
  SVG, BMP, JPEG 2000. Everything else under `image/*` would go through
  ImageMagick, more attack surface than it is worth, and gets the icon;
- PDF, DOCX and ODT, through the sources below;
- text files (the `text` category plus CSV and TSV), thumbnails only;
- originals at most `media.max_source_size` (default 512 MiB), except for
  the text thumbnail, which reads the first 8 KiB whatever the size.

The allowlist above is by declared type, and the upload sniffer lets
through anything `infer` does not recognise. vips, though, picks its loader
by magic and tries ImageMagick last, so a file declared `image/png` holding
an XPM would reach it. Every file handed to vips is therefore sniffed again
right before the call (`media::loader_for`) against the same list, plus PNM
for the pages the helpers write; anything else is `failed` without vips
ever seeing it. The embedded image extractor applies the same check to the
bytes it pulls out of a PDF, since a `DCTDecode` stream holds whatever its
author put there. `VIPS_BLOCK_UNTRUSTED` was considered and rejected: it
also blocks the PDF, SVG, JPEG 2000 and PNM loaders.

One `render` job produces both representations, whatever is `pending` on
the row. The preview is rendered first; the thumbnail is the preview fitted
into the thumbnail box. The original is therefore fetched, decrypted and
decoded once, the two pictures agree, and a thumbnail regenerated on its
own (its object gone) is made from the stored preview without touching the
original. A preview verdict from the current variant version stands: a
`failed` preview gives a `failed` thumbnail, an `unavailable` one leaves
the thumbnail to the text sample or the icon; a preview from another
version, or whose object is gone, is rendered again first. The thumbnail
thus appears after the preview rather than alongside it, a few seconds
for a large PDF, which the icon placeholder and `wait` cover. Two jobs
decrypting and decoding the original twice was the first design, and
rendering cost in practice is what changed it.

A file the renderer rejects, or that runs past the sandbox timeout, leaves
the representation `failed` and the job done: the same bytes would fail the
same way tomorrow. A helper that crashes or runs out of time counts the
same way; one that declines (no embedded image to pull out, a sample that
is not text) hands over to the next source, or leaves the representation
`unavailable` when there is none. Only failures of ours (object store,
database, the renderer missing) fail the job, which retries with backoff
and leaves the representation `pending` meanwhile.

vips exits non-zero for a bad file and for a missing loader or an unknown
option alike, so a libvips built without PDF support, or too old for
`keep=`, would mark every file `failed` and need a variant version bump
once fixed. Before a worker starts it therefore renders a text sample, a
one-pixel PNG and a one-page PDF through the exact command lines the jobs
use (`jobs::render::probe`, files in `crates/agrafe/assets/probe`), and the
process refuses to start if any of them fails. The probe also catches a
`media.agrafe_command` pointing at a binary without the helper subcommands.

Representations live at `<blob id>/thumbnail` and `<blob id>/preview`,
encrypted with the blob's own data key: the streaming AEAD derives a fresh
subkey per stream from a random salt, so one key safely covers the three
objects, and the object key is the associated data as for the original.
They are small and served whole, with a strong `ETag` of the content and
`Cache-Control: private, max-age=86400`: not `immutable`, since a version
bump changes them under the same URL.

Every representation response carries `X-Representation-State`, so a
client can tell a placeholder from the final answer, and `wait=<ms>` in the
signed query (at most 30 s) holds the request until the representation
is no longer pending, for the "upload then show it" flow.

### Original

Served only from `ready`; any other state answers `409` on both ports (the
public port already answers `404` for terminal states). Single-range `Range`
requests are honoured with `206`; multiple ranges, and a `Range` whose
`If-Range` names another `ETag`, get the whole file. The `ETag` is the
SHA-256 of the content. Originals are served `private, immutable` for a year:
the bytes behind a URL never change, and the URL's own expiry bounds how long
it works. Thumbnails and previews are the exception, since they change under
the same URL once rendered (see above).

Every original carries `X-Content-Type-Options: nosniff` and
`Content-Security-Policy: sandbox`: user content is served from this origin,
and an HTML or SVG upload must never run as a page of it. The signed query
parameters `disposition` (`inline`, the default, or `attachment`) and
`filename` set `Content-Disposition`.

### Thumbnail

Always exists. WebP, autorotated from EXIF, fitted into the
`media.thumbnail` box. For files that cannot be rendered, a pre-rendered
category icon with a type badge, served with the same URL shape. The icons
are rendered from `crates/agrafe/assets/icons/icon.svg` by
`scripts/icons.sh` and embedded in the binary, so serving one needs no
renderer and no asset directory.

| Blob | Thumbnail | Served | `Cache-Control` |
|---|---|---|---|
| `ready` | `ready` | the WebP | `private, max-age=86400` |
| `ready` | `pending` | icon, `X-Representation-State: pending` | `no-store` |
| `ready` | `failed`, `unavailable` | icon, state in the header | `private, max-age=86400` |
| not yet `ready` | any | icon, `pending` | `no-store` |
| terminal | any | icon, `unavailable` (private port; public is `404`) | `private, max-age=86400` |

Never `404` or `202`, because the consumer is an `<img>` tag.

Text files get a real thumbnail: the first lines of the file set in a
monospace font on a page, a hint of what the file is rather than a way to
read it. The first 8 KiB are decoded as UTF-8, or as Windows-1252 when they
are not, which is what Latin-1 exports from older systems are in practice;
a sample with NUL bytes or more than a sprinkling of control characters is
not text and gets the icon. The page is rasterised in Rust by the
`agrafe render-text` helper, in the sandbox, with `ab_glyph` and a copy of
DejaVu Sans Mono embedded in the binary (`crates/agrafe/assets/fonts`), so
the result is the same on every host and needs no font, fontconfig or
shaping library installed. The trade: no shaping, so Arabic or Indic text
comes out as unconnected glyphs and anything outside Latin, Greek and
Cyrillic as the notdef box. The helper writes a PGM at twice the box, and
vips downsizes it into the WebP like any other raster. There is no text
preview: a preview that cannot be read is not worth rendering.

### Preview

WebP, not JPEG: smaller, lossless-capable, alpha, universally supported, and it
shares one encoder with thumbnails. Bounded to `media.preview.max_dimension`,
autorotated, always transcoded so decoder bombs and malformed containers never
reach a client.

Preview exists for every file we can render, not only images. The UI must key
on "has preview", never on category. Sources, in order:

1. **Images**: decode and transcode. `preview_source = image`.
2. **Embedded image extraction** (`preview_source = embedded_image`): a
   one-page PDF whose single image XObject covers at least
   `media.embedded_image.min_coverage` of the page, drawn upright, with at
   most `max_text_chars` of text; or a DOCX/ODT with exactly one media file
   (under `word/media/` or `Pictures/`) that is a raster image, and at
   most `max_text_chars` of non-whitespace text in the body. The embedded
   image is pulled out at full resolution and goes through the image
   pipeline: JPEG and JPEG 2000 streams as they are, raw 8 or 16 bit grey
   or RGB samples as PNM. Anything doubtful (form XObjects, rotation,
   masks, exotic colour spaces, EMF) is left to the next source. This
   addresses users who answer "send a screenshot" with a Word file
   containing a screenshot. The parsing runs in the `agrafe extract`
   helper, a hidden subcommand of the binary executed in the sandbox,
   never in a server process.
3. **First-page render** (`preview_source = first_page`): PDF through
   libvips's PDF loader. libvips builds against either poppler or pdfium;
   production images should use pdfium, the most hardened renderer
   available. Linking `pdfium-render` directly was considered and dropped:
   it would add a second native dependency and a second sandboxed
   pipeline for what the one tool already does. Office formats would need
   LibreOffice headless, which is deferred until real need; until then
   they fall back to the icon.

Files with no source answer `404` from the preview endpoint, with
`preview_state = unavailable`; so do `pending` and `failed` ones, each with
its state in `X-Representation-State` and a message saying why. A blob that
is not `ready` answers `409` as the original does.

### Sandboxing

Decoders and renderers are the most attackable code in the service. They run
in a subprocess, never in the API process: `vips` for rendering, the binary's
own hidden `extract` and `render-text` subcommands for container parsing and
text rasterising. Each child gets a clean environment (`PATH` and `HOME`
only, no secrets; vips also gets `TMPDIR` and `VIPS_CONCURRENCY`), `setrlimit` bounds on
address space and data (`media.sandbox.memory`, default 1 GiB), CPU time and
output file size (`max_output_size`), no core dumps, and is killed at the
wall-clock limit (`media.sandbox.timeout`, default 60 s). The address space
limit is enforced by Linux and ignored by macOS, so development machines get
only the timeout. A crash, a decoder bomb or an infinite loop costs one job
attempt, not the process.

## Jobs

No external queue. A `jobs` table in Postgres, workers in the same binary,
`SELECT ... FOR UPDATE SKIP LOCKED` polling woken by `LISTEN/NOTIFY`.

Why: a handful of job kinds at modest volume, every job already touches the
blob row, and enqueueing in the same transaction as the state change removes
the dual-write failure where a crash between commit and publish loses the job.

Job kinds: `scan`, `render` (thumbnail and preview, whichever is pending),
`purge` (delete bucket objects), `reap` (periodic: expire blobs, reset stale
`uploading`, find orphans, prune dead jobs once they have been around long
enough to have been looked at).

Each row: `kind`, `blob_id`, `payload`, `run_at`, `attempts`, `max_attempts`,
`locked_at`, `locked_by`, `last_error`, `failed_at`. A worker claims the
oldest due job of its kind with `UPDATE ... WHERE id = (SELECT ... FOR UPDATE
SKIP LOCKED)`, which locks it and counts the attempt. A locked job is
claimable again once `jobs.lease_timeout` has passed, which is how a crashed
worker's job gets picked up. A live worker renews its lease every third of
it while the handler runs, so a scan longer than the lease is not handed to
a second worker; only a dead one lets the lease lapse. Success deletes the row. Failure, a handler
panic included, unlocks it with exponential backoff (30 s, doubling, capped at an hour) until `max_attempts`
(5), then sets `failed_at` and leaves it locked: never run again, not holding
the one pending slot per kind and blob, and still there to read. A failing
job whose twin was enqueued while it was locked is dropped in favour of the
newer one.

`enqueue` calls `pg_notify` in the same statement, so the notification
travels with the commit. One listener per process wakes the pollers of that
kind; `jobs.poll_interval` is the fallback for a missed wakeup. Concurrency is
per kind in `jobs.concurrency` (defaults: `scan` 4, `render` 4, `purge` 4,
`reap` 1): rendering is CPU bound, scanning waits on clamd. `scan` and
`render` have handlers; `purge` and `reap` rows wait in the table.

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
| Encryption | `aes-gcm` + `hkdf`, Tink streaming format, own crate (`agrafe-crypto`) |
| Sniffing | `infer` |
| Scanning | clamd `INSTREAM` client |
| Images, PDF pages | `vips thumbnail` (libvips) in a sandboxed subprocess |
| Embedded images | `lopdf`, `zip`, `quick-xml` in the sandboxed `extract` helper |
| Text thumbnails | `ab_glyph` with an embedded DejaVu Sans Mono, in the sandboxed `render-text` helper |
| Ids | UUID v7 |
| Time | `jiff`. `Timestamp` throughout the model; `jiff-sqlx` wrappers only at the SQL boundary |

## Build order

1. Config, two listeners, schema, blob state machine, `POST/GET/DELETE /blobs`,
   listing, signed URL minting and verification.
2. `raw` and `multipart` upload with hashing, size enforcement, sniffing,
   encryption, bucket write. Streaming `GET /original` with ranges.
3. Job runner. ClamAV scanning inline and deferred.
4. Thumbnail and preview workers, icon fallback, autorotate, PDF render,
   embedded image extraction.
5. Expiration, reaper, purge. **← current**
6. `rows` to CSV/XLSX/ODS, then `archive` to ZIP.
