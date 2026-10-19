# Serving files

Browsers read files from the public port through signed URLs. Your backend
gets those URLs from agrafe, in the blob's metadata or from a mint call, or
builds them itself with the signing key (see [Minting signed URLs from
another project](signed-urls.md)).

## URLs in the metadata

Every blob's metadata, from `GET /blobs/:id`, the listings, the creation and
the webhook, carries a `urls` object:

```json
{
  "upload": "https://files.example/blobs/v1.k1.1792310400.…/0199f2…",
  "metadata": "https://files.example/blobs/v1.k1.1792393200.…/0199f2…",
  "original": "https://files.example/blobs/v1.k1.1792393200.…/0199f2…/original/report.pdf?e=…",
  "preview": "https://files.example/blobs/v1.k1.1792393200.…/0199f2…/preview/report.pdf?e=…",
  "thumbnail": "https://files.example/blobs/v1.k1.1792393200.…/0199f2…/thumbnail/report.pdf?e=…",
  "expires_at": 1792393200
}
```

| URL | Method | Lifetime | Present |
|---|---|---|---|
| `upload` | `PUT` | `signing.ttl.upload`, 1 h | For `raw` and `multipart` blobs that do not skip the scan; `null` otherwise |
| `metadata` | `GET` | `signing.ttl.metadata`, 24 h | Always |
| `original`, `preview`, `thumbnail` | `GET`, `HEAD` | `signing.ttl.read`, 24 h | Always |

`expires_at` is the Unix time the read URLs stop working.

## Minting with options

`POST /blobs/:id/urls` mints a fresh set with a chosen lifetime and signed
query parameters:

```sh
curl -s "localhost:8080/blobs/$ID/urls" \
  -H 'content-type: application/json' \
  -d '{"ttl": "15m", "query": {"disposition": "attachment", "filename": "facture-12.pdf"}}'
```

| Field | Notes |
|---|---|
| `ttl` | A duration, at most `signing.ttl.max` (seven days). Replaces the read and metadata lifetimes |
| `query.disposition` | `inline` (default) or `attachment`: whether the browser shows or downloads the file |
| `query.filename` | The download name, instead of the blob's own. Validated like a filename |
| `query.wait` | Milliseconds, at most `30000`: how long a preview or thumbnail request waits for a pending render |

The body is optional: an empty request mints the default set. Unknown query
parameters are refused with `422`, so a backend mistake surfaces when
minting and never as an error on a correctly signed URL.

### Rounded expiries

Expiries are rounded up to the next `signing.window` boundary (one hour by
default) before the TTL is added:

```text
expires = ceil(now / window) * window + ttl
```

Every URL minted for a blob inside one window is therefore identical, so
browsers and CDNs can cache them, and a page that lists a hundred
thumbnails does not get a hundred new URLs on every render.

## Originals

The original route serves the bytes as uploaded, decrypted on the fly.

- **Only `ready` blobs are served.** Any other state answers `409` on the
  private port. On the public port a terminal blob answers `404`.
- **`Range` requests** with a single range answer `206 Partial Content`.
  Multiple ranges, or an `If-Range` naming another entity, get the whole
  file.
- **Caching.** The `ETag` is the SHA-256 of the content, and the response
  is `Cache-Control: private, max-age=31536000, immutable`: blobs are
  write-once, so the bytes behind a URL never change, and the URL's own
  expiry bounds how long it works. `If-None-Match` answers `304`.
- **Safe serving.** Every original carries `X-Content-Type-Options:
  nosniff` and `Content-Security-Policy: sandbox`, so an uploaded HTML or
  SVG file never runs as a page of your origin.
- **`Content-Disposition`** follows the signed `disposition` and
  `filename`, with an ASCII fallback and the UTF-8 name (RFC 6266).

The `:filename` segment of the path must match the blob's filename,
percent-decoded, or the answer is `404`. It gives browsers a sensible
download name, and stops a URL from pretending to serve a file it does not.

## Sealed URLs

The read routes are the hot path, and the one database query on it is the
row lookup. A **sealed** URL carries the row instead: the `e` query
parameter holds the blob's **envelope**, an opaque string with everything
needed to serve a `ready` blob, from its wrapped data key to the state of
each representation. The envelope is covered by the signature like the rest
of the query, and the wrapped key is bound to the blob id.

The URLs agrafe mints for a `ready` blob are sealed. A public read with an
`e` verifies the signature, decodes the envelope and goes to the bucket,
with no database on the way. URLs minted before the blob was ready carry no
envelope and look the row up.

Two consequences matter:

- A blob deleted or expired after a sealed URL was minted stays readable
  through it until the purge removes its objects (the reaper interval plus
  `upload.purge_delay`) or the URL expires, whichever comes first. The
  row-backed routes hide it at once. For a blob with an `expires_at`, the
  URLs agrafe mints expire no later than the blob.
- A representation still `pending` in the envelope falls back to the row
  when served, which renders and repairs as usual.

Your backend can store the envelope and mint sealed URLs itself. See
[Minting signed URLs from another project](signed-urls.md#sealed-urls).

## Read cache

Without an envelope, the read routes keep a row they served in an
in-process cache for `blobs.read_cache_ttl`, thirty seconds by default, so a
blob read many times costs one query per process per period. Only settled
rows are kept: `ready` or terminal, with no render pending. Another process
sees a delete late by at most the TTL. `"0s"` disables the cache.

## The private port serves too

The same three read routes exist on the private port without a token:

```sh
curl -s "localhost:8080/blobs/$ID/original/report.pdf" -o report.pdf
curl -s "localhost:8080/blobs/$ID/thumbnail/report.pdf" -o thumb.webp
```

They take the same `disposition`, `filename` and `wait` query parameters,
unsigned. The private port reports terminal blobs by their state: the
original answers `409`, the thumbnail the icon with
`X-Representation-State: unavailable`.

## Signature errors

| Answer | Cause |
|---|---|
| `403 forbidden`, `invalid signature` | Malformed token, unknown key id, or a signature that does not match the method, path or query |
| `403 forbidden`, `signature not valid yet` | An expiry further in the future than `signing.window` plus `signing.ttl.max` |
| `410 gone`, `signature expired` | The expiry has passed |
| `404 not_found` | Valid signature, but the blob does not exist or is terminal |
| `400 bad_request` | The envelope in `e` does not decode: the minter's mistake |

See [Errors](../reference/errors.md) for the full list.
