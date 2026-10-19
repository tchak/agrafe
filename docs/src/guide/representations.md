# Previews and thumbnails

Every `ready` blob gets up to two derived images, rendered by workers right
after the scan:

- the **thumbnail**, a WebP fitted into `media.thumbnail` (256×256 by
  default). It always exists: a file that cannot be rendered gets a category
  icon with a type badge;
- the **preview**, a WebP of at most `media.preview.max_dimension` (2048
  pixels by default) on its longest side, for files that are pictures.

Images are scaled to fit, never cropped and never enlarged. EXIF orientation
is applied, colours are converted to sRGB, and every piece of metadata is
stripped.

## What gets rendered

| File | Preview | Thumbnail |
|---|---|---|
| JPEG, PNG, GIF, WebP, TIFF, HEIF/AVIF, SVG, BMP, JPEG 2000 | the image | the image |
| PDF, DOCX or ODT that wraps a single image | the embedded image | the embedded image |
| Other PDF | none | the first page |
| Other DOCX, ODT and office files | none | the icon |
| Text, CSV and TSV | none | the first lines, in a monospace font |
| Anything else | none | the icon |

A document "wraps a single image" when it is what users produce when asked
for a screenshot and they send a Word file: a one-page PDF whose single image
covers at least `media.embedded_image.min_coverage` (70%) of the page, or a
DOCX or ODT with exactly one raster image, and in both cases at most
`media.embedded_image.max_text_chars` (50) characters of text. The image is
pulled out at full resolution and rendered as an image.

Other image types, which libvips would hand to ImageMagick, get the icon.
Originals larger than `media.max_source_size` (64 MiB) get the icon too,
except text files, whose thumbnail reads only the first 8 KiB.

A user interface should key on whether a blob has a preview
(`preview_state` is `ready`), never on its category.

## States

`thumbnail_state` and `preview_state` take one of:

| State | Meaning |
|---|---|
| `pending` | Not rendered yet, or a render is queued |
| `ready` | Rendered and stored |
| `failed` | A renderer rejected the file. Retrying would fail the same way |
| `unavailable` | Nothing can render this file |
| `disabled` | Left out of `representations` at creation; never rendered |

## Serving

The thumbnail route never answers `404` or `202`, because its consumer is
an `<img>` tag. The preview route answers `404` when there is no preview.
Both carry an `X-Representation-State` header with the state, so a client
can tell a placeholder from the final answer.

| Blob | Representation | Thumbnail route | Preview route |
|---|---|---|---|
| `ready` | `ready` | the WebP, `private, max-age=86400` | the WebP, `private, max-age=86400` |
| `ready` | `pending` | the icon, `no-store` | `404`, `preview is pending` |
| `ready` | `failed`, `unavailable`, `disabled` | the icon, `private, max-age=86400` | `404` with the reason |
| not yet `ready` | any | the icon, `pending`, `no-store` | `409` |
| terminal | any | private port: the icon, `unavailable`; public port: `404` | private port: `409`; public port: `404` |

Ready images carry a strong `ETag` of their content and answer `304` and
`HEAD` without reading the object. They are not `immutable`: a variant
version bump changes them under the same URL.

### Waiting for a render

The usual flow is "upload, then show it". A `wait` query parameter, in
milliseconds and at most `30000`, holds a representation request while the
representation is pending or the blob is not yet `ready`:

```sh
curl -s "localhost:8080/blobs/$ID/urls" -H 'content-type: application/json' \
  -d '{"query": {"wait": "5000"}}' | jq -r .thumbnail
```

On the public port `wait` must be part of the signed query.

## Regenerating

Each representation records the `media.variant_version` it was built with.
Bump the version in the configuration, and every representation is
regenerated on its next request, `failed` and `unavailable` ones included,
since a new renderer may do better. The request that notices the old
version serves the placeholder and queues the render.

A representation whose object has gone missing is repaired the same way.

A representation stuck `pending` because its render job died is not seen by
that repair. Use `POST /blobs/:id/retry` (see [Jobs and retries](jobs.md)).

## Sandboxing

No user file is decoded in the server process. `vips` renders images and
PDF pages, `agrafe-extract` pulls embedded images out of PDF and office
files, and `agrafe-render-text` rasterises text samples, each as a
subprocess with:

- a clean environment, with no secrets;
- an address space limit, `media.sandbox.memory` (1 GiB, enforced on Linux
  only);
- a CPU time limit and an output size limit, `media.sandbox.max_output_size`;
- a wall-clock timeout, `media.sandbox.timeout` (60 seconds).

Every file handed to vips is sniffed again right before the call, against
the same list of formats, so a file declared `image/png` that holds
something else never reaches a loader it was not meant for.

A crash, a decoder bomb or an infinite loop costs one render, not the
process. A file a renderer rejects is marked `failed`, and the job is done.
Only failures of agrafe's own, such as the object store or the database,
fail the job and retry it.

Before the workers start, agrafe renders a text sample, a one-pixel PNG and
a one-page PDF through the exact command lines the jobs use. The process
refuses to start if any of them fails, so a libvips built without PDF
support fails the deployment rather than every render.
