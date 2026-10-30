# Concepts

## Blobs

A **blob** is one file and its metadata. It is identified by a UUID v7, so
ids sort by creation time. A blob is created first, with metadata only, and
receives its bytes afterwards, exactly once.

Every blob has:

- a `filename`, used in URLs and in `Content-Disposition`;
- a `content_type`, normalised to `type/subtype` and checked against the
  bytes when they arrive;
- a `category` derived from the content type: `image`, `document`,
  `spreadsheet`, `presentation`, `archive`, `audio`, `video`, `text` or
  `other`;
- an `input_type`, which says how the bytes arrive;
- a `priority`, `high`, `normal` or `low`, which orders its jobs within
  their queue.

### Input types

| Input type | How the bytes arrive | Produces | Ports |
|---|---|---|---|
| `raw` | The file as the `PUT` body | The file | private and public |
| `multipart` | A `multipart/form-data` body with one part named `file` | The file | private and public |
| `url` | Downloaded by the service from a URL given at creation | The file | private only |
| `rows` | JSONL rows against declared columns | CSV, XLSX or ODS | private only |
| `archive` | A JSONL manifest of paths and blob ids | ZIP | private only |
| `document` | A JSONL typst workspace manifest | PDF, DOCX or ODT | private only |

`rows`, `archive` and `document` blobs are **generated**: agrafe builds the
file in a job after the request. `url`, `rows`, `archive` and `document`
are private-only, because each makes the service read other blobs or the
network on the creator's say-so.

### Write-once

A blob receives bytes once. A failed upload leaves no trace and the blob can
be tried again; a successful one cannot be repeated, and any later `PUT`
answers `409`. A leaked upload URL is therefore worthless after the first
upload, and a URL for an original is truthful for its whole lifetime.

To replace a file, create a new blob, attach it, and detach the old one,
which then expires on its own.

## States

```text
created ──► uploading ──► uploaded ──► ready
   ▲            │   │         │
   └────────────┘   │         └──► infected
   (failed upload,  └──► ready | infected   (scanned inline, or skipped)
    nothing stored)

any non-terminal ──► expired
any unattached   ──► deleted
```

| State | Meaning | Readable |
|---|---|---|
| `created` | The row exists, no bytes yet. A `PUT` is allowed. A generated blob collects its source parts here; a `url` blob waits for its fetch. | no |
| `uploading` | An upload or a fetch is in flight. A concurrent `PUT` gets `409`. | no |
| `uploaded` | The bytes are stored, encrypted. A job still has to finish the blob: the virus scan, or the build of a generated file. | no |
| `ready` | Scanned clean, scan skipped, or built. | **yes** |
| `infected` | The scan found something. Terminal. | never |
| `expired` | Past its `expires_at`. Terminal; objects purged. | never |
| `deleted` | Deleted by the backend. Terminal; objects purged. | never |

A blob is `expired` from the instant its `expires_at` passes, not when the
background reaper gets to it. Every read compares the expiry with the
current time.

Alongside its state, a blob tracks:

- `scan_state`: `pending`, `clean`, `infected` or `skipped`;
- `thumbnail_state` and `preview_state`: `pending`, `ready`, `failed`,
  `unavailable` or `disabled` (see [Previews and
  thumbnails](representations.md));
- `failed_at` and `last_error`, set when the job that was to move the blob
  along ran out of attempts (see [Jobs and retries](jobs.md)).

### Tombstones

A deleted or expired blob keeps its row for `blobs.keep_purged`, ninety
days by default, after its objects are purged. During that time
`GET /blobs/:id` on the private port still answers with the state,
`deleted_at` and `purged_at`, so the backend can tell a blob it deleted
from an id that never existed. The public port answers `404` either way.

## Attachments

An **attachment** records that an owner in the backend uses a blob. It
is three strings: `owner_kind`, `owner_id` and `role`. A
blob can be attached any number of times. Attachments drive the blob's
lifetime: an attached blob never expires, and the last detach gives it back
its default expiry. See [Attachments and lifetime](attachments.md).

## Representations

Each blob is served as up to three things:

- the **original**, the bytes as uploaded, with `Range` support;
- a **preview**, a WebP of at most 2048 pixels for images and for documents
  that only wrap an image;
- a **thumbnail**, a 256×256 WebP that always exists, falling back to a
  category icon.

See [Serving files](serving.md) and [Previews and
thumbnails](representations.md).

## The two ports

| | Private port | Public port |
|---|---|---|
| Default address | `127.0.0.1:8080` | `127.0.0.1:8081` |
| Who calls it | Your backend, an operator | Browsers |
| Authentication | None: trusted network only | An HMAC signature in every URL |
| Terminal blobs | Reported with their real state | `404` |
| Operations | Everything | Upload, metadata, original, preview, thumbnail |

Routes on the public port have the shape of their private counterparts with
a token inserted after `/blobs`:

```text
private:  /blobs/<id>/original/<filename>
public:   /blobs/<token>/<id>/original/<filename>?<signed query>
```

## Signed URLs

A token is one path segment:

```text
v1.<key_id>.<expires_unix>.<base64url(hmac_sha256(key, message))>
message = METHOD "\n" expires_unix "\n" path_and_query_after_token
```

The signature covers the method, the expiry, the path after the token and
the query string. The query can therefore carry instructions from the
minter: `disposition`, `filename`, `wait`, and `e`, the envelope of a sealed
URL. There are no permanent URLs. See [Minting signed URLs from another
project](signed-urls.md).

## Jobs

Everything that happens after a request runs as a job in a Postgres-backed
queue: fetches, deferred scans, renders, builds, purges, webhook deliveries
and the periodic reaper. A job is enqueued in the same transaction as the
state change that calls for it, so a crash can never lose one. See [Jobs and
retries](jobs.md).
