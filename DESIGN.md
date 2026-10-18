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

## Crates

One workspace, split so that the service can be embedded in another axum
application and so that the code that parses user files is never linked
into a server process. Dependencies point downwards only.

| Crate | Contents | Depends on |
|---|---|---|
| `agrafe-crypto` | Streaming AEAD and key wrapping, reviewed in isolation | |
| `agrafe-types` | Content type constants, `classify`, `sniff`, the vips loader allowlist, the text-sample contract, `str_enum!` | `infer`, `serde` |
| `agrafe-sandbox` | Running a subprocess with `setrlimit` bounds and a timeout; the `Limits` config | `tokio`, `libc` |
| `agrafe-extract` | The `agrafe-extract` helper: pulls the one image out of a PDF, DOCX or ODT. Library plus binary | `agrafe-types`, `lopdf`, `zip`, `quick-xml` |
| `agrafe-render-text` | The `agrafe-render-text` helper: rasterises a text sample. Library plus binary | `agrafe-types`, `ab_glyph` |
| `agrafe-render` | Which files render and from what, `vips thumbnail`, the icons, the drivers that spawn the helpers, the startup probe, the `[media]` tool config | `agrafe-types`, `agrafe-sandbox` |
| `agrafe` | The library: config, database, blobs, upload pipeline, encryption, signing, storage, ClamAV, jobs, metrics, URL minting, and the `Agrafe` handle whose methods are the private API | `agrafe-crypto`, `agrafe-types`, `agrafe-render`, `sqlx`, `object_store` |
| `agrafe-axum` | The two HTTP interfaces as axum routers, the extractors, `ApiError` and the mapping of the library's errors onto status codes | `agrafe`, `axum` |
| `agrafe-server` | The clap entry point: `serve`, `migrate`. Produces the binary named `agrafe` | `agrafe`, `agrafe-axum` |

The helpers are separate binaries, not subcommands of the server, so that
the server links none of their parsers: `lopdf`, `zip`, `quick-xml` and
`ab_glyph` exist only in processes the sandbox bounds. They are found on
`PATH` by default (`media.extract_command`, `media.render_text_command`),
read no configuration and write no logs. A deployment ships the three
binaries together. Each future renderer with a dependency tree of its own
(LibreOffice, HTML, Typst) follows the same pattern: a crate implementing
one producer in `agrafe-render`, "turn this file into a raster or a page in
the work directory", after which vips fits the result into the box as it
does today. The `Source` enum and the ordered match in the render job are
the seam that becomes a list of producers when the second one arrives;
the trait is not written against a single implementation.

The library knows nothing of HTTP. Its operations are methods on the
`Agrafe` handle (`create_blob`, `list_blobs`, `find_blob`, `delete_blob`,
`upload`, `urls`, `mint_urls`, `representation`, `list_jobs`, `retry_job`,
`discard_job`, ...), each returning its own `thiserror` enum
(`CreateError`, `UploadError`, `MintError`, `ListError`). `agrafe-axum`
maps those onto `ApiError` in one place, its `error.rs`, and its handlers
are a line or two each: parse the request, call the handle, shape the
response. The upload body reaches the library as a stream whose error is
boxed; the HTTP layer puts its own error in the box and takes it back out,
so a body cut short by the size limit still answers `413` without the
pipeline knowing what that is. Reading an original is the one place the
HTTP layer goes below the handle, to `blob::read::Reader`, because `Range`
handling and streaming the plaintext into a response body are its job.

Three consumers exist: the standalone service, `agrafe-server`; a host
application that builds an `Agrafe` from a configuration and a pool, nests
`agrafe_axum::public_router` under a prefix, runs `jobs::runner::run` as a
task and calls the private API in Rust; and one that also nests
`private_router` on an internal listener. The routers come with their
state applied, so nesting needs no state plumbing in the host. Embedding
into a host's own database (a schema name for `blobs` and `jobs`) is an
open decision, not a blocker.

## Interfaces

### Private port

No signature. Reachable only from trusted networks.

