# Errors

## Shape

Every error on both ports is JSON with one shape:

```json
{"error": {"code": "unprocessable", "message": "checksum is required for raw and multipart input"}}
```

- `code` is stable and machine-readable. Branch on it, together with the
  status.
- `message` is for humans. It names what was wrong and may change between
  versions.

This holds for failures a framework would otherwise answer in plain text:
malformed JSON, unknown query values, a path segment of the wrong shape,
unknown routes and wrong methods. Internal errors are logged in full and
answered as `internal` with the message `internal error`, no detail.

Two answers are not JSON errors: `416 Range Not Satisfiable` on an original,
which has an empty body and a `Content-Range` header, and the thumbnail
route, which answers an icon rather than an error.

## Codes

| Code | Status | Meaning |
|---|---|---|
| `bad_request` | `400` | The request is malformed: unreadable JSON or body, an unknown filter value, an id of the wrong shape, a listing limit out of range, an envelope that does not decode |
| `forbidden` | `403` | A bad or premature signature, a public upload of a blob that does not allow it, a cross-site browser request, or a `Host` outside `server.private_hosts` |
| `not_found` | `404` | No such blob, attachment, job or route, a terminal blob on the public port, a filename that does not match, or no preview |
| `method_not_allowed` | `405` | The route exists, not with this method |
| `conflict` | `409` | The operation does not fit the blob's or job's current state |
| `gone` | `410` | An expired signature, or an expired or deleted blob |
| `payload_too_large` | `413` | A body over its limit |
| `unsupported_media_type` | `415` | A JSON route called without `Content-Type: application/json` |
| `unprocessable` | `422` | The request is well-formed but invalid: a field, a checksum, a size, a type, a source line |
| `infected` | `422` | The uploaded file is infected. The blob is now `infected` |
| `internal` | `500` | agrafe's own failure: the database, the bucket, a key that does not unwrap |

## Request parsing

| Status | Code | Cause |
|---|---|---|
| `400` | `bad_request` | Malformed JSON, a non-UUID or non-integer id in the path, an unknown value for an enum filter such as `state=done` |
| `415` | `unsupported_media_type` | A JSON body without `Content-Type: application/json` |
| `422` | `unprocessable` | Valid JSON of the wrong shape: an unknown field, a missing required field, a wrong type |
| `404` | `not_found` | An unknown route |
| `405` | `method_not_allowed` | A known route, another method |

## Signatures (public port)

| Status | Code | Message | Cause |
|---|---|---|---|
| `403` | `forbidden` | `invalid signature` | Malformed token, unknown key id, or a signature for another method, path or query |
| `403` | `forbidden` | `signature not valid yet` | The expiry lies beyond `signing.window` plus `signing.ttl.max`: a minter's mistake, or a cap lowered since |
| `410` | `gone` | `signature expired` | The expiry has passed |
| `404` | `not_found` | `not found` | Valid signature; no such blob, or a terminal one |
| `400` | `bad_request` | the envelope's decoding error | The `e` parameter does not decode |
| `403` | `forbidden` | `this blob can only be uploaded through the private interface` | A public `PUT` on a blob that is not `raw` or `multipart`, or skips the scan |

## Private port guards

| Status | Code | Message | Cause |
|---|---|---|---|
| `403` | `forbidden` | `cross-site request refused: …` | A browser request that changes something, sent from another site |
| `403` | `forbidden` | `refused: Host "…" is not one of server.private_hosts` | `server.private_hosts` is set and the `Host` is not in it |

## Creating a blob

All `422 unprocessable`, from `POST /blobs`:

