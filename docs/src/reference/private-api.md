# Private HTTP API

The private port serves your backend. It has no authentication and must be
reachable only from a trusted network. Every operation of agrafe is here.

## Conventions

- **Bodies** are JSON with `Content-Type: application/json`, except upload
  bodies. Unknown fields in a JSON body are refused.
- **Ids** are UUIDs for blobs and attachments, integers for jobs. An id of
  the wrong shape answers `400`.
- **Timestamps** are RFC 3339 in UTC, such as
  `"2026-10-19T09:12:03.418273Z"`. **Durations** are strings such as
  `"30m"`, `"7d"`.
- **Listings** answer `{"items": [...], "next_cursor": ...}`. Pass
  `next_cursor` back as `cursor` for the next page; it is `null` on the
  last one. `limit` is 1 to 200, 50 by default.
- **Errors** are JSON, `{"error": {"code": "...", "message": "..."}}`. See
  [Errors](errors.md).
- **Tracing**: a W3C `traceparent` header is continued: the request and
  the jobs it enqueues log the caller's trace id. See
  [Monitoring](../guide/monitoring.md#following-a-request-to-its-jobs).
- **Cross-site browser requests** that change anything are refused with
  `403`, and so is any `Host` outside `server.private_hosts` when it is set.
  See [Security model](../guide/security.md#browsers-on-the-private-port).

## Routes

| Method | Path | Purpose |
|---|---|---|
| `POST` | [`/blobs`](#post-blobs) | Create a blob |
| `GET` | [`/blobs`](#get-blobs) | List blobs |
| `GET` | [`/blobs/:id`](#get-blobsid) | Full metadata |
| `PUT` | [`/blobs/:id`](#put-blobsid) | Upload the file, or the whole source of a generated blob |
| `PUT` | [`/blobs/:id/parts/:n`](#put-blobsidpartsn) | Append part `n` of a source |
| `POST` | [`/blobs/:id/finish`](#post-blobsidfinish) | Seal a source and enqueue its build |
| `DELETE` | [`/blobs/:id`](#delete-blobsid) | Soft delete an unattached blob |
| `POST` | [`/blobs/:id/retry`](#post-blobsidretry) | Enqueue whatever the blob waits on |
| `POST` | [`/blobs/:id/urls`](#post-blobsidurls) | Mint signed public URLs |
| `GET` | [`/blobs/:id/original/:filename`](#get-blobsidoriginalfilename) | Original bytes |
| `GET` | [`/blobs/:id/preview/:filename`](#get-blobsidpreviewfilename) | Preview |
| `GET` | [`/blobs/:id/thumbnail/:filename`](#get-blobsidthumbnailfilename) | Thumbnail |
| `POST` | [`/blobs/:id/attachments`](#post-blobsidattachments) | Attach to a record |
| `GET` | [`/attachments`](#get-attachments) | List attachments |
| `GET` | [`/attachments/:id`](#get-attachmentsid) | One attachment |
| `DELETE` | [`/attachments/:id`](#delete-attachmentsid) | Detach |
| `GET` | [`/jobs`](#get-jobs) | List jobs |
| `GET` | [`/jobs/:id`](#get-jobsid) | One job |
| `POST` | [`/jobs/:id/retry`](#post-jobsidretry) | Retry a dead job |
| `DELETE` | [`/jobs/:id`](#delete-jobsid) | Discard a dead job |
| `GET` | [`/metrics`](#get-metrics) | Prometheus metrics |
| `GET` | [`/healthz`](#get-healthz) | Liveness |
| `GET` | `/dashboard` | The HTML dashboard, when `dashboard.enabled` |

## Blobs

### Blob metadata

Every blob route answers with the full metadata, the same object the
[webhook](../guide/webhooks.md) delivers.

```json
{
  "id": "0199f6a0-5b2c-7c3d-8e4f-0a1b2c3d4e5f",
  "state": "ready",
  "filename": "scan.jpg",
  "content_type": "image/jpeg",
  "category": "image",
  "input_type": "raw",
  "checksum": "n4bQgYhMfWWaL+qgxVrQFaO/TxsrC4Is0V1sFbDwCgg=",
  "checksum_md5": "kDU2JDbbJuRTYpXJ7p6T/Q==",
  "size": 482113,
  "url": null,
  "skip_virus_scan": false,
  "public_upload_allowed": true,
  "expires_at": null,
  "priority": "normal",
  "scan_state": "clean",
  "scanned_at": "2026-10-19T09:12:03.418273Z",
  "scan_signature": null,
  "thumbnail_state": "ready",
  "preview_state": "ready",
  "preview_source": "image",
  "source_parts": null,
  "source_rows": null,
  "source_size": null,
  "archive_entries": null,
  "archive_errors": null,
  "created_at": "2026-10-19T09:11:58.102937Z",
  "updated_at": "2026-10-19T09:12:05.771204Z",
  "uploaded_at": "2026-10-19T09:12:03.418273Z",
  "deleted_at": null,
  "purged_at": null,
  "failed_at": null,
  "last_error": null,
  "attachments": [
    {
      "id": "0199f6a1-0a2b-7c3d-8e4f-1a2b3c4d5e6f",
      "blob_id": "0199f6a0-5b2c-7c3d-8e4f-0a1b2c3d4e5f",
      "record_type": "Dossier",
      "record_id": "42",
      "name": "piece_justificative",
      "created_at": "2026-10-19T09:14:20.004417Z"
    }
  ],
  "urls": {
    "upload": "https://files.example/blobs/v1.k1.1792310400.…/0199f6a0-…",
    "metadata": "https://files.example/blobs/v1.k1.1792393200.…/0199f6a0-…",
    "original": "https://files.example/blobs/v1.k1.1792393200.…/0199f6a0-…/original/scan.jpg?e=…",
    "preview": "https://files.example/blobs/v1.k1.1792393200.…/0199f6a0-…/preview/scan.jpg?e=…",
    "thumbnail": "https://files.example/blobs/v1.k1.1792393200.…/0199f6a0-…/thumbnail/scan.jpg?e=…",
    "expires_at": 1792393200
  },
  "envelope": "AQAEa2VrMQA8AAECAwQF…"
}
```

| Field | Type | Description |
|---|---|---|
| `id` | UUID | UUID v7: ids sort by creation time |
| `state` | string | `created`, `uploading`, `uploaded`, `ready`, `infected`, `expired`, `deleted` |
| `filename` | string | |
| `content_type` | string | Normalised to `type/subtype` |
| `category` | string | `image`, `document`, `spreadsheet`, `presentation`, `archive`, `audio`, `video`, `text`, `other` |
| `input_type` | string | `raw`, `multipart`, `url`, `rows`, `archive`, `document` |
| `checksum` | string or null | SHA-256, base64. As declared, or computed for `url` and generated blobs |
| `checksum_md5` | string or null | MD5, base64, the form of Active Storage's `checksum`. As declared, or computed once the bytes are in |
| `size` | integer or null | Bytes. For an archive, an estimate until `ready`; for a document, null until `ready` |
| `url` | string or null | Where a `url` blob is fetched from, until it has been. A secret |
| `skip_virus_scan` | boolean | Always `true` for generated blobs |
| `public_upload_allowed` | boolean | Whether `urls.upload` exists: `raw` or `multipart`, scanned |
| `expires_at` | timestamp or null | Null while attached |
| `priority` | string | `high`, `normal`, `low` |
| `scan_state` | string | `pending`, `clean`, `infected`, `skipped` |
| `scanned_at` | timestamp or null | |
| `scan_signature` | string or null | What `clamd` found, for an infected blob |
| `thumbnail_state`, `preview_state` | string | `pending`, `ready`, `failed`, `unavailable`, `disabled` |
| `preview_source` | string or null | `image`, `embedded_image` or `geojson` once a preview is rendered |
| `source_parts`, `source_rows`, `source_size` | integer or null | For generated blobs: parts accepted, lines and bytes across them. Null otherwise |
| `archive_entries`, `archive_errors` | integer or null | For a built archive: members written and members skipped |
| `created_at`, `updated_at` | timestamp | |
| `uploaded_at`, `deleted_at`, `purged_at` | timestamp or null | `purged_at` is set once the objects are gone |
| `failed_at`, `last_error` | timestamp, string or null | Set while the job that was to finish the blob is dead |
| `attachments` | array | The records using the blob, oldest first |
| `urls` | object | Signed public URLs, see [Serving files](../guide/serving.md#urls-in-the-metadata) |
| `envelope` | string or null | For a `ready`, unexpired blob: the opaque envelope for [sealed URLs](../guide/signed-urls.md#sealed-urls) |

### `POST /blobs`

Creates a blob, with metadata only. Answers `201` with the blob metadata.
The body is limited to 16 MiB.

| Field | Type | Required | Description |
|---|---|---|---|
| `filename` | string | yes | Trimmed; at most 255 bytes; not `.` or `..`; no `/`, `\`, control or bidirectional formatting characters |
| `content_type` | string | yes | A media type without wildcards; parameters are dropped |
| `input_type` | string | yes | `raw`, `multipart`, `url`, `rows`, `archive`, `document` |
| `checksum` | string | `raw`, `multipart` | SHA-256 of the bytes, base64. Optional for `url`. Refused, like `checksum_md5` and `size`, for generated blobs |
| `checksum_md5` | string | no | MD5 of the bytes, base64, for `raw`, `multipart` and `url` |
| `size` | integer | `raw`, `multipart` | Bytes, at most `upload.max_size`. Optional for `url` |
| `url` | string | `url` | `https`, no credentials, no private address |
| `columns` | array | `rows`, or `sheets` | `[{"name": "...", "type": "date" \| "datetime"}]`, `type` optional |
| `sheets` | array | `rows`, or `columns` | `[{"name": "...", "columns": [...]}]`, XLSX and ODS only |
| `main` | string | no | Entry file of a `document` workspace, `main.typ` by default |
| `skip_virus_scan` | boolean | no | Default `false`. Forced `true` for generated blobs |
| `priority` | string | no | `high`, `normal` (default), `low` |
| `representations` | array | no | Among `thumbnail` and `preview`; both by default |
| `expires_at` | timestamp | no | In the future. Not with `expires_in` or `attachment` |
| `expires_in` | duration | no | Not with `expires_at` or `attachment`. Default `blobs.default_expires_in` |
| `attachment` | object | no | `{"record_type", "record_id", "name"}`: attach in the same transaction; the blob then has no expiry |

The content types a generated blob may declare:

| Input type | Content type |
|---|---|
| `rows` | `text/csv`, `application/vnd.openxmlformats-officedocument.spreadsheetml.sheet`, `application/vnd.oasis.opendocument.spreadsheet` |
| `archive` | `application/zip` |
| `document` | `application/pdf` |

| Status | Code | When |
|---|---|---|
| `201` | | Created |
| `400` | `bad_request` | Malformed JSON |
| `413` | `payload_too_large` | The body exceeds 16 MiB |
| `415` | `unsupported_media_type` | Missing `Content-Type: application/json` |
| `422` | `unprocessable` | A missing, unknown or invalid field; the message names it |

### `GET /blobs`

Lists blobs, newest first, with the full metadata of each.

| Parameter | Values |
|---|---|
| `content_type` | A media type |
| `category` | A category |
| `state` | A blob state |
| `scan_state` | `pending`, `clean`, `infected`, `skipped` |
| `attached` | `true` or `false` |
| `failed` | `true` or `false`: whether the job that was to finish the blob died |
| `cursor` | A blob id, the previous page's `next_cursor` |
| `limit` | 1 to 200, default 50 |

An unknown value or a limit out of range answers `400`.

### `GET /blobs/:id`

The full metadata. Terminal blobs are reported with their state as
tombstones, until `blobs.keep_purged` after their purge. Answers `404` for
an unknown id.

### `PUT /blobs/:id`

Sends the bytes, once.

| Input type | Body | Afterwards |
|---|---|---|
| `raw` | The file | `ready`, or `uploaded` while a scan is deferred |
| `multipart` | `multipart/form-data` with a part named `file` | Same |
| `rows`, `archive`, `document` | The whole JSONL source: part 1 and the finish in one call | `uploaded` until the build is done |
| `url` | Refused: the service fetches it | |

Answers `200` with the blob metadata.

| Status | Code | When |
|---|---|---|
| `400` | `bad_request` | The body could not be read, or a multipart body has no valid boundary |
| `404` | `not_found` | Unknown blob |
| `409` | `conflict` | Not `created`, already uploaded, an upload in flight, a `url` blob, or a generated blob that already has parts |
| `413` | `payload_too_large` | The body exceeds the upload limit |
| `422` | `unprocessable` | Size, checksum, MD5 or content type mismatch; a bad source line, named with its line number; a source over its limits |
| `422` | `infected` | The scan found something; the message names the signature. The blob is now `infected` |

### `PUT /blobs/:id/parts/:n`

Appends part `n` of a `rows`, `archive` or `document` source. Parts are
numbered from 1 and consecutive, at most 10 000. A part is whole lines, each
at most a megabyte, ending with a newline. Answers `200` with the blob
metadata, still `created`, with `source_parts` counting the parts stored.

| Status | Code | When |
|---|---|---|
| `409` | `conflict` | Not the next part (`expected part N`), not a generated blob, or the blob is not `created` |
| `413` | `payload_too_large` | The part exceeds `upload.max_size` |
| `422` | `unprocessable` | A bad line, as `line N: reason`; too many parts; the archive or workspace over its limit. The part is discarded; earlier parts are kept |

### `POST /blobs/:id/finish`

Seals a source sent in parts and enqueues its build. The blob moves to
`uploaded`. Answers `200` with the blob metadata.

| Status | Code | When |
|---|---|---|
| `409` | `conflict` | No parts (`the source has no parts`), not a generated blob, or not `created` |

### `DELETE /blobs/:id`

Soft deletes an unattached blob. Its objects are purged
`upload.purge_delay` later. Answers `204`.

| Status | Code | When |
|---|---|---|
| `404` | `not_found` | Unknown blob |
| `409` | `conflict` | `blob is attached and cannot be deleted; detach it first` |

### `POST /blobs/:id/retry`

Enqueues whatever the blob waits on, at the blob's priority: the fetch of a
`created` `url` blob, the scan or build of an `uploaded` blob, the render
of a `ready` blob with a representation `pending`. Deletes the dead jobs it
replaces and clears `failed_at`. Answers `202` with the blob metadata.

| Status | Code | When |
|---|---|---|
| `404` | `not_found` | Unknown blob |
| `409` | `conflict` | The blob waits on no job (`nothing to retry: …`), its job is already pending or running, or it is `infected` |
| `410` | `gone` | The blob is `expired` or `deleted` |

### `POST /blobs/:id/urls`

Mints signed public URLs. The body is optional.

```json
{"ttl": "15m", "query": {"disposition": "attachment", "filename": "facture.pdf", "wait": "5000"}}
```

| Field | Description |
|---|---|
| `ttl` | Lifetime of the read and metadata URLs, at most `signing.ttl.max` |
| `query.disposition` | `inline` or `attachment` |
| `query.filename` | A download name, validated like a filename |
| `query.wait` | Milliseconds, `0` to `30000` |

Answers `200` with a `urls` object:

```json
{
  "upload": "https://files.example/blobs/…/0199f6a0-…",
  "metadata": "https://files.example/blobs/…/0199f6a0-…",
  "original": "https://files.example/blobs/…/0199f6a0-…/original/scan.jpg?disposition=attachment&e=…",
  "preview": "https://files.example/blobs/…/0199f6a0-…/preview/scan.jpg?disposition=attachment&e=…",
  "thumbnail": "https://files.example/blobs/…/0199f6a0-…/thumbnail/scan.jpg?disposition=attachment&e=…",
  "expires_at": 1792311300
}
```

`upload` is set when `public_upload_allowed`, `null` otherwise, whatever the
blob's state: once the bytes are in, a `PUT` to it answers `409`. The read
URLs are sealed
when the blob is `ready`, and expire no later than the blob's `expires_at`.

| Status | Code | When |
|---|---|---|
| `404` | `not_found` | Unknown blob |
| `422` | `unprocessable` | `ttl` over the cap, an unknown query parameter, or an invalid value |

### `GET /blobs/:id/original/:filename`

The original, as on the [public port](public-api.md#get-blobstokenidoriginalfilename),
without a token. Takes `disposition` and `filename` in the query, unsigned.
A blob that is not `ready`, terminal ones included, answers `409`.

### `GET /blobs/:id/preview/:filename`

The preview, as on the [public port](public-api.md#get-blobstokenidpreviewfilename).
Takes `disposition`, `filename` and `wait`. A terminal blob answers `409`.

### `GET /blobs/:id/thumbnail/:filename`

The thumbnail, as on the [public port](public-api.md#get-blobstokenidthumbnailfilename).
Takes `disposition`, `filename` and `wait`. A terminal blob answers the
icon with `X-Representation-State: unavailable`.

## Attachments

### Attachment

```json
{
  "id": "0199f6a1-0a2b-7c3d-8e4f-1a2b3c4d5e6f",
  "blob_id": "0199f6a0-5b2c-7c3d-8e4f-0a1b2c3d4e5f",
  "record_type": "Dossier",
  "record_id": "42",
  "name": "piece_justificative",
  "created_at": "2026-10-19T09:14:20.004417Z"
}
```

### `POST /blobs/:id/attachments`

Attaches the blob to a record, and clears its expiry.

```json
{"record_type": "Dossier", "record_id": "42", "name": "piece_justificative"}
```

Each string is not blank, at most 255 bytes, without control or
bidirectional formatting characters, and not trimmed.

| Status | Code | When |
|---|---|---|
| `201` | | Attached |
| `200` | | Already attached: the existing attachment |
| `404` | `not_found` | Unknown blob |
| `409` | `conflict` | The blob is `infected` |
| `410` | `gone` | The blob is `expired` or `deleted`, or past its `expires_at` |
| `422` | `unprocessable` | An invalid string |

### `GET /attachments`

Lists attachments, newest first. Each item is an attachment plus its blob's state:

```json
{
  "items": [
    {
      "id": "0199f6a1-…",
      "blob_id": "0199f6a0-…",
      "record_type": "Dossier",
      "record_id": "42",
      "name": "piece_justificative",
      "created_at": "2026-10-19T09:14:20.004417Z",
      "blob_state": "ready",
      "blob_failed_at": null
    }
  ],
  "next_cursor": null
}
```

| Parameter | Values |
|---|---|
| `record_type` | |
| `record_id` | Only with `record_type`; alone it is a `400` |
| `name` | |
| `blob_id` | A blob id |
| `cursor` | An attachment id |
| `limit` | 1 to 200, default 50 |

### `GET /attachments/:id`

One attachment. `404` when unknown.

### `DELETE /attachments/:id`

Detaches. The last detach sets the blob's expiry to
`blobs.default_expires_in` from now. Answers `204`, or `404` when unknown.

## Jobs

### Job

```json
{
  "id": 1842,
  "kind": "scan",
  "state": "dead",
  "blob_id": "0199f6a0-5b2c-7c3d-8e4f-0a1b2c3d4e5f",
  "priority": "normal",
  "payload": {},
  "run_at": "2026-10-19T09:40:11.000000Z",
  "attempts": 5,
  "max_attempts": 5,
  "locked_at": "2026-10-19T09:40:11.204118Z",
  "locked_by": "worker-3-0199f6…/scan/1",
  "last_error": "clamd at 127.0.0.1:3310: Connection refused (os error 111)",
  "failed_at": "2026-10-19T09:40:12.880301Z",
  "trace_id": "4bf92f3577b34da6a3ce929d0e0e4736",
  "created_at": "2026-10-19T09:12:03.418273Z"
}
```

| Field | Description |
|---|---|
| `kind` | `fetch`, `scan`, `render`, `rows`, `archive`, `document`, `purge`, `reap`, `notify`, `audit` |
| `state` | `due`, `scheduled`, `running`, `dead` |
| `blob_id` | Null for `reap` and `audit` |
| `payload` | Kind-specific, such as the event of a `notify` job |
| `run_at` | When the job becomes due |
| `attempts`, `max_attempts` | Attempts made, and allowed |
| `locked_at`, `locked_by` | The worker holding it, or the last one for a dead job |
| `last_error` | The error of the last failed attempt |
| `failed_at` | When it died |
| `trace_id` | The trace of the request or job that enqueued it, which its log lines carry; null for a job enqueued outside one. See [Monitoring](../guide/monitoring.md#following-a-request-to-its-jobs) |

### `GET /jobs`

Lists jobs, newest first.

| Parameter | Values |
|---|---|
| `kind` | A job kind |
| `state` | `due`, `scheduled`, `running`, `dead` |
| `priority` | `high`, `normal`, `low` |
| `blob_id` | A blob id |
| `cursor` | A job id |
| `limit` | 1 to 200, default 50 |

### `GET /jobs/:id`

One job. `404` when unknown.

### `POST /jobs/:id/retry`

Gives a dead job its attempts back and makes it due now. Answers `200` with
the job.

| Status | Code | When |
|---|---|---|
| `404` | `not_found` | Unknown job |
| `409` | `conflict` | The job is not dead, or its blob already has a pending job of that kind |

### `DELETE /jobs/:id`

Discards a dead job. Answers `204`.

| Status | Code | When |
|---|---|---|
| `404` | `not_found` | Unknown job |
| `409` | `conflict` | The job is not dead |

## Operations

### `GET /metrics`

Prometheus text format, version 0.0.4. See
[Monitoring](../guide/monitoring.md#metrics) for the series. Exempt from
`server.private_hosts`.

### `GET /healthz`

Liveness. Answers `200` with the body `ok`. Exempt from
`server.private_hosts`.
