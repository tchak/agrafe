# agrafe — design

A file storage service. Rust server, Postgres for metadata, an object store for
bytes. Two HTTP interfaces on two ports: a **private** one for the backend that
owns the files, and a **public** one exposed to the internet where every URL
carries a signature.

This document records the decisions and the reasoning behind them. It is the
reference when code and intuition disagree.

## Goals

- Store files uploaded by end users (through browsers) or by backends.
- Serve each file as the original plus two derived representations: a WebP
  preview and a WebP thumbnail.
- Encrypt everything at rest with keys the storage provider never sees.
- Scan everything with ClamAV unless the backend explicitly vouches for the
  source.
- Stream on both the write and the read path. Never buffer a whole file in
  memory.
- Generate tabular files (CSV, XLSX, ODS) and ZIP archives server side from a
  streamed description.

## Non-goals

- Multi-tenancy or per-client authentication on the private port. The private
  port is reachable only from trusted networks.
- Replacing a file in place. Blobs are write-once (see below); a backend that
  wants new bytes creates a new blob and swaps its own reference.
- Range requests on previews and thumbnails. They are small and served whole.

## Scale

agrafe is built for billions of blobs: billions of rows in `blobs` and
`attachments`, billions of objects in the bucket. Every design, feature
and performance review starts from that number, not from what a test
database or a first deployment holds.

- No query reads a table whole. Every query a request, a page or a job
  runs is bounded by an index: a lookup by key, a cursor page, a capped
  walk. `COUNT(*)`, `SELECT DISTINCT`, `OFFSET` pagination and an
  unindexed filter are scans at this size; a new filter or sort comes
  with its index, and the plan is checked against it. A pooled
  connection may settle on the generic plan of a statement it runs
  often, built without the parameters' values, so what the planner
  must prove to use a partial index (a state, a limit) is written into
  the SQL rather than bound, and the reaper's plans are checked that
  way.
- Anything that visits every blob or object (a sweep, an audit, a
  migration of data) is a job that takes days: it works in batches,
  saves its place, resumes after a deploy or an error, and yields to
  the request path. Nothing runs it inside one transaction or one
  request.
- Per-blob cost is multiplied by billions: a column, an index, a row
  per event, an extra object per blob or a request to the store per
  read is weighed at that multiple, in storage and in money.
- Hot rows and global counters do not exist. Nothing updates one row
  per upload or per read; aggregates come from metrics, estimates
  (`pg_class.reltuples`) or bounded samples.
- The queue holds only the backlog, never the history, so it stays
  small however many blobs there are (see "Jobs").

## Crates

One workspace, split so that the service can be embedded in another axum
application and so that the code that parses user files is never linked
into a server process. Dependencies point downwards only.

| Crate | Contents | Depends on |
|---|---|---|
| `agrafe-crypto` | Streaming AEAD and key wrapping, reviewed in isolation | |
| `agrafe-types` | Content type constants, `classify`, `sniff`, the office format table (`office`: application, package and extension per type), the vips loader allowlist, the text-sample contract, the map contract (the input formats, the style vocabulary, the spec and the extent the server and the helper exchange), `str_enum!` | `infer`, `serde` |
| `agrafe-sandbox` | Running a subprocess with `setrlimit` bounds and a timeout, confined by Landlock and seccomp on Linux, and by bubblewrap where configured; the `Command` builder that declares what it reads and writes; the `[media.sandbox]` config and the startup probe | `tokio`, `libc`, `landlock`, `seccompiler` |
| `agrafe-extract` | The `agrafe-extract` helper: pulls the one image out of a PDF or a Writer package (DOCX, ODT and their variants). Library plus binary | `agrafe-types`, `lopdf`, `zip`, `quick-xml` |
| `agrafe-render-text` | The `agrafe-render-text` helper: rasterises a text sample. Library plus binary | `agrafe-types`, `ab_glyph` |
| `agrafe-render-document` | The `agrafe-render-document` helper: compiles a typst workspace into a PDF. Library plus binary | `typst`, `typst-pdf`, `typst-assets` |
| `agrafe-render-geo` | The `agrafe-render-geo` helper: reads a GeoJSON, GPX, KML or KMZ file, projects, fits, simplifies and draws it as SVG paths, with the extent it chose. Library plus binary | `agrafe-types`, `geojson`, `quick-xml`, `zip`, `serde_json` |
| `agrafe-render` | Which files render and from what, `vips thumbnail` and `vips composite`, the icons, the drivers that spawn the helpers, the fetcher of map backgrounds, the startup probe, the `[media]` tool config | `agrafe-types`, `agrafe-sandbox`, `reqwest` |
| `agrafe` | The library: config, database, blobs, upload pipeline, the fetch client, the webhook, encryption, signing, storage, ClamAV, jobs, metrics, URL minting, and the `Agrafe` handle whose methods are the private API | `agrafe-crypto`, `agrafe-types`, `agrafe-render`, `sqlx`, `object_store`, `reqwest` |
| `agrafe-axum` | The two HTTP interfaces as axum routers, the extractors, `ApiError` and the mapping of the library's errors onto status codes | `agrafe`, `axum` |
| `agrafe-dashboard` | The HTML dashboard as an axum router: the queue census, the listings and details of jobs, blobs and attachments, the retry and discard forms; compiled templates and a vendored stylesheet | `agrafe`, `agrafe-axum` (the origin check), `axum`, `askama` |
| `agrafe-server` | The clap entry point: `serve`, `migrate`. Produces the binary named `agrafe` | `agrafe`, `agrafe-axum`, `agrafe-dashboard` |

The helpers are separate binaries, not subcommands of the server, so that
the server links none of their parsers: `lopdf`, `zip`, `quick-xml`,
`ab_glyph`, `typst` and `geojson` exist only in processes the sandbox
bounds. A new map format is a reader in `agrafe-render-geo`, not a
helper of its own: one helper, one sandbox profile, one binary, and the
projection, extent and drawing shared. They are found on `PATH` by default (`media.extract_command`,
`media.render_text_command`, `media.render_document_command`,
`media.render_geo_command`), read no configuration, write no logs and
never touch the network. A deployment ships the five binaries together. A
renderer with a dependency tree of its own gets a helper crate the same
way, turning the file into a raster, or into something vips turns into
one, that vips then fits into the box; a `Source` or `Fallback` variant,
a row in the plan table (`agrafe_render::plan`) and a driver in
`agrafe_render::helpers` are where it plugs in. What a helper and the
server exchange beyond a file name, the text sample's length or the
map spec and extent, is a type in `agrafe-types`, so the two sides
agree by construction.

The library knows nothing of HTTP. Its operations are methods on the
`Agrafe` handle (`create_blob`, `list_blobs`, `find_blob`, `delete_blob`,
`upload`, `urls`, `mint_urls`, `representation`, `list_jobs`, `retry_job`,
`discard_job`, ...), each returning its own `thiserror` enum
(`CreateError`, `UploadError`, `MintError`, `ListError`). `agrafe-axum`
maps those onto `ApiError` in one place, its `error.rs`, and its handlers
are a line or two each: parse the request, call the handle, shape the
response. The upload body reaches the library as a stream whose error is
boxed; the HTTP layer puts its own error in the box and takes it back out,
so a body cut short by the size limit still answers `413` without the
pipeline knowing what that is. Reading an original is the one place the
HTTP layer goes below the handle, to `blob::read::Reader`, because `Range`
handling and streaming the plaintext into a response body are its job.

Three consumers exist: the standalone service, `agrafe-server`; a host
application that builds an `Agrafe` from a configuration and a pool, nests
`agrafe_axum::public_router` under a prefix, runs `jobs::runner::run` as a
task and calls the private API in Rust; and one that also nests
`private_router` on an internal listener. The routers come with their
state applied, so nesting needs no state plumbing in the host.

## Interfaces

### Private port

No signature. Reachable only from trusted networks.

| Method | Path | Purpose |
|---|---|---|
| `POST` | `/blobs` | Create a blob (metadata only, no bytes yet) |
| `GET` | `/blobs` | List blobs, cursor paginated, filterable |
| `GET` | `/blobs/:id` | Full metadata, attachments, ready-made signed public URLs and, once `ready`, the envelope |
| `PUT` | `/blobs/:id` | Upload bytes, once: the file of a `raw` or `multipart` blob, the whole source of a `rows`, `archive` or `document` one |
| `PUT` | `/blobs/:id/parts/:n` | Append part `n` of a `rows`, `archive` or `document` source |
| `POST` | `/blobs/:id/finish` | Seal a source sent in parts and enqueue its build |
| `DELETE` | `/blobs/:id` | Soft delete of an unattached blob, bucket objects purged asynchronously |
| `POST` | `/blobs/:id/retry` | Enqueue whatever the blob is waiting on, superseding a dead job (see "Jobs") |
| `POST` | `/blobs/:id/attachments` | Attach the blob to an owner, idempotent (see "Attachments") |
| `GET` | `/attachments` | List attachments, cursor paginated, filterable by owner and blob |
| `GET` | `/attachments/:id` | One attachment |
| `DELETE` | `/attachments/:id` | Detach; the last detach gives the blob back its default expiry |
| `POST` | `/blobs/:id/urls` | Mint signed public URLs with a chosen TTL and query parameters |
| `GET` | `/blobs/:id/original/:filename` | Original bytes, supports `Range` |
| `GET` | `/blobs/:id/preview/:filename` | WebP preview or 404 |
| `GET` | `/blobs/:id/thumbnail/:filename` | WebP thumbnail, always 200 |
| `GET` | `/jobs` | List jobs, cursor paginated, filterable by `kind`, `state`, `priority`, `blob_id` |
| `GET` | `/jobs/:id` | One job, with its last error |
| `POST` | `/jobs/:id/retry` | Give a dead job its attempts back and run it now |
| `DELETE` | `/jobs/:id` | Discard a dead job |
| `GET` | `/metrics` | Prometheus text format, see "Jobs" |
| `GET` | `/healthz` | Liveness |

### Public port

Every route carries a signature token as the first path segment after
`/blobs`. The tail after the token has the same shape as the private routes so
handlers are shared.

| Method | Path | Purpose |
|---|---|---|
| `PUT` | `/blobs/:token/:id` | Upload bytes, only `raw` and `multipart` |
| `GET` | `/blobs/:token/:id` | Reduced metadata: id, state, scan state, representation states, content type, category, size. `404` once the blob is `expired`, `infected` or `deleted` |
| `GET` | `/blobs/:token/:id/original/:filename` | Original bytes, supports `Range` |
| `GET` | `/blobs/:token/:id/preview/:filename` | WebP preview or 404 |
| `GET` | `/blobs/:token/:id/thumbnail/:filename` | WebP thumbnail, always 200 |
| `GET` | `/healthz` | Liveness |

Blobs in a terminal state (`expired`, `infected`, `deleted`) do not exist as
far as the public port is concerned: every public route answers `404`, upload
included, even with a valid signature. A URL that leaks after the blob is gone
therefore reveals nothing, not even that it once existed. The private port
keeps reporting the real state. Public request spans record the path with the
token segment redacted, so no log level writes capabilities to log storage.

The `:filename` segment of representation routes must match the blob's
filename (percent-decoded), otherwise `404`. It exists so browsers get a
sensible download name and so a URL cannot be made to look like it serves a
different file than it does. It is the blob's own filename on every route,
previews and thumbnails included, even though those serve WebP: the segment
names the file the representation is of, and the download name comes from
`Content-Disposition`, which for a representation is the filename with a
`.webp` extension in place of its own (`photo.jpg` → `photo.webp`, `README`
→ `README.webp`, `archive.tar.gz` → `archive.webp`). Browsers take the
header over the URL, so a saved preview is never a WebP file with a `.jpg`
name.

Wherever a filename is split into stem and extension (this download name,
the archive dedupe, the sheet named after a `rows` blob), the extension is
what follows the last dot, with a rule for compression wrappers: a last
extension of `gz`, `bz2`, `xz`, `zst`, `lz`, `lzma`, `Z` or `br` takes the
extension before it when that one is one to five ASCII alphanumerics with a
letter among them. So `a.tar.gz` and `export.csv.gz` split before `.tar.gz`
and `.csv.gz`, while `my.report.gz` splits before `.gz`, since `report` is
no extension, and `.bashrc` or `README` have none.

