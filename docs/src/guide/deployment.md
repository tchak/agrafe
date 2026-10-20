# Deployment

A deployment is Postgres, an object store, `clamd`, and processes running
the `agrafe` binary with its four helpers next to it.

## What to ship

| Binary | Role |
|---|---|
| `agrafe` | The server: `serve` and `migrate` |
| `agrafe-extract` | Pulls the embedded image out of a PDF, DOCX or ODT |
| `agrafe-render-text` | Rasterises the first lines of a text file |
| `agrafe-render-document` | Compiles a typst workspace into a PDF |
| `agrafe-render-geo` | Draws a GeoJSON file as SVG over the map extent it chose |

The helpers are found on the `PATH`, or at the paths given by
`media.extract_command`, `media.render_text_command`,
`media.render_document_command` and `media.render_geo_command`. Ship them
from the same build as the server: they read no configuration, write no
logs and never touch the network, and the startup probe fails if one of
them is not what the server expects.

Workers also need the `vips` command line tool, built with SVG support
(rsvg) for the maps. Run `agrafe migrate` once per release, before the new
processes start.

Outbound connections: the fetch workers reach the URLs of `url` blobs, the
notify workers reach `webhook.url`, and the render workers reach the hosts
of `media.geojson.layers`, if any are configured. Nothing else in agrafe
talks to the network besides Postgres, the object store and `clamd`.

## The sandbox

Each render runs under Landlock and seccomp on Linux (see [Security
model](security.md#sandboxed-rendering)). Nothing is needed for it: a
kernel of 5.13 or newer has Landlock, every kernel in service has
seccomp, and Docker's default seccomp profile allows both. The startup
probe reads a file with `cat`, so the image needs coreutils (a
distroless image does not have it). Set
`media.sandbox.isolation = "required"` in production so that the worker
refuses to start without them, and watch the `sandbox:` line in the
worker's log. A host that keeps fonts or poppler's data outside `/usr`,
`/etc/fonts` and `/var/cache/fontconfig` lists them in
`media.sandbox.readable`; without them a PDF whose fonts are not embedded
renders blank, and the startup probe does not catch that.

## libvips

libvips decodes untrusted files, so its version matters more than its
features.

- **8.15** is the minimum the renderer's options need.
- **8.18.3 or newer** has the fixes for heap overflows in the TIFF, EXIF and
  PDF loaders. Run it, or a distribution build that backports them, such as
  Debian trixie's with its security updates.
- The version number alone does not tell: Debian's patched build still says
  8.16.1, while Ubuntu 26.04's 8.18.0 is in universe and has none of the
  fixes. See the [libvips security
  advisories](https://github.com/libvips/libvips/security/advisories).
- Prefer a libvips built with **pdfium** over poppler for PDF pages; it is
  the more hardened renderer.

## Postgres

agrafe needs one database. `database.max_connections` (ten by default)
sizes the pool of each process. Every job claim, upload and read without a
sealed URL uses the database, so size the server's `max_connections` for
the sum of the pools.

## Object storage

Local storage is for development:

```toml
[storage]
backend = "local"

[storage.local]
path = "./data"
```

Any S3-compatible store works in production:

```toml
[storage]
backend = "s3"

[storage.s3]
bucket = "agrafe"
region = "fr-par"
endpoint = "https://s3.fr-par.scw.cloud"
# access_key_id and secret_access_key from AGRAFE_STORAGE__S3__ACCESS_KEY_ID
# and AGRAFE_STORAGE__S3__SECRET_ACCESS_KEY
```

Configure an **`AbortIncompleteMultipartUpload` lifecycle rule** on the
bucket; one day is fine. A crashed process cannot abort the multipart
uploads it had open, and only the rule reclaims their parts.

Objects are keyed by blob: `<id>/original`, `<id>/preview`,
`<id>/thumbnail`, and `<id>/source/<n>` for the parts of a generated blob.
A purge deletes everything under `<id>/`.

## ClamAV

`clamd`'s defaults are sized for mail. In `clamd.conf`:

| Setting | Value | Why |
|---|---|---|
| `StreamMaxLength` | above `upload.max_size` | Past it, clamd hangs up and the scan fails |
| `MaxScanSize` | above `upload.max_size` | Past it, clamd scans a prefix and answers `OK` |
| `MaxFileSize` | above `upload.max_size` | Same, silently |
| `AlertExceedsMax` | `yes` | A limit hit anyway is reported as `FOUND` rather than clean |
| `MaxThreads` | above `jobs.concurrency.scan` plus `clamav.max_inline_scans` | Every scan holds a connection |

When `clamd` cannot be reached, uploads still succeed and their scans are
deferred to jobs, retried until `clamd` answers. Nothing becomes readable
meanwhile.

## Keys

Generate each key with `openssl rand -base64 32` and pass it through the
environment:

```sh
AGRAFE_DATABASE__URL=postgres://agrafe:…@db/agrafe
AGRAFE_SIGNING__KEYS__K1=…
AGRAFE_ENCRYPTION__KEKS__KEK1=…
AGRAFE_WEBHOOK__SECRET=whsec_…
```

Losing a KEK loses every blob it wraps. Back up the KEKs separately from
the database and the bucket.

## Splitting the API and the workers

`server.role`, or `--role`, picks what a process runs:

| Role | Serves | Runs workers |
|---|---|---|
| `all` (default) | both ports | yes |
| `api` | both ports | no |
| `worker` | `/healthz` and `/metrics` on `server.private_addr` | yes |

```sh
agrafe --config agrafe.toml serve --role api
agrafe --config agrafe.toml serve --role worker
agrafe --config agrafe.toml serve --role worker --kinds archive
```

`--kinds`, or `jobs.kinds`, narrows the job kinds a worker runs. Dedicated
pools are how archive builds get machines of their own, or renders more of
them. Every kind must be handled somewhere. A worker that does not handle
`render` skips the renderer probe, and one that does not handle `reap` does
not schedule the reaper.

API processes are stateless and scale horizontally. A sticky session is not
needed: the read cache is per process and short-lived.

## Behind a proxy

- Expose only the public port to the internet, with TLS terminated at the
  proxy. Set `server.public_base_url` to the URL browsers use, without a
  trailing slash.
- If the proxy serves agrafe under a path prefix, it must strip the prefix,
  and `public_base_url` must include it.
- agrafe sends no CORS headers. For uploads from a page on another origin,
  serve the public port under the application's origin or add CORS headers
  at the proxy.
- Allow long requests and large bodies on the upload routes, up to
  `upload.max_size`.
- Keep the private port on the internal network. If an operator reaches it
  with a browser, put it behind authentication and set
  `server.private_hosts`.

## Probes

- **Liveness and readiness:** `GET /healthz` on either port answers `ok`.
  Worker-only processes answer it on the private address.
- **Startup:** a worker that cannot run `vips` or a helper exits before
  serving, with the failing command in the log.
- **Shutdown:** `SIGTERM` or `SIGINT` stops accepting requests and lets
  in-flight ones finish; workers finish the job they hold and claim no new
  one. A job cut short by a crash or a kill is claimed again once its lease,
  `jobs.lease_timeout`, lapses.

## Logs

Logs go to standard output, filtered by `RUST_LOG` (`info` by default).
The HTTP client crates are capped at `info` whatever the level, since they
log request targets, and fetched URLs are presigned secrets; naming one of
them explicitly in `RUST_LOG` lifts the cap. Public request spans record the
path with the token redacted.