| Method | Path | Purpose |
|---|---|---|
| `POST` | `/blobs` | Create a blob (metadata only, no bytes yet) |
| `GET` | `/blobs` | List blobs, cursor paginated, filterable |
| `GET` | `/blobs/:id` | Full metadata plus ready-made signed public URLs |
| `PUT` | `/blobs/:id` | Upload bytes (all input types), once |
| `PUT` | `/blobs/:id/parts/:n` | Append part `n` of a `rows` or `archive` source |
| `POST` | `/blobs/:id/finish` | Seal a source sent in parts and enqueue its build |
| `DELETE` | `/blobs/:id` | Soft delete, bucket objects purged asynchronously |
| `POST` | `/blobs/:id/urls` | Mint signed public URLs with a chosen TTL and query parameters |
| `GET` | `/blobs/:id/original/:filename` | Original bytes, supports `Range` |
| `GET` | `/blobs/:id/preview/:filename` | WebP preview or 404 |
| `GET` | `/blobs/:id/thumbnail/:filename` | WebP thumbnail, always 200 |
| `GET` | `/jobs` | List jobs, cursor paginated, filterable by `kind`, `state`, `blob_id` |
| `GET` | `/jobs/:id` | One job, with its last error |
| `POST` | `/jobs/:id/retry` | Give a dead job its attempts back and run it now |
| `DELETE` | `/jobs/:id` | Discard a dead job |
| `GET` | `/metrics` | Prometheus text format, see "Jobs" |
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
| `columns` | for `rows` | The columns of the sheet, in order: a list of `{"name": "...", "type": "..."}`, `type` optional. See "Rows" |
| `skip_virus_scan` | no, default `false` | Private-only, backend vouches for the source |
| `priority` | no, default `normal` | `high`, `normal` or `low`: where the blob's jobs go in their kind's queue, see "Jobs" |
| `expires_at` or `expires_in` | no | Absolute timestamp or duration such as `7d` |

Checksum is SHA-256 because browsers compute it cheaply with
`crypto.subtle.digest`, and S3 understands it natively.

### Input types

The name describes the shape of the `PUT` body.

| Input type | Body | Produces |
|---|---|---|
| `raw` | The file bytes | The file |
| `multipart` | `multipart/form-data` with one part named `file` holding the bytes; the part's own filename, if any, is ignored | The file |
| `rows` | JSONL, one array of cell values per line, positional against the `columns` declared at creation | CSV, XLSX or ODS per the blob content type, built by a job after the request (see "Generated blobs") |
| `archive` | JSONL, one `{"path": "...", "blob_id": "..."}` per line | ZIP containing the referenced blobs, built by a job after the request (see "Generated blobs") |

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
| `created` | Row exists, no original. `PUT` allowed. A generated blob holds its source parts here between requests. |
| `uploading` | A `PUT` is in flight. Concurrent `PUT` gets `409`. Rows left behind by a crashed process are reset by the reaper (see below). |
| `uploaded` | What the client sent is in the bucket, encrypted. A job finishes the blob: the scan, or the build of a generated blob. Not readable. |
| `ready` | Scan passed or skipped. Readable. |
| `infected` | Scan failed. Never readable. Objects kept for forensics until the blob is deleted or passes its `expires_at`. |
| `expired` | Past `expires_at`. Not readable. Objects purged. |
| `deleted` | Soft deleted. Objects purged. |

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

A failed or stale upload leaves `uploading` for `created`: nothing of it was
stored, so there is nothing to keep (the parts a generated blob already
holds are not its, see "Generated blobs"; they are what makes a `created`
blob hold objects, and a delete or an expiry of one gets a purge like any
other). Both the abort path and the reaper apply this one
rule. Stale means abandoned, not slow: the pipeline touches
`upload_heartbeat_at` every third of `upload.stale_after` (default ten
minutes) while it runs, so an upload of any length keeps its row as long as
the process running it is alive, and only one that stopped mid-way gets reset
once `stale_after` has passed without a touch. `upload_started_at` is the
upload's claim on the row: the touch, the completion and the reset after a
failure all carry it. An upload that finds its row taken away, at a touch or
at the last check before its object would become visible, stops with `409`
and stores nothing, so one the reaper gave up on can neither finish on top
of the `PUT` that took the blob since nor hand that upload's row back.

A soft delete schedules a purge of the objects `upload.purge_delay` (default one
hour) later, long enough for a download that started before the delete to
finish. Expiry gets the same grace period from the moment the reaper records
it. The purge deletes everything under the blob's `<id>/` prefix and stamps
`purged_at` on the row, which private metadata reports; the row itself is
never removed. An infected blob is terminal and never becomes `expired`, but
its objects are not kept forever either: once it is past `expires_at` the
reaper purges them and leaves the state alone.

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
crash leaves work behind, which is what the stale-upload reaper is for: the
task touches the row while it runs, and a row untouched for
`upload.stale_after` has no task behind it. On S3
the bucket also needs an `AbortIncompleteMultipartUpload` lifecycle rule, since
a crashed process cannot abort the multipart uploads it had open, and
neither an upload nor a build can abort one whose completion request
itself failed: the parts are in the bucket, the handle to them is gone,
and only the rule reclaims them. One day is a fine setting.

