# Uploading files

An upload is two steps: create the blob with its metadata, then send the
bytes. The bytes can come from your backend over the private port, or
straight from the user's browser over the public port.

## Create the blob

```sh
curl -s localhost:8080/blobs \
  -H 'content-type: application/json' \
  -d '{
    "filename": "scan.jpg",
    "content_type": "image/jpeg",
    "input_type": "raw",
    "checksum": "n4bQgYhMfWWaL+qgxVrQFaO/TxsrC4Is0V1sFbDwCgg=",
    "size": 482113
  }'
```

The answer is `201 Created` with the blob's full metadata, including its
`id` and its signed `urls`.

| Field | Required | Notes |
|---|---|---|
| `filename` | yes | Trimmed. At most 255 bytes, not `.` or `..`, no path separators, control characters or bidirectional formatting characters |
| `content_type` | yes | Normalised to `type/subtype`; parameters such as `; charset=` are dropped. One of `upload.content_types` when the installation sets it |
| `input_type` | yes | `raw` or `multipart` for an upload |
| `checksum` | yes | SHA-256 of the bytes, standard base64 |
| `size` | yes | Size in bytes, at most `upload.max_size` |
| `checksum_md5` | no | MD5 of the bytes, base64, checked when given |
| `skip_virus_scan` | no | `true` when the backend vouches for the source. Such a blob cannot be uploaded from a browser |
| `priority` | no | `high`, `normal` (default) or `low` |
| `representations` | no | Which of `thumbnail` and `preview` to render; both by default, `[]` for none |
| `expires_at` or `expires_in` | no | A timestamp or a duration such as `"7d"`; `blobs.default_expires_in`, one day, by default |
| `attachment` | no | `{"owner_kind", "owner_id", "role"}`: create the blob attached, with no expiry |

