# agrafe

[![CI](https://github.com/tchak/agrafe/actions/workflows/ci.yml/badge.svg)](https://github.com/tchak/agrafe/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](Cargo.toml)
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
  network: create, list, upload, delete, mint URLs, inspect jobs, metrics.
- **Public port** for browsers, where every URL carries an HMAC signature
  bound to the method, the path and the query string.
- **Signed URLs** with key rotation, rounded expiries so URLs minted in the
  same window are identical and cacheable, and optional permanent URLs.
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
  nothing.
- **Content sniffing**: the declared type is checked against the bytes,
  with sensible leniency for aliases and containers.
- **Write-once blobs**: a blob receives bytes exactly once, so a leaked
  upload URL is worthless after the first upload and originals can be
  served `immutable`.
- **Raw or multipart** bodies, from a backend or straight from a browser.
- **Expiry** by timestamp or duration, enforced on every read rather than
  whenever the reaper runs.
- **Crash safety**: an interrupted upload stores nothing and hands the blob
  back; a heartbeat lets the reaper reset only uploads whose process died.

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
  HEIF/AVIF, SVG, BMP, JPEG 2000), PDFs, and DOCX and ODT files.
- **Embedded image extraction**: a scan wrapped in a PDF or a Word file
  gets the picture as its preview, not a page with a picture on it.
- **Text thumbnails** showing the first lines of text, CSV and TSV files.
- **Category icons** for everything else. The thumbnail URL always answers
  `200`, so it can go straight into an `<img>` tag.
- **Eager rendering** by workers right after the scan, with lazy repair and
  regeneration on a `variant_version` bump.
- **EXIF orientation applied and all metadata stripped** from every image
  served.

### Generated files

- **Spreadsheets**: send rows as JSONL against declared columns and get a
  CSV, XLSX or ODS back, with typed `date` and `datetime` columns.
- **ZIP archives**: send a manifest of paths and blob ids and get a ZIP of
  those blobs, with path normalisation and collision-safe deduplication.
- **Sent in parts**: large sources arrive as many short requests, so a
  dropped connection or a deploy costs one part, not the whole export.
- **Built by jobs**, streamed into the bucket without touching local disk,
  sized up to hundreds of gigabytes.

### Operations

- **Postgres job queue** with `SKIP LOCKED` and `LISTEN/NOTIFY`, enqueued
  in the same transaction as the state change it follows.
- **Per-kind worker pools** for scan, render, rows, archive, purge and reap,
  with per-blob priorities, retries with backoff and dead-job inspection.
- **Split roles**: run `api`, `worker` or `all` per process, and pick which
  job kinds a worker runs, without code changes.
- **Prometheus metrics** for queue depth and the age of the oldest due job.
- **Jobs API** to list, retry or discard failed jobs.
- **Reaper** for expiry, stale uploads, abandoned exports and delayed
  purges of deleted objects.
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

### Mint download URLs

Choose the lifetime and the signed query parameters per call.

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
curl -s -X DELETE "localhost:8080/blobs/$ID"

curl -s 'localhost:8080/jobs?state=dead' | jq
curl -s -X POST "localhost:8080/jobs/$JOB_ID/retry"
curl -s localhost:8080/metrics
```

The full route tables for both ports are in [DESIGN.md](DESIGN.md#interfaces),
and every configuration key is documented in
[config/example.toml](config/example.toml).

## Crates

| Crate | Contents |
|---|---|
| `agrafe` | The library: domain, jobs, config, and the `Agrafe` handle |
| `agrafe-axum` | The two HTTP interfaces as axum routers |
| `agrafe-server` | The `agrafe` binary: `serve` and `migrate` |
| `agrafe-crypto` | Streaming AEAD and key wrapping |
| `agrafe-types` | Content types, classification, sniffing |
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
to override. They need `vips` on the `PATH` as well, and run the helper
binaries from `target/`, building them if a `cargo test -p agrafe-axum` has
not. ClamAV is faked in tests.

The files under `fixtures/` and the renderer probe files under
`crates/agrafe-render/assets/probe` are built by `fixtures/make.py` (needs
Pillow), the placeholder icons by `scripts/icons.sh`; all outputs are
committed.

Coverage needs [cargo-llvm-cov](https://github.com/taiki-e/cargo-llvm-cov):

```sh
cargo coverage                        # summary per file
cargo coverage --show-missing-lines   # plus the uncovered lines
cargo coverage --html --open          # browsable report
```

## License

MIT