Objects are keyed `<blob id>/original`. Blobs are write-once, so the key is
written exactly once; a failed upload aborts its multipart upload and leaves
no object behind. Representations will live under the same `<blob id>/`
prefix, which is what a purge deletes.

## Generated blobs

`rows` and `archive` blobs are generated by the service rather than
uploaded: the `PUT` carries a description, JSONL in both cases, and the
file is built afterwards by a job. The request accepts; it does not build.

Why: an archive of a hundred thousand entries and a hundred gigabytes takes
between a quarter of an hour and a few hours to assemble, bounded by the
network, while its manifest is ten megabytes and takes a second to send.
Building inside the request would leave the client waiting on a silent
connection for the rest, which every proxy, load balancer and HTTP client
between it and us cuts off after a minute or so, and a deploy of either side
cuts off for sure. Under the write-once rule a dropped `PUT` goes back to
`created`, so the work would be lost and the client would have to resend the
manifest and start over. Building in a job makes a crash or a deploy cost
one attempt, retried from the stored description with no client
involvement, and keeps the backend's side to a background job that sends
the description, in parts when it is large, and polls, the same "accepted,
finished by a job" contract a deferred scan already has.

`rows` would mostly be fast enough to build inline. It follows the same
shape anyway: one contract for the backend, one build job per type, one way
for a generated blob to become `ready`, rather than a second path to keep
correct for the one type that happens to be small.

### Accept

The source arrives in parts. `PUT /blobs/:id/parts/:n` stores part `n`,
numbered from one and consecutive, and `POST /blobs/:id/finish` seals the
source and enqueues the build. `PUT /blobs/:id` remains for a source that
fits one request: it is part one and the finish in one call. The two are
not atomic: a `PUT` that stored its part and then failed on the finish
leaves the blob `created` with one part, where a second `PUT` answers
`409` since part one has landed. `GET` shows `source_parts` at one, and
`POST /blobs/:id/finish` completes it.

Why parts: the expensive side of a `rows` export is the backend, which
paginates through a million records to produce a source of several
gigabytes and takes hours doing it. Behind one `PUT` that work rides on one
connection, every proxy between the two has an idle timeout that a slow page
can trip, and a deploy of either side resets the blob to `created` and the
export to zero. A part is a short request holding what the backend has
produced since the last one, a few tens of megabytes, so a failure costs one
part, a deploy costs nothing, and the backend needs no streaming HTTP
client: it builds a page's lines in memory and sends them. The backend
learns where to resume from `GET /blobs/:id`, which reports `source_parts`;
a part that is not the next one answers `409`, so a retry of one that
landed is harmless.

Each part streams through the upload pipeline like any body: encrypted,
stored as `<blob id>/source/<n>`, bounded by `upload.max_size`, the row in
`uploading` with the heartbeat while it is in flight and back in `created`
after, where it waits for the next part or the finish. The first part
fixes the blob's data key and segment layout on the row; every part and
the built file are streams under that one key, each with its own header,
a part's read off its object when the build opens it. A part is whole
lines, none longer than a megabyte, and must end with a newline; an empty
part is a part with no lines. It is parsed as it arrives and every line is
checked before the response, per the rules of its type below. A failing
line answers `422` naming the line, counted within the part, and the
reason; the part is discarded and the blob returns to `created` with the
parts it already had, like any failed upload: a failed part leaves nothing
of itself. A finish on a blob with no parts, or an uploaded part out of
sequence, answers `409`, as do parts and finish on a `raw` or `multipart`
blob. The finish moves the blob to `uploaded` with its build job, `rows` or
`archive` after the input type, enqueued in the same transaction,
unreadable until the job is done.

This is the one place a blob holds objects before it is `uploaded`.
Everywhere else `created` has meant an empty prefix: the failed-upload rule
says nothing was stored, the reaper resets stale uploads on that basis, and
a purge runs on terminal states only. All of it still holds for the
original, but anything that infers the state of the prefix from the state
of the row, in the code or in an operator's head, has to read `created` as
"no original, maybe parts" from now on. Watch for it when implementing.

