# agrafe

[![CI](https://github.com/tchak/agrafe/actions/workflows/ci.yml/badge.svg)](https://github.com/tchak/agrafe/actions/workflows/ci.yml)
[![Docs](https://github.com/tchak/agrafe/actions/workflows/docs.yml/badge.svg)](https://tchak.github.io/agrafe/)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)
[![Rust 1.94+](https://img.shields.io/badge/rust-1.94%2B-orange.svg)](https://www.rust-lang.org)
[![Edition 2024](https://img.shields.io/badge/edition-2024-orange.svg)](https://doc.rust-lang.org/edition-guide/rust-2024/)

A file storage service: encrypted at rest, virus scanned, with WebP previews
and thumbnails, and server-side generation of spreadsheets, ZIP archives and
PDF documents. Rust, Postgres, and an object store: S3-compatible, Azure
Blob Storage or Google Cloud Storage.

agrafe sits between your backend and a bucket. Your backend creates a blob,
hands the browser a signed upload URL, and gets back signed URLs for the
original, a preview and a thumbnail. agrafe streams the bytes through
hashing, type sniffing, ClamAV and encryption on the way in, renders the
derived images in sandboxed workers, and never keeps a whole file in memory.

It runs as a standalone service (the `agrafe` binary), or inside another
[axum](https://github.com/tokio-rs/axum) application: the `agrafe` crate is
the service as a library with a Rust private API, and `agrafe-axum`
provides its two HTTP interfaces as routers to nest.

The documentation, a guide and a reference for the configuration, both
HTTP APIs and the errors, is at <https://tchak.github.io/agrafe/>, built
from `docs/`. See [DESIGN.md](DESIGN.md) for the architecture and the
reasoning behind every decision.

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
  key-encryption key from your config, or from a file read once at startup
  (a systemd credential, a FIFO fed at boot) so the KEK never rests on the
  host. Rotating a KEK touches the database only, never the bucket.
- **Streaming AEAD** in Tink's AES-GCM-HKDF-STREAMING format, validated
  against cross-language test vectors, with random access for `Range`
  requests.
- **ClamAV scanning**, inline for small files and as a job for large ones
  or when clamd is unreachable. Nothing is rendered or served before it is
  clean.
- **Sandboxed rendering**: no user file is ever decoded in the server
  process. `vips` and the four helper binaries run as subprocesses with
  resource limits and a timeout, confined by Landlock and seccomp on Linux
  to the file they read and the directory they write, with no network,
  and the file parsers are not even linked into the server.
- **Safe serving**: originals carry `nosniff` and a `sandbox` CSP, so an
  uploaded HTML or SVG never runs as a page of your origin.

### Previews and thumbnails

- **WebP preview and thumbnail** for images (JPEG, PNG, GIF, WebP, TIFF,
  HEIF/AVIF, SVG, BMP, JPEG 2000), and for PDF, DOCX and ODT files that
  wrap a single image.
- **Embedded image extraction**: a scan wrapped in a PDF, a Word or an ODT
  file gets the picture as its preview, not a page with a picture on it.
- **First-page thumbnails** for other PDFs, which have no preview: a
  document needs a viewer, not a picture. DOCX, ODT, XLSX and ODS files
  get one too when an optional LibreOffice is configured, see "Thumbnails
  for office files" below; without it they get the icon.
- **Text thumbnails** showing the first lines of text, CSV and TSV files.
- **Maps for GeoJSON files**: the geometry drawn over a WMS background of
  your choosing, none by default, in the file's own
  [simplestyle](https://github.com/mapbox/simplestyle-spec) colours or a
  configured default. A `.geojson` uploaded as JSON or text is recognised
  as one.
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
- **PDF documents**: send a workspace, a [typst](https://typst.app)
  template and its assets by blob id and the document's data inline, and
  get the compiled PDF back, from a compiler embedded in a sandboxed
  helper with its own fonts.
- **Sent in parts**: large sources arrive as many short requests, so a
  dropped connection or a deploy costs one part, not the whole export.
- **Built by jobs**, streamed into the bucket without touching local disk,
  sized up to hundreds of gigabytes.

### Operations

- **Postgres job queue** with `SKIP LOCKED` and `LISTEN/NOTIFY`, enqueued
  in the same transaction as the state change it follows.
- **Per-kind worker pools** for fetch, scan, render, rows, archive,
  document, purge, reap and notify, so a slow kind never holds up another,
  with retries with backoff and dead-job inspection.
- **Webhook** telling the backend when a blob is `ready`, `infected`,
  `failed` or `expired`, carrying the blob's full metadata, signed the
  Standard Webhooks way, enqueued with the state change it reports and
  retried through the same queue.
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
- **Storage backends**: local filesystem, any S3-compatible store, Azure
  Blob Storage or Google Cloud Storage.
- **One TOML file**, every key overridable by `AGRAFE_` environment
  variables, secrets redacted from debug output.

## Quick start

You need Postgres, [libvips](https://www.libvips.org/) 8.15 or newer with
its `vips` command on the `PATH`, and a reachable `clamd` for virus
scanning. On macOS, `brew install vips clamav`; on Debian and Ubuntu,
`apt install libvips-tools clamav-daemon` (Ubuntu 24.04 and Debian trixie
and later ship a recent enough libvips).

8.15 is what the renderer's options need, not what is safe to expose to
uploads: libvips decodes untrusted files, and the TIFF, EXIF and PDF
loaders it is handed have had heap overflows fixed as late as 8.18.3. In
production, run upstream 8.18.3 or newer, or a distribution build that
backports the fixes, such as Debian trixie's with its security updates.
The version number alone does not tell: Debian's patched build still says
8.16.1, while Ubuntu 26.04's 8.18.0 is in universe and has none of the
fixes. See the [libvips security
advisories](https://github.com/libvips/libvips/security/advisories).

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
fetch, with a fresh URL on the row if the backend created a new blob. The
webhook below reports the `ready` and the `failed`. See
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

Declare the columns, send the rows as JSONL, and wait for the `ready`
event, or poll until the blob is `ready`. The content type picks the format: `text/csv`, XLSX or ODS.

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

### Compose a PDF document

Upload the template once as an ordinary blob, attached to a record of
yours so it stays, and keep its id. Each document is then a workspace:
the template and its assets by blob id, the document's own data inline.
The PDF is compiled by [typst](https://typst.app) in a sandboxed helper.

```sh
TEMPLATE=$(curl -s localhost:8080/blobs \
  -H 'content-type: application/json' \
  -d "{\"filename\": \"facture.typ\", \"content_type\": \"text/plain\",
       \"input_type\": \"raw\", \"checksum\": \"$(openssl dgst -sha256 -binary facture.typ | base64)\",
       \"size\": $(wc -c < facture.typ | tr -d ' '), \"skip_virus_scan\": true,
       \"attachment\": {\"record_type\": \"Template\", \"record_id\": \"facture\", \"name\": \"source\"}}" \
  | jq -r .id)
curl -s -X PUT "localhost:8080/blobs/$TEMPLATE" --data-binary @facture.typ > /dev/null

DOC=$(curl -s localhost:8080/blobs \
  -H 'content-type: application/json' \
  -d '{"filename": "facture-12.pdf", "content_type": "application/pdf",
       "input_type": "document"}' | jq -r .id)

printf '%s\n' \
  "{\"path\": \"main.typ\", \"blob_id\": \"$TEMPLATE\"}" \
  "{\"path\": \"logo.png\", \"blob_id\": \"$LOGO\"}" \
  '{"path": "data.json", "json": {"client": "Ada", "lignes": [{"libelle": "Conseil", "montant": "1 200,00"}]}}' \
  | curl -s -X PUT "localhost:8080/blobs/$DOC" --data-binary @- | jq .state
```

The template is plain typst: `#let d = json("data.json")`,
`#image("logo.png")`, `#include "lib/header.typ"`, every path relative to
the workspace. Fonts are the `.ttf` and `.otf` files of the workspace plus
the compiler's embedded ones; packages are not available. `main` at
creation names another entry file than `main.typ`. A template that does
not compile leaves the compiler's diagnostics in the blob's `last_error`.

### Get told when a blob is ready

Rather than polling, configure one URL and agrafe `POST`s to it when a
blob reaches a state the backend waits for: `ready` (scan passed or
skipped, build finished), `infected` (terminal), `failed` (the job that
was to finish the blob died; retry it or give up) and `expired` (the
reaper recorded it, so an attached blob whose upload never came).

```toml
[webhook]
url = "https://backend.example/agrafe/events"
secret = "whsec_…"   # echo "whsec_$(openssl rand -base64 24)", or AGRAFE_WEBHOOK__SECRET
# events = ["ready", "failed"]
# allow_insecure = true   # for an http URL, in development only
```

Each delivery is a JSON body with the event, when it happened, the
attempt number and the blob exactly as `GET /blobs/:id` answers at that
moment, attachments and URLs included, so a late or repeated delivery
tells the backend what is, not what was. Two deliveries of one blob can
cross, so apply `blob.state`, not the event name:

```json
{
  "event": "ready",
  "occurred_at": "2026-10-19T09:12:03.418Z",
  "attempt": 1,
  "blob": {"id": "019a…", "state": "ready", "attachments": [{"record_type": "Dossier", "record_id": "42", "name": "piece_justificative", …}], …}
}
```

The headers follow [Standard Webhooks](https://www.standardwebhooks.com/):
`webhook-id` (the delivery's id, the same on every attempt of one event),
`webhook-timestamp` (Unix seconds) and `webhook-signature`, `v1,` and the
base64 of an HMAC-SHA256 over `id.timestamp.body` with the secret. Any of
the standard's libraries verifies it; by hand, in Ruby:

```ruby
def verify!(secret, headers, raw_body)
  id, timestamp, signatures = headers.values_at("webhook-id", "webhook-timestamp", "webhook-signature")
  raise "stale" if (Time.now.to_i - timestamp.to_i).abs > 300
  key = Base64.strict_decode64(secret.delete_prefix("whsec_"))
  expected = "v1," + Base64.strict_encode64(OpenSSL::HMAC.digest("SHA256", key, "#{id}.#{timestamp}.#{raw_body}"))
  raise "bad signature" unless signatures.split(" ").any? { |s| Rack::Utils.secure_compare(s, expected) }
end
```

A `2xx` within `webhook.timeout` (ten seconds) is delivered. Anything
else is retried like any job, 30 s after the first failure, doubling up to
an hour, five attempts in all, then the delivery is a dead `notify` job:
`GET /jobs?kind=notify&state=dead` lists them with the receiver's answer
in `last_error`, and `POST /jobs/:id/retry` sends again once the backend
is back. Nothing is enqueued without a `url`. See
[DESIGN.md](DESIGN.md#webhooks) for the events and the guarantees.

### List, delete and operate

```sh
curl -s 'localhost:8080/blobs?category=image&state=ready&limit=50' | jq '.items[].filename'
curl -s 'localhost:8080/blobs?attached=false' | jq '.items[].id'
curl -s -X DELETE "localhost:8080/blobs/$ID"

curl -s 'localhost:8080/jobs?state=dead' | jq
curl -s 'localhost:8080/jobs?kind=notify&state=dead' | jq '.items[] | {id, blob_id, last_error}'
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
them. Set `server.private_hosts` to the names the port is reached under
as well, so that a page cannot re-point its own name at the port (DNS
rebinding) and read it from the operator's browser. Both dashboard keys
are off by default.

A host application nests `agrafe_dashboard::dashboard_router` wherever it
likes, behind its own authentication; the pages find their prefix from
the request.

### Thumbnails for office files

A DOCX, ODT, XLSX or ODS that is not a wrapped picture gets the category
icon, unless a LibreOffice is configured to convert it: then its first
page is the thumbnail, rendered like a PDF page, and there is still no
preview.

```toml
[media.office]
command = "soffice"
```

LibreOffice runs as one short-lived process per document, in the sandbox
like `vips`, with a private user profile under the job's work directory.
On Debian and Ubuntu, install `libreoffice-writer-nogui`,
`libreoffice-calc-nogui`, `fonts-dejavu-core` and `fonts-liberation`
(about 400 MB). The fonts are not optional: without them a page renders
as boxes and LibreOffice exits 0, which nothing reports. Turning the
converter on later gives existing blobs their thumbnail through a
`media.variant_version` bump, no backfill. The startup probe converts a
two-page DOCX and an XLSX, so a LibreOffice without Writer or Calc, or
too old for the first-page export (7.4), fails the deployment rather than
every office file.

LibreOffice is a far larger attack surface than anything else in the
sandbox, which is why it is off by default. Where it is on:

- run it on a worker that handles `render` only (`jobs.kinds`) with no
  outbound network: LibreOffice follows linked images and OLE links, and
  the sandbox bounds resources, not egress. The API image never needs it;
- LibreOffice needs a unix socket for its single-instance pipe, so the
  sandbox lets it have unix sockets, and only those, where every other
  tool has none. A local Postgres listening on a unix socket with peer
  authentication for the worker's user would be reachable from it:
  connect to Postgres over TCP on that worker, or run it where there is
  no such socket;
- the pipe lives in `/tmp`, where the sandbox lets LibreOffice create and
  remove socket files: set `media.tmp_dir` to a directory outside `/tmp`,
  so that no other job's files are there;
- `[media.office.sandbox]` gives it its own limits, 2 GiB of address space
  and 120 s by default.

A fresh user profile costs LibreOffice a little startup time on every
run. To skip it, build a warmed profile into the image with a throwaway
conversion and point `profile_template` at it; it is copied into the work
directory per run, so every run still has a private profile:

```dockerfile
RUN echo warm > /tmp/warm.txt \
 && soffice -env:UserInstallation=file:///var/lib/agrafe/soffice-profile \
      --headless --norestore --nologo --convert-to pdf --outdir /tmp /tmp/warm.txt \
 && rm /tmp/warm.txt /tmp/warm.pdf
```

```toml
[media.office]
command = "soffice"
profile_template = "/var/lib/agrafe/soffice-profile"
```

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
| `agrafe-sandbox` | Subprocesses with resource limits, Landlock and seccomp |
| `agrafe-render` | What renders and how: `vips`, the helper drivers, the icons |
| `agrafe-extract` | Sandboxed helper that pulls the image out of a PDF, DOCX or ODT |
| `agrafe-render-text` | Sandboxed helper that rasterises a text sample |
| `agrafe-render-document` | Sandboxed helper that compiles a typst workspace into a PDF |
| `agrafe-render-geo` | Sandboxed helper that draws a GeoJSON file as SVG over the map extent it chose |

A deployment ships five binaries together: `agrafe`, `agrafe-extract`,
`agrafe-render-text`, `agrafe-render-document` and `agrafe-render-geo`.
On Linux the workers confine each of them, and `vips`, with Landlock and
seccomp by default, with nothing to install; a default Docker container
has both. Set `media.sandbox.isolation = "required"` in production so a
kernel without them fails the deployment instead of running unconfined.
The render workers are the one place that talks to the network besides the
fetch and webhook jobs: with `media.geojson.layers` configured they fetch
map backgrounds from those hosts over HTTPS, and with the default empty
list they fetch nothing.

## Development

```sh
createdb agrafe_test
cargo nextest run --workspace
cargo clippy --all-targets
cargo fmt
```

[cargo-nextest](https://nexte.st) runs each test in a process of its own,
all at once, and kills one still running after a minute
(`.config/nextest.toml`); `cargo test` works too, slower, and only
aborts a stuck API test after two minutes.

Tests connect as the current OS user to `agrafe_test`; set `DATABASE_URL`
to override. They need `vips` on the `PATH` as well. The integration
tests, in `agrafe-axum` and `agrafe-dashboard`, use `#[sqlx::test]`: each
creates a database of its own on that server, migrates it, and drops it
when it passes, so the role needs `CREATEDB`. A failed test leaves its
database behind for a look; the next run of that test drops it. The
database is named after the test, so two runs against one server at once
fail each other's tests: run one at a time, or give each its own server.
The API tests run the helper binaries from `target/`: a workspace run
builds them, and `-p agrafe-axum` builds them on demand. ClamAV is faked
in tests. The office thumbnail tests need `soffice` on the `PATH` and
skip with a message without it; CI and the Lima VM have it.

The files under `fixtures/` and the renderer probe files under
`crates/agrafe-render/assets/probe` are built by `fixtures/make.py` (needs
Pillow), except the probe's typst workspace under `probe/document`, which
is written by hand; the placeholder icons by `scripts/icons.sh`, the
dashboard's stylesheet by `scripts/daisyui.sh`; all outputs are committed.

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
