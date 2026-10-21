# Public HTTP API

The public port serves browsers. Every route but `/healthz` carries a
signature token as the first segment after `/blobs`, and the signature is
verified before anything touches the database.

```text
https://files.example/blobs/<token>/<id>[/<kind>/<filename>][?<query>]
```

URLs come from the [private API](private-api.md) or from a minter holding
the signing key; see [Minting signed URLs from another
project](../guide/signed-urls.md) for the token format.

## Conventions

- **Signature first.** A request whose token is missing, malformed, signed
  with an unknown key, or signed for another method, path or query answers
  `403` before anything else. An expired token answers `410`.
- **Terminal blobs do not exist.** A blob that is `expired`, `infected` or
  `deleted` answers `404` on every route, upload included, even with a valid
  signature. So does a blob past its `expires_at` that the reaper has not
  recorded yet.
- **`HEAD`** is accepted wherever `GET` is, and verified as `GET`.
- **Errors** are JSON, `{"error": {"code": "...", "message": "..."}}`. See
  [Errors](errors.md).
- **CORS** headers are sent to the origins `server.cors.allowed_origins`
  lists, or to any with `["*"]`, on every response, errors included; none
  by default. Allowed: `GET`, `HEAD`, `PUT`, and the request headers
  `content-type`, `range`, `if-range`, `if-none-match`,
  `if-modified-since`. Exposed: `content-disposition`, `content-range`,
  `accept-ranges`, `etag`, `last-modified`. Never credentials.
- **Preflights** (`OPTIONS` with `access-control-request-method`) are
  answered without a signature when CORS is on, and touch nothing. With
  CORS off, `OPTIONS` is a `405`.

## `GET /healthz`

Liveness. Answers `200` with the body `ok`.

## `PUT /blobs/:token/:id`

Uploads the file of a `raw` or `multipart` blob, once.

**Body.** For a `raw` blob, the file bytes. For a `multipart` blob, a
`multipart/form-data` body with one part named `file`.

**Response.** `200` with the [public metadata](#public-metadata) of the
blob, now `uploaded` or `ready`.

| Status | Code | When |
|---|---|---|
| `403` | `forbidden` | The blob is not publicly uploadable: another input type, or created with `skip_virus_scan` |
| `404` | `not_found` | No such blob, or terminal |
| `409` | `conflict` | Already uploaded, or an upload is in flight |
| `400` | `bad_request` | A `multipart` body without a valid `multipart/form-data` boundary, or a body that could not be read |
| `413` | `payload_too_large` | The body exceeds the upload limit |
| `422` | `unprocessable` | Size, checksum or content type mismatch, or no part named `file`. Nothing is stored and the blob can be uploaded again |
| `422` | `infected` | The scan found something. The blob is now `infected`; the message does not name the signature |

## `GET /blobs/:token/:id`

The reduced metadata of a blob, for a browser to poll after an upload.

### Public metadata

```json
{
  "id": "0199f6a0-5b2c-7c3d-8e4f-0a1b2c3d4e5f",
  "state": "ready",
  "content_type": "image/jpeg",
  "category": "image",
  "size": 482113,
  "scan_state": "clean",
  "thumbnail_state": "ready",
  "preview_state": "pending"
}
```

| Field | Type | Values |
|---|---|---|
| `id` | UUID | |
| `state` | string | `created`, `uploading`, `uploaded`, `ready` |
| `content_type` | string | |
| `category` | string | `image`, `document`, `spreadsheet`, `presentation`, `archive`, `audio`, `video`, `text`, `other` |
| `size` | integer or null | Bytes; null for a generated blob not yet built |
| `scan_state` | string | `pending`, `clean`, `infected`, `skipped` |
| `thumbnail_state`, `preview_state` | string | `pending`, `ready`, `failed`, `unavailable`, `disabled` |

## `GET /blobs/:token/:id/original/:filename`

The original bytes, decrypted.

**Path.** `:filename` must be the blob's filename, percent-encoded as one
segment; another name answers `404`.

**Signed query.**

| Parameter | Values | Effect |
|---|---|---|
| `disposition` | `inline` (default), `attachment` | `Content-Disposition` type |
| `filename` | a filename | Download name in `Content-Disposition` |
| `e` | an envelope | Serve from the envelope, without a database lookup |

**Request headers.** `Range` with a single byte range; `If-Range` with the
`ETag`; `If-None-Match`.

**Response headers.**

| Header | Value |
|---|---|
| `Content-Type` | The blob's content type |
| `Content-Length` | Exact |
| `Content-Disposition` | `inline` or `attachment`, with an ASCII `filename` and a UTF-8 `filename*` |
| `ETag` | The hex SHA-256 of the content, quoted |
| `Last-Modified` | The upload time |
| `Accept-Ranges` | `bytes` |
| `Cache-Control` | `private, max-age=31536000, immutable` |
| `X-Content-Type-Options` | `nosniff` |
| `Content-Security-Policy` | `sandbox` |
| `Content-Range` | On `206` and `416` |

| Status | When |
|---|---|
| `200` | The whole file |
| `206` | A single satisfiable range |
| `304` | `If-None-Match` names the `ETag` |
| `400` | The `e` envelope does not decode, or the signed `disposition` or `filename` is invalid |
| `404` | No such blob, terminal, filename mismatch, or, on a sealed URL, the objects were purged |
| `409` | The blob is not `ready` yet |
| `416` | The range starts past the end. No body |

Multiple ranges, malformed ranges, and an `If-Range` naming another entity
get the whole file with `200`.

## `GET /blobs/:token/:id/thumbnail/:filename`

The thumbnail, a WebP. Always an image while the blob exists: the rendered
thumbnail, or the category icon.

**Signed query.** `disposition`, `filename` and `e` as for the original,
plus `wait`, milliseconds from `0` to `30000` to hold the request while the
thumbnail is pending or the blob is not yet `ready`. The download name gets
a `.webp` extension.

**Response headers.**

| Header | Value |
|---|---|
| `Content-Type` | `image/webp` |
| `X-Representation-State` | `ready`, `pending`, `failed`, `unavailable` or `disabled` |
| `ETag` | On a rendered thumbnail |
| `Cache-Control` | `private, max-age=86400`, or `no-store` while pending |

| Status | When |
|---|---|
| `200` | The thumbnail or the icon |
| `304` | `If-None-Match` names the `ETag` of a rendered thumbnail |
| `404` | No such blob, terminal, or filename mismatch |

## `GET /blobs/:token/:id/preview/:filename`

The preview, a WebP, for blobs that have one.

**Signed query.** As for the thumbnail.

| Status | When |
|---|---|
| `200` | The preview, with `X-Representation-State: ready` |
| `304` | `If-None-Match` names its `ETag` |
| `404` | No preview: `preview is pending`, `preview could not be generated`, `no preview for this file` or `preview disabled for this blob`, with the state in `X-Representation-State`. Also no such blob, terminal, or filename mismatch, without the header |
| `409` | The blob is not `ready` yet |

## Signature errors

| Status | Code | Message |
|---|---|---|
| `403` | `forbidden` | `invalid signature` |
| `403` | `forbidden` | `signature not valid yet`: the expiry is further out than `signing.window` plus `signing.ttl.max` |
| `410` | `gone` | `signature expired` |