What the accept pass has to carry across parts stays small and sits on the
row: `source_parts`, `source_size` (bytes of source), `source_rows`
(lines) and `source_updated_at`, the first three reported by `GET` for
generated blobs. The `source_columns` of a `rows` blob are written at
creation and only read by accept and build. Anything per line is left to
the build, which reads the source in order anyway. Parts are capped at
10 000.

A source that is never finished is an abandoned export. `DELETE` is the
backend's way out; as a backstop the reaper expires a `created` blob whose
last part is older than `upload.source_stale_after` (default one day), and
the purge takes the parts with the prefix.

### Build

The build job streams the generated file into the bucket as it produces
it: nothing touches local disk and nothing is held whole in memory. The
bytes go through the same encryptor and multipart upload as an uploaded
file, with 32 MiB parts (S3 allows 10 000 parts per upload; the default
5 MiB would cap an archive at 50 GB), to `<blob id>/original`, written
once: a failed attempt aborts its multipart upload and the next starts over
from the source.

No scan: a generated file contains only what the service wrote, from
members that are `ready` or from scalar values, so a generated blob is
created `skip_virus_scan` whatever the request said. A `ready` member was
scanned clean, or was uploaded `skip_virus_scan` by the backend, which is
its call to make for its own content and which the archive inherits: an
archive is no more and no less trusted than its members. No thumbnail or preview beyond
the category icon. When the build finishes the blob moves to `ready` with
the checksum and the final size on the row. Out of attempts, the blob is stuck in `uploaded`,
unreadable, like a scan that never completes. No new state: `uploaded`
already means that what the client sent is in the bucket and a job has to
finish the blob. The source is purged with the rest of the prefix.

Builds are two job kinds, `rows` and `archive`, each with its own worker
pool: `jobs.concurrency.rows` (default two) and `jobs.concurrency.archive`
(default one). A kind's workers are the only thing that runs it, so a batch
of exports cannot take a scan or render worker, and a hundred-gigabyte
archive cannot hold up a rows export of a hundred lines. An archive build
saturates a network interface, hence one at a time per process; a process
given `jobs.kinds = ["archive"]` on a machine of its own is how to run
more. Within `archive`, the blob's priority orders the queue: an export a
user is waiting on, created `high`, goes before a nightly batch created
`low`.

### Rows

The columns are declared at creation: `columns` is a list of one object
per column, in sheet order, with a `name` (the header text, any non-empty
string, duplicates allowed) and an optional `type`. The list is required
for a `rows` blob, refused on any other input type, and stored on the row
as `source_columns`, from which accept and build read it. Each line of the
source is a JSON array with exactly one value per column, in that order; a
line that is not an array, or has the wrong length, is refused. A value is
a scalar: a string, a number, a boolean or `null`, which is an empty cell.
An object or an array as a value is refused.

Why declare rather than infer from the first row or the union of keys: the
backend always knows its columns, and a column order inferred from the data
changes between two exports of the same table when an optional field first
appears on a different row. Why arrays rather than objects: with the
columns declared, a key on every line is only repetition, a few hundred
megabytes over a million rows of twenty columns, and object keys cannot
express two columns with the same header, which form exports produce.
Why not nested values: a spreadsheet cell holds a scalar, and any
flattening (JSON text, dotted keys) is a guess better made by the backend.

A column without a `type` takes what the JSON scalar says: a number is a
numeric cell, a boolean a boolean cell, a string a text cell. Two types can
be declared, `date` and `datetime`, whose values must be `null` or a string
in ISO 8601 form, checked at accept: `YYYY-MM-DD` for a date,
`YYYY-MM-DDTHH:MM:SS` with an optional fraction and an optional offset or
`Z` for a datetime. Spreadsheets have no timezone, so a datetime is written
as the wall-clock time given and the offset is dropped: the backend formats
in the zone its users read, and what it sends is what shows. Dates are
never inferred from the shape of a string: a free-text value that happens
to look like a date must stay text, the backend knows which columns are
dates, and a wrong guess is the classic spreadsheet corruption of
user-entered data. No other types for now: durations, currencies and
percentages are display formats, and a backend that wants `Oui` rather
than `VRAI` sends the string.

Numbers are written as their JSON lexeme, never parsed to a float and back,
so `1.10` stays `1.10`; one that does not fit a double, `1e999`, is refused
at accept, since no cell can hold it. Excel keeps fifteen significant
digits, so a backend exporting 64-bit identifiers sends them as strings.