A heuristic, and a known one. The compression suffix is an encoding over
the real extension, as `Content-Encoding` is over `Content-Type`, and the
usual way to find the real one is to strip the encoding and look the rest
up in an extension table (Python's `mimetypes`, Apache's `mod_mime`,
nginx's `gzip_static`). agrafe has no such table and does not want one
for a cosmetic rule, so "is a known extension" is replaced by "looks like
one", and the cap of five letters is what lets `jsonl` and `fastq`
through. It also lets `notes`, `data` and `draft` through: `my.notes.gz`
splits before `.notes.gz`. That costs a dedupe counter in an odd place
(`my-1.notes.gz`), a sheet named `my` and a preview called `my.webp`,
all of them cosmetic, and is preferred to the alternatives: the last dot
alone, which breaks every `a.tar.gz` and is what Rails, Go and Firefox
live with; a curated list, which is what Chromium (`tar` only) and the
freedesktop database do and is never complete, since `.csv.gz` and
`.sql.xz` are at least as common as tarballs in what agrafe stores; or
appending to the download name (`archive.tar.gz.webp`), which never
errs and is uglier every time.

### Errors

Every error response, on both ports, is JSON with one shape:

```json
{"error": {"code": "unprocessable", "message": "checksum is required for raw and multipart input"}}
```

`code` is stable and machine readable; `message` is for humans and may change.
This includes failures axum would otherwise answer in plain text: malformed
JSON, unknown query values, a non-UUID path segment, unknown routes (`404`)
and wrong methods (`405`). Handlers use wrapper extractors in `extract.rs`
that convert those rejections. Internal errors are logged in full and reported
as `internal` with no detail.

### CORS

A page on another origin than the public port's needs CORS headers to
`PUT` an upload, which the browser preflights, or to read a response it
fetched. `server.cors.allowed_origins` lists those origins, or is `["*"]`
for any; empty, the default, the public port sends no CORS headers and an
`OPTIONS` is a `405`, as before the key existed. The private port never
sends any: its client is the backend, and a page reaching it is what
`private_hosts` and the cross-site check refuse.

The layer is tower-http's `CorsLayer`, inside `public_router`, so an
embedder gets it from the same configuration. It answers a preflight
before routing, so `OPTIONS` never meets the `Signed` extractor: a URL is
signed for the method it was minted for, and a preflight touches nothing
and returns static headers, which keeps "verify before touching the
database" true. Every other response carries the headers, errors
included, so a page reads the `code` of a `403` or a `410` instead of an
opaque network failure.

- **Methods** `GET`, `HEAD`, `PUT`; **request headers** `content-type`,
  `range`, `if-range`, `if-none-match`, `if-modified-since`; **exposed
  headers** `content-disposition`, `content-range`, `accept-ranges`,
  `etag`, `last-modified`.
- **No credentials, ever.** CORS guards responses a browser authenticates
  with ambient credentials, cookies or HTTP auth, and the public port has
  none: the URL is the whole authorisation. So `"*"` gives a page nothing
  that the holder of a URL could not get with `curl`, and it is offered
  plainly rather than discouraged. The allowlist is policy (which sites
  may use the upload URLs), not a security boundary.
- **Origins are checked at load** against their own serialization, what a
  browser sends in `Origin` and what the layer compares byte for byte:
  `https://app.example/` or `https://App.Example` would never match, and
  the error says what to write instead. `"*"` stands alone. Wildcard
  subdomains are left out: an exact list is easy to reason about, and
  they can be added without breaking one.
- **Preflights per upload.** A browser caches a preflight per URL, and
  every public URL carries its own token, so each cross-origin upload is
  an `OPTIONS` and a `PUT`, whatever `max_age` says (an hour by default);
  it spares the preflight of a retry only. Neither touches the database
  or the bucket, so the cost does not grow with the blob count. A `GET`
  with no other header than those a browser adds itself is not
  preflighted.

### Dashboard

Everything an operator wants to see is in the private API as JSON, and
following a blob to its jobs or an attachment to its blob is three `curl |
jq` commands. The dashboard is the same thing as HTML, in the spirit of
Sidekiq's web UI: the queue at a glance, then the blobs and the attachments
with the filters the JSON routes take, every row linking to the related
objects. Nothing is created, uploaded or attached from it. The one write
is retry (and discard, for a dead job), and its presence is configurable.

It is a second presentation of the `Agrafe` handle, in its own crate,
`agrafe-dashboard`, not a module of `agrafe-axum`: that crate has no
presentation dependency, and a host that nests the public router or calls
the private API in Rust should not compile templates and a stylesheet it
never serves. The dashboard answers with HTML pages, not `ApiError`
bodies; the one thing it takes from `agrafe-axum` is the cross-site check
below. The library changed only to expose the queue census through the
handle, `Agrafe::job_counts`, which `/metrics` reads too.

`dashboard_router(state)` is one router with its state applied, like the
other two. `agrafe-server` nests it under `/dashboard` on the private
listener when `dashboard.enabled` is set, on a process that serves the
private interface only; a host
nests it wherever it likes, behind its own authentication. The pages build
their links from the prefix they find themselves under (the part of the
original URI the nest stripped), and embed the thumbnail from the private
API's `GET /blobs/:id/thumbnail/:filename`, which is assumed one segment
up: `/dashboard` under `/`, `/internal/dashboard` under `/internal`.

Rendering: askama templates compiled into the binary, type-checked against
the structs they render, so a nestable library needs no files next to the
binary; one stylesheet served from the router, daisyUI's reset, themes
and the components the pages use, assembled from the release's plain CSS
files by `scripts/daisyui.sh` and committed, with the few layout
utilities the pages need written in the base template; no Tailwind, no
JavaScript, no build step. Filters are `GET` forms whose fields
mirror the list query structs, so the URL is the state and a filtered view
is a link an operator can paste. An enum filter is a select; so are the
`owner_kind` and `role` of attachments, fed by `Agrafe::attachment_facets`,
the distinct values of each, with the value a pasted link names still
offered when it is gone. A backend has a handful of each, but the table can
hold a billion rows and Postgres has no loose index scan, so `SELECT
DISTINCT` would read them all on every page view; the facets come instead
from a recursive CTE that walks the distinct `(owner_kind, role)` pairs
along `attachments_kind_role_idx`, one index probe per pair, capped at
`MAX_FACETS` pairs. No cache: the walk costs less than rendering the page,
and a cache would only add a staleness window. Pagination is the existing cursor, a
"next" link. Actions are `POST` forms that redirect (`303`) to the page
they came from with a one-line notice in the query. The queue page takes
`?refresh=5` and reloads itself through `<meta http-equiv="refresh">`.
Errors are pages too, with the status in the title: a bad filter is `400`,
an id of the wrong shape or an unknown path is `404`.

Views. **Jobs**: the census from `Agrafe::job_counts`, a total per state
and, per kind and priority, the count in each state and the age of the
oldest due job; below it the listing with a tab per state and the `kind`,
`priority` and `blob_id` filters, and the first line of `last_error` on
each row. The detail shows the payload pretty-printed, the whole error,
and on a dead job the retry and discard buttons. **Blobs**: the six
filters of `blob::ListParams`; the detail embeds the thumbnail (always
`200`, so no special case), lists the attachments and the jobs of the
blob, and shows the retry button when `failed_at` is set. **Attachments**:
the four filters of `attachment::ListParams`, each row with the state and
the failure of its blob, as the JSON listing carries them.

Actions and cross-site requests. The private port has no authentication
because only trusted networks reach it. A browser breaks that assumption:
an operator with the dashboard open on an internal address can be made to
submit a form to it by any page they visit, since a cross-origin form
`POST` goes through without a preflight. The JSON API has the same
exposure on its body-less `POST`s (`retry`, `finish`, `urls`; `PUT`,
`DELETE` and JSON bodies are preflighted), so the check is one function,
`agrafe_axum::origin::check`, and the private router applies it to every
unsafe request; the dashboard applies it to its action routes as an
extractor, so it keeps it wherever a host nests it. A request is refused
when the browser marks it as another site's: `Sec-Fetch-Site` other than
`same-origin` or `none`, or, without that header, an `Origin` that is not
the `Host`. A request with neither header is a non-browser client, the
backend, and goes through: a browser never sends a `POST` without
`Origin`. Stateless, no token and no session, which the private port does
not have. A refused action is a `403` page on the dashboard, a `forbidden`
error on the API, and the dashboard checks it before the actions flag, so
nothing is learnt from a read-only dashboard either.

What that check cannot see is DNS rebinding: a page on `evil.test`, served
on the private port's number, has its name re-pointed at the private
address once loaded, and the browser then treats `evil.test:PORT` as
same-origin, sends `Sec-Fetch-Site: same-origin`, reads the answers, and
can mint signed URLs for any blob. The header that gives it away is
`Host`, which carries the rebound name. So `server.private_hosts` lists
the names and ports the private interface is reached under, and when it
is set every private request, whatever its method, has to name one of
them or is a `403` (`agrafe_axum::host`); `/healthz` and `/metrics` are
exempt, since probes and scrapers address a pod by its address and
neither says anything a rebinder wants. The list is applied inside the
private router and again around the nested dashboard, since a layer on a
router does not reach routes nested after it. Unset, nothing is checked,
so an existing deployment keeps working on upgrade, and the server warns
at startup when the dashboard is enabled without it: a dashboard is what
puts a browser on the port. Neither check is authentication; a dashboard
in daily use belongs behind a proxy that has some.

The notice an action leaves is a code in the query (`?notice=job_retried`),
and the page holds the sentences: a pasted link cannot make the dashboard
say anything of its own, and an unknown code is ignored.

Both flags are off by default: an existing deployment does not grow an
interface by upgrading. Considered and not done: a module of
`agrafe-axum` (a template engine in every host, and a different error
surface anyway); `maud` (markup as Rust code, harder to read for pages
this size); `minijinja` for hot reload (loses the type checking, the
wrong trade for a page that changes rarely); htmx or a JSON-driven front
end (nothing needs partial updates, and the JSON API exists for anything
programmatic); a CSRF token (the `Sec-Fetch-Site` / `Origin` check covers
cross-site forms without a session store).

## Signed URLs

Token format, one path segment, no slashes:

```
v1.<key_id>.<expires_unix>.<base64url(hmac_sha256(key, message))>
```

```
message = METHOD "\n" expires_unix "\n" path_and_query_after_token
```

- `path_and_query_after_token` is the raw request target as received, starting
  at `/<id>`, including the query string if any. Nothing is decoded,
  reordered or re-encoded. The server is the only minter, so both sides use the
  same encoder.
- `HEAD` is verified as `GET`.
- There are no permanent URLs. `signing.ttl.max` (default seven days) caps
  every TTL in config and the `ttl` a mint may ask for, and verification
  refuses a token whose expiry lies further than one window plus the cap
  in the future (`403`), so the ceiling holds for anyone minting with the
  key, not only for this server. Such a token is not void: it becomes
  valid once its expiry comes within reach, and still dies at it, so no
  token works for longer than a window plus the cap at a stretch. The
  bound is checked against the running config, so lowering `signing.ttl.max`
  or `signing.window` cuts off URLs in flight that were minted under the
  old values. `expires_unix = 0` is simply expired. A link that must stay
  stable is a backend route that re-mints on each hit; the rounding below
  makes that cheap.
- Several keys live in config. One is current for minting, all are valid for
  verification. The key id in the token selects the key. This makes rotation
  possible without invalidating URLs in flight. Ids are restricted to
  `[A-Za-z0-9_-]` so they are safe path segments and cannot contain the token
  separator.
- Minted expiries are rounded: `expires = ceil(now / window) * window + ttl`.
  All URLs minted for the same blob inside one window are identical, so
  browsers and CDNs can cache them. The window is one setting,
  `signing.window`; the TTL is per operation (`signing.ttl.upload`,
  `metadata`, `read`), and `POST /blobs/:id/urls` may override the read and
  metadata TTLs with one `ttl`, up to `signing.ttl.max`.
- HMAC compared in constant time. Bad signature is `403`, expired is `410`,
  valid but too far in the future is `403` with a message that names no
  config, a valid signature on a missing blob is `404`.
- The scheme is pinned by `fixtures/signing-vectors.json`: tokens, expiries,
  path segments and query strings computed by the server from fixed inputs,
  checked by a test here and by any minter elsewhere, a backend holding the
  signing key included. Query parameters are serialised as
  `application/x-www-form-urlencoded` (a space is `+`), sorted by name.
  `fixtures/envelope-vectors.json` pins the envelope encoding the same way,
  since backends store envelopes; a format change bumps the version and
  must keep decoding them. `cargo test -p agrafe -- --ignored
  write_vectors` regenerates both files.
- Because everything after the token is signed, query parameters are a safe
  way for the minter to control behaviour: `disposition=attachment`, a
  filename override, `wait=<ms>` for pending representations. They are
  validated when minted, unknown names included, so a backend mistake is a
  `422` on the private port and never a client error on a correctly signed
  URL.

### Sealed URLs

The read routes are the hot path, and the one query on it is the row
lookup. A sealed URL carries the row instead: the **envelope**, one opaque
string with everything serving a `ready` blob needs (the wrapped data key
and its KEK id, the stream header and segment size, the size, checksum and
content type of the original, the category, and for each representation
its state, variant version, digest and size; format in
`blob::envelope`). It travels as the signed query parameter `e`, so it is
covered by the signature like the rest of the target, and the wrapped key
is bound to the blob id, so nothing in it needs protecting on its own. The
public read routes with an `e` verify the signature, decode the envelope
and go to the bucket: no database on either side. Those without one look
the row up as before.

- The server seals the read URLs it mints for a blob that is `ready` and
  not expired. `GET /blobs/:id` on the private port returns the same
  `envelope`, so a backend holding the signing key stores it with the id
  and the filename and mints sealed URLs itself, without a call, as it
  would with `rails_blob_path`. It is a snapshot: a representation still
  `pending` in it falls back to the row when served, and the backend
  refreshes its copy once the render is done, when it polls the metadata
  anyway. Blobs are write-once, so the rest never changes.
- The original is served from the envelope alone. An object that is not
  in the bucket is `404`: the blob was purged.
- A representation is served from the envelope when it records it `ready`
  with the current variant version: `304` and `HEAD` from the digest and
  size, the bytes from the bucket. Anything else, a pending render, an
  older version, a missing object, is handed to the row, which knows
  better and does the repair.
- Nothing on the sealed path reads the row, so a blob deleted or expired
  after the URL was minted stays readable until the purge takes its
  objects (the reaper's interval plus `upload.purge_delay`) or the URL
  expires, whichever comes first; the row-backed routes hide it at once.
  URLs the server mints for a blob with an `expires_at` expire no later
  than the blob. A backend minting its own picks the TTL it stands behind,
  within `signing.ttl.max`.
- An envelope that does not decode is `400`: the URL was signed, so it is
  the minter's mistake, not a forgery. So is one whose wrapped key does
  not open for the blob in the path, another blob's envelope, rather
  than the `500` a row that does not unwrap earns. One naming a KEK the
  process does not know is handed to the row instead: a rewrap retiring
  that KEK is the one thing that makes a stored envelope stale, the row
  was rewrapped and knows better, and the backend's copies serve on,
  one lookup each, until it refreshes them; nothing has to be timed
  with the retirement. Decoding also refuses what the server never writes, a
  wrapped key of the wrong length or a segment size past
  `encryption.chunk_size`'s bound, so the memory a request holds is the
  operator's choice, not the minter's.

## Blobs

### Creation

`POST /blobs` takes:

| Field | Required | Notes |
|---|---|---|
| `filename` | yes | Trimmed, then stored and used in URLs and `Content-Disposition`. At most 255 bytes, not `.` or `..`, no path separators, control characters or bidirectional formatting characters (which can disguise an extension) |
| `content_type` | yes | Normalised to `type/subtype`, classified into a category |
| `input_type` | yes | `raw`, `multipart`, `url`, `rows`, `archive`, `document` |
| `url` | for `url` | Where to fetch the bytes from: `https`, no credentials, no private address. See "Fetched blobs" |
| `checksum` | for `raw`, `multipart`; optional for `url` | SHA-256 of the bytes, base64. Checked when given; for `url`, computed and written to the row otherwise |
| `checksum_md5` | no, for `raw`, `multipart`, `url` | MD5 of the bytes, base64: the `checksum` of an Active Storage blob. Checked when given |
| `size` | for `raw`, `multipart`; optional for `url` | Declared size in bytes, enforced on upload; a `url` blob without one is bounded by `upload.max_size` |
| `columns` | for `rows`, or `sheets` | The columns of the one sheet, in order: a list of `{"name": "...", "type": "..."}`, `type` optional. See "Rows" |
| `sheets` | for `rows`, or `columns` | The sheets of an XLSX or ODS workbook, in order: a list of `{"name": "...", "columns": [...]}`. See "Rows" |
| `main` | no, for `document` | The entry file of the workspace, a relative path; `main.typ` by default. See "Documents" |
| `skip_virus_scan` | no, default `false` | Private-only, backend vouches for the source |
| `priority` | no, default `normal` | `high`, `normal` or `low`: where the blob's fetch, scan, render and build jobs go in their kind's queue, see "Jobs" |
| `representations` | no, default both | The representations to render, among `thumbnail` and `preview`. `[]` renders nothing, for files nobody will look at through the service. The others are `disabled` for good, see "Derived representations" |
| `expires_at` or `expires_in` | no, default `blobs.default_expires_in` | Absolute timestamp or duration such as `7d`, in the future. Refused together with `attachment`, and cleared by the first attachment, see "Attachments" |
| `attachment` | no | `owner_kind`, `owner_id` and `role`: attach in the same transaction as the creation, see "Attachments" |

Checksum is SHA-256 because browsers compute it cheaply with
`crypto.subtle.digest`, and S3 understands it natively.

The metadata also carries `checksum_md5`, the base64 MD5 of the same bytes:
the `checksum` an Active Storage blob record holds, in the same encoding
as our SHA-256 field, so a record's value pastes straight in. It is there
for a backend that keeps records in both systems while it migrates, in
both directions: declared at creation, it is checked against the bytes
like the SHA-256, so a fetched or uploaded file is known to be the one
the record describes; left out, the service computes it in the upload
pipeline and in the build, next to the SHA-256, so the backend can fill in
or reconcile the legacy column without reading the file back. Nothing in
the service depends on it.

The request body is capped at 16 MiB, enough for the widest single sheet's
`columns`: 16 384 of them with a kilobyte of name each. The cap is on the
whole declaration, so a workbook whose sheets together go beyond it is
refused with `413`.

### Input types

The name describes the shape of the `PUT` body, or, for `url`, that there
is none.

| Input type | Body | Produces |
|---|---|---|
| `raw` | The file bytes | The file |
| `multipart` | `multipart/form-data` with one part named `file` holding the bytes; the part's own filename, if any, is ignored | The file |
| `url` | No `PUT`: the service downloads the bytes from the `url` given at creation, in a job (see "Fetched blobs") | The file |
| `rows` | JSONL, one array of cell values per line, positional against the columns of the current sheet; an object line `{"sheet": "..."}` switches to a later sheet | CSV, XLSX or ODS per the blob content type, built by a job after the request (see "Generated blobs") |
| `archive` | JSONL, one `{"path": "...", "blob_id": "..."}` per line | ZIP containing the referenced blobs, built by a job after the request (see "Generated blobs") |
| `document` | JSONL, one workspace file per line: `{"path": "...", "blob_id": "..."}` for a file taken from a blob, `{"path": "...", "json": ...}` or `{"path": "...", "text": "..."}` for one given inline | PDF compiled by typst from the workspace, built by a job after the request (see "Documents") |

`url`, `rows`, `archive` and `document` are **private-only**. An `archive`
or `document` upload reads other blobs, so letting a browser drive it
would be a confused-deputy path into files the uploader does not own, and
a `url` blob makes the service fetch from the network on the creator's
say-so.

### Write-once

A blob receives bytes exactly once. `PUT` is accepted from `created` only; a
failed upload goes back to `created`, a successful one never does. There is no
re-upload, no per-blob switch, and no object version: the original lives at
one key (see "Object keys"). A backend that needs different bytes creates a new blob,
attaches it and detaches the old one, which then expires on its own: the way
Active Storage treats blobs as immutable and attachments as the thing that
changes.

Why: a public `PUT` URL is reusable until it expires, and will leak into
browser histories and proxy logs. Write-once means a leaked URL can only earn
a `409` after the first successful upload. It also means a signed URL for the
original is truthful for its whole lifetime, so originals can be served
`immutable`, and the server never has to reason about readers of a previous
version, re-scanning, or regenerating representations after a swap.

### Public upload rule

```
public_upload_allowed = !skip_virus_scan && input_type in (raw, multipart)
```

Enforced twice: when minting (the private API leaves the public upload URL
out for such a blob) and in the public `PUT` handler. Returned in metadata as a
computed field.

### State machine

```
created ──► uploading ──► uploaded ──► ready
   ▲            │   │         │
   └────────────┘   │         └──► infected
   (failed upload,  └──► ready | infected   (scanned inline, or skipped)
    nothing stored)

generated:  created ──► uploading ──► created   (a part stored, or refused)
            created ──► uploaded ──► ready      (the finish, then the build)

any non-terminal ──► expired
any unattached ──► deleted
```

| State | Meaning |
|---|---|
| `created` | Row exists, no original. `PUT` allowed. A generated blob holds its source parts here between requests; a `url` blob waits here for its fetch. |
| `uploading` | A `PUT`, or the fetch of a `url` blob, is in flight. Concurrent `PUT` gets `409`. Rows left behind by a crashed process are reset by the reaper (see below). |
| `uploaded` | What the client sent is in the bucket, encrypted. A job finishes the blob: the scan, or the build of a generated blob. Not readable. |
| `ready` | Scan passed or skipped, or the build done. Readable. |
| `infected` | Scan failed. Never readable. Objects kept for forensics until the blob is deleted or passes its `expires_at`. |
| `expired` | Past `expires_at`. Not readable. Objects purged. |
| `deleted` | Soft deleted. Objects purged. |

A blob is `expired` from the instant `expires_at` passes, not from whenever
the reaper gets to it: every read, upload and public lookup compares
`expires_at` with the current time and treats a blob past it as `expired`.
The reaper only records the state and purges the object, so expiry never
depends on its schedule. Until it runs, private metadata still reports the
stored state alongside the past `expires_at`.

Transitions are conditional updates (`UPDATE ... WHERE state = $from`) so two
concurrent requests cannot both win. Write-once is enforced by refusing `PUT`
from any state other than `created`. The upload's checksum and size are the
ones the blob was created with.

A failed or stale upload leaves `uploading` for `created`: nothing of it was
recorded, so there is nothing to keep (the parts a generated blob already
holds are not its, see "Generated blobs"; they are what makes a `created`
blob hold objects, and a delete or an expiry of one gets a purge like any
other; so does a blob an upload began on, for an object one may have left,
see "Object keys"). Both the abort path and the reaper apply this one
rule. Stale means abandoned, not slow: the pipeline touches
`upload_heartbeat_at` every third of `upload.stale_after` (default ten
minutes) while it runs, so an upload of any length keeps its row as long as
the process running it is alive, and only one that stopped mid-way gets reset
once `stale_after` has passed without a touch. `upload_started_at` is the
upload's claim on the row: the touch, the completion and the reset after a
failure all carry it. An upload that finds its row taken away, at a touch or
at the last check before its object would become visible, stops with `409`
and stores nothing, so one the reaper gave up on can neither finish on top
of the `PUT` that took the blob since nor hand that upload's row back.

A soft delete is refused for an attached blob (see "Attachments"); for any
other it schedules a purge of the objects `upload.purge_delay` (default one
hour) later, long enough for a download that started before the delete to
finish. Expiry gets the same grace period from the moment the reaper records
it. The purge deletes everything under the blob's prefix (see "Object
keys") and stamps `purged_at` on the row, which private metadata reports. An infected blob is
terminal and never becomes `expired`, but its objects are not kept forever
either: once it is past `expires_at` the reaper purges them and leaves the
state alone.

The row outlives the purge as a tombstone: `GET /blobs/:id` keeps answering
with the state, `deleted_at` and `purged_at`, so the backend can tell a blob
it deleted from an id that never existed, while the public port says `404`
either way. The reaper prunes tombstones `blobs.keep_purged` (default ninety
days) after `purged_at`, long enough for the question to have been asked;
the attachments and jobs that reference the row go with it, and the id is
then unknown like any other. Each reap prunes at most ten thousand, so a
backlog drains over successive runs, brought forward one after the other
(see "Jobs"), instead of one transaction holding every row and its
cascades. A terminal row that never had anything to
purge, a blob created and never uploaded to, has no `purged_at` and is pruned
the same time after it went terminal (its `updated_at`). Why a bound: every
blob carries an expiry until it is attached, so every direct upload a user
walked away from ends as a terminal row, and a table that only grows pays
for it in every index. An `infected` blob is pruned only through its purge,
which needs its expiry: one the backend holds on to stays.

Separate from the blob state, each blob tracks `scan_state`
(`pending`, `clean`, `infected`, `skipped`), `scan_signature` (what clamd
found, for `infected`, whichever path scanned the file; private metadata
only), a state per derived representation (`pending`, `ready`, `failed`,
`unavailable`, `disabled`), and `failed_at` with `last_error` when the job
that was to move it along died (see "Jobs").

### Attachments

An attachment records that something in the backend, its owner, uses a
blob: three strings, `owner_kind`, `owner_id` and `role`. `owner_kind` is
the kind of owner (`expense`, `user`), `owner_id` its identifier as the
backend writes it, `role` the role the blob plays for it (`receipt`,
`avatar`). A blob can be attached any number of times, from owners that
need not know about each other, and an owner can attach several blobs in
one role: agrafe does not know whether a role is one-to-one or
one-to-many, that is the backend's rule.

Why "owner": the attachments are what keeps a blob alive, the way owners
keep a reference-counted value alive, and a blob may have several. The
strings are opaque to agrafe, compared and never parsed, so the names say
what they are for rather than borrow an ORM's (Active Storage's
`record_type`, `record_id` and `name` map onto them one for one). Two
columns rather than one opaque `owner`: listing by kind alone is a query
the dashboard and the backend both need, and a separator inside one string
would be a format agrafe would have to parse.

Why in agrafe rather than in the backend: a blob's lifetime should follow
its use, and a blob row alone cannot say whether anything uses it. With the
attachments here, "in use" is a query and expiry is what happens to a blob
nothing uses.

**Lifetime.** A blob is created with an expiry: `expires_at` or `expires_in`
from the request, `blobs.default_expires_in` (default one day, at least an
hour) otherwise, long enough for a direct upload to be created, sent by the
browser and attached when the form is submitted. The floor is a guard on
the configuration: below it, a form takes longer to fill than its upload
lives. The first attachment clears
`expires_at`; detaching the last one sets it to the default again, counted
from that moment. There is no way to create a blob without an expiry: a blob
is either expiring or attached, and permanence is spelled "attached". The
invariant: `expires_at IS NULL` ⇔ attached. Attach and the last detach are
the only writes to `expires_at`, and no state transition makes one, so it
holds for a terminal blob too: an abandoned one the reaper expired keeps
its attachments and no expiry, a detach re-arms it. (An unattached
infected blob is therefore purged within the default expiry; the forensic
window for one the backend holds on to lasts until it lets go.)

**Attach.** `POST /blobs/:id/attachments` with the three fields. Idempotent:
a triple already attached returns the existing attachment. Refused for a
blob in a terminal state, or past its `expires_at` whether or not the reaper
has recorded it (`410` for an expired or deleted blob, `409` for an infected
one). A blob may be attached in `created`, before its bytes: a backend
creating a `rows`, `archive` or `document` blob knows its owner at creation, and
`POST /blobs` takes an optional `attachment` to do both in one transaction.

**Detach.** `DELETE /attachments/:id`. Works on a blob in any state; the last
detach re-arms the expiry even on a terminal blob, which is what gives an
infected blob its purge clock once the backend lets go of it.

**Delete.** `DELETE /blobs/:id` refuses an attached blob with `409`. Why:
blobs are shared across attachments that do not know of each other, so a
backend acting for one owner cannot know that the blob is free. Detach is
the only safe operation on behalf of one owner; a blob nobody uses expires
on its own, and a backend in a hurry detaches, then deletes. This is the
Active Storage footgun not reproduced: `purge` there destroys a blob other
records may still reference.

No blob state transition touches attachments. Delete cannot happen while
attached and ordinary expiry cannot either, so the terminal blobs that still
carry attachments are the ones the backend did not cause: an abandoned
blob, `created` and past `upload.abandoned_after` with no part arriving
or, attached, with nothing arriving at all, which the reaper expires
whatever its attachments (see "Abandoned blobs"), and `infected`. Both
keep their attachments so the backend can find out from the owner's side,
and the listing carries the blob's state, `blob_state`, so that the answer
does not cost a second request. The tombstone prune takes the attachments
with the row.

**Abandoned blobs.** An attachment at creation promises that the blob is
in use; it does not promise that its bytes will arrive. A browser that
never sends the `PUT`, a backend whose upload keeps failing, or a fetch
that gave up leave a blob `created`, attached, with no job and no expiry,
and nothing of its own would ever move it. The
reaper therefore treats a `created` blob that has been attached and
untouched for `upload.abandoned_after` (default one day, by its
`updated_at`) the way it treats a source nobody finished: it is expired,
attachments and all, and the owner's side sees `expired` in
`blob_state`. A `url` blob counts from its fetch's death instead, its
`failed_at`, since a fetch waiting in the queue touches nothing (see
"Fetched blobs"). An unattached `created` blob is left to its own
`expires_at`, which may be longer than a day on purpose. The rule is a
backstop: a backend that knows an upload is dead detaches and deletes.

**Concurrency.** Attach and detach lock the blob row (`SELECT ... FOR
UPDATE`) for their transaction, then change the attachments and
`expires_at` together. The reaper's expiry update waits on the same lock and
re-evaluates its predicate after, so an attach racing an expiry lands on one
side or the other, never both.

**Schema and listing.**

```
attachments (
  id          uuid        PRIMARY KEY  -- v7, the listing cursor
  blob_id     uuid        NOT NULL REFERENCES blobs ON DELETE CASCADE
  owner_kind  text        NOT NULL
  owner_id    text        NOT NULL
  role        text        NOT NULL
  created_at  timestamptz NOT NULL
)
UNIQUE (owner_kind, owner_id, role, blob_id)
INDEX  (blob_id)
INDEX  (owner_kind, role, id)
INDEX  (owner_kind, id)
```

`owner_kind` and `role` have a handful of values each; `owner_id` has as
many as the backend has owners. The unique index makes attach idempotent
and serves every lookup by owner, which is why `owner_id` is only accepted
together with `owner_kind`: an id means nothing without its kind, and there
is no index on it alone. `(owner_kind, role, id)` and `(owner_kind, id)`
serve the low-cardinality scans with the cursor; the first's prefix does not
serve the second, since the id order the cursor pages on is lost after
`role`. `role` alone has no index: nothing lists every avatar regardless of
what it belongs to. `(blob_id, id)` serves a blob's own attachments in
order and the last-detach check.

`GET /attachments` is cursor paginated on the attachment id like `/blobs`,
with filters `owner_kind`, `owner_id` (with `owner_kind`), `role` (with
`owner_kind` or `blob_id`: no index leads with it, and a rare role alone
would be a scan of the table) and `blob_id`; each row carries the blob's state as `blob_state`. `GET
/blobs/:id` and `GET /blobs` inline each blob's attachments, the first
hundred: a blob shared by many owners would otherwise make a metadata
read, and a page of them, as large as its fan-out. Past a hundred,
`attachments_truncated` says so and the listing by `blob_id` has the
rest. `GET /blobs` takes
`attached`. The three strings are validated like `filename`: not blank, at
most 255 bytes, no control or bidirectional formatting characters; unlike
`filename` they are not trimmed, since an identifier is the backend's to
write.

The public port never sees attachments. They are the backend's statements
about its own data, and nothing a browser is told.

### Listing

`GET /blobs` is cursor paginated on the blob id, which is a UUID v7 so ids
sort by creation time. Filters: `content_type`, `category`, `state`,
`scan_state`, `attached` (`true` or `false`: whether any attachment exists,
which is whether `expires_at` is null, see "Attachments"), `failed` (`true`
or `false`: whether the job that was to finish the blob died, see "Jobs").
Response carries `next_cursor` or null.

`attached` is asked of the blob row, not of `attachments`. An `EXISTS`
there is an anti-join no index serves: to fill a page of unattached blobs
it probes every attached one newer than the first match, and when old
blobs are all attached that is the whole table. On the row, the two
answers are two partial indexes on `id`, `WHERE expires_at IS NULL` and
`WHERE expires_at IS NOT NULL`, each walked backwards from the cursor.
Every row is in exactly one, so the pair costs one more index over the
table, appended at its right edge like the primary key; an attach or a
detach moves the row between them, a write it already pays for in
`blobs_expires_at_idx`. A test pins both plans.

Each filter has its own `(column, id)` index, and filters combine by AND
with no composite index behind them: two filters are one index walked
from the cursor and the other checked on each row. A page is cheap when
the combination is common among the rows the walk meets, and costs as
many rows as lie between matches when it is rare: `state=ready` with an
infected `scan_state` walks every ready blob for an empty last page.
That is a bound accepted for an operator's listing, which is the
dashboard's and the backend's occasional look, not a request path; a
combination that turns out to be both rare and asked often gets its own
composite index rather than the listing a scan budget.

The three listings, `/blobs`, `/attachments` and `/jobs`, take a `limit`,
default 50, at most 200. A limit out of that range, an unknown filter value
an `owner_id` without `owner_kind`, or a `role` without `owner_kind` or
`blob_id`, answers `400`.

### Classification

Content type is mapped to a category at creation and re-checked against sniffed
magic bytes during upload (`infer`, on the first 8 KiB). A mismatch between
declared and sniffed type is rejected with `422`. The check is lenient where
rejecting would be wrong: aliases (`image/jpg`), a declared container of the
sniffed type in either direction (a DOCX is a ZIP), the text sniffers (HTML,
XML, shell) against any declaration without magic of its own, and
`application/octet-stream`, which declares nothing. The text sniffers are
heuristics on the first bytes: an SVG that opens on a comment sniffs as
HTML, a script declared as JavaScript as a shell script. They contradict a
`text/*`, JSON, script or XML-packaged declaration (SVG, XHTML, GPX) never,
and a declaration `infer` has a matcher for or that is packaged in a ZIP or
an OLE file always, since its real bytes would have sniffed as it: a PNG
declaration on HTML bytes is a mismatch. Siblings such as DOCX against XLSX
are rejected, and so is the common mislabel of an XLSX declared as
`application/vnd.ms-excel`, which Excel-centric backends and some browsers
send for an `.xlsx`: to the sniffer it is another kind of file, and the
backend declares the type the file has. A
variant the sniffer cannot tell from its base format is checked as that
format: a macro-enabled or template Office file sniffs as the plain DOCX,
XLSX or PPTX, an OpenDocument template or master as its document, a HEIC
image as HEIF, a TrueType or OpenType font as the SFNT container, a WOFF2
font as WOFF, so an XLSM is accepted on XLSX bytes and refused on DOCX
ones. A variant is not an alias of its base: aliases are spellings of one
format (`image/jpg`, `application/x-gzip`), and only those are collapsed
before the allowlist and the category look at a type. The rules live in
`sniff.rs`.

**Accepted types.** `upload.content_types` is an installation's
allowlist, exact types or `type/*`, empty by default to accept any. It
is one list for the installation, as the installation serves one
application; a narrower list per form field is the application's to
enforce before it creates the blob. It is checked on the declaration of
a `raw`, `multipart` or `url` blob at creation, and again at upload in
case the list changed meanwhile. Generated blobs get their type from the
service's own build and are not checked. An exact entry takes the
aliases of its type and not its variants (listing `image/heif` does not
admit `image/heic`), so a list says what it admits.
An allowlist, not a blocklist: a blocklist misses what nobody thought to
list. The bytes are checked too where the declaration says nothing:
`application/octet-stream` (or `binary/octet-stream`, S3's spelling of
it) is compatible with any bytes, so a blob
declared as such must sniff as a listed type or as something packaged in
one (listing ZIP admits a DOCX). Otherwise declaring it would get any
file past the list. Not the other way round, and with none of the
leniency the sniffer has for a declaration: listing DOCX does not admit
a bare ZIP, listing `text/csv` does not admit HTML, listing Word 97 does
not admit the OLE storage every Office 97 file and every MSI installer
sniffs as. That leniency
exists because a declaration carries information the sniffer may fall
short of, and `octet-stream` carries none. Bytes that sniff as nothing
pass on the declaration alone. The
check is a lookup in a list held in memory, nothing per blob in the
database. The rules live in `allowlist.rs`.

JSON has no magic, but a JSON document that opens on a `Feature` or a
`FeatureCollection` in its first KiB is a GeoJSON one, and the sniffer says
so. Declared as `application/json`, `text/json` or `text/plain`, which is
how a `.geojson` tends to arrive, the blob's `content_type` becomes
`application/geo+json` as the upload completes (`sniff::refine`). The XML
analogue: an XML document whose root element, past the declaration,
comments and a doctype, is `<gpx>` or `<kml>` within the first KiB is a
GPX or a KML one, `application/gpx+xml` or
`application/vnd.google-earth.kml+xml`, and a declaration of
`application/xml`, `text/xml` or `text/plain` is refined to it. The ZIP
analogue: a ZIP whose first local file header names a `.kml` entry,
`doc.kml` as Google Earth writes it first, is a KMZ,
`application/vnd.google-earth.kmz`, and a declaration of
`application/zip` is refined to it; entry order is the writer's, so an
archive that starts with an icon is not recognised, and a client that
knows the field is a map declares the type itself. The helper is never
run on every ZIP to see whether it declines. These are the only cases
where the bytes change the declaration, in the direction of precision
only, and the category, `text` for the XML ones and `archive` for the
KMZ, is the same either way. The row and the
`Content-Type` the original is served with carry the refined type. A
refinement `upload.content_types` does not admit is not made: the blob
keeps its declaration, which is in the list, so a list never ends up
with a type outside it on a row. Categories:

`image`, `document`, `spreadsheet`, `presentation`, `archive`, `audio`,
`video`, `text`, `other`.

The mapping table lives in code (`classify.rs`), on the canonical type,
so an alias (`text/rtf`, `application/x-gzip`) lands where its type does.
PDF is a `document`. An office format classifies by the LibreOffice
application that reads it, from the one table of office formats
(`agrafe-types/src/office.rs`, also what the renderer converts and the
extractor opens): every Writer format is a `document`, every Calc one a
`spreadsheet`, every Impress one a `presentation`, the legacy binary
formats (DOC, XLS, PPT), RTF and the macro-enabled, template, slideshow
and master variants (DOCM, DOTX, XLSM, XLSB, PPSX, POTX, OTT, ODM, …)
with their base, so an `.xlsm` is not `other` and gets the spreadsheet
icon and filter. Draw's ODG and OTG are `document`s: pages, with nothing
closer. Every `+json` or `+xml` type is `text` whatever the registry put
before the suffix (XHTML, RSS, JSON-LD, GeoJSON, GPX, KML), as are JSON
and the script and configuration types (JavaScript, YAML, TOML, SQL,
shell, JSON lines), so they get the text sample as a thumbnail; the
compression formats xz, lzip, lz4 and compress are `archive`s with gzip,
bzip2 and zstd.

## Upload pipeline

Bytes flow once through a chain of consumers:

1. Size counter, rejects on exceeding the declared size, itself capped at
   `upload.max_size` when declared, or `upload.max_size` itself for a
   `url` blob that declared none. The HTTP body limit on the upload routes
   is `max_size` plus 1 MiB of slack for multipart framing, a backstop that
   the counter normally reaches first. A `raw` body whose `Content-Length`
   disagrees with the declared size is refused before it is read, and so
   is a fetched response's. The counter sees only the file, so a
   multipart body's framing is held to the slack on its own, as it is
   read: the bytes fed to the parser may run ahead of the file bytes it
   hands out by at most 1 MiB, or the body is a `413`. Why: until the
   `file` part yields, the parser buffers everything it reads and
   rescans the buffer on each read, so a body that never reaches a
   boundary, or a part header that never ends, would otherwise grow the
   buffer to the body limit (5 GiB by default) in quadratic time, for a
   blob declaring a file of any size. Parts before the file count as
   framing, and are therefore limited to about 1 MiB together. The parser
   drains every ready chunk before parsing, so the body is fed to it
   64 KiB at a time, yielding between pieces; otherwise a fast client's
   file would be buffered whole and refused.
2. SHA-256 and MD5 hashers, each compared against its declared checksum
   at the end when there is one, and written to the row otherwise (see
   "Creation").
3. Magic-byte sniffer on the first 8 KiB, compared against the declared
   content type.
4. ClamAV `INSTREAM` when the blob is not `skip_virus_scan` and the declared
   size is at or below `upload.inline_scan_max_size`. Above that, or when
   clamd cannot be reached, the scan runs as a job after storage and the
   blob waits in `uploaded`.
5. Chunked AEAD encryption (see below).
6. Multipart upload to the object store.

A failed check aborts the multipart upload, hands the blob back to the state
it came from and returns `422` (checksum, size, type) or `409` (state). On
success the blob moves to `uploaded`, or straight to `ready` when the scan was
inline or skipped, and the next jobs are enqueued in the same transaction: a
`scan` for a blob waiting in `uploaded`, a `render` for one that is
`ready`. The scan job enqueues the representations once the file is
clean, so nothing renders a file that has not been scanned.

An infected verdict from the inline scan is not a failed check. The object is
stored and the blob becomes `infected`, the same terminal state a deferred
scan produces, so the backend learns of it from the metadata whatever the
file size. The `PUT` itself answers `422` with code `infected`, so the
uploader learns at once rather than from a `200` that leads nowhere. The
signature name is in the message on the private port only: a backend wants
it, an anonymous uploader would use it to learn which signature to evade.
The verdict is read after the other checks: a wrong checksum
on an infected file is a plain `422` and nothing is stored.

The pipeline runs in its own task, fed by the request handler through a
channel. A handler future can be dropped at any moment (hyper cancels it on an
HTTP/2 stream reset, and a closed connection ends it on HTTP/1), and nothing
in that future is allowed to matter: dropping it closes the channel, the task
sees the body end before the declared size, aborts the multipart upload and
hands the blob back exactly as any other failed check does. Only a process
crash leaves work behind, which is what the stale-upload reaper is for: the
task touches the row while it runs, and a row untouched for
`upload.stale_after` has no task behind it. On S3 and Google Cloud Storage
the bucket also needs an `AbortIncompleteMultipartUpload` lifecycle rule, since
a crashed process cannot abort the multipart uploads it had open, and
neither an upload nor a build can abort one whose completion request
itself failed: the parts are in the bucket, the handle to them is gone,
and only the rule reclaims them. Azure needs no rule: it discards
uncommitted blocks after seven days.

### Object keys

Objects are keyed `<shard>/<blob id>/<name>`: `original` for the file,
`thumbnail` and `preview` for the representations, `source/<n>` for the
parts of a generated blob, with the shard the last two hex digits of the
id (`5b/0190b5a2-1c3d-7e4f-8a9b-0c1d2e3f4a5b/original`). Every key is
built, and parsed by the audit, in `blob::key`, so the layout has one
definition. Blobs are write-once, so the original's key is written exactly
once; a failed upload aborts its multipart upload and leaves no object
behind. One that fails after its object is complete, at the statement
that records it, hands the row back: taken back, the row proves the
statement did not commit, and the object is deleted; not taken back (the
statement committed and only its reply was lost, or another upload holds
the row by now), the object stays. Should the hand back fail too, or the
process die between the object and the statement, the object is left
under a row that says nothing of it; so any blob an upload began on
(`upload_started_at` set) counts as holding objects, and its delete or
expiry purges its prefix. Everything under `<shard>/<blob id>/` is the blob's, and that
prefix is what a purge deletes.

The shard is for the stores that split a bucket into key ranges as it
grows. OpenIO, which is what OVHcloud's S3 offer runs on, keeps a bucket's
listing in one database until it holds a critical number of objects, then
splits it in two at the median key, and splits each half again as it
fills; S3 partitioned the same way before 2018. Ids are UUID v7, ordered
by time, so a key that began with the id would be the newest key in the
bucket every time: every write would land in the last range, which would
split again and again while the others sat idle. That is the worst case
OVHcloud's performance guide describes, with the `503 SlowDown` throttling
that comes with each split, against a default of 300 writes a second per
bucket; its advice is a prefix of high cardinality to the left, and a date
prefix would be the same problem spelt out. The low byte of the id is
random for every id (the `uuid` crate fills the low 32 bits of a v7 at
random, the 42 above them are a counter) and spreads new keys over 256
ranges, so the splits divide incoming objects evenly. A hash of the id
would do the same with a computation the database cannot repeat cheaply;
the low byte it can, `get_byte(uuid_send(id), 15)`.

What it costs is the audit's merge order. The stores list keys in byte
order, which for this layout is the shard, then the id (the hyphenated
lowercase UUID sorts as the `uuid` column does), and the audit pages the
rows in that order through an expression index on the same two,
`blobs_shard_idx`: one more index, about a sixth larger than the primary
key for its integer column, walked once a week (see "Audit"). Nothing
else finds an object by anything but the blob's id, which the shard is a
function of.

That index is the one structure in the system whose writes scatter. The
primary key on v7 ids grows at its right edge, so inserts land on a few
hot pages; the shard index is keyed by a random byte, so every new blob
dirties a random leaf of an index that at a billion rows is some 70 GB
and does not sit in cache: a random read and a full-page WAL image per
insert. The spread that spares the bucket is paid for by the table. It
is the price of any index on a random id, Postgres takes it routinely,
and at the write rates one bucket allows it is nowhere near the
bottleneck, so the index stays. Should inserts ever show it, the audit
can do without: list the bucket as 256 prefix listings, one per shard,
each sorted by id, and merge them into one id-ordered stream with a heap,
paging the rows by the primary key as before. The cost moves to the
weekly job, about 40 MB for 256 pages of a thousand objects, and the
progress carries 256 cursors instead of one. Unlike the layout, that is a
decision open after deployment: dropping an index is cheap.

The layout is the one
decision a deployment cannot revisit cheaply, since objects are immutable
and moving a billion of them is a copy of the bucket, so it was
taken before the first one. A copy is what it would be: the AEAD binds a
ciphertext to the object's name within the blob, `<blob id>/<name>`, not
to the shard in front of it (see "Encryption at rest"), so a bucket laid out
anew decrypts as it is.

## Generated blobs

`rows`, `archive` and `document` blobs are generated by the service rather
than uploaded: the `PUT` carries a description, JSONL in every case, and
the file is built afterwards by a job. The request accepts; it does not
build.

Why: an archive of a hundred thousand entries and a hundred gigabytes takes
between a quarter of an hour and a few hours to assemble, bounded by the
network, while its manifest is ten megabytes and takes a second to send.
Building inside the request would leave the client waiting on a silent
connection for the rest, which every proxy, load balancer and HTTP client
between it and us cuts off after a minute or so, and a deploy of either side
cuts off for sure. Under the write-once rule a dropped `PUT` goes back to
`created`, so the work would be lost and the client would have to resend the
manifest and start over. Building in a job makes a crash or a deploy cost
one attempt, retried from the stored description with no client
involvement, and keeps the backend's side to a background job that sends
the description, in parts when it is large, and is told when it is done,
the same "accepted, finished by a job, reported by the webhook" contract a
deferred scan already has (see "Webhooks"; polling `GET /blobs/:id` is
the fallback).

`rows` would mostly be fast enough to build inline. It follows the same
shape anyway: one contract for the backend, one build job per type, one way
for a generated blob to become `ready`, rather than a second path to keep
correct for the one type that happens to be small.

### Accept

The source arrives in parts. `PUT /blobs/:id/parts/:n` stores part `n`,
numbered from one and consecutive, and `POST /blobs/:id/finish` seals the
source and enqueues the build. `PUT /blobs/:id` remains for a source that
fits one request: it is part one and the finish in one call. The two are
not atomic: a `PUT` that stored its part and then failed on the finish
leaves the blob `created` with one part, where a second `PUT` answers
`409` since part one has landed. `GET` shows `source_parts` at one, and
`POST /blobs/:id/finish` completes it.

Why parts: the expensive side of a `rows` export is the backend, which
paginates through a million records to produce a source of several
gigabytes and takes hours doing it. Behind one `PUT` that work rides on one
connection, every proxy between the two has an idle timeout that a slow page
can trip, and a deploy of either side resets the blob to `created` and the
export to zero. A part is a short request holding what the backend has
produced since the last one, a few tens of megabytes, so a failure costs one
part, a deploy costs nothing, and the backend needs no streaming HTTP
client: it builds a page's lines in memory and sends them. The backend
learns where to resume from `GET /blobs/:id`, which reports `source_parts`;
a part that is not the next one answers `409`, so a retry of one that
landed is harmless.

Each part streams through the upload pipeline like any body: encrypted,
stored as `<shard>/<blob id>/source/<n>`, bounded by `upload.max_size`, the row in
`uploading` with the heartbeat while it is in flight and back in `created`
after, where it waits for the next part or the finish. The first part
fixes the blob's data key and segment layout on the row; every part and
the built file are streams under that one key, each with its own header,
a part's read off its object when the build opens it. A part is whole
lines, none longer than a megabyte, and must end with a newline; an empty
part is a part with no lines. It is parsed as it arrives and every line is
checked before the response, per the rules of its type below. A failing
line answers `422` naming the line, counted within the part, and the
reason; the part is discarded and the blob returns to `created` with the
parts it already had, like any failed upload: a failed part leaves nothing
of itself. A finish on a blob with no parts, or an uploaded part out of
sequence, answers `409`, as do parts and finish on a `raw` or `multipart`
blob. The finish moves the blob to `uploaded` with its build job, `rows`,
`archive` or `document` after the input type, enqueued in the same
transaction, unreadable until the job is done.

This is the one place a blob holds objects before it is `uploaded`, so
`created` means "no original, maybe parts", not an empty prefix. The
failed-upload rule, the stale-upload reset and the purge on terminal states
are about the original; the parts go with the purge of the prefix.

What the accept pass has to carry across parts stays small and sits on the
row: `source_parts`, `source_size` (bytes of source), `source_rows`
(lines) and `source_updated_at`, the first three reported by `GET` for
generated blobs, and for a `rows` blob the sheet the source is filling and
the rows in it so far (`source_sheet`, `source_sheet_rows`), since the row
limit is per sheet. The `source_columns` of a `rows` blob are written at
creation and only read by accept and build. Anything per line is left to
the build, which reads the source in order anyway. Parts are capped at
10 000.

A source that is never finished is an abandoned export. `DELETE` is the
backend's way out; as a backstop the reaper expires a `created` blob whose
last part is older than `upload.abandoned_after` (default one day), and
the purge takes the parts with the prefix. The same clock expires an
attached `created` blob with no parts that nothing has touched, see
"Attachments".

### Build

The build job streams the generated file into the bucket as it produces
it: nothing is held whole in memory, and nothing touches local disk but
the workspace a `document` build hands to its compiler (see
"Documents"). The
bytes go through the same encryptor and multipart upload as an uploaded
file, with 32 MiB parts (S3 allows 10 000 parts per upload; the default
5 MiB would cap an archive at 50 GB), to the original's key, written
once: a failed attempt aborts its multipart upload and the next starts over
from the source.

No scan: a generated file contains only what the service wrote, from
members that are `ready` or from scalar values, so a generated blob is
created `skip_virus_scan` whatever the request said. A `ready` member was
scanned clean, or was uploaded `skip_virus_scan` by the backend, which is
its call to make for its own content and which the archive inherits: an
archive is no more and no less trusted than its members. Once `ready`, a
generated blob is rendered like any other: a CSV is text and gets a text
thumbnail, an XLSX or ODS its first page when `media.office.command` names
a LibreOffice and the category icon otherwise, a ZIP the category icon,
and none has a preview.
When the build finishes the blob moves to `ready` with
the checksum and the final size on the row. A failure the next attempt
would repeat, a source line that does not parse, a row missing its key
material, a key that does not unwrap, a stored source or member that is
damaged (a broken header, a length no stream produces, a segment that fails
authentication), or a package the ZIP writer refuses, kills the job at once
rather than through the backoff; a bucket or database error, or an object
cut short in transit, is retried. Out of attempts, the blob is stuck in
`uploaded`, unreadable, like a scan that never completes. No new state: `uploaded`
already means that what the client sent is in the bucket and a job has to
finish the blob. The source is purged with the rest of the prefix.

Builds are three job kinds, `rows`, `archive` and `document`, each with
its own worker pool: `jobs.concurrency.rows` (default two),
`jobs.concurrency.archive` (default one) and `jobs.concurrency.document`
(default two). A kind's workers are the only thing that runs it, so a batch
of exports cannot take a scan or render worker, and a hundred-gigabyte
archive cannot hold up a rows export of a hundred lines. An archive build
saturates a network interface, hence one at a time per process; a process
given `jobs.kinds = ["archive"]` on a machine of its own is how to run
more. Within `archive`, the blob's priority orders the queue: an export a
user is waiting on, created `high`, goes before a nightly batch created
`low`.

### Rows

The sheets are declared at creation, in one of two forms. `columns`, a list
of columns, declares one sheet, named after the filename's stem. `sheets`, a
list of `{"name": "...", "columns": [...]}` in workbook order, declares a
workbook; a list of one is a single sheet with a chosen name. `sheets` is
refused for `text/csv`, which holds one sheet, and capped at 1 000 sheets:
Excel has no limit and LibreOffice allows ten thousand, but the declaration
is read back on every part and nothing legitimate needs more. One of the two
is required for a `rows` blob and both are refused on any other input type.
Either is stored on the row as `source_columns`, a list of sheets, from which
accept and build read it, with a query of their own: every other read of the
row names its columns and leaves this one out, so that a listing page of
`rows` blobs does not fetch and decode up to 16 MiB of JSON for each.

A column is an object with a `name` (the header text, any non-empty string
without control or bidirectional formatting characters, the filename's rule,
duplicates allowed) and an optional `type`. Each line of the source is a JSON
array with exactly one value per column of the current sheet, in that order;
a line that is not an array, or has the wrong length, is refused. A value is
a scalar: a string, a number, a boolean or `null`, which is an empty cell.
An object or an array as a value is refused.

Why declare rather than infer from the first row or the union of keys: the
backend always knows its columns, and a column order inferred from the data
changes between two exports of the same table when an optional field first
appears on a different row. Why arrays rather than objects: with the
columns declared, a key on every line is only repetition, a few hundred
megabytes over a million rows of twenty columns, and object keys cannot
express two columns with the same header, which form exports produce.
Why not nested values: a spreadsheet cell holds a scalar, and any
flattening (JSON text, dotted keys) is a guess better made by the backend.

A column without a `type` takes what the JSON scalar says: a number is a
numeric cell, a boolean a boolean cell, a string a text cell. Two types can
be declared, `date` and `datetime`, whose values must be `null` or a string
in ISO 8601 form, checked at accept: `YYYY-MM-DD` for a date,
`YYYY-MM-DDTHH:MM:SS` with an optional fraction and an optional offset in
any ISO 8601 form, `Z`, `±HH`, `±HHMM` or `±HH:MM`, for a datetime.
Spreadsheets have no timezone, so a datetime is written
as the wall-clock time given and the offset is dropped: the backend formats
in the zone its users read, and what it sends is what shows. Dates are
never inferred from the shape of a string: a free-text value that happens
to look like a date must stay text, the backend knows which columns are
dates, and a wrong guess is the classic spreadsheet corruption of
user-entered data. There are no other types: durations, currencies and
percentages are display formats, and a backend that wants `Oui` rather
than `VRAI` sends the string.

Numbers are written as their JSON lexeme, never parsed to a float and back,
so `1.10` stays `1.10`; one that does not fit a double, `1e999`, is refused
at accept, since no cell can hold it. Excel keeps fifteen significant
digits, so a backend exporting 64-bit identifiers sends them as strings.

**Sheet names.** A declared name is validated as Excel wants it, since
Excel refuses to open a workbook that breaks the rules: not blank, at most
31 characters in UTF-16 units, none of `[ ] : * ? / \`, no control
characters, not starting or ending with an apostrophe, not `History`, which
Excel reserves for its change tracking, and unique among the sheets
ignoring case. LibreOffice forbids the same characters. The name taken from
the filename's stem is made to fit the same rules, and is `Sheet1` when
nothing is left or when the stem is `History`.

**Switching sheets.** In the source, an object line `{"sheet": "..."}`
switches the current sheet; rows are arrays, so the shape is unambiguous.
Lines before any switch belong to the first sheet, so a single-sheet source
never carries one. The switch must name the current sheet or a declared
sheet after it, which keeps the source grouped by sheet in workbook order:
an XLSX sheet is one zip entry written whole before the next, an ODS
`content.xml` holds its tables in sequence, and without grouping the build
would need one pass over the source per sheet. Naming the current sheet is
a no-op, the first sheet included, so that a backend looping over its
sheets may announce each one before its rows without special-casing the
first; it does not start the per-sheet row count over. A switch to an
earlier sheet or to an undeclared name fails the part at its line, like any
bad line, and so does an object line that is anything but
`{"sheet": "..."}`. A sheet no switch points at gets a header only. The
build reads switches the same way, skipping one that names the sheet it is
writing rather than ending the sheet on it. A switch is valid in any part, so
parts stay uniform and resuming stays `source_parts` alone; switch lines are
not counted in `source_rows`. Parts per sheet were rejected because they
break that one-counter resume rule, and a sheet field on every line because
it doubles the bytes per line for a value that changes a handful of times
per export.

**Formats.** The output format is the blob's `content_type`: `text/csv`,
the XLSX type or the ODS type; anything else is refused at creation. All
three are streamed row by row, the header first:

- CSV per RFC 4180, UTF-8 with a byte order mark so Excel reads the
  accents. Numbers are written bare, booleans as `true` and `false`, dates
  and datetimes as their ISO string. A value starting with `=`, `+`, `-`,
  `@`, a tab or a carriage return is prefixed with a single quote: a CSV
  opened in a spreadsheet evaluates such cells as formulas, and a backend
  exporting user-entered text must not be able to ship `=HYPERLINK(...)` to
  an agent's desktop. CSV has no types, and a French Excel splits on `;`
  and reads `3.14` as text, so the CSV is the format for machines and the
  XLSX is what an agent opens.
- XLSX with typed cells and inline strings rather than a shared string
  table, which would need a second pass over the data. Text cells are never
  evaluated. Dates and datetimes are serial numbers styled with the
  built-in number formats 14 and 22, so Excel shows them in the user's
  locale; `t="d"` cells exist but older Excel does not read them.
- ODS with typed cells, the typed value in its attribute and the displayed
  text alongside, so a viewer without a data style still shows something
  sensible.

No spreadsheet library: the ones available build the workbook in memory.
The XML is written by hand and deflated as it is produced, and the package
goes through the archive's own ZIP writer (see "Archives"). A deflated
entry's header goes out before its size is known, so it always carries
zip64 fields: a sheet near the row limit with a few dozen text columns is
more than 4 GiB of XML, and without them the build would die on it after
the backend had streamed the whole source. The format details, escaping,
the characters XML cannot carry, Excel's 1900 leap year and the whitespace
ODF collapses, are documented on the writers in `jobs::rows`.

XLSX and ODS sheets hold at most 1 048 576 rows, the header included, and
16 384 columns, and a cell at most 32 767 characters, counted as Excel
counts them, in UTF-16 units. A sheet beyond the column limit, or a column
name beyond the cell limit, is refused at creation; a source beyond a
sheet's row limit, or a text beyond the cell limit, at accept with the line
that crossed it, whichever part it is in. Excel repairs a workbook with a
longer text, truncating the cell, and says so on opening. CSV has none of
these limits. A `rows` blob with no lines produces a header only.

### Archives

An `archive` blob is declared `application/zip`; anything else is refused
at creation. Its source is one `{"path": "...", "blob_id": "..."}` per line,
the manifest. At accept, per line:

- The path is normalised to NFC, with `/` as the separator, then refused
  if it is empty, absolute, has an empty, `.` or `..` component (so no
  trailing slash either), a backslash, a control or bidirectional
  formatting character (the filename rule), or exceeds 65 514 bytes: the
  ZIP limit of 65 535 less the 21 a deduplicated name can grow by, so the
  build never meets a name it cannot write. `ERRORS.txt` at the root is
  reserved, see below, and refused whatever its case.
- The blob is looked up in batches of a few hundred ids (`WHERE id =
  ANY($1)`) and must exist and be `ready`. Anything else (unknown, not yet
  scanned, infected, expired, deleted) is refused.
- The same blob may appear under several paths and is written each time.

Duplicate paths are deduplicated by the build rather than refused at accept,
in manifest order: a path already taken becomes `stem-1.ext`, `stem-2.ext`,
... with the counter before the extension (`a.tar.gz` collides into
`a-1.tar.gz`; a name without a dot, or with only a leading one, gets the
counter at the end), the first free name wins and is taken in turn. Taken is
case-insensitive, since `Report.pdf` and `report.pdf` land on the same file
when extracted on macOS or Windows. The taken set is the build's, held in
memory for the one pass; a hundred thousand paths is a few megabytes, and
nothing about it needs to survive between parts.

Why dedupe: paths come from user filenames, which collide all the time, and
a refused manifest for two `invoice.pdf` would push every backend to write
the same renaming loop. Why in manifest order: the first occurrence keeps its
name, which is what a user expects.

Entries are stored, not compressed: the members are PDFs, images and office
files, already compressed, and deflate would cost an hour of CPU per hundred
gigabytes for nothing. With stored entries and every member's size on its
row, the archive's size is known at the finish, the sum of the members plus
each entry's headers, a fixed part and its path twice, accumulated part by
part on the row's `size`,
which is therefore an estimate while the blob is `created` or `uploaded`
and exact once it is `ready`. It is checked against `archive.max_size`, a
limit of its own since `upload.max_size` is sized for one request, at most
309 GiB: 10 000 parts of 32 MiB, less one percent because the parts carry
the ciphertext, a tag per segment longer than the estimate, plus the
`ERRORS.txt` and the end records the estimate leaves out. The entries are
bounded too, by `archive.max_entries` (default a million), checked as the
manifest is accepted: the build keeps a few hundred bytes per entry, the
central directory, the names taken and the errors, until the end, and
that memory needs a ceiling as the size has one.

The build streams the ZIP as it assembles it, through a writer of its own
in `jobs::build::zip` rather than the `zip` crate, which stays out of the
server crates. The format is sequential by design: each entry is a local
header, the bytes, then a data descriptor with the CRC and sizes, and the
central directory comes last, so the writer never seeks. Sizes are known
up front, so the local header carries them too and only the CRC waits for
the descriptor. Zip64 is decided per entry from its size before its header
goes out, since a streamed header cannot be rewritten, and a deflated
entry of unknown size, which only a spreadsheet package has, always takes
it; the central directory switches to zip64 on its own for the entry
count and the offsets. Entries are made on Unix with mode `0644` and the member's
`uploaded_at` as their time. Readers that go by the central directory,
which is every desktop tool, handle data descriptors; strictly sequential
readers of stored entries may not, which is accepted.

Sources are read in manifest order through the usual decrypting reader, a
few fetched ahead of the one being copied, because most entries are small
and a serial build would spend longer waiting for the first byte of each
object than copying it. A member up to 8 MiB, which is most of them, is
read whole when fetched ahead; a larger one is opened when its turn comes.
Holding a response open and idle while a long entry copies is what the
look-ahead must not do: an object store cuts such a connection off after a
while, and that would fail the attempt. Memory is bounded by 8 MiB times
the look-ahead, plus a segment for the member being streamed and the parts
in flight.

A source that was `ready` at accept time can be deleted, expire or be purged
before the build reaches it. The build does not fail for it: the entry is
skipped, and the archive ends with an `ERRORS.txt` at its root listing every
skipped entry, one per line, as the path, the blob id and a reason code
(`deleted`, `expired`, `infected`), tab separated, which a path cannot
contain. No such file is written when nothing was skipped. The row records the number of entries and of skipped ones, which
`GET` reports as `archive_entries` and `archive_errors`, so the backend
learns that an archive is incomplete without downloading it.

Why: a user waiting for an export of a thousand files wants the nine hundred
and ninety-nine that exist and a note about the one that does not, not a
failure an hour later. Why only for sources that are gone: a bucket or
database error while reading a source is ours and fails the attempt, which
retries; an archive must never come out incomplete because of a transient
error of ours.


### Documents

A `document` blob is declared `application/pdf`; anything else is refused
at creation. It is a PDF compiled by [typst](https://typst.app) from a
workspace of files: a template, its includes, its images and its fonts,
most of them blobs the backend already holds, plus the data of the one
document, inline. An optional `main` at creation names the entry file,
`main.typ` by default, as a path under the manifest's rules below; the row
keeps it as `source_main` for the build.

**Templates are blobs, registration is attachment.** No template registry,
no configuration entry, no `template` flag on blobs. A backend uploads
`invoice.typ` as an ordinary `raw` blob, attaches it to a `template` owner of
its own and remembers the id. A new version of the template is a new blob,
attached in place of the old one, which expires on its own: the write-once
rule and the swap that replaces any file, nothing new. Why not configuration:
templates change with the product, not the deployment. Why not a registry
API: it would duplicate the storage, encryption, listing and lifetime that
blobs already provide.

**One manifest, no custom protocol.** The source is a workspace manifest,
JSONL like the other generated types: the archive's line shape, plus one
form for inline files.

```jsonl
{"path": "main.typ",       "blob_id": "…"}
{"path": "lib/header.typ", "blob_id": "…"}
{"path": "logo.png",       "blob_id": "…"}
{"path": "Inter.ttf",      "blob_id": "…"}
{"path": "data.json",      "json": {"client": "…", "items": []}}
```

Per line, at accept:

- The path follows the archive's rules without the ZIP particulars: NFC,
  `/` separated, relative, no empty, `.` or `..` component, no backslash,
  control or bidirectional formatting character, at most 1 024 bytes with
  no component beyond 255, which is what the worker's file system takes.
  A path given twice in one part is refused at its line. Twice across
  parts, or a path that is a file on one line and a directory on another
  (`lib` and `lib/a.typ`), is the build's to refuse, since only the build
  sees every part: carrying the set of paths on the row would buy a `422`
  a few seconds earlier for a manifest of a handful of lines that the
  build reads whole anyway.
- A `blob_id` line is a file taken from a blob: the archive's batched
  lookup, and the blob must exist and be `ready`.
- A `json` or `text` line is an inline file, bounded by the one-megabyte
  line rule; larger data becomes a blob first. The JSON reaches the
  workspace exactly as written, key order and formatting included, and a
  `null` is a value. A line takes exactly one of the three.
- The workspace's size, the members' sizes plus the inline bytes, is
  accumulated on the row (`workspace_size`) and bounded by
  `document.max_workspace_size` (default 256 MiB) as the manifest is
  accepted, then again by the build. A limit of its own: `upload.max_size`
  is sized for one request, and the workspace is written to the worker's
  disk. The files are bounded too, by `document.max_files` (default
  10 000), since bytes alone bound nothing: an empty inline file costs
  none, and a manifest of a few million of them would cost the worker as
  many files and directories on disk. Lines bound the files and not the
  directories their paths make, a directory per component, so a path is
  at most 32 components deep at accept, and the build refuses a
  workspace whose directories run past `document.max_files` as well. The PDF's size is unknown until it
  is built, so unlike an archive's, a document's `size` is null until it
  is `ready`.
- Nothing checks that the manifest holds the main file: it may come in a
  later part, and a workspace without it fails to compile with the
  compiler's own "file not found".

The template is plain typst: `#let d = json("data.json")`, `#include
"lib/header.typ"`, `#image("logo.png")`. Paths are relative to the
workspace root; escaping it is refused by typst's own virtual paths, which
the helper resolves every file through. Fonts: every `.ttf` and `.otf` in
the workspace is registered in the font book, in path order, plus the
compiler's embedded fonts (Libertinus Serif, New Computer Modern, DejaVu
Sans Mono), so a template works with nothing declared; a shared
`media.fonts_dir` can come later. Packages (`@preview/...`) are refused
with an explicit error: the sandbox has no network, and the workspace is
meant to hold every file a document uses. `datetime.today()` is the UTC
date, since the sandbox has no local time zone.

Template, data, assets and includes are all just files; which ones are
reused across documents is the backend's business. Accept, parts and
finish are reused unchanged. `document` is private-only, like `rows` and
`archive`: it reads other blobs on the creator's say-so.

**No JSON schema.** The backend owns both the template and the data; typst
fails loudly on a missing key and the job reports the diagnostic, file,
line, column and message, as its error. A schema would only turn a dead
job into a `422` a few seconds earlier, and there is nowhere natural to
hang it without a registry. Additive later, as one more inline file with a
declared role, if ever needed.

**Embedded compiler, inside a helper binary.** The crate
`agrafe-render-document`, a library plus a binary like `agrafe-extract`,
links the `typst` library with a `World` of its own over the workspace
directory. The server never links it; the `document` job runs the binary
through `agrafe_sandbox` with the usual limits and timeout. Why not
`typst-cli`: its world is the real file system under `--root`, a weak
jail, its fonts come from system discovery, its `@preview` imports fetch
from the network, and the custom world is what makes the workspace model
work. Why not in-process: `typst` pulls in image decoders, SVG and font
parsers, exactly what "nothing decodes user files in the API process"
forbids.

The plumbing matches the other helpers: `agrafe-render-document --workspace
<dir> --main <path> --output <file>`, stdin null, a clean environment, no
configuration and no logs; what it writes on stderr is the compiler's
diagnostics. The job writes the workspace into a fresh directory under
`media.tmp_dir`, inline files as they are and members decrypted through
the usual reader, runs the helper on it, and streams the PDF it wrote
through the same encryptor and multipart upload as the other builds. This
is the one build that touches local disk; the directory goes with the job.

**Build semantics** follow `rows` and `archive`: a `document` job kind with
its own worker pool, `jobs.concurrency.document` (default two: a compile
is CPU bound and the sandbox bounds it), no scan (`skip_virus_scan` forced,
the members were `ready`), the blob's priority ordering the queue. Dead at
once rather than through the backoff: a document that does not compile,
the diagnostics as the error, one per line as `path:line:column: error:
message`, warnings before errors since the sandbox keeps the tail of
stderr; a referenced blob gone between accept and build, which unlike an
archive's cannot be skipped, since a template cannot do without a file; a
path twice or both file and directory; a workspace over the limit; a
compile the sandbox had to kill at the time limit, since the same template
would take as long again. The helper tells the two apart by its exit code:
2 is the compiler's verdict, with the diagnostics on stderr; anything else
(a full disk, a crash) is the helper's own failure. Retried: those, the
sandbox failing to start the helper, the temporary directory, the bucket,
the database. Out of
attempts, the blob is stuck in `uploaded` with `failed_at` and
`last_error`, like any build. Once `ready` the PDF gets its preview and
thumbnail through the existing PDF path: the first page as its thumbnail,
no preview unless it is a wrapped image.

**Probe.** A worker that handles `document` jobs compiles a probe workspace
through the exact command line the job uses before it starts, as the
renderer probes vips and the text helper: a template importing a file,
reading JSON and placing an image. Templates are written against a
compiler version, pinned in the helper (typst 0.15), and a
`render_document_command` that is not the helper, or a helper that cannot
read its workspace, must fail the deployment rather than every document.

Out of scope for now: a template registry or schema validation, typst
packages, PNG or SVG output (which the content type could select later),
a shared fonts directory.

## Fetched blobs

A `url` blob is one the service downloads instead of receiving through a
`PUT`: the backend gives the URL at creation and sends no bytes. The first
use is migration from another storage service (Active Storage on S3, a
CDN): one `POST /blobs` per record, with a presigned URL, and the files
move between the two stores without passing through the backend.

### A fetch is an upload the service performs

No accept phase, no `PUT`, no new blob state. A `url` blob is created with
its URL and a `fetch` job enqueued in the same transaction. The job is an
uploader: it claims the row `created → uploading` the way the `PUT`
handler does, feeds the HTTP response body into the existing pipeline
(size counter, SHA-256 and MD5 hashers, sniffer, inline clamd, encryption,
multipart store) and lands on `uploaded` or `ready` like any other upload.
Write-once, the heartbeat, the stale-upload reset, the hand-back to
`created` on failure and the type checks all apply unchanged. The claim is
taken once the remote has answered, so the row is `uploading` for the
body alone, not for a slow connect.

Why not parts and finish: that contract exists because a `rows` or
`archive` source is large and arrives over time. A URL is one line and
arrives with the creation. `uploaded` would also be false for it, since
nothing of the client's is in the bucket. `created` means "no original
yet", which is true of a blob waiting for its fetch.

`PUT /blobs/:id`, parts and finish answer `409` for a `url` blob, as parts
do for a `raw` one. `url` is private-only and `public_upload_allowed` is
false for it: the private API leaves the upload URL out of the creation
response.

### Creation

`input_type: "url"` plus a required `url` field. The rest of the
declaration is the usual one; `priority` is what a migration batch sets
`low`, so a fetch a user is waiting on goes first. Checksums and size:

| Field | `url` | `raw`, `multipart` |
|---|---|---|
| `checksum` (SHA-256, base64) | optional, checked when given; the computed value is written to the row otherwise | required |
| `checksum_md5` (MD5, base64) | optional, checked when given | optional, checked when given |
| `size` | optional; enforced when given, `upload.max_size` otherwise; a `Content-Length` that disagrees is refused before the body is read | required |

Active Storage's `checksum` is the base64 of the raw MD5 digest, the same
encoding as our SHA-256 field, so a record's value pastes straight into
`checksum_md5`. The pipeline already computes MD5 next to SHA-256; the
change is a compare in the checker and a declaration with optional digests
and an optional size. A response that carries a `Content-Length` and no
declared size takes it as the cap, not as a declaration: the inline-scan
decision is made on it as on a declared size, hyper fails the body if it
falls short of it, and a mismatch is never reported as one against a size
the backend did not give.

The URL is stored on the row, since retries need it, reported in private
metadata only, and cleared by the completion of the upload: a retry reads
it only while the blob is `created`, and after that it would be a stale
secret on every listing. It never appears in `last_error`, logs or public
metadata: migration URLs are presigned, so they are secrets with an
expiry. The HTTP client's errors carry the URL and are stripped of it
before they are written anywhere, and the server caps the client's own
crates (`reqwest`, `hyper`) at `info` in its log filter, since they
name request targets at `debug` and below. Their expiry is one more
reason the retry window stays short.

### The fetch job

A `fetch` job kind with its own worker pool (`jobs.concurrency.fetch`,
default two), so a migration batch cannot take a scan or render worker.
The handler: find the blob, open the URL, claim the row, hand the body
stream to the pipeline. On a blob that is not `created`: `uploading` is a
transient error, not a quiet success. The lease timeout and the
stale-upload timeout are both ten minutes, so after a crash mid-fetch the
job can be reclaimed before the reaper has handed the row back to
`created`; a handler that gave up quietly there would be deleted as done
and leave the blob with no job at all. The retry is scheduled for when
the reaper will have reset the row, `upload.stale_after` past its last
heartbeat plus one `jobs.reap_interval`, rather than through the backoff,
so one crash costs the job two attempts (the one that crashed and the one
that waited) and not the whole budget. Any other state (`uploaded`,
`ready`, terminal), expiry included, is "nothing to do", like the scan
job on a blob that is not `uploaded`.

Errors are a `FetchError` with an `is_permanent`, like `BuildError`:

- Permanent, dead at once: a 4xx other than 408, 425 and 429; a checksum,
  size or type mismatch; a body over the cap; a host that resolves to a
  refused address; a TLS failure; a redirect to a refused destination, or
  too many of them; a URL refused by the policy below.
- Transient, retried through the backoff: a 5xx; 408, 425, 429; a host
  that does not resolve, since a resolver's bad minute looks the same as a
  typo and a migration batch must not die of one; connect, read and
  whole-response timeouts; a reset; a body shorter than `Content-Length`;
  the row found `uploading`; a bucket or database error.

`Retry-After` on 429 and 503 is honoured, as delta seconds or an HTTP
date: the job carries the requested delay and `fail` schedules the retry
at `max(backoff, retry_after)`. A value past `fetch.max_retry_after`
(default fifteen minutes) is treated as permanent rather than parking the
job. With the existing backoff (30 s doubling) and `max_attempts`, a fetch
gives up after roughly eight minutes end to end, which is the intent: not
very long.

Out of attempts, the blob stays `created` and the machinery that already
exists for a dead scan, build or render takes over (see "Jobs"): the
runner writes `failed_at` and `last_error` on the blob row, since `fetch`
is a kind that finishes its blob, so private `GET /blobs/:id` shows them,
the attachments listing shows `blob_failed_at`, and `GET
/blobs?failed=true` finds them after the dead job itself is pruned.
Nothing fetch-specific is reported from the job row.

Repair is `POST /blobs/:id/retry`: a `created` blob with `input_type:
url` waits on a `fetch`, so the retry enqueues one, deletes the dead fetch
job and clears the failure, like for the other kinds. It is the backend's
call whether that is useful: a presigned URL has an expiry, and a retry a
day later usually needs a new blob with a fresh URL instead. `POST
/jobs/:id/retry` works too while the dead job is around.

Lifetime: an unattached `url` blob expires on its own. An attached one (a
migration creates with `attachment`, which clears the expiry) whose fetch
gave up is caught by the abandoned rule: a `created`, attached `url` blob
whose fetch died more than `upload.abandoned_after` ago (default one day,
by `failed_at`) is expired by the reaper, attachments kept, so the owner's
side sees `blob_state: expired` (see "Attachments"). The day counts from
the failure, which gives the backend a day to see the failure before the
blob goes, and only from it: a fetch waiting in the queue touches nothing,
and a migration's backlog may hold one for longer than a day, so a `url`
blob is not abandoned while its fetch is pending however old its row. An
unattached one has no such reprieve: its `expires_at` is the backend's to
size, and a migration that does not attach as it creates sets an
`expires_in` that covers the queue. A migration therefore has two queries
for its failures: `GET /blobs?failed=true` within the day, `GET
/attachments?owner_kind=…` with `blob_state` after it.

### Network policy

The private port is trusted, but the policy is cheap now and hard to
retrofit:

- `https` only; no credentials in the URL; a fixed `User-Agent`; no
  cookies, no forwarded headers, no proxy.
- The host is resolved by the service, loopback, private, link-local and
  cloud metadata ranges are refused (the full list is `jobs::fetch::refused`:
  it includes shared address space, the IPv6 unique-local and link-local
  blocks, and an IPv4 address mapped into IPv6), and the connection is
  made to the resolved addresses, so a DNS rebinding cannot swap it between
  the check and the connect: the client's resolver is ours, and a name that
  answers with one refused address among several is refused whole. A
  literal address in the URL never reaches a resolver and is checked on
  its own, at creation already, so a backend mistake is a `422` rather
  than a dead job.
- Redirects: at most `fetch.max_redirects` (default five), every hop
  through the same checks, `https` only.
- `fetch.connect_timeout`; `fetch.read_timeout` (default thirty seconds)
  on every wait for the next bytes, which is what stops a remote that
  trickles from holding one of the few fetch workers, and what lets a
  large file on a slow link run as long as it keeps flowing. The wait is
  timed from the moment the pipeline asks for the bytes, not from the
  last ones: a store write or a scan holding the pipeline for longer is
  not the remote's silence;
  `fetch.timeout` (default six hours) on the whole response as a backstop.
  The size is bounded by the pipeline's counter.
- `fetch.allow_private_networks` (default `false`) lifts the scheme and
  address rules for local development and the integration tests, which
  serve fixtures from an in-process listener on localhost over plain
  `http`.

The declared `content_type` is what the blob gets; the response's
`Content-Type` is ignored and the sniffer checks the bytes, as for any
upload.

`reqwest` is already in the dependency tree through `object_store`'s S3
support, with rustls like sqlx, so no new TLS stack. The client is built
once per process from `[fetch]`.

### What a migration looks like

For each Active Storage blob: `POST /blobs` with `input_type: "url"`, the
presigned URL, `filename`, `content_type`, `checksum_md5` from the
record's `checksum`, `size` from `byte_size`, `skip_virus_scan` if the
backend vouches for the source, `priority: "low"` and the `attachment`.
Then take the `ready`, `failed` and `expired` events from the webhook (see
"Webhooks"), or poll `GET /blobs/:id`, or sweep `GET /blobs?failed=true`
and `GET /attachments?owner_kind=…&role=…` for `blob_state` of `expired`.

Out of scope for now: `url` on the public port; authenticated fetches
(headers, tokens), since the backend presigns instead; resuming a partial
download (`Range`) across attempts, every attempt starts over, like a
build; a per-host allowlist beyond the address policy.

## Encryption at rest

**Envelope encryption with a chunked streaming AEAD.** Not age: its
recipient header buys nothing when the service is the only reader, its key
rotation rewrites every object header, and its fixed 64 KiB chunks and
sync-only seek make range requests awkward.

- Each blob gets a random 256-bit data key when its upload starts, wrapped
  with a key-encryption key (KEK) from config: AES-256-GCM under a key
  derived from the KEK with HKDF and the blob id, with a random nonce and
  the blob id as associated data, so a wrapped key moved to another row
  does not unwrap. Deriving the wrapping key per blob keeps every AES-GCM
  key to one message, so the 2^32 random nonces NIST allows under one key
  are never approached, however many blobs a KEK wraps. The wrapped key
  and the `kek_id` are stored on the blob row. Several KEKs live in config: `encryption.current_kek` wraps new keys
  and the row's `kek_id` picks the one that unwraps, so a new KEK is a
  config change. Retiring an old one means rewrapping the keys it wraps, a
  database update with zero bucket traffic, for which agrafe has no command.
- **KEKs need not rest on the host.** `encryption.keks_file` names a path
  read exactly once at startup, in the shape of the `keks` table, and
  merged with it (an id in both is an error). What sits there is the
  operator's choice: a plain file, a systemd credential sealed to the TPM,
  or a FIFO fed by `systemd-ask-password`, which is what ds_proxy does and
  what the security policy that governs it demands: nothing at rest on the
  host decrypts the data, at the price of a human at every boot. One
  mechanism covers the whole ladder, so a deployment moves up it by
  changing its unit file, not its agrafe. Not a passphrase-wrapped keyring
  as in ds_proxy: its password became a random key anyway, and rotating
  the human-held secret is just a new KEK plus a rewrap. What none of this
  does is keep the KEK out of the running process's memory; only an
  external key service would, and wrap and unwrap being the only two KEK
  operations, one could be added behind `Kek` later. A malformed file is
  reported by line number, never by quoting the line: the error lands in
  the journal and the line is a key, likely one just mistyped at the
  prompt. The file's text, the parsed secrets and their decoded bytes are
  wiped once the `Kek`s are built; the buffers the TOML parser passes
  through on the way are not agrafe's to wipe.
- The stream's associated data is the object's name within its blob,
  `<blob id>/<name>`: the object key without its shard (`blob::key::
  associated_data`). A ciphertext copied under another blob or another
  name does not decrypt. The shard is left out because it is a function
  of the id and binds nothing more, and because with it in, a copy of the
  bucket to another layout would be a re-encryption of every object
  instead of a copy (see "Object keys").
- Payload follows **Tink's AES-GCM-HKDF-STREAMING** wire format. It is a
  published spec with reference implementations in Go and Java, so our
  implementation can be validated against cross-language test vectors.
- Chunk (segment) size is `encryption.chunk_size`, default 256 KiB, at most
  16 MiB because a segment is held in memory whole on both paths, once per
  in-flight request. Larger chunks mean fewer tags; smaller chunks mean
  finer range granularity. Stored on the row so it can change without
  migrating objects.
- Per-chunk nonce is `nonce_prefix || counter || last_flag`, which prevents
  reordering, truncation and chunk swapping between blobs.
- Random access is arithmetic. The 40-byte header (a length byte, a 32-byte
  salt and a 7-byte nonce prefix)
  shares the first chunk with the plaintext, so chunk `0` holds
  `chunk - 16 - 40` plaintext bytes and every later chunk `chunk - 16`; chunk
  `i > 0` starts at ciphertext offset `i * chunk`. The header is also cached
  on the blob row (`encryption_header`), so a `Range` request maps to a chunk
  range, one byte-range `GET` to the bucket, decrypt, trim, stream.
  `Content-Length` is exact. The first segment is opened before the response
  starts, so a wrong key, a tampered object or a missing one is a logged
  `500`. A segment that fails later cannot change the status any more: the
  connection is cut, and the failure is logged, because the client cannot
  tell a corrupt object from a dropped connection.

The crypto code is small (a few hundred lines on RustCrypto `aes-gcm` and
`hkdf`) and owned by us, in the `agrafe-crypto` crate so it can be reviewed in
isolation. Key material is wiped on drop. It has tamper and truncation tests,
and known-answer tests against ciphertexts produced by Tink's Python
implementation (`scripts/tink-vectors.py`), among them a plaintext that
fills its segments exactly, the one place streaming implementations
disagree.

Server-side encryption on the bucket is deliberately not relied on: the
requirement is that the provider cannot read the data, which SSE does not give.

## Virus scanning

ClamAV via `clamd` `INSTREAM` over TCP. Inline during upload for files up to
`upload.inline_scan_max_size` (default 25 MiB, matching clamd's default stream
limit). Larger files are stored first and scanned by a job that decrypts the
object segment by segment and streams it to clamd. Until the scan passes the
blob is `uploaded` and unreadable. Infected blobs become `infected`,
permanently unreadable.

An inline session lasts as long as the upload it rides on, and on the public
port the uploader sets that pace, so an anonymous client trickling bytes
could hold a clamd thread indefinitely, and a handful of them every thread
clamd has. `clamav.max_inline_scans` (default 4) caps the inline sessions
open at once; an upload that finds them all taken is stored and scanned by a
job, without waiting. clamd's `MaxThreads` must stay above that cap plus the
scan workers, which hold a connection each while they run.

clamd being down must not stop uploads, and must not make anything readable
either. When it cannot be reached at the start of an upload, or fails midway,
the upload completes and the scan is deferred to a job, which is retried with
backoff until clamd answers. In the job, a connection failure or an `ERROR`
reply (the stream limit exceeded, for instance) is a job failure and the blob
stays `uploaded`; once the job is out of attempts the blob is stuck there,
unreadable, until it expires or is deleted. clamd scans only once the stream
is complete, but hangs up before that at its stream limit or when it dies,
so the client treats a failed write as "stop sending and read the reply",
not as an error.

A verdict is only as good as the clamd behind it, and clamd's defaults are
sized for mail, not for files of `upload.max_size`. Three limits matter:

- `StreamMaxLength`: above it clamd hangs up with an `ERROR`, the scan fails
  and the blob stays `uploaded`. Loud, so merely inconvenient.
- `MaxScanSize` and `MaxFileSize`: above these clamd scans a prefix of the
  file and still answers `OK`. Silent, so dangerous: a large file passes
  without having been looked at.

The clamd the workers talk to must set all three above `upload.max_size`,
and `AlertExceedsMax yes` so that any limit hit anyway becomes a `FOUND`
(`Heuristics.Limits.Exceeded.*`) rather than a pass. A file that cannot be
scanned is treated as infected, never as clean. `MaxThreads` must stay above
`jobs.concurrency.scan` plus `clamav.max_inline_scans`.

Skipped only when the blob was created with `skip_virus_scan = true`, which is
a statement by the backend about where the bytes come from. Such blobs cannot
be uploaded through the public port.

## Derived representations

Generated **eagerly on upload** by workers, not lazily on first request. The
read path stays a plain read, the renderer runs only in a sandboxed
subprocess, listing pages with many thumbnails do not stampede, and the
common "upload then show it" flow gets its thumbnail within seconds.

A lazy path exists only for repair: if the object is missing, or the stored
representation was built with another `media.variant_version`, the `GET`
handler sets it back to `pending`, enqueues a generation job (deduplicated
with any pending one) and serves the placeholder. Bumping the version in
config therefore regenerates everything on demand, `failed` and
`unavailable` ones included, since a new renderer may do better. Each
representation records the version it was built with
(`thumbnail_variant_version`, `preview_variant_version`).

A representation left out of `representations` at creation is `disabled`:
set once, never `pending`, so neither the eager render nor the lazy repair
touches it, and a blob with both disabled gets no render job at all. The
thumbnail endpoint serves the icon for it and the preview endpoint `404`,
each with `disabled` in the header, so a client can tell a choice from an
inability. A thumbnail whose preview is disabled is rendered on its own,
through the preview sources straight into the thumbnail box: the same
picture, only the larger object spared. The choice is final, as the blob
is write-once; turning rendering on later would take an endpoint of its
own, which does not exist.

### Rendering

Everything goes through one operation, `vips thumbnail`, run as a subprocess
(see "Sandboxing"): shrink-on-load where the format allows it, EXIF
orientation applied, colour converted to sRGB, every piece of metadata
dropped, lossy WebP out. The image is scaled to fit the box, never cropped.
Pixels are never enlarged, a small image stays small; a PDF page or an SVG
is vector with a nominal size, so it is rendered at whatever scale fills
the box. `--fail-on error` makes a truncated or malformed file a rejection
rather than a partial render. Each vips process runs two threads
(`VIPS_CONCURRENCY`): parallelism comes from the workers, and a narrow
process keeps the memory limit meaningful on any core count. Its `TMPDIR`
is the job's work directory, so anything it spills is cleaned up with it.

Only files with a known source are attempted:

- image types libvips loads natively: JPEG, PNG, GIF, WebP, TIFF, HEIF/AVIF,
  SVG, JPEG 2000. Everything else under `image/*` would go through
  ImageMagick, more attack surface than it is worth, and gets the icon.
  BMP is in that tail: libvips has no loader of its own for it, whatever
  the format's age suggests;
- PDF and the Writer packages (DOCX, ODT and their variants), through the
  sources below; a PDF that is not one gets a thumbnail of its first
  page, and no preview, and so does every office format in
  `agrafe-types/src/office.rs` (Word, Excel, PowerPoint and OpenDocument
  files, their legacy, macro-enabled and template variants, RTF, Draw
  drawings) when `media.office.command` names a LibreOffice to convert it
  (see "Thumbnail");
- text files (the `text` category plus CSV and TSV), thumbnails only;
- GeoJSON, GPX, KML and KMZ files, as a map (see "Preview"), with the
  text thumbnail when the XML ones hold no geometry and the archive
  icon when the KMZ does;
- originals at most `media.max_source_size` (default 64 MiB), except for
  the text thumbnail, which reads the first 8 KiB whatever the size. The
  default is low on purpose: the pictures and scans people want to see
  are a few MiB, and a larger file is one where the icon will do rather
  than a render that may run to the sandbox timeout on every attempt.

The allowlist above is by declared type, and the upload sniffer lets
through anything `infer` does not recognise. vips, though, picks its loader
by magic and tries ImageMagick last, so a file declared `image/png` holding
an XPM would reach it. Every file handed to vips is therefore sniffed again
right before the call (`agrafe_types::sniff::loader_for`) against the same
list, plus PNM for the pages the helpers write and vips's own `.v` for the
map `vips composite` writes; anything else is `failed` without vips ever
seeing it. The same check, narrowed to rasters, is applied to the
background layers fetched for a map before vips is handed them. The embedded image extractor applies the same check to the
bytes it pulls out of a PDF, since a `DCTDecode` stream holds whatever its
author put there. `VIPS_BLOCK_UNTRUSTED` would not do: it also blocks the
PDF, SVG, JPEG 2000 and PNM loaders.

Each content type has a plan (`agrafe_render::plan`): the sources of its
**picture**, the file rendered as a whole, tried in order, and what the
thumbnail falls back to when no source yields one. The preview is the
picture in the preview box. One `render` job produces both
representations, whatever is `pending` on the row. The preview is rendered
first; the thumbnail is the preview fitted into the thumbnail box. The
original is therefore fetched, decrypted and decoded once, the two pictures
agree, and a thumbnail regenerated on its own (its object gone) is made
from the stored preview without touching the original. A preview verdict
from the current variant version stands: a `failed` or `unavailable`
preview leaves the thumbnail to the plan's fallback (the text sample, the
first page of a PDF), and without one the thumbnail is `failed` or the
icon respectively. A PDF whose embedded image is broken thus still shows
its first page, since the page renderer is not the extractor that gave up
on it. A thumbnail rendered on its own, the preview disabled, follows the
same rule from its own attempt at the picture. The exception is a
timeout: a file that exhausted one tool's time limit is not handed to a
second for as long again, and the thumbnail is `failed` with the
preview. A preview from another version, or whose object is gone, is
rendered again first. The thumbnail thus appears after the preview rather
than alongside it, a few seconds for a large image, which the icon
placeholder and `wait` cover.

A source that vips rejects or that runs past the sandbox timeout, or a
helper that crashes or runs out of time, hands over to the next source, as
does a helper that declines (no embedded image to pull out, a sample that is
not text). When no source renders, the representation is `failed` if one of
them rejected the file and `unavailable` if all declined, and the job is
done either way: the same bytes would fail the same way tomorrow. Only failures of ours (object store,
database, the renderer missing) fail the job, which retries with backoff
and leaves the representation `pending` meanwhile.

vips exits non-zero for a bad file and for a missing loader or an unknown
option alike, so a libvips built without PDF support, or too old for
`keep=`, would mark every file `failed` and need a variant version bump
once fixed. Before a worker starts it therefore renders a text sample, a
one-pixel PNG, a one-page PDF and an image for each other loader on the
allowlist that a libvips may be built without or a distribution packages
apart (GIF, WebP, TIFF, HEIC, AVIF, JPEG 2000: Debian ships libheif's
HEVC and AV1 decoders as plugins of their own) through the exact command
lines the jobs use (`agrafe_render::probe`, files in `crates/agrafe-render/assets/probe`),
and the process refuses to start if any of them fails, and so the map of a
one-point GeoJSON with no background layer, through the helper and `vips
composite`, which catches a libvips without SVG support, and the JPEG
`agrafe-extract` pulls out of a PDF that wraps one. The probe also catches
a `media.render_text_command`, `media.extract_command` or
`media.render_geo_command` that is not the helper (a missing extractor
fails every render job of a PDF, DOCX or ODT, a wrong one declines every
embedded image), and it never touches the network. With
`media.office.command` set, it converts one file per LibreOffice
application through the exact command lines and renders the results: a
two-page DOCX, a two-page XLSX, a two-slide PPTX, a two-page ODG and an
ODT carrying a macro (`probe/pages.docx`, `sheet.xlsx`, `slides.pptx`,
`drawing.odg`, `macro.odt`). A LibreOffice without one of Writer, Calc,
Impress or Draw (Debian packages them apart, and a deployment for
spreadsheets and letters may well lack Impress), without fonts, or too
old for the JSON filter options fails the deployment rather than every
office file of that kind. The DOCX, the XLSX (a manual page break), the
PPTX and the ODG have two pages so that `PageRange` is known to be honoured by each export filter, since
there is at least one report of it being ignored, and a slide export has
notes and handout settings of its own: vips must render page one of the
export and refuse page two, which tells a one-page PDF from a longer one
without a PDF parser in the server. The ODT binds a Basic macro to its
load event that creates a folder next to the document; the folder must
not be there afterwards (see "Thumbnail" on macros). Five conversions
instead of two at worker start, about a second on Linux with a fresh
profile.

Representations live at `<shard>/<blob id>/thumbnail` and
`<shard>/<blob id>/preview`, encrypted with the blob's own data key: the streaming AEAD derives a fresh
subkey per stream from a random salt, so one key safely covers the three
objects, and the associated data is the object's name within the blob,
as for the original.
They are small and served whole, with a strong `ETag` of the content and
`Cache-Control: private, max-age=86400`: not `immutable`, since a version
bump changes them under the same URL. The row records the SHA-256 and the
size of each rendered WebP (`thumbnail_sha256`, `thumbnail_size`, and the
same for the preview), set with `ready` and cleared with any other state,
so a conditional request answered `304` and a `HEAD` never read the
object; only a `GET` with a body to send does.

Every picture or placeholder a representation route serves carries
`X-Representation-State`, so a client can tell a placeholder from the final
answer; a refusal (`409`, or `404` for a filename that does not match) has
none. `wait=<ms>` in the signed query (at most 30 s) holds the request while
the representation is pending, or the blob itself is not yet `ready`, for
the "upload then show it" flow.

### Original

The read routes are the hot path, and the row lookup is the one query on
it. They fetch a `BlobHead`, the columns serving needs (identity, state,
the envelope, the representation states), never the whole row: a `rows`
blob keeps its column declaration on the row as JSON, up to megabytes a
download has no use for.

A sealed URL (see "Sealed URLs") carries the row and skips the lookup
altogether. Otherwise it is answered from an in-process cache for a row
served lately,
`blobs.read_cache_ttl` (default thirty seconds), so a blob read many times
costs one query per process per period. Only a settled row is kept: `ready`
or terminal, with no representation pending. Such a row changes only by
going terminal, which another process shows late by at most the TTL, and
the objects are gone only once the purge ran anyway; the process that
deleted the blob stops serving it at once. A row in flux, an upload or a
render under way, is never kept, so polling the metadata and the `wait` of
a representation request see every change as it lands, and the read path's
own writes, setting a representation back to `pending`, drop the row from
the cache. A missing row is kept too, so a URL with a forged id does not
cost a query a time. `agrafe_blob_head_cache_total{result}` counts hits
and misses.

Served only from `ready`; any other state answers `409` on both ports (the
public port already answers `404` for terminal states). Single-range `Range`
requests are honoured with `206`; multiple ranges, and a `Range` whose
`If-Range` names another `ETag`, get the whole file. The `ETag` is the
SHA-256 of the content. Originals are served `private, immutable` for a year:
the bytes behind a URL never change, and the URL's own expiry bounds how long
it works. Thumbnails and previews are the exception, since they change under
the same URL once rendered (see above).

Every original carries `X-Content-Type-Options: nosniff` and
`Content-Security-Policy: sandbox`: user content is served from this origin,
and an HTML or SVG upload must never run as a page of it. The signed query
parameters `disposition` (`inline`, the default, or `attachment`) and
`filename` set `Content-Disposition`.

### Thumbnail

Always exists. WebP, autorotated from EXIF, fitted into the
`media.thumbnail` box. For files that cannot be rendered, a pre-rendered
category icon with a type badge, served with the same URL shape. The icons
are rendered from `crates/agrafe-render/assets/icons/icon.svg` by
`scripts/icons.sh` and embedded in the binary, so serving one needs no
renderer and no asset directory.

| Blob | Thumbnail | Served | `Cache-Control` |
|---|---|---|---|
| `ready` | `ready` | the WebP | `private, max-age=86400` |
| `ready` | `pending` | icon, `X-Representation-State: pending` | `no-store` |
| `ready` | `failed`, `unavailable`, `disabled` | icon, state in the header | `private, max-age=86400` |
| not yet `ready` | any | icon, `pending` | `no-store` |
| terminal | any | icon, `unavailable` (private port; public is `404`) | `private, max-age=86400` |

Never `404` or `202`, because the consumer is an `<img>` tag.

Text files get a real thumbnail: the first lines of the file set in a
monospace font on a page, a hint of what the file is rather than a way to
read it. The first 8 KiB are decoded as UTF-8, or as Windows-1252 when they
are not, which is what Latin-1 exports from older systems are in practice;
a sample with NUL bytes or more than a sprinkling of control characters is
not text and gets the icon. The page is rasterised in Rust by the
`agrafe-render-text` helper, in the sandbox, with `ab_glyph` and a copy of
DejaVu Sans Mono embedded in that binary
(`crates/agrafe-render-text/assets/fonts`), so
the result is the same on every host and needs no font, fontconfig or
shaping library installed. The trade: no shaping, so Arabic or Indic text
comes out as unconnected glyphs and anything outside Latin, Greek and
Cyrillic as the notdef box. The helper writes a PGM at twice the box, and
vips downsizes it into the WebP like any other raster. There is no text
preview: a preview that cannot be read is not worth rendering. A GeoJSON
file is a text file with a picture: its thumbnail is its map fitted into
the box, and the text sample when it holds no geometry to draw, or a
coordinate off the globe.

A PDF that is not an embedded image (see "Preview") gets its first page as
its thumbnail, through libvips's PDF loader, and no preview for the same
reason: a page of a document fitted into a preview box is not readable
either, and a multi-page document needs a viewer, not a picture. libvips
builds against either poppler or pdfium; production images should use
pdfium, the most hardened renderer available, rather than linking
`pdfium-render`, a second native dependency for what vips already does.
The thumbnail is subject to `media.max_source_size` like a preview, since
the loader reads the whole file.

An office file that is not an embedded image gets its first page the
same way, once LibreOffice has converted it to a PDF, and no preview for
the same reason: the Word, Excel and PowerPoint formats, their Office
97-2003 binary ancestors (DOC, XLS, PPT), their macro-enabled, template
and slideshow variants (DOCM, DOTX, DOTM, XLSM, XLTX, XLTM, XLSB, PPTM,
PPSX, PPSM, POTX, POTM), RTF, and the OpenDocument text, spreadsheet,
presentation and drawing formats with their templates and masters (ODT,
OTT, ODM, OTM, OTH, ODS, OTS, ODP, OTP, ODG, OTG), the table in
`agrafe-types/src/office.rs`. A single forms platform in production
receives tens of thousands of `.doc` files, thousands of `.pptx` and
`.rtf`, hundreds of the rest. A faithful page needs a layout engine, and
the only one that reads these formats is LibreOffice. It is optional
(`media.office.command`, unset by default): a deployment without it
behaves as before, the category icon or the embedded-image preview when
the file is "really an image", and one that turns it on later gets
thumbnails for existing blobs through the variant version, no backfill.
LibreOffice does one thing: convert the office file to a PDF in the job's
work directory, and the first page of that PDF then takes the path a PDF
thumbnail takes, the same loader sniff, the same box. LibreOffice never
produces the picture, so there is one raster path, one probed PDF loader,
and a DOCX and a PDF look the same; exporting PNG from LibreOffice would
tie the resolution to its export options and add a second raster path for
nothing. The export asks for the first page only (`PageRange`, the JSON
filter options of LibreOffice 7.4 and later), so a 300-page document is
not laid out and written in full; the probe checks the option is honoured
by each export filter (see "Rendering"). The export filter follows the
application that owns the type, `writer_pdf_Export`, `calc_pdf_Export`,
`impress_pdf_Export` or `draw_pdf_Export`, and the input is linked into
the work directory under the extension of its own content type, not its
base format's, since LibreOffice picks its import filter from the name
and `.xlsb` is a different filter from `.xlsx`. The plan
(`agrafe_render::plan`) carries it as the `ConvertedFirstPage` fallback:
the preview sources do not change, the Writer packages (DOCX, ODT and
their ZIP-based variants, which the extractor opens as the package they
derive from) keep the embedded-image preview and everything else has
none, the binary formats and RTF because the extractor reads ZIP
packages, presentations because a slide's first picture is as likely a
logo as the slide, and the fallback runs once the preview has declined,
like the PDF first page. Over `media.max_source_size`, or without a
converter, it declines and the representation is `unavailable`, which is
what office files got before. A LibreOffice that exits non-zero or runs
out of time is a rejection, as for the other tools, and a file it cannot
load is one ("source file could not be loaded", exit 1, on 26.2 and
26.8): a broken DOCX or PPTX is `failed` like a broken PDF, which the
integration tests pin with a well-formed ZIP whose document part is cut
mid-element (`fixtures/broken.docx`, `broken.pptx`). An exit of zero with
no PDF, which no version has been seen to produce, declines rather than
failing the blob for a fault nothing names. Note that LibreOffice
converts a file that is not a ZIP at all as plain text, so garbage named
`.docx` gets a page of its bytes, not a failure.

The legacy binary formats go through LibreOffice's OLE import filters, a
larger and older parser surface than the ZIP formats', and the one
Office exploits have historically targeted. The sandbox is the same one,
Landlock, seccomp and the limits, and the bytes were scanned on upload;
that is the accepted cost of rendering user files at all, and `.doc` is
the office format citizens send most.

*Macros.* The macro-enabled variants (DOCM, XLSM, PPTM, …) and the
legacy formats may carry macros, and a document can bind one to its own
load event. Two things keep them from running. LibreOffice loads a file
it converts with no `MacroExecutionMode` in its descriptor
(`desktop/source/app/dispatchwatcher.cxx` sets one for documents it
opens, not for conversions), which `SfxObjectShell` reads as
never-execute, whatever the profile says: measured on 26.8, a document
macro bound to the load event runs when invoked and does not run through
`--convert-to` at any security level, "low" included. That is a default
in LibreOffice's code, not a setting of ours, so the user profile every
conversion runs with also pins three settings of `Security/Scripting`:
`MacroSecurityLevel` to 3, "very high", under which only macros from
the trusted locations run; `SecureURL`, the trusted locations, to the
empty list, since "very high" is only as strict as that list and a
template warmed by hand may have trusted a path, a parent of the work
directory included; and `DisableMacrosExecution`, the switch above the
levels, to on. The driver writes the items into the profile's
`registrymodifications.xcu` before every run, appended last to a
template's, where a later item overrides an earlier one for the same
property, so a template whose level was set low or whose trusted
locations were widened is still pinned, and as the whole registry of a
fresh profile, which LibreOffice reads on first start; 26.8 keeps all
three when it rewrites the registry, each over the template's earlier
value. The probe's `macro.odt` checks the behaviour itself, whichever
of the two lines provides it (see "Rendering"). How it runs, and what it
costs, is under "Sandboxing".

### Preview

WebP, not JPEG: smaller, lossless-capable, alpha, universally supported, and it
shares one encoder with thumbnails. Bounded to `media.preview.max_dimension`,
autorotated, always transcoded so decoder bombs and malformed containers never
reach a client.

Preview exists for every file we can render, not only images. The UI must key
on "has preview", never on category. Sources, in order:

1. **Images**: decode and transcode. `preview_source = image`.
2. **Embedded image extraction** (`preview_source = embedded_image`): a
   one-page PDF whose single image XObject covers at least
   `media.embedded_image.min_coverage` of the page box (the part of the
   image inside it), drawn upright on a page the viewer does not turn
   (no `Rotate`), with at most `max_text_chars` of text, all of it drawn
   invisible (rendering mode 3, a scan's OCR layer), and nothing else
   painted: no path filled or stroked, shading, inline image, clip,
   optional content, visible text, Type 3 font, graphics state that masks,
   fades, blends or remaps, or annotation other than a link, any of which
   could cover what the image shows, a black rectangle or a run of full
   blocks over a scan to begin with; or a DOCX/ODT with exactly one media file
   (under `word/media/` or `Pictures/`) that is a raster image, and at
   most `max_text_chars` of non-whitespace text in the body, CDATA and
   references included, read within 8 MiB (a body cut there is not
   sparse, since what follows is unread). The embedded
   image is pulled out at full resolution and goes through the image
   pipeline: JPEG and JPEG 2000 streams as they are, in a grey, RGB or
   CMYK colour space, raw 8 or 16 bit grey or RGB samples as PNM. Anything doubtful (form XObjects, rotation,
   masks of any kind, a decode array, exotic colour spaces, EMF) is left
   to the next source, whichever the stream's encoding. This
   addresses users who answer "send a screenshot" with a Word file
   containing a screenshot. The parsing runs in the `agrafe-extract`
   helper, its own binary executed in the sandbox, never in a server
   process.
3. **Map** (`preview_source = geojson`, `gpx` or `kml`, one value per
   format, so the row says what was drawn; a KMZ is a KML document in a
   ZIP, and what was drawn is that document): a GeoJSON, GPX, KML or
   KMZ file's geometry drawn over a map background, the way a form's
   map field prints in a PDF export. Three steps, two of them in the
   sandbox. The
   `agrafe-render-geo` helper reads the file, in the format the spec
   names, since the server knows the content type and the helper does
   not sniff, and writes `paths.svg`, the geometry as SVG paths in the
   pixel box, and `extent.json`, the bounding box it chose in EPSG:3857
   metres with the pixel size and the layers that apply to it; the
   `geojson`, `quick-xml` and `zip` parsers live only there. The worker
   checks
   the extent against the spec it wrote (the pixel box it
   asked for, a box that is a box, and exactly the layers the spec's
   rules give for that box, `Extent::applicable`, which both sides call)
   before acting on it, since the helper is the untrusted side and a
   compromised one must not choose what the worker fetches or indexes:
   a mismatch is a rejection. The box remains the helper's, so what a
   compromised one can still ask for is a configured layer over some box
   inside the world. The worker, trusted code, then fetches one
   WMS `GetMap` image per applicable layer, concurrently, into the work
   directory, and decodes each into a `.v` at exactly the pixel box with
   `vips thumbnail` forced to that size: a WMS may cap or round the size
   it was asked for, and `vips composite` takes the size of its bottom
   image, so an unfitted layer of another size would crop or misalign
   the drawing silently; the image still covers the bounding box, so it
   is stretched rather than refused. Then `vips composite` stacks the
   fitted layers and the SVG, in the sandbox, into a `.v` that goes
   through `to_webp` as a raster like any picture: the background PNGs
   are untrusted bytes decoded where untrusted bytes are decoded, and the
   SVG goes through the rsvg loader the `image/svg+xml` source already
   relies on. At 72 dpi an SVG with `width` and `height` in pixels renders
   at exactly those pixels, which is what makes the composite line up.
   One render costs one helper run, one request and one vips run per
   applicable layer, and two vips runs.

   Inside the helper, the seam between the formats is the list of
   projected shapes, each with its style, that the drawing works from:
   each format has a reader that yields geometry, as `geojson::Geometry`
   values built in memory, a style and a coordinate system, and the
   projection, the extent, the simplification and the SVG are shared.
   Nothing serialises to GeoJSON bytes and parses them back. The GPX
   reader is a hundred lines over `quick-xml` events: a `<wpt>` is a
   point, a `<rte>` is a line of its `<rtept>`, each `<trkseg>` of a
   `<trk>` is a line of its `<trkpt>`, a segment of fewer than two
   points is dropped like an empty line, the `lat` and `lon` attributes
   are always WGS 84 by the specification, and every element the reader
   does not know, extensions included, is skipped, so future elements
   cost nothing. GPX has no style vocabulary: Garmin's and OsmAnd's
   colour extensions are ignored like `marker-symbol`, and the file is
   drawn in the configured default. A document that is not GPX, or
   holds no point with coordinates, declines; one cut short is drawn as
   far as it goes, between two elements or inside a tag (an error at the
   very end of the input is the cut, not a malformed document), the
   segment it was cut in included. The georust `gpx` crate was weighed and passed over:
   the glue to it is about the size of the subset needed, and it brings
   `xml-rs` where the helper already has `quick-xml`.

   The KML reader is the one with a style vocabulary, and it maps onto
   simplestyle cleanly. Geometry: `Point`, `LineString`, `LinearRing`
   (a closed line on its own, a boundary inside a `Polygon`), `Polygon`
   with its `outerBoundaryIs` and `innerBoundaryIs`, `MultiGeometry`
   (flattened, like a geometry collection) and `gx:Track` with its
   `gx:coord` elements, which is how Google Earth writes a GPS track;
   `<coordinates>` is whitespace-separated `lon,lat[,alt]` tuples,
   always WGS 84 by the specification, and a tuple that is not two
   numbers is skipped. `Document`, `Folder` and `Placemark` nest;
   `NetworkLink`, `GroundOverlay`, `ExtendedData`, `Schema` and
   everything else are skipped. Styles: a `<Style id>` holds
   `LineStyle`, `PolyStyle` and `IconStyle`; a placemark names one with
   `<styleUrl>#id</styleUrl>` or holds one inline, the inline one laid
   over the named one key by key, as a feature's properties are laid
   over the default; a `<StyleMap id>` holds `normal` and `highlight`
   pairs, of which `normal` is taken, and may point at another map, so
   resolution follows at most eight hops and a circle ends in the
   default. Shared styles may be declared after their use, so the
   reader keeps each placemark's reference and resolves it once the
   document is read. The mapping: `LineStyle/color` is the stroke and,
   by its alpha, the stroke opacity (`aabbggrr`: alpha first, bytes
   reversed); `LineStyle/width` the stroke width in pixels, clamped
   like simplestyle's; `PolyStyle/color` the fill and its opacity;
   `PolyStyle/fill` at 0 zeroes the fill opacity and
   `PolyStyle/outline` at 0 the stroke opacity, for polygons only,
   since `outline` is a polygon's switch and a line in the same style
   keeps its stroke; `IconStyle/color` tints the marker, white being
   untinted and leaving the field unset; `IconStyle/scale` below 0.8 is
   a small marker, above 1.3 a large one, medium between. The icon's
   `href` is not honoured, for the reason `marker-symbol` is not, and
   `LabelStyle`'s colour and scale are a label's, not a marker's. A
   colour that is not eight hex digits, a width that is not a number:
   ignored, never an error. A document that is not KML, or holds no
   placemark with a geometry, declines; one cut short is drawn as far
   as it goes, wherever the cut falls, the placemark it was cut in
   closed as its end tags would have, less the last tuple of its
   coordinates when the cut is in them, since it may be split. A KMZ is a ZIP whose first `.kml` entry, `doc.kml` by
   convention, not counting the `__MACOSX/` sidecars a Mac zips in, is
   the document, read through `zip` with a 64 MiB cap on the inflated
   entry, the size a bare document may be, as the sandbox bounds the
   rest; the icons and overlays beside it are ignored. The georust
   `kml` crate was passed over for the reason `gpx` was: it brings
   `geo-types` for a subset the reader covers in a few hundred lines.

   The helper projects every coordinate to Web Mercator, clamping the
   latitude to the projection's edge. RFC 7946 allows only longitude and
   latitude in degrees and has no way to declare another system, but the
   2008 specification had a top-level `crs` member, and GDAL, hence
   QGIS, still writes it for a layer that is not WGS 84, which is how
   most French administrative data arrives: Lambert-93 (EPSG:2154), with
   eastings in the hundreds of thousands and northings in the millions.
   The helper reads that member and honours a short list, by EPSG code
   in any of its spellings: 4326 and CRS84 are degrees, 2154 goes
   through the inverse Lambert conformal conic on GRS 80, thirty lines
   of arithmetic checked against PROJ, 3857 is the target itself. A
   `crs` naming anything else, or not a named CRS, is a decline. So is
   a coordinate that is not finite or not on Earth in its system,
   without a `crs`, most likely a projected file that did not say so:
   valid data for its producer, nothing this helper can place. Nothing
   is guessed from the ranges of the numbers, which would be right for
   France and silently wrong elsewhere. A decline, not a rejection: the
   file gets the text thumbnail and no preview, never `failed`. The extent is the
   geometry's bounding box padded by `media.map.padding` of its larger
   side, grown to at least `min_span` metres so a single point or a tiny
   parcel still gets a readable map, then widened to the aspect ratio of
   the box: the image is the whole box, never letterboxed. The map is
   drawn at the preview box or `max_size`, whichever is smaller. Rings and
   lines are simplified with Douglas–Peucker at half a pixel, 256 points
   at a time, the ends of each kept: whole, it is quadratic on a zigzag
   that keeps every point, which a file of a few hundred thousand made
   run past the timeout, and in pieces it is linear. The
   drawing stops at `max_vertices` or `max_shapes`, whichever comes
   first, so a 60 MiB file cannot produce a 60 MiB SVG or make rsvg
   crawl; the sandbox timeout bounds the rest. Both budgets, because
   rsvg pays per element as well as per vertex: 200 000 points are
   within the vertex budget and take rsvg five seconds and half a
   gigabyte, 20 000 shapes well under a second. A line or a ring
   without positions is dropped before the extent is chosen, so a file
   made of them declines instead of getting a map of nowhere and a
   request to the provider for it.
   Points are discs with a ring, lines and polygons get a darker casing
   under the stroke, polygons use `fill-rule="evenodd"` so holes stay
   holes, and the first drawn layer's attribution is burned into a corner
   as outlined text, since the preview travels without a caption. A
   FeatureCollection, a Feature or a bare Geometry at the top level;
   GeometryCollections recurse. Anything else, or nothing with
   coordinates, is a decline, and the thumbnail is the text sample.

   A GeoJSON feature's style is the configured default, `[media.map.style]`,
   overlaid with its own simplestyle-spec properties: the convention most
   exports write, so a client that wants some features drawn green sets
   `stroke` on them at export time, and there is no second mechanism in
   the config. Honoured: `stroke`, `fill` and `marker-color` as hex
   colours of 3 or 6 digits, `#` optional, parsed and re-serialised;
   `stroke-opacity` and `fill-opacity` clamped to 0..1; `stroke-width` in
   pixels clamped to 0..20, so a file cannot paint the whole map;
   `marker-size` as `small`, `medium` or `large`. Ignored: `marker-symbol`
   (Maki icons would need an icon set and fonts in the sandbox), `title`,
   `description`, and any value of the wrong type, never an error: nothing
   from the file reaches the SVG verbatim. The casing is not a colour of
   its own but the stroke darkened, so a red feature gets a dark-red halo.
   One `Style` type, in `agrafe-types`, serves the config section, the
   spec the server writes for the helper and a feature's properties, with
   a merge of the two.

   The background is a list of layers, `[[media.map.layers]]`, each a
   `GetMap` URL template with `{bbox}`, `{width}` and `{height}` to fill
   in, composited bottom to top, and drawn only on an extent between its
   `min_span` and `max_span`: a boundaries-and-names layer that carries no
   toponym at parcel scale is skipped there, and most maps are
   parcel-sized and cost one request. No layer is drawn on a box that
   leaves the Web Mercator square, as a world-spanning file's does once
   padded and widened: a WMS may refuse that bbox on every attempt, and
   the retries would end in a dead job rather than a map. A `GetMap` with the bbox in
   EPSG:3857 and the output size gives the background at exactly the pixel
   box wanted, with no tile stitching, no zoom rounding, no crop; an XYZ
   layer kind can come later, stitched with `vips arrayjoin`. The code
   names no provider; IGN's Géoplateforme is the example in
   `config/example.toml`. With no layer applicable, or none configured,
   which is the default and the test configuration, the geometry is drawn
   on a flat neutral background and nothing leaves the worker.

   Fetching: one request per applicable layer, each under
   `fetch_timeout` and `max_layer_size`, at most four in flight per
   process, since a `variant_version` bump re-renders every map blob
   and must not be a burst against the provider. Up to three redirects
   are followed on the layer's own host and port, never from https to
   http: a provider that sends the worker to another host, an internal
   address to begin with, has answered with its redirect, which fails the
   attempt. The response is an image
   only if its `Content-Type` says so and the loader sniff agrees: the
   Géoplateforme answers an XML `ServiceException` with a `200`. A
   failure to fetch, a layer that does not decode included, which the
   fit of that one layer finds out, is not the file's fault: it is an
   `Error`, the job attempt fails and is retried
   with the backoff, and the representations stay `pending` meanwhile. The
   file never gets `failed` for the provider's downtime, and the provider's
   URL, which carries the extent, never reaches a log line: errors name
   the layer and the host. The layer bytes are written to the work
   directory and never reach a client; the composite is re-encoded like
   every other render.

   Privacy: every render sends the extent of a user's geometry to the
   provider. That is the trade a deployment makes when it configures a
   layer, which is why the list is empty by default and why the render
   workers' egress has to allow the configured hosts and no more. A
   deployment that wants a self-hosted background runs a tile server and
   points the config at it; rendering one locally was weighed and
   rejected, since raster coverage of a country at parcel zoom is hundreds
   of gigabytes and vector tiles need a style-aware renderer that does not
   exist in Rust and would have to be written inside a sandboxed helper.
   Out of scope for now: XYZ and WMTS layers, caching backgrounds between
   renders, and Shapefile, which would be another reader in this helper.
A document that is not one of these has no preview, whether PDF or office:
the first page of a PDF is its thumbnail (see "Thumbnail"), not a preview.

Files with no source answer `404` from the preview endpoint, with
`preview_state = unavailable`; so do `pending`, `failed` and `disabled`
ones, each with its state in `X-Representation-State` and a message saying
why. A blob that
is not `ready` answers `409` as the original does.

### Sandboxing

Decoders and renderers are the most attackable code in the service. They run
in a subprocess, never in the API process: `vips` for rendering and
compositing, the `agrafe-extract`, `agrafe-render-text`,
`agrafe-render-document` and `agrafe-render-geo` binaries for container
parsing, text rasterising, document compiling and GeoJSON drawing, each
its own crate so that the server links none of their parsers (see
"Crates"). The helpers never touch the network: the one network step of a
render, fetching a map's background, is the worker's own, outside the
sandbox, against a host the operator configured. A crash, a decoder bomb
or an infinite loop costs one job attempt, not the process.

Three layers confine the child, from the inside out. The first is on every
platform; the other two are Linux's, installed by the child between `fork`
and `exec`, next to the `setrlimit` calls, both setting `no_new_privs`.
A fourth, bubblewrap, is optional and takes Landlock's place where it
runs; it is described after the three.

**Resource limits and a timeout.** A clean environment (`PATH` and `HOME`
only, no secrets; vips also gets `TMPDIR`, `VIPS_CONCURRENCY` and
`VIPS_WARNING=0`, and a coverage build alone passes `LLVM_PROFILE_FILE`
through), `setrlimit` bounds on address space and data
(`media.sandbox.memory`, default 1 GiB), CPU time, output file size
(`max_output_size`) and process count (`max_processes`, default 1024),
no core dumps, and a kill at the wall-clock limit
(`media.sandbox.timeout`, default 60 s). The process count is
`RLIMIT_NPROC`, which the kernel counts per uid, every process and
thread of it, the worker's own included: it bounds a fork bomb at what
a uid of the worker's own never reaches, and a worker sharing its uid
with much else raises it. The CPU time limit is the
wall-clock limit plus a second, summed over the process's threads, which
is why every tool is kept narrow (two threads for vips and LibreOffice):
a process spread over sixteen cores would exhaust it in a tenth of the
time. Its soft limit sends `SIGXCPU` and the hard one, a second later,
`SIGKILL`; a tool that died of `SIGXCPU` timed out, and is reported as
such rather than as a fault of the file. LibreOffice is the exception:
the process that spends the CPU is `soffice.bin`, two launchers down,
which catches `SIGXCPU` and aborts, and `oosplash` exits 134 for it, the
code of any crash; the limit there is a plain rejection of the file.
(With the two limits equal the
kernel checks the hard one first and sends `SIGKILL`, which says
nothing.) The address space limit is enforced by Linux and ignored by
macOS, so development machines get only the timeout. The child's stderr
is a pipe, which no rlimit bounds: the worker reads it as it is written
and keeps the last 4 KiB, so a decoder that prints an error per broken
object of a hostile file costs the worker 4 KiB, not the flood. A test
floods it with 32 MiB and checks the worker's high-water mark.

**Landlock**, through the `landlock` crate, maintained by the feature's
kernel author. The child's filesystem is the *system set*, read and
execute: `/usr`, `/lib`, `/lib64`, `/bin`, `/etc/ld.so.cache`,
`/etc/alternatives`, `/etc/fonts`, `/etc/localtime`,
`/var/cache/fontconfig`, `/dev/urandom`, `/dev/zero`, with paths missing
on a host skipped, and `/dev/null` read and write, since a library that
silences a stream reopens it there; the program itself; the paths the caller
declared readable; the one directory it declared writable, with every
right but execute; and nothing else. TCP `bind` and `connect` are handled
with no rule, so refused, and the domain is *scoped*: no signal to a
process outside it and no abstract unix socket. The child runs as the
worker's uid, so without the signal scope `kill` would reach the worker,
and a poisoned file would take it down on every retry. Fonts and
poppler's data are in the set on
purpose: without them a PDF with non-embedded fonts renders as a blank
page and exits 0, which nothing reports (a production service learned this
the hard way); `media.sandbox.readable` adds paths for a host that keeps
fonts elsewhere. The crate's best-effort compatibility degrades on an
older kernel instead of failing: ABI 1 (5.13) gives the filesystem rules,
ABI 2 (5.19) renames across directories, ABI 4 (6.7) the TCP refusal,
ABI 6 (6.12) the signal and socket scopes, and the handled accesses this
build knows and the kernel does not are dropped. A Landlock domain also
forbids `mount`, which is why it yields to bubblewrap below.

**seccomp**, through `seccompiler` (pure Rust, from Firecracker, no
libseccomp), as a *denylist* answering `EPERM`: `socket`, `socketpair`,
`connect`, `bind`, `listen`, `accept`, `accept4`, `ptrace`,
`process_vm_readv`, `process_vm_writev`, `pidfd_getfd`, `kcmp`,
`perf_event_open`, `unshare`, `setns`, `mount`,
`umount2`, `pivot_root`, the new mount API (`open_tree`, `move_mount`,
`fsopen`, `fsconfig`, `fsmount`, `fspick`, `mount_setattr`), `bpf`,
`keyctl`, `add_key`, `request_key`, `userfaultfd`, `io_uring_setup`,
`kexec_load`, `kexec_file_load`, `init_module`, `finit_module`,
`setsid`, `setpgid`, `ioprio_set`, `process_madvise`, `migrate_pages`,
`move_pages`, `pidfd_open`, `clone`
with any namespace flag, `prlimit64`, `sched_setaffinity`,
`sched_setscheduler`, `sched_setparam` and `sched_setattr` on any pid
but 0 (glibc's `setrlimit` and `getrlimit` pass 0, and so does a thread
pool pinning itself), and `setpriority` on anything but `PRIO_PROCESS`
0, which is what `nice` passes. `clone3` carries its flags in a struct
the filter cannot read, so it is refused whole, in a second filter
answering `ENOSYS`: that is the one errno glibc falls back from to
`clone`, which the first filter does inspect, so threads keep working and
namespaces do not. The filters are compiled once per process and
installed per spawn; a command that asks for unix sockets (below) gets a
variant.

*Unix sockets, on request.* LibreOffice cannot start without binding its
single-instance pipe, a unix socket in `/tmp`, or in `/var/tmp` when
`/tmp` is not writable, and its launcher refuses to start when neither
is: with `socket` refused it reports a bootstrap error and exits, and
with the bind refused by Landlock the same. A command may
therefore ask for `unix_sockets(dir)`: the seccomp filter then refuses
`socket` and `socketpair` for every family but `AF_UNIX` instead of
refusing every socket call (`connect`, `bind`, `listen`, `accept` only
act on a socket that exists), and Landlock grants `dir` the right to make
and remove socket files, nothing else there. The office driver asks for
the directory LibreOffice will pick, `/tmp` when the worker can write
there and `/var/tmp` otherwise. The trade is that path-based unix
sockets the child can see, such as a local Postgres's, are no longer
closed to it, since no Landlock ABI governs them (ABI 6 scopes abstract
sockets only), and that a compromised LibreOffice may remove files under
`/tmp` that its uid owns, since Landlock has no "remove sockets only"
right. Hence the deployment advice under "Thumbnail", a render-only
worker and no local Postgres socket, and a boot check
(`check_office_host`) that `media.tmp_dir` is outside the pipe directory,
so that no other job's decrypted original is where the converter may
remove files: the default work directory is the system temporary
directory, which is `/tmp` itself. A LibreOffice removes its pipe on
exit, not when killed at the timeout, and the pipe's name hashes the
profile path, unique to the run, so every kill would leave one more
socket file in `/tmp` for good, at an uploader's pleasure: after any
conversion that did not succeed, since a `soffice.bin` killed by a signal
or its CPU limit reaches the driver as the launcher's exit code, the
driver sweeps the pipe directory of the `OSL_PIPE_<uid>_` sockets
nothing listens on, told from another job's live one by connecting (a
listener accepts, a dead socket refuses), which needs no knowledge of the
hash. Nothing else asks for sockets, and the default filter is
unchanged. The two filter variants are compiled once each.

*Process groups.* `soffice` is a script around `oosplash` around
`soffice.bin`, so a kill at the timeout that reached the direct child
alone would leave LibreOffice running. Every child is made the leader of a
process group of its own, and a timeout kills the group, reaps the child
and waits, five seconds at most, for the group to be empty before it
answers: a `soffice.bin` still tearing down its address space keeps its
pipe listening, and the sweep that follows would take it for another
job's. The resource limits were already inherited down the tree.
A group is only as good as its membership: a child that called `setsid`
or `setpgid` would leave it and outlive the timeout, confined but
immortal, and with `RLIMIT_NPROC` it could fork until the worker's own
spawns failed, since they share a uid. So the filter refuses both calls
(`bwrap` makes its own session before it installs the filter), the
process count is bounded, and a test times out a `sleep` forked through
`setsid` and finds it gone.
Under bubblewrap the group holds `bwrap` alone, since its pid 1 starts
a session of its own (`--new-session`); there `--die-with-parent` does
the work, killing pid 1 when `bwrap` dies, and the kernel takes the pid
namespace down with it. The same test runs under bubblewrap and finds
the `sleep` gone from the host's `/proc`.

Why a denylist: an allowlist has to track every syscall glibc, glib and
libvips make and breaks silently on an upgrade, in the probe if we are
lucky. The denylist buys what Landlock cannot: no sockets at all, which
closes path-based unix sockets (Postgres over its socket with peer auth),
which no Landlock ABI governs (ABI 6 scopes *abstract* sockets only); no
namespaces, ptrace or kernel-facing surface from a process that only has
to turn bytes into pixels; and, since the child runs as the worker's uid,
none of what the same uid may do to the worker: steal its descriptors
with `pidfd_getfd` (Yama's `ptrace_scope` is 0 on Debian and in a
container that inherits it, and a container running as root passes the
check anyway), set its limits with `prlimit64`, renice it, pin it to a
core or change its scheduling class with `setpriority` and the
`sched_set*` calls, put its I/O in the idle class with `ioprio_set`, or
page its memory out with `process_madvise` on a `pidfd_open`. A review
found the scheduling calls open and a test now reaches for each from
the child. The signal scope above
closes `kill`. `EPERM` rather than kill, so a library that
probes a feature and tolerates failure keeps working, and so the error
shows up in the stderr tail the job already keeps.

Why not reimplement namespaces in Rust with `unshare`: the same kernel
requirements as bubblewrap, without its review history. Namespaces are
bubblewrap's job, below, where a deployment can grant them.

**bubblewrap**, optional: `media.sandbox.bubblewrap = "/usr/bin/bwrap"`
(unset by default) puts `bwrap` around the same child. What it adds over
Landlock: a mount namespace where the configuration file does not exist
rather than is denied, a pid namespace where the worker does not exist,
a network namespace with loopback alone, a fresh `/tmp` (a tmpfs of 64
MiB; nothing agrafe runs writes there but LibreOffice's pipe), and a
fixed uid of the child's own, 10000, mapped from the worker's, so the
same-uid vectors above close as a class rather than one by one. The
argument list is demarche-numerique's `SandboxedCommand`, the reviewed
template: `--unshare-all --unshare-user --disable-userns --new-session
--die-with-parent --cap-drop ALL --uid 10000 --gid 10000 --hostname
agrafe --size <64 MiB> --tmpfs /tmp --proc /proc --chdir <cwd or />`,
then `--ro-bind`
of each path of the system set (`--dev-bind` for the three device
nodes), of the program, of `media.sandbox.readable` and of the declared
readable paths, each at its own path and skipped when missing on the
host, as the ruleset skips them; `--bind` of the writable directory at
its own path, so the argv needs no rewriting; `--add-seccomp-fd`
for each filter; and `--remount-ro / --`, last, since nothing mounts
after it. Nothing is bound for the unix socket directory: inside the
sandbox `/tmp` is a writable tmpfs, so LibreOffice's `access` check
picks it there whatever the host's `/tmp` says, and a read-write bind
of the host's `/var/tmp`, which is what the fallback would be, is the
other jobs' directories, far more than the ruleset's right to make and
remove sockets. The work directory, under the host's `/tmp` by default, is
masked by the fresh `/tmp` and re-exposed by its bind, which works; the
deployment guide prefers `media.tmp_dir` outside `/tmp` all the same.
The `/proc` is the one addition to the system set: a procfs of the pid
namespace, showing `bwrap` and the child and nothing of the worker's,
which LibreOffice refuses to start without (`/proc not mounted`).
Landlock grants no `/proc`, since the worker's own `/proc/<pid>`,
`environ` included, would be in it for a child of the same uid; it only
works under Landlock because `stat` is not governed, so the directory
exists and LibreOffice's check passes. The integration suite run through
bubblewrap by hand is what found this.
No `--clearenv`: the parent already clears the environment and `bwrap`
passes its own through. No `prlimit`: the `setrlimit` calls in
`pre_exec` apply to `bwrap` and are inherited by the child unchanged.
Two differences from the ruleset are accepted: a bind has no `noexec`,
so the work directory and the readable paths are executable under
bubblewrap where Landlock withheld execute, which gives a program that
already runs `/usr/bin/python3` nothing it lacked; and a declared path
given relative is resolved by `bwrap` against the worker's working
directory rather than by the child, so the callers declare absolute
paths, as they all do.

How the layers combine, which is the design decision this layer adds:
the rlimits stay in `pre_exec` in both modes; seccomp moves, since
`bwrap` itself needs `unshare`, `mount` and `pivot_root`, which the
filter refuses, so the filters cannot be installed ahead of it. The same
BPF programs `seccompiler` compiled are written to two memory files
(`memfd_create`), whose descriptors the child clears close-on-exec on
between `fork` and `exec`, and `bwrap --add-seccomp-fd` installs them in
the child after the namespaces are set up: one filter, two delivery
routes. Landlock is off under bubblewrap: a Landlock domain forbids
`mount`, which `bwrap` needs, and the mount namespace covers what the
ruleset covered; `HOME` and `XDG_CACHE_HOME` still move to the work
directory. A `bwrap` that fails before the program runs (exit 1, stderr
starting `bwrap: `) is `Error::Bubblewrap`, never `Outcome::Failed`: a
host matter, so the job is retried rather than the blob marked failed;
`bwrap: execvp <program>: ...` alone is reported as the spawn failure it
is without bubblewrap. `bwrap`, the init of its pid namespace, reports a
child the kernel killed as an exit of 128 plus the signal, so the CPU
limit's `SIGXCPU` is read back from 152 and still a timeout; the office
driver's pipe sweep finds nothing, since the pipe was in the sandbox's
own `/tmp`. Asked for and
impossible, the sandbox is an error in every mode short of `off`:
`isolation = "required"` with `bubblewrap` set means "`bwrap` must
start", not "Landlock must install", and `auto` never falls back to
Landlock, since the operator asked for namespaces and would not learn
from a log line that they were not there.

Why optional and why on a VM: `bwrap` needs unprivileged user
namespaces, and granting them is a host decision with a cost. Ubuntu
restricts them through AppArmor
(`kernel.apparmor_restrict_unprivileged_userns`), because user
namespaces were in the exploit chain of about 44% of kCTF kernel
submissions, and the right grant is a profile for `bwrap` alone, not the
sysctl: Ubuntu 26.04's `apparmor` package ships one,
`bwrap-userns-restrict`, which gives `/usr/bin/bwrap`, attached by
path, its namespace and the capabilities inside it, and moves every
child to a stacked `unpriv_bwrap` profile that strips capabilities
again and audits an attempt; a `bwrap` at another path fails at its
first privileged step (`bwrap: loopback: Failed RTM_NEWADDR: Operation
not permitted`), which the probe reports and a test reproduces by
pointing the setting at a copy. Ubuntu 24.04 has the restriction and not
the profile, so the operator installs the 26.04 one. Inside Docker the
default seccomp profile masks the namespace flags on `clone` and gates
`unshare` behind `CAP_SYS_ADMIN`, and the `docker-default` AppArmor
profile denies `mount`, so a container that runs `bwrap` has to be
started with a custom seccomp profile and AppArmor unconfined, and the
host still needs the userns grant: runtime flags on the orchestrator
that loosen confinement of the whole server process to tighten it
around the decoder. On a container platform that trade is usually wrong
and Landlock with seccomp is the answer; on a VM it is right. The
deployment guide says this in so many words.

*The builder is the contract.* Callers put paths in the argv, and the
sandbox could not tell an input from an output. `agrafe_sandbox::Command`
replaces the bare `tokio::process::Command`: `arg`, `env`, `current_dir`
as before, plus `readable(path)` for a file the child reads and
`writable(dir)` for the one directory it writes. Every caller
(`vips::to_webp`, `composite`, `fit_layer`, the four helpers, the probe)
has one input and one work directory, and the input lives in that
directory, so each declares `writable(workdir)` and the argv falls under
it; the declaration stays explicit because the confinement is the
contract and the argv a means. The program is resolved on `PATH` to an
absolute path when the command is built, because Landlock has to grant
execute on the file the child executes; `media.vips_command` and the
helper settings keep accepting a bare name. Under confinement `HOME` and
`XDG_CACHE_HOME` point at the work directory, so a fontconfig cache
rebuild lands there and is removed with it instead of failing on a
read-only home. The flip side is that nothing persists between jobs: a
host whose `/var/cache/fontconfig` is missing or stale rebuilds the cache
on every PDF render, seconds a job that the probe does not notice, so the
deployment guide has the image run `fc-cache -f`.

*What runs where.* The ruleset is built in the parent, where opening
paths and allocating are fine; the BPF is compiled in the parent once.
The child, between `fork` and `exec`, only calls
`landlock_restrict_self` on the ruleset's descriptor and installs the
filters with `prctl` and `seccomp`: nothing on that path allocates or
takes a lock.
`chdir` happens before, so a `current_dir` under the writable directory
works, and the environment is applied after.

*Configuration*, in `[media.sandbox]`: `isolation = "auto"` (the
default) confines where the kernel can and logs what it got, so a
development VM, CI and a default Docker container get it with nothing to
configure (Docker's default seccomp profile allows the `landlock_*`
syscalls and `seccomp`); `"required"` refuses to start unless Landlock
and seccomp both install, which production sets so a kernel downgrade or
a seccomp profile that blocks them cannot quietly turn confinement off.
It asks that the layers install, not that the kernel knows every access
the ruleset names: a 5.15 kernel passes with the filesystem rules alone,
and the boot line names the ABI, so the floor is the operator's to read;
`"off"` keeps the limits and the timeout. `readable = []` is the operator
list of extra read-only paths. Under `auto`, a layer that fails to
install in the child is skipped; under `required` it fails the spawn.

*The probe asserts a refusal.* Before the render probe, the worker runs
`agrafe_sandbox::probe`: two sibling temporary directories, one granted,
and `cat` on a file in each, first under the seccomp filter alone, to
learn whether it installs, then under both layers. `cat` is the one tool
the probe needs, resolved first so that an image without coreutils says
so instead of reporting a missing layer. The granted read must succeed, or the system
set misses something this host needs, which is a startup error in every
mode; the other must fail where Landlock is enforced, or the ruleset is
not doing its job. Under `required` a kernel that cannot refuse is a
startup error naming the layer. With `bubblewrap` set the same two reads
go through `bwrap`, after `bwrap --version` for the boot line: a `bwrap:
` error is a configuration failure naming the configured binary and
pointing at the user namespace grant, and the other read must fail, with
"no such file" now rather than "permission denied". The worker then logs
what it got: `sandbox: landlock abi 4, seccomp, rlimits`, `sandbox:
bubblewrap 0.11.1, seccomp, rlimits`, or `sandbox: rlimits only
(macos)`. The render probe that follows runs the real command lines on
known-good files under the same confinement, so a path missing from the
system set fails the deployment instead of every thumbnail.

*Tests* in `agrafe-sandbox` assert what is refused: a read outside the
granted directories, a write outside the writable one, a `connect` to a
listening socket on localhost, `unshare`, `setsid` and `setpgid`, the
worker's limits, priority, affinity and I/O class; and what is granted:
`media.sandbox.readable` reads and does not write, the home is the work
directory, threads still work and a spawn stays cheap. They skip with a
message where the kernel cannot confine, except under
`AGRAFE_SANDBOX_REQUIRED=1`, which CI sets so a regression is not a
green empty suite. The integration suite runs under
`auto` on CI, which confines every render it makes. The bubblewrap tests
are the same refusals through `/usr/bin/bwrap`, plus what the namespaces
add: the configuration file does not exist, the network is loopback
alone with nothing listening (seen without the filter, which refuses the
socket first), the worker's pid does not exist, a unix socket bound in
`/tmp` never reaches the host's, a death by signal reads back as 128
plus the signal, a `bwrap` failure is an error and not an outcome, a
timeout takes the namespace down with the `sleep` the program forked,
and a copy of `bwrap` at another path is refused where the AppArmor
restriction is on. They need the packaged binary and skip without it,
under the same variable; CI installs `bubblewrap` on its Ubuntu 26.04
runner, whose AppArmor grants it the namespace with nothing to
configure. `AGRAFE_TEST_BUBBLEWRAP=/usr/bin/bwrap` puts the whole
integration suite through bubblewrap: the probe, vips, the helpers and
LibreOffice under the real namespaces. CI runs the API suite that way
as a second step, since a mount namespace checks the system set harder
than Landlock does (the `/proc` LibreOffice needs is the example), and
the same variable serves on the Lima VM.

*LibreOffice.* The office converter (see "Thumbnail") runs through the
same `Command`, one short-lived process per document and no resident
`soffice --accept` server or pool: the sandbox's unit of isolation is one
document, and a pool would make it N documents from N tenants, where a
document that compromises the process reads every file that member
converts until it is recycled. A hang costs one attempt, as with vips. If
the render queue ever shows conversions dominated by startup, a pool with
a short recycle count is a contained addition later; measured on Ubuntu
26.04 (LibreOffice 26.2, aarch64), a conversion is about 200 ms with a
fresh profile and 135 ms with a warmed one; on macOS (LibreOffice 26.8)
1.5 s and 0.9 s. A pool is not where this starts, and the profile
template below is where the cold start goes. Each run gets a private
user profile under the work directory
(`-env:UserInstallation`, a percent-encoded `file://` URL, since a work
directory with a space in its name is a possibility and LibreOffice would
read the profile elsewhere), so concurrent workers never fight over a
profile lock and nothing a document does persists past the job;
`media.office.profile_template` names a profile warmed at image build
time that is copied in instead, a copy being milliseconds. Either way
the profile's registry gets the macro security pin before the run (see
"Thumbnail"). `HOME`,
`XDG_CACHE_HOME` and `TMPDIR` are the work directory too: LibreOffice
stages its PDF in `TMPDIR` before moving it to `--outdir`, and a
`TMPDIR` outside the writable directory fails the write. Its thread pool
is capped at two (`MAX_CONCURRENCY`), as vips's is: Calc's threaded
formula groups would otherwise take every core, and the CPU time limit,
summed over threads, with them. Debian keeps
LibreOffice's registry and printer configuration under `/etc/libreoffice`,
which the system set does not cover, so the driver declares it readable,
skipped where absent; other layouts go through `media.sandbox.readable`.
Its limits are `[media.office.sandbox]`, 2 GiB and 120 s by default
(LibreOffice needs more address space and more time than vips; 1 GiB was
enough for the probe files, not for a document with pictures), the rest
of its sandbox is `[media.sandbox]`'s. LibreOffice is a far larger attack
surface than anything else in the sandbox, which is why it is off by
default. It is also only as contained as Landlock makes it: a document
names files to LibreOffice (a linked section, a DDE field, a linked
image), and under Landlock those resolve to nothing outside the work
directory, while without it the configuration and the KEK file, readable
to the worker's uid, are a 72-point paragraph away from the thumbnail,
which is small but fits a key. So a worker with `media.office.command`
set refuses to start when the sandbox probe found neither Landlock nor
bubblewrap, whose mount namespace confines the filesystem the same
(`check_office_host`, next to the boot line), unless
`media.sandbox.isolation` is `off`, which is the operator accepting that
on a development machine. Where it is on, it belongs on a worker that handles `render`
only (`jobs.kinds`), with no outbound network, as defence in depth:
LibreOffice follows linked images and OLE links in a document, and what
closes egress to it is the seccomp variant it runs under, which refuses
`socket` for every family but `AF_UNIX` on any kernel with seccomp (TCP,
UDP, netlink alike), with Landlock refusing TCP as well from ABI 4. A
network the worker cannot reach is one less layer to be wrong about, and
bubblewrap's network namespace closes egress for every tool where it
runs. The API image never carries it. The fonts are not optional: without them
a page renders as boxes and exits 0, which nothing reports.

*Costs.* A ruleset and two filters add well under a millisecond per
spawn, and `bwrap` about two milliseconds (twenty spawns of `true`
through it in 43 ms on the Lima VM); libvips startup, which the per-job
pipeline already pays, is 25 to 40 ms. Out of scope: macOS confinement
(`sandbox-exec`), a seccomp allowlist, cgroup memory accounting
(`RLIMIT_AS` plus the pod or service limit stay the bound), and a uid of
the child's own without bubblewrap: the same-uid rules above close the
vectors we know of, and bubblewrap's uid closes the class where it runs.

## Jobs

No external queue. A `jobs` table in Postgres, workers in the same binary,
`SELECT ... FOR UPDATE SKIP LOCKED` polling woken by `LISTEN/NOTIFY`.

Why: a handful of job kinds at modest volume, every job already touches the
blob row, and enqueueing in the same transaction as the state change removes
the dual-write failure where a crash between commit and publish loses the job.

A broker, Redis or a queue library such as apalis, was weighed and is not
used. What the table gives is the transaction: a state change and the job it
implies commit together, dedupe is a unique index, the reaper schedules
itself with a row. A broker loses that and needs an outbox, which is this table plus a relay. Nor
is the queue where the load goes: a billion blobs is a big `blobs` table, the
`jobs` table holds only the backlog, the claim is one indexed `SKIP LOCKED`
update, and the slow part is the workers, vips, clamd and zip. What a broker
was wanted for, a view of the queue and a way to act on dead jobs, is the
`/metrics` and `/jobs` endpoints below. Isolation between kinds comes from
the worker pools, not the queue, see "Build". Revisit when a measurement
says the database is the bottleneck: oldest-due age growing with idle
workers, or dead-tuple churn on `jobs` that autovacuum settings do not
absorb. The enqueue function stays the seam; the table would become the
outbox.

Job kinds: `fetch` (download the bytes of a `url` blob, see "Fetched
blobs"), `scan`, `render` (thumbnail and preview, whichever is pending),
`rows`, `archive` and `document` (generate a blob of that input type from
its stored source, see "Generated blobs"), `purge`
(delete the objects under a blob's prefix), `reap`, the periodic
housekeeping, `notify` (deliver one event of a blob to the webhook,
see "Webhooks"), and `audit`, the weekly sweep of the bucket against the
`blobs` table, see "Audit". One reap, in one transaction, in this order: record
`expired` on the non-terminal blobs past their `expires_at`; expire the
abandoned `created` blobs, holding source parts untouched for
`upload.abandoned_after` or attached and untouched altogether for as long,
or attached with a fetch dead for as long (see "Attachments"); enqueue a purge, `upload.purge_delay` out, for the
blobs just expired or abandoned that hold objects, and for the infected
ones past their expiry that no purge job is going to take care of;
declare dead the jobs out of attempts whose last lease lapsed; prune
dead jobs that failed more than `jobs.keep_failed` (default seven days)
ago, long enough to have been looked at, and enqueue a purge, due now,
for the blob of each dead purge among them; hand `uploading` rows older
than `upload.stale_after` back to `created`; prune tombstones purged more
than `blobs.keep_purged` ago, their attachments and jobs with them.
Expiry itself never waits for the reaper, see "State machine".

Each step takes at most ten thousand rows, picked on its index in the
order it walks, so one reap is a transaction of seconds however much
fell due at once: a bulk import's blobs all expiring on one day, a mass
detach, a reaper down for a week. A run that filled a batch brings the
pending reap forward to now, and the backlog drains run after run, each
committed, ten thousand of each kind at a time. No step walks the blobs
whose purges are pending, which after a mass expiry are millions for as
long as the purges take: every purge is enqueued where its blob goes
terminal (a delete, an expiry), and again where a dead one is removed
(pruned here, or discarded with `DELETE /jobs/:id`), so the one walk left
is of the infected blobs past their expiry, on an index of their own,
which passes the few whose purge is pending or dead. A batch is picked without locks and the
update evaluates its condition again on each row it then holds, so an
attach that holds the row first still wins over the expiry.

The reaper keeps its own clock: each run enqueues the next, `jobs.reap_interval`
(default one minute) later, before doing its work, so a failing run is
superseded by the one it scheduled rather than retried with backoff or left
dead, and the error goes to the log. The runner enqueues the first one when it
starts. The unique index on pending jobs treats a null `blob_id` as a value,
so blob-less kinds dedupe like the rest and however many processes start, or
restart while a reap is pending, one row is pending at a time. A reap holds
an advisory lock for its transaction, so a run longer than the interval and
the one it scheduled, on another process, never update the same rows at
once: the second finds the lock taken and does nothing. The lock is keyed
on the installation, see "Advisory locks" below.

A purge refuses a blob that is not in a terminal state: the job is dead at
once, without the backoff, and stays in the table as evidence of the bug
that enqueued it. An
object missing between the listing and its delete is not an error, so a purge
that runs twice is harmless.

Each row: `id` (a sequence, the listing cursor), `kind`, `blob_id`,
`priority`, `payload`, `run_at`, `attempts`, `max_attempts`, `locked_at`,
`locked_by`, `last_error`, `failed_at`, `created_at`. A
worker claims the highest-priority, then oldest, due job of its kind with
`UPDATE ... WHERE id = (SELECT ... FOR UPDATE SKIP LOCKED)`, which locks it
and counts the attempt. A locked job is
claimable again once `jobs.lease_timeout` has passed, which is how a crashed
worker's job gets picked up. A live worker renews its lease every third of
it while the handler runs, so a scan longer than the lease is not handed to
a second worker; only a dead one lets the lease lapse. One that was only
stalled, and finds at its next renewal that another worker holds the job,
abandons the attempt where it stands, and a build checks its lease once
more before it completes its object: two runs of one job would write the
same key, the row taking one header and the object the other's bytes.
Success deletes the row. Failure, a handler panic included, unlocks it with exponential backoff
(30 s, doubling, capped at an hour, or longer when the failure asks for
it: a remote's `Retry-After`) until `max_attempts` (5), then sets
`failed_at` and leaves it locked: never run again, not holding the one
pending slot per kind and blob, and still there to read. A failure another
attempt would repeat (a job without its blob, a purge of a live blob, a
build's permanent errors, see "Build") is dead at once, without the
backoff. An attempt that takes its worker down, a process killed or out
of memory, never reaches the failure: the claim does not take a job out
of attempts, so the fifth such attempt is the last, and the reaper
declares the job dead once its lease has lapsed, with the blob's verdict
and the `failed` event as for any other death. A failing
job whose twin was enqueued while it was locked is dropped in favour of the
newer one.

A dead job that was to finish its blob, a fetch, a scan, a build or a
render, leaves its verdict on the blob as well: `failed_at` and `last_error` on
the row, in private metadata, as `blob_failed_at` on the attachments
listing, and behind `GET /blobs?failed=true`. Why twice: the dead job is
pruned after `jobs.keep_failed`, the blob it left in `uploaded`, or
`ready` with a representation `pending`, is not, and an attached one is
never expired either. Without the row's copy, a blob whose job died a
week ago is indistinguishable from one whose job is queued. The verdict
follows the job that will finish the blob, not the one that died: it is
cleared when a job of a finishing kind is enqueued, retried or succeeds
for the blob (not the audit's stamp on a lost original, see "Audit": no
job brings an original back), a job dying while a twin of it is live says nothing (the
twin was enqueued while it ran, the running one holding no pending slot),
and a terminal transition clears it too, since nothing is going to finish
the blob and `GET /blobs?failed=true` is for the ones something could. A
dead purge says nothing on the blob: it is housekeeping of a terminal
blob, and the dead job itself is the evidence of the bug that enqueued it.

The repair is `POST /blobs/:id/retry`: it enqueues whatever the blob's
state calls for, the fetch of a `created` `url` blob, the scan or the
build of an `uploaded` blob, the render of a `ready` one with a
representation `pending`, at the blob's priority,
deleting the dead jobs of those kinds for the blob on the way, and
answers `202` with the metadata, the failure cleared. It needs no job id,
so it works after the dead job is pruned, and it is the only way to get a
render going again for a `pending` representation whose job died, which
the read path's repair, keyed on a version or a missing object, does not
see. `409` when the blob waits on no job (`created` and not fetched, or `ready`
with its representations settled), when the job it waits on is pending or running
already, which does the work as for `POST /jobs/:id/retry`, or when the
blob is `infected`; `410` for an expired or deleted one. The row is
locked for the operation, so a transition racing it lands on one side or
the other. `POST /jobs/:id/retry` stays for the operator looking at the
dead jobs; the blob's retry is for the backend looking at its records.

`enqueue` calls `pg_notify` in the same statement, so the notification
travels with the commit. One listener per process wakes the pollers of that
kind; `jobs.poll_interval` is the fallback for a missed wakeup. Concurrency is
per kind in `jobs.concurrency` (defaults: `fetch` 2, `scan` 4, `render` 4,
`rows` 2, `archive` 1, `document` 2, `purge` 4, `reap` 1, `notify` 4,
`audit` 1): rendering and compiling are CPU bound, scanning waits on
clamd, a rows or archive build, a fetch and a delivery on the network,
and one reaper or auditor per process is plenty since a row is pending at
a time anyway.

Isolation is by kind. Every kind has its own workers, so however many slow
jobs of one kind are queued, the others keep flowing; that is the property
production relies on and the reason there is no priority across kinds.
Priority orders within a kind: `high`, `normal` or `low`, set on the blob
at creation and copied onto its fetch, scan, render, build and notify jobs. Purges and
blob-less jobs run `normal` whatever the blob says: they are housekeeping
nobody waits on. It orders, it does not reserve: a kind fed `high` jobs
without end runs its `low` ones never, which is the point for an archive
queue and visible on the per-priority oldest-due metric for everything
else. Scans and renders carry it too, so an interactive upload can jump a
bulk import of the same kind, but the default, `normal` everywhere, is a
FIFO per kind.

`server.role` (or `serve --role`), `api`, `worker` or `all`, lets workers
move to separate machines without code changes; `server.interfaces` (or
`serve --interfaces`, comma separated), `private`, `public` or both, the
default, names the interfaces an API process serves, so that the hosts
the internet reaches need not carry the private API at all, and the
backend's host need not open a public port. Whatever it says, every
process binds `server.private_addr`: the private API when it serves it,
`/healthz` and `/metrics` alone otherwise, so that one address is always
there to probe and scrape; `server.public_addr` is bound only when
served. `jobs.kinds` (or
`serve --kinds`, comma separated) names the kinds a process runs workers for, every kind when
omitted. That is how the archive builders get machines of their own, or
the renderers more of them. A kind no process handles only piles up, which
the oldest-due metric shows; a process not handling `render` skips the
renderer probe, one not handling `reap` does not schedule the first reap,
and one not handling `audit` does not schedule the first audit.

### Advisory locks

The reap and the audit each run one at a time across a deployment, under
a Postgres advisory lock: the reap's for its transaction, the audit's for
its session. Advisory locks belong to the server, not to the database, so
a key naming the job alone is one lock for every agrafe on the server,
and a staging and a production instance sharing a host would take turns
at each other's reaps, a long one on either stalling the other's. The
key's first half is therefore a hash of the database and the schema, the
second the job: two installations on one server, or in two schemas of
one database, never contend, while the processes of one installation see
the same names and do. The helpers in `jobs.rs` are the one place the
key is built.

### Audit

Nothing on the request path checks that the `blobs` table and the bucket
agree, and the mismatches worth catching are fleet-wide: a bucket or a
database restored from backup, a bad purge, a lifecycle rule. A per-blob
`POST /blobs/:id/verify` was considered and rejected as the shape: the
backend has no reason to ask about one id. The `audit` job sweeps instead,
weekly (`jobs.audit_interval`): each run enqueues the next once it is
over, the runner enqueues the first one an interval out (not now: a
restart is not a reason to list the bucket, and a pending one survives
it, the duplicate dropped; so does a running one, or one whose worker
went away with its lease, which counts as pending: a twin beside a
sweep in progress would supersede it at its first failure, its progress
with it), and `agrafe audit` on the command line runs
the same sweep on demand, for the operator who just did a restore. With
`--progress <file>` it saves its place to that file as the job does to its
row, the options with it, and resumes from it: the command is the one
sweep that deletes or reads back, the slowest there is, and a dropped
session must not send it back to the first key. Unlike
the reaper, the next run is scheduled at the end and not the start: a
blob-less job that fails while its twin is pending is redundant, not
retried (`fail`, "superseded"), and the retry is what carries a sweep
on. The last attempt and a run that stepped aside schedule it too, so a
dead job does not end the weekly cadence.

A sweep of a billion blobs is days of listing, and a deploy or a
transient error must not send it back to the start. The job keeps its
place in its payload (`Progress`: the last key listed and checked, the
last row passed, the tally so far), saved every minute and when the
listing ends, and the attempt that finds it there resumes from it, the
listing with `list_with_offset` (S3 and GCS start after a key on the
server; Azure lists from the start and the rest is skipped) and the rows
with their cursor. A save is progress made and gives the job its
attempts back, since the attempts guard against failing without getting
anywhere, and a restart that lets the lease lapse costs an attempt
(`claim` counts it). What is between the last save and the failure is
done twice, which every action tolerates: the stamp is conditional, the
render request is conditional, the delete takes `NotFound` as done; the
counts of that stretch are added twice, which a tally lives with. Acting
on a finding never ends an attempt: a stamp, a render request or a delete
that fails, or an original the deep check cannot read, is logged at
`error`, counted as not acted on (`errors`, the `agrafe_audit_errors`
gauge, a non-zero exit of the command) and left for the next run; only
the listing and the rows failing do, and the runner retries from the
last save.

Three things can be wrong, at different costs. A row says present and the
bucket says missing: a non-terminal blob without its original, or
a representation `ready` without its WebP. The bucket has objects no row
accounts for: an original under a row that never recorded it (a `PUT`
whose row update failed), a part or a representation the row says is not
there, a purged blob's leftovers, which are orphans, and a prefix with no
row at all, which is a finding of its own, `unknown`, see below. And an
object present but wrong: truncated or corrupted ciphertext.

The sweep merges the bucket listing with the `blobs` table in key order,
one pass over each and nothing held for the length of the bucket, in
memory or in the database. S3, GCS and Azure list their keys sorted, and
a key is the blob's shard then its hyphenated lowercase id (see "Object
keys"), which sorts as `(get_byte(uuid_send(id), 15), id)` does in the
database, so the objects of a blob come as one group and the rows between
two groups are the ones the listing has nothing for; a key whose id is
any other spelling of a UUID, or sits under a shard that is not its own,
is foreign, not a prefix. The listing is one request per thousand
objects, the rows one query per thousand, paged in the same order through
the expression index `blobs_shard_idx` over the rows that require objects
(an original, source parts, a `ready` representation; not terminal, not
purged), and a listed prefix with no such row is looked up by id, since a
tombstone's leftovers and a `created` blob's stray object are orphans, and
a prefix with no row at all is not one. A backend that listed a prefix out
of order would be refused (`AuditError::Unsorted`), not misread. The local backend walks its
directory in filesystem order and its listing, development-sized, is
sorted in memory. The listing carries sizes,
so truncation is caught without a read: the expected ciphertext length
of the original and of each representation follows from the plaintext
size and `chunk_size` (`Layout::ciphertext_len`), and a source part's
length must be one a stream of that layout produces. The deep check,
`agrafe audit --deep`, decrypts the originals and compares their SHA-256
with `checksum_sha256`, every one or one in N (`--sample N`, picked by
the random low bits of the id so the sample is the same from one run to
the next and a run can be repeated to confirm), which is reading the
bucket and is never the job's. A data key wrapped under a KEK the process
does not have is an error to act on, not a damaged original: it says
nothing of the bytes.

What it does with a finding, each one a `warn` line in the log:

- Missing or damaged original on a non-terminal blob: report only. There
  is no repair, the client sent the bytes once. The row gets `failed_at`
  and `last_error` (`audit: ...`) unless a verdict is on it already, so
  the blob shows in `GET /blobs?failed=true` and on the attachments
  listing; no new state, and no `failed` webhook event, which is the
  news of a dead job.
- Representation `ready` but the object missing or the wrong length: set
  back to `pending` and the render enqueued, the read path's own repair.
  Report only when the original is missing or damaged too, the shape of a
  bucket restored from an older backup: there is nothing to render from,
  and the render job would only fail its attempts and replace the
  original's stamp with its own error.
- Missing or truncated source part under a `created` or `uploaded`
  generated blob: report only; the build fails on it itself. The parts
  stay after the build and go with the purge, so under a `ready` blob
  they are accounted for and not required.
What the row requires is checked only once the row has stood still for
`upload.stale_after` before the listing passed its prefix (`updated_at`,
kept by the trigger, against the time the listing of that stretch began;
for the rows the listing never showed, the time it ended). The listing
and the row are read at different moments, and a row that changed in
between may name an object written after the listing looked, an upload
finishing as the audit went by, or a new size for one it listed, a
render replacing it: what the listing shows of it is not evidence. Such
a row is counted as unsettled and left for the next run, which is also
what makes the rows created while the listing ran safe, wherever their
shard puts them in its order, since they are unsettled by construction. What the
row does not account for is judged by the object's own age: one written
within `upload.stale_after` of the listing passing it is what an upload
or a render in flight looks like, its row not yet updated, and is left
for the next run too. The stamp on a missing or damaged
original is conditional on the state the row was read in and on no
purge since, so a row that moved on meanwhile is not written.

- Orphan object, under a known prefix: report by default. The row is
  authoritative, so it is garbage, and `agrafe audit --fix` deletes it,
  only when older than `upload.stale_after`, so an upload or a render
  that has written its object and not yet updated its row is never
  touched; a thousand per request through `delete_stream`, which S3
  takes as one `DeleteObjects`, since a bucket restored from backup
  leaves orphans by the million. A terminal blob awaiting its purge is skipped altogether, its
  prefix is the purge's. A key outside the layout, not
  `<shard>/<blob id>/<name>`, is reported as foreign and never deleted: it
  is not agrafe's.
- Unknown prefix, one no row has: report, with the number of such
  prefixes and the span of their ids, which being UUID v7 is the span of
  the blobs' creation times. It is not an orphan, because nothing says
  it is garbage. A purge's leftover whose tombstone the reaper has since
  pruned looks like this, rare and harmless; so does every blob uploaded
  after the backup a database was restored from, which is the finding
  the audit exists for, and there the object is the file: the backend
  holds the envelope with the wrapped key, the public read path serves
  the original from it without a row, and the rows can be rebuilt from
  it as long as the objects are there. `--fix` never touches them; the
  tally says "under N prefixes, blobs created between A and B", which
  after a restore reads as what the backup missed, and deleting them
  takes `--delete-unknown` on its own, once the database is as recovered
  as it will be. Older than `upload.stale_after`, as orphans are.

The decision taken up front is that the job never deletes. The
destructive path is always a human running a command with the report in
front of them, as the reaper only ever acts on what a row already says,
and a command that deletes what no row says anything about is a second
one. The command exits non-zero when anything was found, so a restore
procedure can end with it; a foreign object is reported and does not
count, a README in the bucket is not a failed restore, and the alert on
findings leaves orphans and foreign objects out for the same reason,
they cost storage and nothing else. Unknown prefixes stay in, they are
the sign of lost rows. What the listing does not show the
audit does not see: a multipart upload a crashed process left open on S3
or GCS is parts, not an object, and the bucket's
`AbortIncompleteMultipartUpload` lifecycle rule is what reclaims them
(see the deployment guide).

Results go to the log and to gauges, see "Observability": counts per
finding kind from the last run, what it listed and checked, and when it
finished. No new table. One audit runs at a time: the session holds an
advisory lock (see "Advisory locks"), and a job finding it held logs and
steps aside, the command line reports it. A job with progress does not:
the holder may be its own previous attempt, whose session outlives a
lapsed lease until the server sees the socket go, and stepping aside
would delete the row and the progress with it; it fails the attempt
instead, and the retry finds the lock free.

Later, once the sweep exists, a `?verify=true` on the private
`GET /blobs/:id` that `HEAD`s the blob's objects is a few lines over the
same check; for the person debugging one file, not the design.

### Observability

`GET /metrics` on the private port, Prometheus text format. The queue gauges
are read from the `jobs` table at scrape time, so any process reports for the
whole deployment: `agrafe_jobs{kind,state,priority}` with `state` one of
`due` (unclaimed, runnable now), `scheduled` (unclaimed, `run_at` ahead: a
retry backing off, a purge waiting for its delay), `running` (lease
current) and `dead`; and `agrafe_jobs_oldest_due_seconds{kind,priority}`,
the wait of the oldest due job, which is the number to alert on: it grows
when a kind's workers cannot keep up, or, for one priority alone, when the
higher ones starve it. A lapsed lease counts as unclaimed, as the claim query sees
it. The age is read off the head of `jobs_claimable_idx`, one probe per
kind and priority, so it costs the same however long the queue; the
counts are a scan of the backlog, half a second per five million rows,
and run under a five-second statement timeout. A census that fails,
too slow or the database out of reach (the wait for a connection
counts), leaves the queue gauges at the last one's values with
`agrafe_jobs_census_ok` at 0, and `/metrics` still answers with what the
process knows of itself. Per process: `agrafe_job_runs_total{kind,outcome}` with `ok`, `retry`,
`dead` and `superseded`, `agrafe_job_duration_seconds{kind}`,
`agrafe_workers{kind}` and `agrafe_webhook_deliveries_total{event,result}`
with `delivered` or `failed`, see "Webhooks". The last audit this process
ran, see "Audit": `agrafe_audit_findings{kind}` with `missing_original`,
`damaged_original`, `missing_representation`, `missing_source`, `orphan`,
`unknown` and `foreign`, `agrafe_audit_objects` and `agrafe_audit_blobs` for what
it listed and checked, and `agrafe_audit_last_run_timestamp_seconds`,
zero until one has run; gauges, since a sweep is a census and not a
rate, and per process, since the one worker handling `audit` is the one
that knows. A `worker` process has no API; it binds
`server.private_addr` all the same, serving `/healthz` and `/metrics` only.

`GET /jobs` lists jobs newest first, cursor paginated on the id, filtered by
`kind`, `state`, `priority` and `blob_id`. `GET /jobs/:id` shows one with its
`last_error`. `POST /jobs/:id/retry` resets a dead job's attempts, makes it
due and notifies the workers; `DELETE /jobs/:id` removes one, and a dead
purge's blob gets a new purge, due now, since nothing else would take its
objects. Both answer
`409` for a job that is not dead: a pending job is the only thing that will
finish its blob, and a running one is a worker's. A retry whose blob already
has a pending job of that kind is `409` too; the newer job does the work.
The same census and listing as pages, with the buttons, is the dashboard;
see "Dashboard" under "Interfaces".

### Tracing

**A request and the jobs it leads to are one trace.** The work an upload
sets off is asynchronous and spread over processes: the request enqueues a
scan, the scan's run a render, the render's a webhook. Each of those units
runs under a W3C trace context, `agrafe::trace::TraceContext`: a trace id
they share and an id of their own. The request middleware and the job
runner set it in a task-local for the duration of the unit, and
`jobs::enqueue` (and the notify inserts) store the current one on the row
as `traceparent`, naming the enqueuing unit as the job's parent. A run
continues the stored context with an id of its own; a job without one
starts a trace. Retries are further children of the same parent, the
operator's retry of a dead job included: `jobs::retry` leaves the stored
context alone, so the run continues the trace of the work it was part of,
not the retry request's, which is where the failure is being looked at. A
duplicate dropped by `ON CONFLICT DO NOTHING` keeps the first enqueuer's
context; a `notify` that replaces the pending one takes the newer event's,
as it takes its payload.

The periodic `reap` and `audit` schedule their successor from inside their
own run, where the ambient context is the run's: stored as is, it would
chain every run since the worker started into one trace, with every purge
and expiry webhook any of them enqueued. `reap::schedule` and
`audit::schedule` therefore enqueue under `TraceContext::detached`, which
sets no context, so each run is a root and its own fan-out is its trace.
That fan-out is large by nature, thousands of purges from one reap on a
big table, which is right for the logs, one id for one sweep's
consequences, and heavy for an exporter; span links are the answer when
one lands.

The context is ambient rather than a parameter because the enqueues sit
deep under the handle's methods, a dozen call sites, and every one of them
would otherwise thread it through. A task-local does not cross
`tokio::spawn`, so the one spawn in a request's path, the upload
pipeline's task, goes through `trace::spawn`, which carries the context
and the current span. A host calling the handle directly runs the call in
`TraceContext::scope` to have its jobs join its trace.

**The private port continues the caller's trace; the public port does not.**
A single valid `traceparent` header on a private request is continued, so
the host application's trace id is the one in agrafe's logs; none, several
or a malformed one starts a new trace, as W3C says. Public requests always
start one: their clients are browsers and anyone, and a trace id chosen by
the client could merge unrelated work or flood one trace. Roots are marked
sampled: the sampling decision is an exporter's, and a parent marked
unsampled would make a parent-based sampler drop the jobs.

**Spans.** `request` (method, path, `trace_id`) and `job` (id, kind, blob,
attempt, `trace_id`) are at `info`, so every log line inside one carries
the trace id. The public span records the path with the token redacted, the
private one the path without its query string. Inside them, `storage`
(`op`, `path`) wraps every object store call, through `storage::Traced`,
which `Agrafe::with_store` puts around any store; a read's span ends at the
response head, a delete, which `object_store` runs as a stream of paths,
has one span for the stream and no path, and listings have none, being
streams. `sandbox`
(`program`, `outcome`) wraps every subprocess, never naming its arguments,
which are paths of user files. `clamav` covers the connection and the wait
for the verdict. The fetch and webhook calls are not spanned apart: each is
the whole of its job.

**No exporter yet.** The spans and the stored context are what an
OpenTelemetry or Sentry layer needs: the job span's parent comes from
`Job::trace_context`, the request span's from the middleware's context.
Until then the trace id ties log lines together, and the jobs API and the
dashboard show it on each job. When an exporter lands, the context should
come from its span instead of being generated here, so that the stored
parent ids name spans the backend has.

## Webhooks

The backend is told when a blob reaches a state it waits for: agrafe
`POST`s to one URL it configures once, `webhook.url`. Polling `GET
/blobs/:id` stays as the fallback and the webhook takes it out of the
normal path, where the backend would otherwise run a polling job per blob
it is waiting on: every blob finished by a job (a deferred scan, a `rows`,
`archive` or `document` build, a fetch), every dead job, and every record whose
upload the user walked away from.

Four events. `ready`: the original became readable, the scan passed or was
skipped, or the build finished. `infected`: the scan failed; terminal.
`failed`: the job that was to finish the blob died, `failed_at` set on the
row by `jobs::fail`; retryable through `POST /blobs/:id/retry` or `POST
/jobs/:id/retry`, after which a `ready` or a second `failed` follows.
`expired`: the reaper recorded the blob `expired`. `infected` and `failed`
are two events because they call for two actions, one is final and the
other is retry or give up. `expired` fires when the reaper records it, not
at the instant `expires_at` passes, so it lags by up to
`jobs.reap_interval`; attached blobs never expire, so in practice it is
the abandonment signal, an attached blob whose file never came. No event
for `deleted`, the backend caused it, nor for representations, which have
`wait` and the placeholder.

Delivery goes through the jobs table: a `notify` job, the blob id and the
event in its payload, enqueued in the same transaction as the state change
(`repo::enqueue_next` for `ready` and `infected`, which the inline scan,
the deferred one and the builds all reach; `jobs::fail` in the branch that
stamps the blob; the reaper for the ids `expire` and `expire_abandoned`
return). Postgres gives the outbox for free: no transition without its
notification, no notification for a transition that lost its conditional
`UPDATE`. The retries with backoff, the dead state, the `/jobs` listing,
`POST /jobs/:id/retry` and the `keep_failed` pruning all come along. A
delivery gets ten attempts where other jobs get five, about three hours of
the doubling backoff rather than seven and a half minutes, so a backend
down for an hour drains its backlog with no new machinery, and an
operator sees undelivered notifications where dead jobs already are. A
dead `notify` says nothing on the blob: it is the news of the blob, not
its progress. When no URL is configured nothing is enqueued, so a
deployment without a webhook has no idle jobs; `webhook.events` narrows
the set.

The request is a JSON body with `event`, `occurred_at` (when the
transition was recorded), `attempt` (deliveries of this event so far, this
one included) and `blob`, the same object `GET /blobs/:id` answers, read at
delivery time: attachments, URLs and envelope included. A state-carrying
body makes at-least-once delivery and reordering harmless: the backend
applies what it sees, and a retried `failed` landing after the retry that
fixed it still says `state: ready`. One shape for the poll and the push
means one parser on the backend's side; the struct lives in the library,
`blob::metadata`, and the HTTP layer serialises the same one. One `notify`
job is pending per blob, the unique index on pending jobs, and a second
event raised while the first is still undelivered replaces it: the newest
event, its attempts started over, with the state behind it at delivery.
The backend then hears `expired` rather than a `ready` that is no longer
true, and `ready` after a `failed` the retry already mended. A job that
is running holds no pending slot, so an event raised meanwhile gets a job
of its own and the two deliveries can cross: the receiver applies the
`blob` it is handed, its `state` and `updated_at`, and treats `event` as
the reason it was called, never as the state to record. A blob pruned
before its delivery (`blobs.keep_purged`) takes its jobs with it.

The headers follow [Standard Webhooks](https://www.standardwebhooks.com/):
`webhook-id` is an id minted when the event is enqueued, stable across the
retries of one delivery so the receiver can deduplicate on it, and new when
a later event replaces the pending job: the job id would be the same for
both, and a receiver that took the first delivery while its response was
lost on the way back would drop the second as a repeat. `webhook-timestamp`
is the Unix seconds
at sending, and `webhook-signature` is `v1,` followed by the base64 of the
HMAC-SHA256 of `id.timestamp.body` under the secret. The receiver checks
the signature over the raw body, then rejects a stale timestamp. The
secret, `webhook.secret`, is `whsec_` followed by base64 as the standard's
libraries expect it, so a backend verifies with one of them and no code.
It is its own secret, not a `[signing]` key: those mint URLs and should
not double as a shared secret with the backend. A `2xx` within
`webhook.timeout` (default ten seconds) is delivered. Anything else, a
timeout or a connection error fails the attempt and goes through the
backoff, the head of a non-`2xx` body in `last_error`. No redirect is
followed: the URL is the operator's and is taken as given. It must be
`https` unless `webhook.allow_insecure` says otherwise: the body carries
the presigned source of a `url` blob and the blob's envelope, as `GET
/blobs/:id` does, and the private port's cleartext is at least confined to
the trusted network where the webhook's is not.

One URL per deployment, not a per-blob callback URL at creation: there is
one backend, and a per-blob URL is an SSRF primitive handed to whoever
holds the private port. A list of endpoints is a small extension if a
second consumer ever appears. Delivering from the transition itself,
outside the jobs table, would lose the outbox guarantee and the retry,
dead and listing machinery for nothing; a single `changed` event carrying
only the id would have the backend poll anyway, where sending the
metadata is one query at delivery time.

`agrafe_webhook_deliveries_total{event,result}` counts the attempts of
this process, `delivered` or `failed`; the `notify` series of the queue
gauges show the backlog, and `agrafe_jobs{kind="notify",state="dead"}` is
the number to alert on, the deliveries given up.

## Configuration

One TOML file, path from `--config` or `AGRAFE_CONFIG`, every key overridable
by environment variables prefixed `AGRAFE_` with `__` as the nesting separator.
Secrets (database URL, KEKs, signing keys, bucket credentials) are expected
from the environment in production, and are held in types whose `Debug` output
is redacted so a dumped config cannot leak them. See `config/example.toml`.

**One schema, by search path.** `database.schema` puts the tables in a
schema of the operator's choosing, for a host application that keeps
agrafe in its own database. The queries and the migrations name their
tables unqualified, so the schema is a connection setting, `search_path`
passed as a startup parameter of every pooled connection, not a string
spliced into SQL: the same query runs against `public` and against a
named schema, and the schema can differ per deployment. `serve` and
`audit` refuse to start while a migration of their build is missing or
differs from the one applied (`db::check_migrated`, which reads
`_sqlx_migrations`): `/healthz` would answer for a process that fails
every query. `migrate` creates
the schema when it is missing, before the migrator runs, so that
`_sqlx_migrations` lands there too and a host that also uses sqlx keeps
its own history in `public`. The name is validated at load to a plain
lowercase identifier, which is what makes interpolating it into `CREATE
SCHEMA` safe and keeps the unquoted `search_path` and the quoted form
the same name. The advisory locks are keyed by database and schema, so
two agrafe installations in two schemas of one database never contend.

## Stack

| Concern | Choice |
|---|---|
| HTTP | `axum` on `tokio`, two routers sharing state, two listeners |
| Database | Postgres via `sqlx`, migrations in `crates/agrafe/migrations/` |
| Object store | `object_store`, local filesystem, S3, Azure Blob Storage or Google Cloud Storage |
| Fetching `url` blobs, webhook deliveries | `reqwest` with rustls, a resolver of our own enforcing the address policy on fetches |
| Config | `figment` with TOML and env providers; an unknown key is refused, not left at its default |
| Signing | `hmac` + `sha2` |
| Encryption | `aes-gcm` + `hkdf`, Tink streaming format, own crate (`agrafe-crypto`) |
| Sniffing | `infer` |
| Metrics | `prometheus`, text exposition on `/metrics` |
| Dashboard | `askama` templates compiled in, `askama_web` for the axum glue, daisyUI's component CSS vendored |
| Scanning | clamd `INSTREAM` client |
| Images, PDF pages | `vips thumbnail` (libvips) in a sandboxed subprocess |
| Office first pages | LibreOffice (`soffice --convert-to pdf`), optional, one sandboxed process per document with unix sockets allowed, then `vips thumbnail` |
| Embedded images | `lopdf`, `zip`, `quick-xml` in the sandboxed `agrafe-extract` helper |
| Generated ZIP, XLSX, ODS | Own streaming ZIP writer (`jobs::build::zip`), `flate2` for the deflated sheet XML, XML written by hand |
| Text thumbnails | `ab_glyph` with an embedded DejaVu Sans Mono, in the sandboxed `agrafe-render-text` helper |
| Generated PDF documents | `typst` and `typst-pdf` with the embedded fonts of `typst-assets`, in the sandboxed `agrafe-render-document` helper |
| Maps | `geojson`, `quick-xml` and `serde_json` in the sandboxed `agrafe-render-geo` helper, the WMS of `[media.map.layers]` through `reqwest`, `vips composite` with rsvg in the sandbox |
| Ids | UUID v7 |
| Object keys | `<shard>/<blob id>/<name>`, the shard the low byte of the id (see "Object keys") |
| Time | `jiff`. `Timestamp` throughout the model; `jiff-sqlx` wrappers only at the SQL boundary |

## Releases

Every crate is released together, at the version in `[workspace.package]`;
the internal requirements in `[workspace.dependencies]` carry the same
one. The crates are one product cut in pieces for the reasons in
[Crates](#crates), not libraries with lives of their own, and versioning
them apart would ask every change which of twelve versions it bumps.

The changelog is written by hand, under `Unreleased`, in the commit that
makes the change. The commit messages say what changed for a reader of
the code; the changelog says it for an operator or an embedder, and
generating one from the other (release-plz, git-cliff) would need
conventional commits and still list every test and refactor.

A release is a tag. `cargo xtask release <version>` bumps the version,
dates the section, commits `Release <version>` on `main`, tags it
`v<version>` and pushes both with jj; nothing else writes to `main` or
creates tags. The task is Rust, an `xtask` crate that is never
published, rather than a shell script: versions are ordered by the
`semver` crate, Cargo's, which puts a pre-release before its release as
`sort -V` does not; `Cargo.toml` is edited with `toml_edit`, keeping its
comments and layout, rather than with `sed`; and the ordering, the bump
and the changelog edits are unit tests run with the rest. The workflow
reads the release notes through the same code (`cargo xtask notes`). A pre-release takes no
changelog section: its notes are `Unreleased`, which the release after it
takes whole, so the release's entry says everything since the last one. The tag runs `.github/workflows/release.yml`, in the order
that keeps an irreversible step last but one:

1. the tag against the workspace version and the changelog section;
2. the CI suite, `ci.yml` called on the tagged commit;
3. the Docker images, both variants built per architecture on native
   runners, pushed by digest, then tagged together;
4. the crates, `cargo publish --workspace`, which orders them and waits
   for the index between them. crates.io never takes a version back, so
   this waits for everything that can fail before it; re-running the
   workflow skips the crates already published. It authenticates
   through crates.io's trusted publishing (OIDC, no stored token) once
   the crates exist; the first release, which creates them, uses a
   `CARGO_REGISTRY_TOKEN` secret, deleted afterwards;
5. the GitHub release, its notes the changelog section.

The images are Debian trixie, whose libvips carries the security fixes
(see "libvips" in the deployment guide), built in two stages: the
toolchain in a `rust:*-slim-trixie` stage, the binaries copied into a
`debian:trixie-slim` one, so that what runs is linked against the glibc
it was built with and carries no compiler. Two variants, the second
adding LibreOffice and a warmed profile: LibreOffice is 400 MB and the
largest attack surface in the sandbox, and most deployments, every API
process among them, never need it. The image's settings for a container
(addresses on all interfaces, `media.tmp_dir` outside `/tmp`, the office
converter) are a configuration file named by `AGRAFE_CONFIG`, not
environment variables, since those would override an operator's own file.