Unknown fields are refused. The full list, including the fields of other
input types, is in the [private API reference](../reference/private-api.md#post-blobs).

### Why a checksum and a size up front

agrafe checks both while the bytes stream through, and stores nothing if
either does not match. Declaring them at creation means a truncated or
corrupted upload is caught before it becomes a file anyone can download.
SHA-256 is cheap to compute in a browser:

```js
async function sha256Base64(file) {
  const digest = await crypto.subtle.digest("SHA-256", await file.arrayBuffer());
  return btoa(String.fromCharCode(...new Uint8Array(digest)));
}
```

For files of several hundred megabytes, hash in chunks with a streaming
SHA-256 library rather than reading the whole file into memory.

## Send the bytes from the backend

A `raw` blob takes the file as the request body:

```sh
curl -s -X PUT "localhost:8080/blobs/$ID" --data-binary @scan.jpg
```

A `multipart` blob takes a `multipart/form-data` body with one part named
`file`. The part's own filename is ignored; the blob's is used.

```sh
curl -s -X PUT "localhost:8080/blobs/$ID" -F file=@scan.jpg
```

Both answer `200` with the updated metadata. The blob is now:

- `ready` when the file was scanned inline and found clean, or the scan was
  skipped;
- `uploaded` when the scan was deferred to a job, because the file is larger
  than `upload.inline_scan_max_size` (25 MiB by default), because `clamd`
  could not be reached, or because every inline scan slot was taken. It
  becomes `ready` once the job finds it clean.

## Let the browser upload

Rather than relaying bytes through your backend, hand the browser the
blob's upload URL. It is in the metadata as `urls.upload` for every `raw`
and `multipart` blob that does not skip the virus scan.

```js
// The backend created the blob and passed `urls` to the page.
await fetch(urls.upload, { method: "PUT", body: file });

// Or, for a multipart blob:
const form = new FormData();
form.append("file", file);
await fetch(urls.upload, { method: "PUT", body: form });
```

The public `PUT` answers with reduced metadata: `id`, `state`,
`content_type`, `category`, `size`, `scan_state`, `thumbnail_state` and
`preview_state`. The browser can poll `urls.metadata` for the same fields
until the blob is `ready`.

The upload URL lives for `signing.ttl.upload`, one hour by default, and works
once: blobs are write-once.

> **Cross-origin uploads.** agrafe sends no CORS headers. A `fetch` with
> `PUT` from a page on another origin is preflighted by the browser and
> fails. Serve the public port under your application's origin through a
> reverse proxy, for example at `https://app.example/files/`, or add the
> CORS headers at the proxy. `<img>` tags and plain links need neither.
> When the proxy serves agrafe under a path prefix, it must strip the
> prefix before forwarding, and `server.public_base_url` must include it so
> minted URLs carry it. The signature covers only what follows the token, so
> the prefix does not affect it.

## What is checked

The bytes flow once through a chain of checks, in this order:

1. **Size.** A body larger than the declared size is cut off. A `raw` body
   whose `Content-Length` disagrees with the declared size is refused before
   a byte is read.
2. **Checksums.** SHA-256, and MD5 when declared, are compared at the end.
3. **Content type.** The first 8 KiB are sniffed and compared with the
   declared type. The check is lenient where rejecting would be wrong:
   aliases such as `image/jpg`, a container and its contents in either
   direction (a DOCX is a ZIP), any text sniffer against a `text/*`
   declaration, and `application/octet-stream`, which declares nothing.
   Siblings, such as a DOCX declared as an XLSX, are refused. A variant
   the bytes cannot tell from its base format, such as a macro-enabled
   workbook or an OpenDocument template, is checked as that format. Three
   declarations are refined rather than checked: a JSON document that
   opens on a GeoJSON `Feature` or `FeatureCollection`, declared as
   `application/json`, `text/json` or `text/plain`, gets
   `application/geo+json` as its `content_type`; an XML document
   whose root element is `<gpx>` or `<kml>`, declared as
   `application/xml`, `text/xml` or `text/plain`, gets
   `application/gpx+xml` or `application/vnd.google-earth.kml+xml`; and
   a ZIP whose first entry is a `.kml`, declared as `application/zip`,
   gets `application/vnd.google-earth.kmz`, so all are drawn as maps.
   When `upload.content_types` is set, the declaration
   must still be in it, and the bytes of a blob declared as
   `application/octet-stream` must be of a listed type: that declaration
   does not widen the list. A file is refined only when the list admits
   the refined type; otherwise it keeps its declaration.
4. **Virus scan**, inline when the file is small enough and `clamd` is
   reachable.
5. **Encryption** and the multipart upload to the object store.

A failed check stores nothing, hands the blob back to `created` and answers
`422` with a message naming the check. The blob can be uploaded again.

An infected file is not a failed check. The object is kept for forensics,
the blob becomes `infected` for good, and the `PUT` answers `422` with the
code `infected`. On the private port the message names the signature; on
the public port it does not, so an anonymous uploader cannot learn which
signature to evade.

## Interrupted uploads

The upload runs in a task of its own, fed by the request through a channel.
A client that disconnects mid-way closes the channel, the task aborts the
multipart upload and the blob goes back to `created`, ready for another
attempt.

Only a process crash leaves work behind. While an upload runs it touches
the blob every third of `upload.stale_after` (ten minutes by default). The
reaper resets a blob left in `uploading` without a touch for that long, so
a slow upload is never reset and a dead one is reset within minutes.

On S3 and Google Cloud Storage, also configure an `AbortIncompleteMultipartUpload` lifecycle rule on
the bucket, since a crashed process cannot abort the multipart uploads it
had open.

## Expiry

Every blob is created with an expiry, a day after its creation by default,
long enough for a direct upload to be created, sent and attached when the
form is submitted. Attaching the blob clears the expiry. See [Attachments
and lifetime](attachments.md).

```json
{"filename": "draft.pdf", "content_type": "application/pdf", "input_type": "raw",
 "checksum": "…", "size": 1024, "expires_in": "7d"}
```

`expires_in` takes a duration (`"30m"`, `"12h"`, `"7d"`) and `expires_at`
an RFC 3339 timestamp; they cannot be combined, and neither can be combined
with `attachment`.

## Opting out of renders

A file nobody will look at through agrafe, such as an import kept for
audit, does not need a thumbnail:

```json
{"filename": "import.csv", "content_type": "text/csv", "input_type": "raw",
 "checksum": "…", "size": 52011, "representations": []}
```

The skipped representations are `disabled` for good. The thumbnail route
still answers the category icon.