The one sheet is named after the filename's stem, made acceptable to
Excel: its forbidden characters `[ ] : * ? / \` replaced, at most 31
characters, `Sheet1` when nothing is left.

The output format is the blob's `content_type`: `text/csv`, the XLSX type
or the ODS type; anything else is refused at creation. All three are
written row by row, the header first:

- CSV per RFC 4180, comma separated, CRLF, every field quoted when it needs
  to be, UTF-8 with a byte order mark so Excel reads the accents. A value
  starting with `=`, `+`, `-`, `@`, a tab or a carriage return is prefixed
  with a single quote: a CSV opened in a spreadsheet evaluates such cells as
  formulas, and a backend exporting user-entered text must not be able to
  ship `=HYPERLINK(...)` to an agent's desktop. Numbers are written bare,
  booleans as `true` and `false`, dates and datetimes as their ISO string.
  CSV has no types, so all of it is text to the reader; and a French Excel
  splits on `;` and reads `3.14` as text, so the CSV is the format for
  machines and the XLSX is what an agent opens.
- XLSX as a streamed package: one sheet, inline strings rather than a
  shared string table, which would need a second pass over the data.
  Numbers are numeric cells, booleans `t="b"` cells holding `1` or `0`,
  strings text cells, which Excel never evaluates. Dates and datetimes are
  serial numbers (days since 1899-12-30, the time as the fraction) with a
  cell style pointing at the built-in number formats 14 and 22, so Excel
  shows them in the user's locale; the small static `styles.xml` this needs
  is part of the package anyway. The spec also allows ISO 8601 `t="d"`
  cells, but older Excel does not read them. Excel takes 1900 for a leap
  year, so a date before 1900-03-01 has no serial it shows right and is
  written as its ISO string in a text cell, which is what Excel does when
  one is typed. A text that happens to contain Excel's own `_x0041_`
  escape is escaped in turn, so it shows as sent. No spreadsheet library,
  since the ones available build the workbook in memory: the XML is
  written by hand, there being nothing to it but escaping, and the
  package through the archive's own ZIP writer, which for this grew
  deflated entries of unknown size, compressed as the XML is produced
  (`flate2`). Such an entry's header goes out before its size is known,
  so it always carries zip64 fields, which the format allows at any size,
  and the descriptor holds the real sizes: a sheet near the row limit with
  a few dozen text columns is more than 4 GiB of XML, and without this
  the build would die on it after the backend had streamed the whole
  source, with nothing on the blob to say why. A character XML 1.0
  cannot carry, a control character other than tab and
  line breaks, becomes U+FFFD in either package; it could not show in a
  cell either way.
- ODS the same way: `mimetype` first and stored as the format requires,
  with its CRC in the header and no data descriptor, so the media type sits
  at the fixed offset detection reads it from; then the manifest and a
  streamed `content.xml`, with typed cells: `office:value-type` `float`,
  `boolean`, `string`, `date`, the typed value in `office:value`,
  `office:boolean-value` or `office:date-value` and the displayed text
  alongside, the ISO string for a date so a viewer without a data style
  still shows something sensible. A text's line breaks become paragraphs
  and its tabs and runs of spaces the markup ODF needs to keep them, since
  it collapses whitespace in a paragraph otherwise.

A boolean cell is rendered by the viewer in its locale, `VRAI` and `FAUX`
in a French Excel: that is what a typed cell is for.

XLSX and ODS sheets hold at most 1 048 576 rows and 16 384 columns, and a
cell at most 32 767 characters, counted as Excel counts them, in UTF-16
units. A `columns` list beyond the column limit, or a column name beyond
the cell limit, is refused at creation; a source beyond the row limit, or
a text beyond the cell limit, at accept with the line that crossed it,
whichever part it is in. Excel repairs a workbook with a longer text,
truncating the cell, and says so on opening. CSV has none of these
limits. A `rows` blob with no lines produces a header only.

Multi-sheet workbooks are not supported yet, but the design leaves them
an additive extension. `source_columns` is stored as a list of sheets from
the start, the one sheet of `columns` named after the filename stem, and
the build is one function that emits a sheet from a column list and a row
iterator, called once. The extension, when needed:

- Create takes `sheets`, a list of `{"name": "...", "columns": [...]}` in
  workbook order, mutually exclusive with `columns`, refused for
  `text/csv`. Sheet names are validated at creation: XLSX caps them at 31
  characters, forbids `[ ] : * ? / \` and wants them unique
  case-insensitively; LibreOffice forbids the same characters.
- In the source, an object line `{"sheet": "..."}` switches the current
  sheet; rows are arrays, so the shape is unambiguous. Lines before any
  switch belong to the first sheet, so a single-sheet source never carries
  one. The switch must name a declared sheet after the current one, which
  keeps the source grouped by sheet in workbook order: an XLSX sheet is one
  zip entry written whole before the next, an ODS `content.xml` holds its
  tables in sequence, and without grouping the build would need one pass
  over the source per sheet. A sheet no switch points at gets a header
  only.
- Accept carries two more fields on the row, the current sheet index and
  the row count within it, since the row limit is per sheet; `source_rows`
  stays the total. The switch line is valid in any part, so parts stay
  uniform and resuming stays `source_parts` alone.
- The build loops over the sheets, consuming the source up to the next
  switch line; the workbook relationships and the ODS table list are the
  only extra output.

Rejected: parts per sheet (`PUT /blobs/:id/sheets/:s/parts/:n`), which
breaks the one-counter resume rule for a backend that produces the sheets
sequentially anyway; and a sheet field on every line, which doubles the
bytes per line to carry a value that changes a handful of times per export.

### Archives

One `{"path": "...", "blob_id": "..."}` per line, the manifest. At accept,
per line:

- The path is normalised to NFC, with `/` as the separator, then refused
  if it is empty, absolute, has an empty, `.` or `..` component (so no
  trailing slash either), a backslash, a control or bidirectional
  formatting character (the filename rule), or exceeds 65 514 bytes: the
  ZIP limit of 65 535 less the 21 a deduplicated name can grow by, so the
  build never meets a name it cannot write. `ERRORS.txt` at the root is
  reserved, see below, and refused whatever its case.
- The blob is looked up in batches of a few hundred ids (`WHERE id =
  ANY($1)`) and must exist and be `ready`. Anything else (unknown, not yet
  scanned, infected, expired, deleted) is refused.
- The same blob may appear under several paths and is written each time.

Duplicate paths are deduplicated by the build rather than refused at accept,
in manifest order: a path already taken becomes `stem-1.ext`, `stem-2.ext`,
... with the counter before the last extension (`a.tar.gz` collides into
`a.tar-1.gz`; a name without a dot, or with only a leading one, gets the
counter at the end), the first free name wins and is taken in turn. Taken is
case-insensitive, since `Report.pdf` and `report.pdf` land on the same file
when extracted on macOS or Windows. The taken set is the build's, held in
memory for the one pass; a hundred thousand paths is a few megabytes, and
nothing about it needs to survive between parts.

Why dedupe: paths come from user filenames, which collide all the time, and
a refused manifest for two `facture.pdf` would push every backend to write
the same renaming loop. Why in manifest order: the first occurrence keeps its
name, which is what a user expects.

Entries are stored, not compressed: the members are PDFs, images and office
files, already compressed, and deflate would cost an hour of CPU per hundred
gigabytes for nothing. With stored entries and every member's size on its
row, the archive's size is known at the finish, the sum of the members plus
a fixed overhead per entry, accumulated part by part on the row's `size`,
which is therefore an estimate while the blob is `created` or `uploaded`
and exact once it is `ready`. It is checked against `archive.max_size`, a
limit of its own since `upload.max_size` is sized for one request, at most
309 GiB: 10 000 parts of 32 MiB, less one percent because the parts carry
the ciphertext, a tag per segment longer than the estimate, plus the
`ERRORS.txt` and the end records the estimate leaves out.

The build streams the ZIP as it assembles it, through a writer of its own
in `jobs::build::zip` rather than the `zip` crate, which stays out of the
server crates. The format is sequential by design: each entry is a local
header, the bytes, then a data descriptor with the CRC and sizes, and the
central directory comes last, so the writer never seeks. Sizes are known
up front, so the local header carries them too and only the CRC waits for
the descriptor. Zip64 is decided per entry from its size before its header
goes out, since a streamed header cannot be rewritten, and a deflated
entry of unknown size, which only a spreadsheet package has, always takes
it; the central directory switches to zip64 on its own for the entry
count and the offsets. Entries are made on Unix with mode `0644` and the member's
`uploaded_at` as their time. Readers that go by the central directory,
which is every desktop tool, handle data descriptors; strictly sequential
readers of stored entries may not, which is accepted.

Sources are read in manifest order through the usual decrypting reader, a
few fetched ahead of the one being copied, because most entries are small
and a serial build would spend longer waiting for the first byte of each
object than copying it. A member up to 8 MiB, which is most of them, is
read whole when fetched ahead; a larger one is opened when its turn comes.
Holding a response open and idle while a long entry copies is what the
look-ahead must not do: an object store cuts such a connection off after a
while, and that would fail the attempt. Memory is bounded by 8 MiB times
the look-ahead, plus a segment for the member being streamed and the parts
in flight.

A source that was `ready` at accept time can be deleted, expire or be purged
before the build reaches it. The build does not fail for it: the entry is
skipped, and the archive ends with an `ERRORS.txt` at its root listing every
skipped entry, one per line, as the path, the blob id and a reason code
(`deleted`, `expired`, `infected`), tab separated, which a path cannot
contain. No such file is written when nothing was skipped. The row records the number of entries and of skipped ones, which
`GET` reports as `archive_entries` and `archive_errors`, so the backend
learns that an archive is incomplete without downloading it.

Why: a user waiting for an export of a thousand files wants the nine hundred
and ninety-nine that exist and a note about the one that does not, not a
failure an hour later. Why only for sources that are gone: a bucket or
database error while reading a source is ours and fails the attempt, which
retries; an archive must never come out incomplete because of a transient
error of ours.

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
use (`agrafe_render::probe`, files in `crates/agrafe-render/assets/probe`),
and the process refuses to start if any of them fails. The probe also
catches a `media.render_text_command` that is not the helper.

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
are rendered from `crates/agrafe-render/assets/icons/icon.svg` by
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
`agrafe-render-text` helper, in the sandbox, with `ab_glyph` and a copy of
DejaVu Sans Mono embedded in that binary
(`crates/agrafe-render-text/assets/fonts`), so
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
   containing a screenshot. The parsing runs in the `agrafe-extract`
   helper, its own binary executed in the sandbox, never in a server
   process.
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
in a subprocess, never in the API process: `vips` for rendering, the
`agrafe-extract` and `agrafe-render-text` binaries for container parsing
and text rasterising, each its own crate so that the server links none of
their parsers (see "Crates"). Each child gets a clean environment (`PATH` and `HOME`
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

Revisited in October 2026 against apalis (1.0, Postgres and Redis backends,
a board UI), with a Redis queue in production in mind, and kept. What the
table gives is the transaction: a state change and the job it implies commit
together, dedupe is a unique index, the reaper schedules itself with a row. A
broker loses that and needs an outbox, which is this table plus a relay. Nor
is the queue where the load goes: a billion blobs is a big `blobs` table, the
`jobs` table holds only the backlog, the claim is one indexed `SKIP LOCKED`
update, and the slow part is the workers, vips, clamd and zip. What a broker
was wanted for, a view of the queue and a way to act on dead jobs, is the
`/metrics` and `/jobs` endpoints below. Isolation between kinds comes from
the worker pools, not the queue, see "Build". Revisit when a measurement
says the database is the bottleneck: oldest-due age growing with idle
workers, or dead-tuple churn on `jobs` that autovacuum settings do not
absorb. The enqueue function stays the seam; the table would become the
outbox.

Job kinds: `scan`, `render` (thumbnail and preview, whichever is pending),
`rows` and `archive` (generate a blob of that input type from its stored
source, see "Generated blobs"), `purge`
(delete the objects under a blob's prefix) and `reap`, the periodic
housekeeping. One reap, in one transaction: record `expired` on every
non-terminal blob past its `expires_at`; prune dead jobs that failed more
than `jobs.keep_failed` (default seven days) ago, long enough to have been
looked at; enqueue a purge, `upload.purge_delay` out, for every terminal
blob still holding objects that no purge job is going to take care of (the
ones just expired, infected ones past their expiry, and any whose purge was
lost, a pruned dead one included); hand `uploading` rows older than
`upload.stale_after` back to `created`; expire `created` blobs holding
source parts untouched for `upload.source_stale_after`. Expiry itself never
waits for the reaper, see "State machine".

The reaper keeps its own clock: each run enqueues the next, `jobs.reap_interval`
(default one minute) later, before doing its work, so a failing run is
superseded by the one it scheduled rather than retried with backoff or left
dead, and the error goes to the log. The runner enqueues the first one when it
starts. The unique index on pending jobs treats a null `blob_id` as a value,
so blob-less kinds dedupe like the rest and however many processes start, or
restart while a reap is pending, one row is pending at a time. A reap holds
an advisory lock for its transaction, so a run longer than the interval and
the one it scheduled, on another process, never update the same rows at
once: the second finds the lock taken and does nothing.

A purge refuses a blob that is not in a terminal state and fails instead,
leaving the job in the table as evidence of the bug that enqueued it. An
object missing between the listing and its delete is not an error, so a purge
that runs twice is harmless.

Each row: `kind`, `blob_id`, `priority`, `payload`, `run_at`, `attempts`,
`max_attempts`, `locked_at`, `locked_by`, `last_error`, `failed_at`. A
worker claims the highest-priority, then oldest, due job of its kind with
`UPDATE ... WHERE id = (SELECT ... FOR UPDATE SKIP LOCKED)`, which locks it
and counts the attempt. A locked job is
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
per kind in `jobs.concurrency` (defaults: `scan` 4, `render` 4, `rows` 2,
`archive` 1, `purge` 4, `reap` 1): rendering is CPU bound, scanning waits on
clamd, a build on the network, and one reaper per process is plenty since a
row is pending at a time anyway.

Isolation is by kind. Every kind has its own workers, so however many slow
jobs of one kind are queued, the others keep flowing; that is the property
production relies on and the reason there is no priority across kinds.
Priority orders within a kind: `high`, `normal` or `low`, set on the blob
at creation and copied onto every job enqueued for it, blob-less jobs
running `normal`. It orders, it does not reserve: a kind fed `high` jobs
without end runs its `low` ones never, which is the point for an archive
queue and visible on the per-priority oldest-due metric for everything
else. Scans and renders carry it too, so an interactive upload can jump a
bulk import of the same kind, but the default, `normal` everywhere, is a
FIFO per kind.

The binary takes `--role api|worker|all` so workers can move to separate
machines without code changes, and `jobs.kinds` (or `--kinds`, comma
separated) names the kinds a process runs workers for, every kind when
omitted. That is how the archive builders get machines of their own, or
the renderers more of them. A kind no process handles only piles up, which
the oldest-due metric shows; a process not handling `render` skips the
renderer probe, one not handling `reap` does not schedule the first reap.

### Observability

`GET /metrics` on the private port, Prometheus text format. The queue gauges
are read from the `jobs` table at scrape time, so any process reports for the
whole deployment: `agrafe_jobs{kind,state,priority}` with `state` one of
`due` (unclaimed, runnable now), `scheduled` (unclaimed, `run_at` ahead: a
retry backing off, a purge waiting for its delay), `running` (lease
current) and `dead`; and `agrafe_jobs_oldest_due_seconds{kind,priority}`,
the wait of the oldest due job, which is the number to alert on: it grows
when a kind's workers cannot keep up, or, for one priority alone, when the
higher ones starve it. A lapsed lease counts as unclaimed, as the claim query sees
it. Per process: `agrafe_job_runs_total{kind,outcome}` with `ok`, `retry`,
`dead` and `superseded`, `agrafe_job_duration_seconds{kind}` and
`agrafe_workers{kind}`. A `worker` process has no API; it binds
`server.private_addr` all the same, serving `/healthz` and `/metrics` only.

`GET /jobs` lists jobs newest first, cursor paginated on the id, filtered by
`kind`, `state`, `priority` and `blob_id`. `GET /jobs/:id` shows one with its
`last_error`. `POST /jobs/:id/retry` resets a dead job's attempts, makes it
due and notifies the workers; `DELETE /jobs/:id` removes one. Both answer
`409` for a job that is not dead: a pending job is the only thing that will
finish its blob, and a running one is a worker's. A retry whose blob already
has a pending job of that kind is `409` too; the newer job does the work.

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
| Metrics | `prometheus`, text exposition on `/metrics` |
| Scanning | clamd `INSTREAM` client |
| Images, PDF pages | `vips thumbnail` (libvips) in a sandboxed subprocess |
| Embedded images | `lopdf`, `zip`, `quick-xml` in the sandboxed `agrafe-extract` helper |
| Generated ZIP, XLSX, ODS | Own streaming ZIP writer (`jobs::build::zip`), `flate2` for the deflated sheet XML, XML written by hand |
| Text thumbnails | `ab_glyph` with an embedded DejaVu Sans Mono, in the sandboxed `agrafe-render-text` helper |
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
5. Expiration, reaper, purge.
6. Crate split, first stage: types, sandbox, the two helpers and the
   renderer out of the server crate.
7. Crate split, second stage: `agrafe-axum` and `agrafe-server` off the
   library; domain error types; the private API as a Rust handle.
8. `archive` to ZIP: source parts and finish, the build job, the streaming
   ZIP writer, abandoned sources.
9. `rows` to CSV/XLSX/ODS on the same parts and finish.
