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
| `agrafe-render-geo` | Draws a GeoJSON, GPX, KML or KMZ file as SVG over the map extent it chose |

The helpers are found on the `PATH`, or at the paths given by
`media.extract_command`, `media.render_text_command`,
`media.render_document_command` and `media.render_geo_command`. Ship them
from the same build as the server: they read no configuration, write no
logs and never touch the network, and the startup probe fails if one of
them is not what the server expects.

Workers also need the `vips` command line tool, built with SVG support
(rsvg) for the maps, and, for office thumbnails, an optional
[LibreOffice](#libreoffice). Run `agrafe migrate` once per release,
before the new processes start.

Outbound connections: the fetch workers reach the URLs of `url` blobs, the
notify workers reach `webhook.url`, and the render workers reach the hosts
of `media.map.layers`, if any are configured. Nothing else in agrafe
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

Under confinement the subprocess's home and cache directory are its work
directory, deleted after the job, so fontconfig cannot keep a cache of
its own between jobs: it reads the system one in `/var/cache/fontconfig`
and, if that is missing or stale, rebuilds it on every PDF render, which
costs seconds a job and nothing reports. Build it in the image, after the
fonts are installed:

```dockerfile
RUN fc-cache -f
```

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

## LibreOffice

Optional, for the first-page thumbnails of office files: Word, Excel and
PowerPoint files, their Office 97-2003, macro-enabled and template
variants, RTF, and OpenDocument texts, spreadsheets, presentations and
drawings (see [Previews and
thumbnails](representations.md#what-gets-rendered)). Without it, office
files get the icon.

```toml
[media.office]
command = "soffice"
```

On Debian and Ubuntu:

```sh
apt install libreoffice-writer-nogui libreoffice-calc-nogui \
  libreoffice-impress-nogui libreoffice-draw-nogui fonts-dejavu-core fonts-liberation
```

About 400 MB. The fonts are not optional: without them a page renders as
boxes and LibreOffice exits 0, which nothing reports. The startup probe
converts a two-page DOCX, an XLSX, a two-slide PPTX, a two-page ODG and
an ODT carrying a macro, and checks that each two-page file came out as
one page and that the macro did not run, so a LibreOffice without one of
Writer, Calc, Impress or Draw, or older than 7.4, fails the deployment
rather than every office file of that kind.

Macros in a document never run during a conversion: LibreOffice loads
the file in never-execute mode, and besides, the user profile of every
run, `profile_template` or not, pins macro security to "very high" with
no trusted location and macro execution switched off, over whatever the
template says.

LibreOffice runs as one short-lived process per document, sandboxed like
`vips`, with a private user profile under the job's work directory, and
it is a far larger attack surface than anything else in the sandbox,
which is why it is off by default. Where it is on:

- it needs Landlock: a document names files to LibreOffice (linked
  sections, DDE fields, linked images), and Landlock is what keeps those
  inside the work directory rather than the configuration and the keys,
  which a document could lay out on its first page. A worker with the
  converter configured refuses to start when the sandbox has no Landlock,
  unless `media.sandbox.isolation` is `"off"`, which is you accepting
  that on a development machine;
- run it on a worker that handles `render` only (`jobs.kinds`), with no
  outbound network as defence in depth: LibreOffice follows linked images
  and OLE links in a document, and the sandbox refuses it every socket
  but unix ones, so a link to a host fails there first. The API image
  never needs it;
- LibreOffice cannot start without a unix socket for its single-instance
  pipe, so the sandbox lets it have unix sockets, and only those, where
  every other tool has none. A local Postgres listening on a unix socket
  with peer authentication for the worker's user would be reachable from
  it: connect to Postgres over TCP on that worker, or run it where there
  is no such socket;
- the pipe lives in `/tmp`, where the sandbox lets LibreOffice create and
  remove socket files: set `media.tmp_dir` to a directory outside `/tmp`,
  so that no other job's files are there. The default is the system
  temporary directory, `/tmp` itself, and the worker refuses to start
  with the converter configured until it is moved;
- `[media.office.sandbox]` gives it its own limits, 2 GiB of address space
  and 120 s by default; the rest of its sandbox is `[media.sandbox]`'s.
  Debian's `/etc/libreoffice` is readable to it; another layout that keeps
  LibreOffice's files outside `/usr` lists them in
  `media.sandbox.readable`.

A fresh user profile costs LibreOffice some startup time on every run. To
skip it, build a warmed profile into the image with a throwaway
conversion and point `profile_template` at it; it is copied into the work
directory per run, so every run still has a private profile:

```dockerfile
RUN echo warm > /tmp/warm.txt \
 && soffice -env:UserInstallation=file:///var/lib/agrafe/soffice-profile \
      --headless --norestore --nologo --convert-to pdf --outdir /tmp /tmp/warm.txt \
 && rm /tmp/warm.txt /tmp/warm.pdf
```

```toml
[media.office]
command = "soffice"
profile_template = "/var/lib/agrafe/soffice-profile"
```

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

Azure Blob Storage and Google Cloud Storage have backends of their own:

```toml
[storage]
backend = "azure"

[storage.azure]
account = "agrafe"
container = "agrafe"
# access_key from AGRAFE_STORAGE__AZURE__ACCESS_KEY, or a managed identity
```

```toml
[storage]
backend = "gcs"

[storage.gcs]
bucket = "agrafe"
# service_account_key from AGRAFE_STORAGE__GCS__SERVICE_ACCOUNT_KEY, or the
# application default credentials
```

Google Cloud Storage needs the same `AbortIncompleteMultipartUpload`
lifecycle rule. Azure needs none: it discards uncommitted blocks after
seven days.

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

Where the KEKs must not rest on the host, `encryption.keks_file` reads
them once at startup from a systemd credential or a FIFO fed by
`systemd-ask-password`; see [Keeping the KEKs off the
host](security.md#keeping-the-keks-off-the-host).

## Splitting the API and the workers

`server.role`, or `--role`, picks what a process runs:

| Role | Serves | Runs workers |
|---|---|---|
| `all` (default) | the interfaces `server.interfaces` lists, both by default | yes |
| `api` | the interfaces `server.interfaces` lists, both by default | no |
| `worker` | `/healthz` and `/metrics` on `server.private_addr` | yes |

```sh
agrafe --config agrafe.toml serve --role api
agrafe --config agrafe.toml serve --role worker
agrafe --config agrafe.toml serve --role worker --kinds archive
```

`--interfaces`, or `server.interfaces`, narrows the interfaces an API
process serves: `private` for the process the backend talks to, `public`
for the one behind the internet-facing proxy, so that the private API need
not exist on a host the internet reaches at all. Whatever it says, every
process binds `server.private_addr`: one that does not serve the private
API answers `/healthz` and `/metrics` there alone, and does not bind
`server.public_addr` unless it serves the public interface.

```sh
agrafe --config agrafe.toml serve --role api --interfaces public
agrafe --config agrafe.toml serve --role all --interfaces private
```

`--kinds`, or `jobs.kinds`, narrows the job kinds a worker runs. Dedicated
pools are how archive builds get machines of their own, or renders more of
them. Every kind must be handled somewhere. A worker that does not handle
`render` skips the renderer probe, and one that does not handle `reap` or `audit` does
not schedule the reaper or the audit.

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
  Every process answers it on the private address, worker-only and
  public-only ones included.
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
