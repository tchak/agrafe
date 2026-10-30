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
| JPEG, PNG, GIF, WebP, TIFF, HEIF/AVIF, SVG, JPEG 2000 | the image | the image |
| PDF, DOCX or ODT (or a DOCM, DOTX, OTT, ODM, … variant) that wraps a single image | the embedded image | the embedded image |
| Other PDF | none | the first page |
| Other office files: Word, Excel and PowerPoint, their Office 97-2003, macro-enabled and template variants, RTF, OpenDocument texts, spreadsheets, presentations and drawings | none | the first page or slide, through [LibreOffice](deployment.md#libreoffice) when `media.office.command` is set; the icon otherwise |
| Text, CSV and TSV | none | the first lines, in a monospace font |
| GeoJSON, GPX, KML, KMZ, GML, zipped shapefiles | the geometry over a map, see [Maps](#maps) | the map |
| Anything else | none | the icon |

A document "wraps a single image" when it is what users produce when asked
for a screenshot and they send a Word file: a one-page PDF whose single image
covers at least `media.embedded_image.min_coverage` (70%) of the page, with
nothing else drawn over or around it (a shape, visible text, a clip, a
transparency, an annotation), or a DOCX or ODT with exactly one raster
image, and in both cases at most `media.embedded_image.max_text_chars`
(50) characters of text; in the PDF, only invisible text counts, the OCR
layer of a scan. The image is
pulled out at full resolution and rendered as an image.

Other image types, which libvips would hand to ImageMagick, get the icon.
Originals larger than `media.max_source_size` (64 MiB) get the icon too,
except text files, whose thumbnail reads only the first 8 KiB.

Office documents need a layout engine for a faithful page, and the only
one that reads them is LibreOffice. It is optional: with
`media.office.command` set, LibreOffice converts the first page of an
office file to a PDF in the job's work directory, one short-lived
sandboxed process per document, and that page is rendered like a PDF's.
The formats are those of Writer (DOCX, DOC, RTF, ODT and the DOCM, DOTX,
DOTM, OTT, ODM, OTM, OTH variants), Calc (XLSX, XLS, ODS and XLSM, XLTX,
XLTM, XLSB, OTS), Impress (PPTX, PPT, ODP and PPTM, PPSX, PPSM, POTX,
POTM, OTP) and Draw (ODG, OTG). Macros in a document never run: the
conversion loads it in never-execute mode, and the profile it runs with
pins macro security to "very high", with no trusted location and macro
execution switched off, besides. Without a converter, office
files get the icon, as they do when the file is over
`media.max_source_size`. Turning it on later regenerates existing
thumbnails through a `media.variant_version` bump. Spreadsheets exported
by agrafe are ordinary blobs and get a thumbnail too.

A user interface should key on whether a blob has a preview
(`preview_state` is `ready`), never on its category.

## Maps

A GeoJSON file, `application/geo+json`, a GPX file,
`application/gpx+xml`, a KML file, `application/vnd.google-earth.kml+xml`,
a KMZ file, `application/vnd.google-earth.kmz`, a GML file,
`application/gml+xml`, or a zipped shapefile, `application/x-shapefile`,
gets its geometry
drawn over a map as its preview, and that map fitted into the thumbnail
box. A `.geojson` uploaded as
`application/json` or `text/plain`, which is how they tend to arrive, is
recognised by its first KiB and its `content_type` becomes
`application/geo+json` as the upload completes. The recognition is a look
at that first KiB for a `"type"` of `Feature` or `FeatureCollection` at
any depth, so a JSON document that merely embeds a feature there is
refined too; it holds no GeoJSON at the top level and gets the text
thumbnail. Likewise a `.gpx` or a `.kml` uploaded as `application/xml`,
`text/xml` or `text/plain` whose root element, within the first KiB, is
`<gpx>` or `<kml>` becomes `application/gpx+xml` or
`application/vnd.google-earth.kml+xml`. A `.gml` has no root element of
its own, so one uploaded the same way is recognised by its root element
declaring a GML namespace (`http://www.opengis.net/gml` or
`http://www.opengis.net/gml/3.2`) within the first KiB, and becomes
`application/gml+xml`. A `.kmz` uploaded as
`application/zip` whose first entry is a `.kml`, as Google Earth writes
it, becomes `application/vnd.google-earth.kmz`; one whose first entry is
a `.shp`, `.shx`, `.dbf` or `.prj`, as QGIS writes them, becomes
`application/x-shapefile`. A ZIP that starts with another entry is not
recognised, so a client that knows the file is a map declares its type.
A file with no geometry has no map and gets the text thumbnail instead,
or the archive icon for a KMZ or a shapefile.

GeoJSON coordinates are longitude and latitude in degrees, as RFC 7946
requires. A file that declares another system with the legacy `crs`
member, as QGIS and GDAL write it for a layer that is not WGS 84, is drawn
when that system is Lambert-93 (EPSG:2154) or Web Mercator (EPSG:3857).
Any other declared system, or coordinates that are not degrees with no
declaration, is nothing the renderer can place: no map, the text
thumbnail.

A GPX file is drawn from its waypoints (points), its routes (a line each)
and the segments of its tracks (a line each); a segment of fewer than two
points is dropped. Coordinates are always WGS 84 by the specification.
GPX has no style vocabulary: names, symbols, elevations, times and the
colour extensions of Garmin or OsmAnd are ignored, and the file is drawn
in the default style.

A KML file is drawn from its placemarks: points, lines, rings, polygons
with their holes, multi-geometries and the `gx:Track` of a Google Earth
GPS track. Network links, ground overlays, extended data and anything
else are ignored. A KMZ is read from its first `.kml` entry, `doc.kml` as
Google Earth writes it; the icons and overlays beside it are ignored. KML
styles, shared through `styleUrl` and style maps (of which the `normal`
pair is taken) or written inline, are honoured over the default of
`[media.map.style]`:

| KML | Honoured as |
|---|---|
| `LineStyle/color` | `stroke` and `stroke-opacity` (the alpha of `aabbggrr`) |
| `LineStyle/width` | `stroke-width`, pixels, clamped to 0..20 |
| `PolyStyle/color` | `fill` and `fill-opacity` |
| `PolyStyle/fill` of 0 | `fill-opacity` of 0 |
| `PolyStyle/outline` of 0 | `stroke-opacity` of 0, for polygons |
| `IconStyle/color` | `marker-color`; white is untinted and leaves the default |
| `IconStyle/scale` | `marker-size`: small below 0.8, large above 1.3, medium between |

The icon's `href` is not drawn, like `marker-symbol`, and a value that
does not parse is ignored.

A zipped shapefile is drawn from every `.shp` layer in it, in a folder or
not: points, multipoints, polylines and polygons with their holes, with
their `Z` and `M` variants; multipatches are skipped. Each layer is drawn
in the coordinate system of the `.prj` beside it, recognised by its
parameters whatever its name: WGS 84 (or RGF93, ETRS89) degrees,
Lambert-93 or Web Mercator. A layer without a `.prj`, or with one in any
other system, is nothing the renderer can place, and the archive gets the
archive icon. The attributes in the `.dbf` are not drawn, and shapefiles
have no style: the default of `[media.map.style]` applies. The layers are
read up to 64 MiB in all, uncompressed.

A GML file, GML 2 or 3 as a WFS serves it or QGIS writes it, is drawn
from every geometry in it, whatever the features holding them are
called: points, lines, polygons with their holes (`exterior` and
`interior`, or GML 2's `outerBoundaryIs` and `innerBoundaryIs`), the
multi-geometries, curves of line segments, surfaces of polygon patches
and rings of curves. Arcs, circles, solids and envelopes are not drawn.
Each geometry is drawn in the system its `srsName` names, or the nearest
enclosing element's, or the one on the document's first `boundedBy`
envelope, where a WFS puts it: WGS 84 degrees, Lambert-93 or Web
Mercator, in any of their spellings. EPSG:4326 written as a URN
(`urn:ogc:def:crs:EPSG::4326`) or as an `opengis.net/def` URL is latitude
first, as the OGC specifies, and is read so; `EPSG:4326`, GML 2's
`epsg.xml#4326` and CRS84 are longitude first. GML has no default
system, so a geometry without a `srsName` anywhere, or in another
system, is nothing the renderer can place, and the file gets the text
thumbnail. GML has no style (SLD is a separate document): the default of
`[media.map.style]` applies. A zipped `.gml` is an archive, and gets the
archive icon.

The map shows the geometry's bounding box, padded by `media.map.padding`
(a tenth of its larger side) and at least `min_span` (200 m) wide, so a
single point still gets a readable map, widened to the shape of the
preview box. It is drawn at the preview box or `media.map.max_size`
(2048 pixels), whichever is smaller. GeoJSON features keep their
[simplestyle-spec](https://github.com/mapbox/simplestyle-spec) properties,
which geojson.io, uMap and most GIS exports write, over the default of
`[media.map.style]`:

| Property | Honoured as |
|---|---|
| `stroke`, `fill`, `marker-color` | a hex colour of 3 or 6 digits, `#` optional |
| `stroke-opacity`, `fill-opacity` | a number, clamped to 0..1 |
| `stroke-width` | pixels, clamped to 0..20 |
| `marker-size` | `small`, `medium` or `large` |

`marker-symbol`, `title` and `description` are not drawn, and a value of
the wrong kind is ignored. Lines and polygons get a darker casing under
their stroke, polygon holes stay holes, and points are discs with a ring.

The background comes from the WMS layers of `[[media.map.layers]]`,
each a `GetMap` URL template that agrafe fills with the bounding box in
EPSG:3857 and the pixel size, composited bottom to top, and each drawn
only on an extent between its `min_span` and `max_span`; a map whose
extent leaves the world, a file spanning the globe, gets no layer. Use
`max_span` to keep requests within what a layer covers: a provider that
refuses a bbox fails the attempt every time. The attribution
of the first drawn layer is burned into a corner of the map. There is no
layer by default: the geometry is then drawn on a flat background and
nothing leaves the worker. Every render with a layer configured sends the
extent of a user's geometry to that provider, so the render workers'
egress must allow those hosts, and nothing else needs it; a redirect to
another host is not followed. A layer that
cannot be fetched, times out or does not decode fails the render attempt,
which is retried; the file is never marked `failed` for the provider's
downtime. See [Configuration](../reference/configuration.md#mediamap).

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
PDF pages and composites maps, `agrafe-extract` pulls embedded images out
of PDF and office files, `agrafe-render-text` rasterises text samples,
`agrafe-render-geo` draws GeoJSON, GPX, KML, KMZ and GML files and zipped
shapefiles, and LibreOffice, when
configured, converts office files to PDF, each as a subprocess with:

- a clean environment, with no secrets;
- an address space limit, `media.sandbox.memory` (1 GiB, enforced on Linux
  only);
- a CPU time limit, the wall-clock limit summed over the process's
  threads, which is why vips and LibreOffice run two threads each, an
  output size limit, `media.sandbox.max_output_size`, and a process
  count, `media.sandbox.max_processes`;
- a wall-clock timeout, `media.sandbox.timeout` (60 seconds);
- on Linux, a Landlock ruleset and a seccomp filter: the process sees the
  system libraries and fonts, the file it renders and the directory it
  writes, and nothing else, not the configuration file, not the other
  jobs' files, not the network, not even a unix socket. LibreOffice
  alone may have unix sockets, which it cannot start without, and its
  own limits, `media.office.sandbox`. See [Security
  model](security.md#sandboxed-rendering);
- optionally, on a host that can grant it a user namespace,
  [bubblewrap](deployment.md#bubblewrap) around all of that: a mount,
  pid and network namespace of the process's own.

Every file handed to vips is sniffed again right before the call, against
the same list of formats, so a file declared `image/png` that holds
something else never reaches a loader it was not meant for.

A crash, a decoder bomb or an infinite loop costs one render, not the
process. A file a renderer rejects is marked `failed`, and the job is done.
Only failures of agrafe's own, such as the object store or the database,
fail the job and retry it.

Before the workers start, agrafe checks that the sandbox refuses a read it
did not grant, then renders a text sample, a one-pixel PNG, a one-page PDF,
a GIF, a WebP, a TIFF, a HEIC, an AVIF, a JPEG 2000 image, the JPEG `agrafe-extract` pulls out of a PDF wrapping one and the map of a
one-point GeoJSON, without a background layer, through
the exact command lines the jobs use, and, with `media.office.command`
set, converts a two-page DOCX and an XLSX through LibreOffice and checks
that the DOCX came out as one page. The process refuses to start if any
of them fails, so a libvips built without PDF, SVG or HEIF support, a font
directory the sandbox does not expose, or a LibreOffice without Calc or
too old for the first-page export, fails the deployment rather than
every render. The worker logs what the sandbox gives it, `sandbox:
landlock abi 4, seccomp, rlimits`, `sandbox: bubblewrap 0.11.1, seccomp,
rlimits` or `sandbox: rlimits only (macos)`.