| Message | Cause |
|---|---|
| `filename must not be empty` | Blank after trimming |
| `filename is too long` | Over 255 bytes |
| `filename is not valid` | `.` or `..` |
| `filename must not contain path separators or control characters` | `/`, `\`, control or bidirectional formatting characters |
| `content_type is not a valid media type`, `content_type must not contain wildcards`, `content_type is too long` | |
| `` content_type `…` is not accepted `` | A `raw`, `multipart` or `url` blob of a type outside `upload.content_types` |
| `checksum is required for raw and multipart input`, `size is required for raw and multipart input` | |
| `checksum must be base64`, `checksum must be a base64 SHA-256 digest (32 bytes)` | |
| `checksum_md5 must be base64`, `checksum_md5 must be a base64 MD5 digest (16 bytes)` | |
| `size must not be negative`, `size exceeds the maximum of N bytes` | Over `upload.max_size` |
| `checksum, checksum_md5 and size must be omitted for generated input (rows, archive, document)` | |
| `url is required for url input`, `url applies to url input only` | |
| `url: …` | Not `https`, credentials in it, a refused literal address |
| `` content_type `…` cannot be produced from `…` input `` | A generated blob with a content type it cannot produce |
| `rows input requires columns or sheets`, `columns and sheets are mutually exclusive` | |
| `columns apply to rows input only`, `sheets apply to rows input only` | |
| `columns must name at least one column`, `a sheet holds at most 16384 columns` | |
| `column N has an empty name`, `column N has a name with control characters`, `column N has a name longer than the 32767 characters a sheet cell holds` | |
| `sheets apply to XLSX and ODS output; a CSV holds one sheet, declared with columns` | |
| `sheets must declare at least one sheet`, `a workbook holds at most 1000 sheets` | |
| `sheet N has a name that …`, `sheet N has the same name as sheet M, ignoring case`, `sheet N: …` | A sheet name or column Excel would refuse |
| `main: …`, `main applies to document input only` | |
| `expires_at and expires_in are mutually exclusive`, `expires_in is out of range`, `expiration must be in the future` | |
| `expires_at and expires_in do not apply to a blob created attached` | |
| `attachment: …` | An invalid `record_type`, `record_id` or `name` |

A body over 16 MiB answers `413 payload_too_large`.

## Uploading

From `PUT /blobs/:id` on either port, and `PUT /blobs/:id/parts/:n`:

| Status | Code | Message | Cause |
|---|---|---|---|
| `404` | `not_found` | `no such blob` | |
| `409` | `conflict` | `blob is uploading and cannot be uploaded` | Another upload is in flight |
| `409` | `conflict` | `blob is ready and cannot be uploaded` | Write-once: it already has its bytes. Any state but `created` |
| `409` | `conflict` | `a rows blob is built from a source, not uploaded as a file` | |
| `409` | `conflict` | `a url blob is fetched by the service from its url, not uploaded` | |
| `409` | `conflict` | `parts and finish apply to rows, archive and document blobs, not to a raw blob` | |
| `409` | `conflict` | `expected part N` | A part out of sequence, or part 1 already stored |
| `409` | `conflict` | `the source has no parts` | A finish with nothing to build |
| `413` | `payload_too_large` | `request body exceeds the upload size limit` | |
| `413` | `payload_too_large` | `part exceeds the maximum of N bytes` | |
| `422` | `unprocessable` | `Content-Length N does not match the declared size M` | Refused before reading |
| `422` | `unprocessable` | `body size does not match the declared size` | |
| `422` | `unprocessable` | `body exceeds the maximum of N bytes` | A `url` blob with no declared size |
| `422` | `unprocessable` | `body checksum does not match the declared checksum` | |
| `422` | `unprocessable` | `body MD5 does not match the declared checksum_md5` | |
| `422` | `unprocessable` | `` content looks like `…`, not `…` `` | The sniffed type contradicts the declared one |
| `422` | `unprocessable` | `` content type `…` is not accepted `` | The declared type is no longer in `upload.content_types`, or the bytes of a blob declared as `application/octet-stream` are of a type outside it |
| `422` | `unprocessable` | ``multipart body has no part named `file` `` | |
| `422` | `unprocessable` | `line N: …` | A source line failed its rules, see below |
| `422` | `unprocessable` | `a source has at most 10000 parts` | |
| `422` | `unprocessable` | `the archive would exceed the maximum of N bytes (archive.max_size)` | |
| `422` | `unprocessable` | `the workspace would exceed the maximum of N bytes (document.max_workspace_size)` | |
| `422` | `unprocessable` | `the workspace would exceed the maximum of N files (document.max_files)` | |
| `422` | `infected` | `file is infected: <signature>` on the private port, `file is infected` on the public one | Stored; the blob is `infected` |

A `422` other than `infected` stores nothing. The blob is back in `created`
with the parts it already had, and the request can be repeated.

### Source lines

A failing line answers `line N: reason`, `N` counted from 1 within the
part. Common reasons:

| Input | Reason |
|---|---|
| any | `line is longer than 1048576 bytes`, `last line does not end with a newline` |
| `rows` | `expected a JSON array of cell values: …`, `expected N cells, one per declared column, got M` |
| `rows` | `cell N (column): …`, such as a value that is not a scalar, a malformed date, `is a number too large for a spreadsheet cell`, or a text over the cell limit |
| `rows` | `no sheet named "…" is declared`, `sheet "…" comes before the current sheet "…"` |
| `archive`, `document` | `path must not be empty`, `path must be relative`, `path must not contain empty components or end with a slash`, ``path must not contain `.` or `..` components``, `path must not contain backslashes, control or bidirectional formatting characters`, `path is longer than N bytes` |
| `archive`, `document` | `blob … does not exist`, `blob … is uploaded, not ready` |
| `archive` | `ERRORS.txt at the root is reserved` |
| `document` | `a line needs one of blob_id, json and text`, `a line takes only one of blob_id, json and text`, ``path `…` appears twice in the part`` |

## Attachments

| Status | Code | Message | Cause |
|---|---|---|---|
| `404` | `not_found` | `not found` | Unknown blob or attachment |
| `409` | `conflict` | `blob is infected and cannot be attached` | |
| `410` | `gone` | `blob is expired and cannot be attached` | Also for `deleted`, and for a blob past its `expires_at` |
| `422` | `unprocessable` | the field and the rule it broke | Blank, over 255 bytes, control or bidirectional formatting characters |
| `400` | `bad_request` | `record_id is only accepted together with record_type` | `GET /attachments` |

## Deleting and retrying

| Route | Status | Code | Message |
|---|---|---|---|
| `DELETE /blobs/:id` | `409` | `conflict` | `blob is attached and cannot be deleted; detach it first` |
| `POST /blobs/:id/retry` | `409` | `conflict` | `nothing to retry: a ready blob waits on no job` |
| `POST /blobs/:id/retry` | `409` | `conflict` | `a job is already pending for this blob` |
| `POST /blobs/:id/retry` | `409` | `conflict` | `blob is infected` |
| `POST /blobs/:id/retry` | `410` | `gone` | `blob is expired`, `blob is deleted` |
| `POST /jobs/:id/retry`, `DELETE /jobs/:id` | `409` | `conflict` | `job is running, only a dead job can be retried or discarded` |
| `POST /jobs/:id/retry` | `409` | `conflict` | `a scan job is already pending for this blob` |

## Minting URLs

From `POST /blobs/:id/urls`, all `422 unprocessable` except the first:

| Message | Cause |
|---|---|
| `not found` (`404`) | Unknown blob |
| `ttl must be at most Ns (signing.ttl.max)` | |
| `` unknown query parameter `…`; allowed: disposition, filename, wait `` | |
| `` disposition must be inline or attachment, not `…` `` | |
| `` wait must be a number of milliseconds, not `…` ``, `wait must be at most 30000 ms` | |
| `query filename: …` | A filename that breaks the filename rules |

## Listings

`GET /blobs`, `GET /attachments` and `GET /jobs` answer `400 bad_request`
with `limit must be between 1 and 200` for a limit out of range, and for an
unknown filter value.

## Reading

| Route | Status | Code | Message |
|---|---|---|---|
| original | `409` | `conflict` | `blob is uploaded and not readable` |
| original, preview, thumbnail | `404` | `not_found` | `not found`: the filename does not match |
| original, preview, thumbnail | `400` | `bad_request` | `` disposition must be inline or attachment, not `…` ``, `filename is not valid` |
| preview | `404` | `not_found` | `preview is pending`, `preview could not be generated`, `no preview for this file`, `preview disabled for this blob` |
| preview | `409` | `conflict` | `blob is created and not readable` |
| original | `500` | `internal` | The object is missing or fails to decrypt; logged with the blob id |

A segment that fails to decrypt after the response has started cannot change
the status any more. The connection is cut and the failure is logged.

## Job errors

Jobs do not answer requests; their failures are recorded as `last_error`
on the job, and, for a job that was to finish its blob, on the blob as
`last_error` with `failed_at`. Messages name their source:

| Prefix | Source |
|---|---|
| `clamd at …` | The virus scanner: unreachable, timed out, or replied an error |
| `reading the object: …`, `blob …: …`, `reading the … of blob …`, `writing the … of blob …` | The bucket, or an object that fails to decrypt |
| `database: …` | Postgres |
| `fetch: …` | A `url` blob's download; the URL itself is never included |
| `build: …` | A `rows`, `archive` or `document` build, including a document's compiler diagnostics as `path:line:column: error: message` |
| `webhook: …` | A delivery, with the receiver's status and the head of its body |
| `purge: …` | Deleting objects |
| `temporary files: …` | The worker's `media.tmp_dir` |
| `handler panicked: …` | A bug: please report it |

## Dashboard

The dashboard answers HTML pages, not JSON, with the status in the title:
`400` for a bad filter, `403` for a cross-site action or an unknown `Host`,
`404` for an unknown path or an id of the wrong shape, and for the action
routes when `dashboard.actions` is off.

## Configuration

Startup errors are printed as one line before the process exits. See
[Configuration](configuration.md#validation-at-startup).
