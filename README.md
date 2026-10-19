# agrafe

[![CI](https://github.com/tchak/agrafe/actions/workflows/ci.yml/badge.svg)](https://github.com/tchak/agrafe/actions/workflows/ci.yml)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)
[![Rust 1.94+](https://img.shields.io/badge/rust-1.94%2B-orange.svg)](https://www.rust-lang.org)
[![Edition 2024](https://img.shields.io/badge/edition-2024-orange.svg)](https://doc.rust-lang.org/edition-guide/rust-2024/)

A file storage service: encrypted at rest, virus scanned, with WebP previews
and thumbnails, and server-side generation of spreadsheets and ZIP archives.
Rust, Postgres, and any S3-compatible object store.

agrafe sits between your backend and a bucket. Your backend creates a blob,
hands the browser a signed upload URL, and gets back signed URLs for the
original, a preview and a thumbnail. agrafe streams the bytes through
hashing, type sniffing, ClamAV and encryption on the way in, renders the
derived images in sandboxed workers, and never keeps a whole file in memory.

It runs as a standalone service (the `agrafe` binary), or inside another
[axum](https://github.com/tokio-rs/axum) application: the `agrafe` crate is
the service as a library with a Rust private API, and `agrafe-axum`
provides its two HTTP interfaces as routers to nest.

See [DESIGN.md](DESIGN.md) for the architecture and the reasoning behind
every decision.

## Features

### Two interfaces

- **Private port** for the backend that owns the files, on a trusted
  network: create, list, upload, attach, delete, mint URLs, inspect jobs,
  metrics.
- **Public port** for browsers, where every URL carries an HMAC signature
  bound to the method, the path and the query string.
- **Signed URLs** with key rotation, rounded expiries so URLs minted in the
  same window are identical and cacheable, and a global ceiling on how long
  any URL may live; there are no permanent URLs.
- **Signed query parameters** let the backend choose `disposition`
  (`inline` or `attachment`), a download `filename`, and `wait` to hold a
  request until a representation is ready.
- **Leak-proof public lookups**: a deleted, expired or infected blob answers
  `404` on the public port, even with a valid signature, and tokens are
  redacted from request logs.
- **One JSON error shape** on both ports, with a stable machine-readable
  `code`.

### Uploads

- **Streaming pipeline**: size check, SHA-256, magic-byte sniffing, ClamAV
  and encryption run in one pass straight into a multipart upload.
- **Declared checksum and size**, enforced on the way in. A mismatch stores
  nothing. The metadata also reports the MD5 in Active Storage's `checksum`
  form, for backends migrating from it.
- **Content sniffing**: the declared type is checked against the bytes,
  with sensible leniency for aliases and containers.
- **Write-once blobs**: a blob receives bytes exactly once, so a leaked
  upload URL is worthless after the first upload and originals can be
  served `immutable`.
- **Raw or multipart** bodies, from a backend or straight from a browser.
- **Fetched from a URL**: a `url` blob is downloaded by a job from a
  presigned URL the backend gives at creation, through the same pipeline,
  with no bytes through the backend. Made for migrating from another
  store; checksums and size are optional and checked when given, and the
  network policy keeps fetches off private addresses.
- **Expiry** by timestamp or duration, enforced on every read rather than
  whenever the reaper runs. Every blob starts with one, a day by default,
  long enough for a direct upload to be attached when the form is
  submitted.
- **Crash safety**: an interrupted upload stores nothing and hands the blob
  back; a heartbeat lets the reaper reset only uploads whose process died.

### Attachments

- **Records, not reference counts**: attach a blob to a record with the
  Active Storage triple (`record_type`, `record_id`, `name`), as many times
  as there are records using it, and list by record or by blob.
- **Lifetime follows use**: the first attachment clears the blob's expiry,
  the last detach sets it again. A blob nobody uses expires on its own, and
  "permanent" is spelled "attached".
- **No purge footgun**: deleting an attached blob is refused, since a
  backend acting for one record cannot know the others are done with it.
  Detach is the only operation on behalf of one record.
- **Tombstones**: a deleted or expired blob keeps answering with its state
  for ninety days (by default) after its objects are purged, so the backend
  can tell a blob it deleted from an id that never existed.

### Security

- **Envelope encryption at rest**: a random data key per blob, wrapped by a
  key-encryption key from your config. Rotating a KEK touches the database
  only, never the bucket.
- **Streaming AEAD** in Tink's AES-GCM-HKDF-STREAMING format, validated
  against cross-language test vectors, with random access for `Range`
  requests.
- **ClamAV scanning**, inline for small files and as a job for large ones
  or when clamd is unreachable. Nothing is rendered or served before it is
  clean.
- **Sandboxed rendering**: no user file is ever decoded in the server
  process. `vips` and the two helper binaries run as subprocesses with
  resource limits and a timeout, and the file parsers are not even linked
  into the server.
- **Safe serving**: originals carry `nosniff` and a `sandbox` CSP, so an
  uploaded HTML or SVG never runs as a page of your origin.

### Previews and thumbnails

- **WebP preview and thumbnail** for images (JPEG, PNG, GIF, WebP, TIFF,
  HEIF/AVIF, SVG, BMP, JPEG 2000), and for PDF, DOCX and ODT files that
  wrap a single image.
- **Embedded image extraction**: a scan wrapped in a PDF, a Word or an ODT
  file gets the picture as its preview, not a page with a picture on it.
- **First-page thumbnails** for other PDFs, which have no preview: a
  document needs a viewer, not a picture. Office files have no page render.
- **Text thumbnails** showing the first lines of text, CSV and TSV files.
- **Category icons** for everything else. The thumbnail URL answers `200`
  for as long as the blob exists, so it can go straight into an `<img>`
  tag.
- **Opt out per blob** with `representations` at creation, for files
  nobody will look at through the service.
- **Eager rendering** by workers right after the scan, with lazy repair and
  regeneration on a `variant_version` bump.
- **EXIF orientation applied and all metadata stripped** from every image
  served.

### Generated files

- **Spreadsheets**: send rows as JSONL against declared columns and get a
  CSV, XLSX or ODS back, with typed `date` and `datetime` columns and, for
  XLSX and ODS, several sheets per workbook.
- **ZIP archives**: send a manifest of paths and blob ids and get a ZIP of
  those blobs, with path normalisation and collision-safe deduplication. A
  member deleted before the build reaches it is skipped and listed in an
  `ERRORS.txt` at the archive's root.
- **Sent in parts**: large sources arrive as many short requests, so a
  dropped connection or a deploy costs one part, not the whole export.
- **Built by jobs**, streamed into the bucket without touching local disk,
  sized up to hundreds of gigabytes.

### Operations

- **Postgres job queue** with `SKIP LOCKED` and `LISTEN/NOTIFY`, enqueued
  in the same transaction as the state change it follows.
- **Per-kind worker pools** for fetch, scan, render, rows, archive, purge
  and reap, so a slow kind never holds up another, with retries with
  backoff and dead-job inspection.
- **Per-blob priorities** (`high`, `normal`, `low`) ordering the blob's
  fetch, scan, render and build jobs within their kind.
- **Split roles**: run `api`, `worker` or `all` per process, and pick which
  job kinds a worker runs, without code changes.
- **Prometheus metrics** for queue depth and the age of the oldest due job,
  per kind and priority, plus job runs, durations and workers per process.
- **Jobs API** to list, retry or discard failed jobs, and a retry on the
  blob itself, which records the death of the job it was waiting on.
- **Dashboard**, off by default: the queue, the blobs and the attachments
  as HTML pages on the private port, with the same filters as the API and
  optional retry and discard buttons. No JavaScript.
- **Reaper** for expiry, stale uploads, abandoned exports, delayed purges
  of deleted and expired objects, and the pruning of old dead jobs and
  tombstones.
- **Storage backends**: local filesystem or any S3-compatible store.
- **One TOML file**, every key overridable by `AGRAFE_` environment
  variables, secrets redacted from debug output.

## Quick start

You need Postgres, [libvips](https://www.libvips.org/) 8.17 or newer with
its `vips` command on the `PATH`, and a reachable `clamd` for virus
scanning. On macOS, `brew install vips clamav`. Debian trixie and Ubuntu
24.04 ship older versions; Debian forky has a recent enough
`libvips-tools`.

```sh
createdb agrafe_dev
cp config/example.toml config/agrafe.toml
# set database.url, and replace the two keys with: openssl rand -base64 32

cargo build --workspace --release
export PATH="$PWD/target/release:$PATH"   # the helper binaries live here

agrafe --config config/agrafe.toml migrate
agrafe --config config/agrafe.toml serve
```

The private interface listens on `127.0.0.1:8080` and the public one on
`127.0.0.1:8081`. Secrets can come from the environment instead of the file:

```sh
export AGRAFE_DATABASE__URL=postgres://agrafe@db/agrafe
export AGRAFE_SIGNING__KEYS__K1=$(openssl rand -base64 32)
export AGRAFE_ENCRYPTION__KEKS__KEK1=$(openssl rand -base64 32)
```

To run the API and the workers on separate machines:

```sh
agrafe --config config/agrafe.toml serve --role api
agrafe --config config/agrafe.toml serve --role worker
agrafe --config config/agrafe.toml serve --role worker --kinds archive
```

## Usage

The examples below talk to the private port with `curl` and `jq`.

### Upload a file

Create the blob with its checksum and size, then send the bytes.

```sh
FILE=report.pdf
CHECKSUM=$(openssl dgst -sha256 -binary "$FILE" | base64)
SIZE=$(wc -c < "$FILE" | tr -d ' ')

ID=$(curl -s localhost:8080/blobs \
  -H 'content-type: application/json' \
  -d "{\"filename\": \"$FILE\", \"content_type\": \"application/pdf\",
       \"input_type\": \"raw\", \"checksum\": \"$CHECKSUM\", \"size\": $SIZE,
       \"expires_in\": \"30d\"}" | jq -r .id)

curl -s -X PUT "localhost:8080/blobs/$ID" --data-binary "@$FILE" | jq .state
```

### Fetch it from a URL

For a file that already sits behind a URL, such as an Active Storage blob
on S3 during a migration, let the service download it. The backend
presigns the URL and sends no bytes; `checksum_md5` takes the record's
`checksum` as it is, and `size` its `byte_size`. Both are optional and
checked when given.

```sh
ID=$(curl -s localhost:8080/blobs \
  -H 'content-type: application/json' \
  -d "{\"filename\": \"$FILE\", \"content_type\": \"application/pdf\",
       \"input_type\": \"url\", \"url\": \"$PRESIGNED_URL\",
       \"checksum_md5\": \"$MD5\", \"size\": $SIZE, \"priority\": \"low\",
       \"attachment\": {\"record_type\": \"Dossier\", \"record_id\": \"42\",
                      \"name\": \"piece_justificative\"}}" | jq -r .id)

curl -s "localhost:8080/blobs/$ID" | jq .state
```

The blob is `created` until the fetch job has run, then `uploaded` or
`ready` like an upload. A fetch that gives up, on a `404` or a checksum
mismatch at once, on a `5xx` after a few retries, leaves `failed_at` and
`last_error` on the blob, and `POST /blobs/:id/retry` enqueues a new
fetch, with a fresh URL on the row if the backend created a new blob. See
[DESIGN.md](DESIGN.md#fetched-blobs) for the policy and the failure
handling.

### Let a browser upload it

Instead of sending the bytes from the backend, create the blob the same way
and let the browser upload them. The blob metadata carries ready-made signed
URLs for the public port. `urls.upload` is there for `raw` and `multipart`
blobs that do not skip the virus scan, and works once.

```sh
curl -s "localhost:8080/blobs/$ID" | jq .urls
```

```json
{
  "upload": "http://127.0.0.1:8081/blobs/v1.k1.1792310400.3q2-7w…/0199f2…",
  "metadata": "http://127.0.0.1:8081/blobs/v1.k1.1792393200.Yk9…/0199f2…",
  "original": "http://127.0.0.1:8081/blobs/v1.k1.1792393200.Pp1…/0199f2…/original/report.pdf",
  "preview": "http://127.0.0.1:8081/blobs/v1.k1.1792393200.c2V…/0199f2…/preview/report.pdf",
  "thumbnail": "http://127.0.0.1:8081/blobs/v1.k1.1792393200.Zm9…/0199f2…/thumbnail/report.pdf",
  "expires_at": 1792393200
}
```

```js
await fetch(urls.upload, { method: "PUT", body: file });
```

The browser can poll `urls.metadata` for `state`, `scan_state` and the
representation states.

### Attach it to a record

A blob expires a day after its creation unless something uses it. Attach it
when the form is submitted, and it stays as long as the attachment does.

```sh
curl -s "localhost:8080/blobs/$ID/attachments" \
  -H 'content-type: application/json' \
  -d '{"record_type": "Dossier", "record_id": "42", "name": "piece_justificative"}'
```

Attaching the same triple twice returns the existing attachment. A blob can
be attached to several records, and a record can attach several blobs under
one name. To release it, detach; the last detach gives the blob back its
default expiry, and deleting an attached blob is refused.

```sh
curl -s 'localhost:8080/attachments?record_type=Dossier&record_id=42' | jq '.items[]'
curl -s -X DELETE "localhost:8080/attachments/$ATTACHMENT_ID"
```

A blob the backend knows the record of at creation can be created attached,
with no expiry at all:

```sh
curl -s localhost:8080/blobs \
  -H 'content-type: application/json' \
  -d '{"filename": "export.zip", "content_type": "application/zip",
       "input_type": "archive",
       "attachment": {"record_type": "Procedure", "record_id": "7", "name": "export"}}'
```

### Mint download URLs

Choose the lifetime, up to `signing.ttl.max`, and the signed query
parameters per call.

```sh
curl -s "localhost:8080/blobs/$ID/urls" \
  -H 'content-type: application/json' \
  -d '{"ttl": "15m", "query": {"disposition": "attachment", "wait": "5000"}}' \
  | jq -r .original
```

### Export a spreadsheet

Declare the columns, send the rows as JSONL, and poll until the blob is
`ready`. The content type picks the format: `text/csv`, XLSX or ODS.

```sh
SHEET=$(curl -s localhost:8080/blobs \
  -H 'content-type: application/json' \
  -d '{"filename": "users.xlsx",
       "content_type": "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
       "input_type": "rows",
       "columns": [{"name": "Name"}, {"name": "Signed up", "type": "date"}, {"name": "Active"}]}' \
  | jq -r .id)

printf '%s\n' '["Ada", "2026-01-15", true]' '["Grace", "2026-02-03", false]' \
  | curl -s -X PUT "localhost:8080/blobs/$SHEET" --data-binary @- | jq .state
```

Large exports go in parts, then a finish call starts the build:

```sh
curl -s -X PUT "localhost:8080/blobs/$SHEET/parts/1" --data-binary @page-1.jsonl
curl -s -X PUT "localhost:8080/blobs/$SHEET/parts/2" --data-binary @page-2.jsonl
curl -s -X POST "localhost:8080/blobs/$SHEET/finish"
```

An XLSX or ODS workbook can hold several sheets: declare `sheets` instead
of `columns`, send the rows of the first sheet, then a `{"sheet": "..."}`
line before the rows of each following sheet, in the declared order. A
sheet given no rows gets its header alone.

```sh
BOOK=$(curl -s localhost:8080/blobs \
  -H 'content-type: application/json' \
  -d '{"filename": "users.xlsx",
       "content_type": "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
       "input_type": "rows",
       "sheets": [
         {"name": "Users", "columns": [{"name": "Name"}, {"name": "Signed up", "type": "date"}]},
         {"name": "Teams", "columns": [{"name": "Team"}, {"name": "Members"}]}]}' \
  | jq -r .id)

printf '%s\n' '["Ada", "2026-01-15"]' '["Grace", "2026-02-03"]' \
  '{"sheet": "Teams"}' '["Compilers", 2]' \
  | curl -s -X PUT "localhost:8080/blobs/$BOOK" --data-binary @- | jq .state
```

### Build a ZIP archive

Reference existing `ready` blobs by id and path.

```sh
ZIP=$(curl -s localhost:8080/blobs \
  -H 'content-type: application/json' \
  -d '{"filename": "dossier.zip", "content_type": "application/zip",
       "input_type": "archive", "priority": "high"}' | jq -r .id)

printf '{"path": "pieces/%s", "blob_id": "%s"}\n' report.pdf "$ID" \
  | curl -s -X PUT "localhost:8080/blobs/$ZIP" --data-binary @- | jq .state
```

### List, delete and operate

```sh
curl -s 'localhost:8080/blobs?category=image&state=ready&limit=50' | jq '.items[].filename'
curl -s 'localhost:8080/blobs?attached=false' | jq '.items[].id'
curl -s -X DELETE "localhost:8080/blobs/$ID"

curl -s 'localhost:8080/jobs?state=dead' | jq
curl -s -X POST "localhost:8080/jobs/$JOB_ID/retry"
curl -s localhost:8080/metrics
```

A blob whose fetch, scan, build or render ran out of attempts records it
as `failed_at` and `last_error` in its metadata, and as `blob_failed_at` on
the attachments listing, for as long as it stays stuck; the dead job itself
is pruned after a week. Find them and give them another go from the blob's
side, whichever job that takes:

```sh
curl -s 'localhost:8080/blobs?failed=true' | jq '.items[] | {id, state, last_error}'
curl -s -X POST "localhost:8080/blobs/$ID/retry" | jq .state
```

A blob created attached and never uploaded is expired by the reaper after
`upload.abandoned_after` (a day), attachments kept, so the record's side
sees `expired` as its `blob_state`; an unattached one keeps to its own
`expires_at`.

### Watch it in a browser

The same listings as pages, for an operator on the trusted network:

```toml
[dashboard]
enabled = true   # /dashboard on the private port
actions = true   # the retry and discard buttons; off, the pages are read-only
```

`http://localhost:8080/dashboard` shows the queue census and the jobs with
a tab per state, then the blobs and the attachments with the filters of
the JSON routes, every row linking to the related objects. The URL holds
the filters, so a view can be pasted; `?refresh=5` on the jobs page makes
it reload every five seconds. A form a browser marks as another site's is
refused, on the dashboard and on the private API's `POST` routes alike,
which is what keeps a page the operator visits elsewhere from submitting
them. Both keys are off by default.

A host application nests `agrafe_dashboard::dashboard_router` wherever it
likes, behind its own authentication; the pages find their prefix from
the request.

The full route tables for both ports are in [DESIGN.md](DESIGN.md#interfaces),
and every configuration key is documented in
[config/example.toml](config/example.toml).

## Crates

| Crate | Contents |
|---|---|
| `agrafe` | The library: domain, jobs, config, and the `Agrafe` handle |
| `agrafe-axum` | The two HTTP interfaces as axum routers, and the mapping of errors onto status codes |
| `agrafe-dashboard` | The HTML dashboard as an axum router, templates compiled in |
| `agrafe-server` | The `agrafe` binary: `serve` and `migrate` |
| `agrafe-crypto` | Streaming AEAD and key wrapping |
| `agrafe-types` | Content types, classification, sniffing, the text-sample contract |
| `agrafe-sandbox` | Subprocesses with resource limits |
| `agrafe-render` | What renders and how: `vips`, the helper drivers, the icons |
| `agrafe-extract` | Sandboxed helper that pulls the image out of a PDF, DOCX or ODT |
| `agrafe-render-text` | Sandboxed helper that rasterises a text sample |

A deployment ships three binaries together: `agrafe`, `agrafe-extract` and
`agrafe-render-text`.

## Development

```sh
createdb agrafe_test
cargo test
cargo clippy --all-targets
cargo fmt
```

Tests connect as the current OS user to `agrafe_test`; set `DATABASE_URL`
to override. They need `vips` on the `PATH` as well. The integration tests,
in `agrafe-axum` and `agrafe-dashboard`, share that database. The former
run the helper binaries from `target/`: a plain `cargo test` builds them,
and `cargo test -p agrafe-axum` builds them on demand. ClamAV is faked in
tests.

The files under `fixtures/` and the renderer probe files under
`crates/agrafe-render/assets/probe` are built by `fixtures/make.py` (needs
Pillow), the placeholder icons by `scripts/icons.sh`, the dashboard's
stylesheet by `scripts/daisyui.sh`; all outputs are committed.

Coverage needs [cargo-llvm-cov](https://github.com/taiki-e/cargo-llvm-cov):

```sh
cargo coverage                        # summary per file
cargo coverage --show-missing-lines   # plus the uncovered lines
cargo coverage --html --open          # browsable report
```

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or
  <https://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or
  <https://opensource.org/licenses/MIT>)

at your option.

The DejaVu Sans Mono font embedded in `agrafe-render-text` is distributed
under its own license, in
[crates/agrafe-render-text/assets/fonts/LICENSE](crates/agrafe-render-text/assets/fonts/LICENSE).

### Contribution

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in the work by you, as defined in the Apache-2.0
license, shall be dual licensed as above, without any additional terms or
conditions.
