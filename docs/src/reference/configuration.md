# Configuration

agrafe reads one TOML file, given by `--config` or `AGRAFE_CONFIG`, then
environment variables. Every key can be set from the environment: prefix it
with `AGRAFE_`, upper-case it, and join the levels with a double underscore.

| Key | Environment variable |
|---|---|
| `database.url` | `AGRAFE_DATABASE__URL` |
| `signing.keys.k1` | `AGRAFE_SIGNING__KEYS__K1` |
| `storage.s3.secret_access_key` | `AGRAFE_STORAGE__S3__SECRET_ACCESS_KEY` |
| `jobs.concurrency.archive` | `AGRAFE_JOBS__CONCURRENCY__ARCHIVE` |

The environment takes precedence over the file. Pass secrets through it in
production: the database URL, the signing keys, the KEKs, the bucket
credentials and the webhook secret are redacted from debug output, but a
file is still a file.

## Value formats

| Type | Format | Examples |
|---|---|---|
| Duration | A number and a unit, as [humantime](https://docs.rs/humantime) parses it | `"30s"`, `"10m"`, `"1h"`, `"7d"`, `"1h 30m"` |
| Size | A number and a unit, binary or decimal | `"256 KiB"`, `"64 MiB"`, `"5 GiB"`, `"10 MB"` |
| Key | Random bytes, standard base64 | the output of `openssl rand -base64 32` |
| Address | `host:port` | `"127.0.0.1:8080"`, `"0.0.0.0:8081"` |

Sections and keys marked **required** have no default. Everything else can
be left out.

## `[server]` (required)

| Key | Default | Description |
|---|---|---|
| `private_addr` | **required** | Address of the private interface. A worker-only process binds it too, for `/healthz` and `/metrics` |
| `public_addr` | **required** | Address of the public interface |
| `public_base_url` | **required** | Base of minted public URLs, as browsers reach the public port. No trailing slash |
| `role` | `"all"` | `"api"`, `"worker"` or `"all"`. `serve --role` takes precedence |
| `private_hosts` | `[]` | The `host:port` names the private interface is reached under. When set, any other `Host` is refused with `403`, except on `/healthz` and `/metrics`. Set it whenever a browser uses the port |

## `[database]` (required)

| Key | Default | Description |
|---|---|---|
| `url` | **required** | Postgres connection URL. A secret |
| `max_connections` | `10` | Pool size of this process |

## `[storage]` (required)

| Key | Default | Description |
|---|---|---|
| `backend` | **required** | `"local"`, `"s3"`, `"azure"` or `"gcs"` |

### `[storage.local]`

| Key | Default | Description |
|---|---|---|
| `path` | **required** with `local` | Directory holding the objects |

### `[storage.s3]`

| Key | Default | Description |
|---|---|---|
| `bucket` | **required** with `s3` | Bucket name |
| `region` | none | Region |
| `endpoint` | none | Endpoint URL, for S3-compatible stores |
| `access_key_id` | none | Access key |
| `secret_access_key` | none | Secret key. A secret |

The bucket needs an `AbortIncompleteMultipartUpload` lifecycle rule.

### `[storage.azure]`

| Key | Default | Description |
|---|---|---|
| `account` | **required** with `azure` | Storage account name |
| `container` | **required** with `azure` | Container name |
| `access_key` | none | Account key. A secret. Without it, the `AZURE_*` environment variables or a managed identity |
| `endpoint` | none | Endpoint URL, for Azurite or a sovereign cloud |

### `[storage.gcs]`

| Key | Default | Description |
|---|---|---|
| `bucket` | **required** with `gcs` | Bucket name |
| `service_account_key` | none | The JSON key of a service account, its content rather than a path. A secret. Without it, `GOOGLE_SERVICE_ACCOUNT`, the application default credentials or the metadata server |

The bucket needs an `AbortIncompleteMultipartUpload` lifecycle rule.

## `[signing]` (required)

| Key | Default | Description |
|---|---|---|
| `current_key` | **required** | Id of the key agrafe mints with. Must be in `keys` |
| `window` | `"1h"` | Minted expiries are rounded up to a multiple of this before the TTL is added |
| `keys` | **required** | Table of key id to key. Ids use `[A-Za-z0-9_-]`; keys are at least 32 bytes. Every key verifies |

### `[signing.ttl]`

| Key | Default | Description |
|---|---|---|
| `upload` | `"1h"` | Lifetime of upload URLs |
| `metadata` | `"24h"` | Lifetime of public metadata URLs |
| `read` | `"24h"` | Lifetime of original, preview and thumbnail URLs |
| `max` | `"7d"` | The longest any URL may live. Caps the three above, the `ttl` of a mint request, and the expiry of any token the public port accepts |

## `[upload]`

| Key | Default | Description |
|---|---|---|
| `max_size` | `"5 GiB"` | Largest upload, and largest source part |
| `inline_scan_max_size` | `"25 MiB"` | Files up to this size are scanned during the upload; larger ones by a job |
| `stale_after` | `"10m"` | An upload not touched for this long was left by a crashed process, and the reaper resets it. Running uploads touch their blob every third of it |
| `purge_delay` | `"1h"` | Grace period between a delete or an expiry and the purge of the objects |
| `abandoned_after` | `"1d"` | An unfinished generated source, or an attached `created` blob, untouched for this long is expired by the reaper |

## `[blobs]`

| Key | Default | Description |
|---|---|---|
| `default_expires_in` | `"1d"` | Expiry of a blob created without one, and of a blob after its last detach. At least one hour |
| `keep_purged` | `"90d"` | How long a terminal blob's row stays as a tombstone after its purge |
| `read_cache_ttl` | `"30s"` | How long the read routes keep a settled row in memory. `"0s"` disables the cache |
| `read_cache_entries` | `100000` | Rows kept at most, per process |

## `[archive]`

| Key | Default | Description |
|---|---|---|
| `max_size` | `"100 GiB"` | Largest generated archive, checked against the members' sizes as the manifest arrives. At most 309 GiB |

## `[document]`

| Key | Default | Description |
|---|---|---|
| `max_workspace_size` | `"256 MiB"` | Largest workspace, members plus inline files |
| `max_files` | `10000` | Most files in a workspace, across its parts |

## `[fetch]`

| Key | Default | Description |
|---|---|---|
| `connect_timeout` | `"10s"` | TCP connect and TLS handshake |
| `read_timeout` | `"30s"` | Longest wait for the next bytes of a response |
| `timeout` | `"6h"` | The whole response, as a backstop |
| `max_redirects` | `5` | Redirects followed, each checked like the first URL |
| `max_retry_after` | `"15m"` | A longer `Retry-After` makes the fetch permanent |
| `allow_private_networks` | `false` | Lift the `https` and address rules. Development and tests only |
| `user_agent` | `"agrafe/<version>"` | Sent with every fetch |

## `[encryption]`

| Key | Default | Description |
|---|---|---|
| `current_kek` | **required** | Id of the KEK that wraps new data keys. Must be in `keks` or in the `keks_file` |
| `chunk_size` | `"256 KiB"` | Segment size of the streaming encryption, at most 16 MiB. Stored per blob, so it can change |
| `keks` | | Table of KEK id to key, each exactly 32 bytes. Keep every KEK that still wraps a key |
| `keks_file` | | A file of `id = "…"` lines, the body of `[encryption.keks]`, read once at startup. May be a FIFO or a systemd credential, so the KEKs stay off the disk; see [Keeping the KEKs off the host](../guide/security.md#keeping-the-keks-off-the-host). An id in both places is an error |

One of `keks` and `keks_file` must hold `current_kek`.

## `[clamav]`

| Key | Default | Description |
|---|---|---|
| `addr` | `"127.0.0.1:3310"` | `clamd`'s TCP socket |
| `timeout` | `"60s"` | Each write and the wait for the verdict, not a whole scan |
| `connect_timeout` | `"5s"` | The TCP connect; an upload waits this long at most before deferring its scan |
| `max_inline_scans` | `4` | Inline scans open at once; uploads beyond it are scanned by a job |

`clamd`'s `StreamMaxLength`, `MaxScanSize` and `MaxFileSize` must be above
`upload.max_size`, `AlertExceedsMax` must be `yes`, and `MaxThreads` must be
above `max_inline_scans` plus `jobs.concurrency.scan`.

## `[media]`

| Key | Default | Description |
|---|---|---|
| `variant_version` | `1` | Bump to regenerate every representation on its next request. At least 1 |
| `vips_command` | `"vips"` | The libvips command line tool |
| `extract_command` | `"agrafe-extract"` | The embedded image helper |
| `render_text_command` | `"agrafe-render-text"` | The text thumbnail helper |
| `render_document_command` | `"agrafe-render-document"` | The typst helper |
| `render_geo_command` | `"agrafe-render-geo"` | The GeoJSON map helper |
| `max_source_size` | `"64 MiB"` | Larger originals get the icon and no preview; text thumbnails still render |
| `tmp_dir` | the system temporary directory | Where workers unpack originals and document workspaces |

### `[media.sandbox]`

| Key | Default | Description |
|---|---|---|
| `timeout` | `"60s"` | Wall-clock limit per subprocess |
| `memory` | `"1 GiB"` | Address space limit, enforced on Linux only |
| `max_output_size` | `"256 MiB"` | Largest file a subprocess may write |
| `isolation` | `"auto"` | Landlock and seccomp around each subprocess: `auto` where the kernel can, `required` or the worker does not start, `off` |
| `readable` | `[]` | Extra read-only paths for the subprocesses, such as a fonts directory outside `/usr` |

### `[media.thumbnail]`

| Key | Default | Description |
|---|---|---|
| `width` | `256` | Box width in pixels |
| `height` | `256` | Box height in pixels |
| `quality` | `80` | WebP quality |

### `[media.preview]`

| Key | Default | Description |
|---|---|---|
| `max_dimension` | `2048` | Longest side in pixels |
| `quality` | `85` | WebP quality |

### `[media.embedded_image]`

| Key | Default | Description |
|---|---|---|
| `min_coverage` | `0.7` | Fraction of the page a single image must cover for a PDF to count as a wrapped image |
| `max_text_chars` | `50` | Most characters of text a wrapped image's document may have |

### `[media.geojson]`

How the map of a GeoJSON file is drawn; see [Maps](../guide/representations.md#maps).

| Key | Default | Description |
|---|---|---|
| `padding` | `0.1` | Added around the geometry, as a fraction of its larger side |
| `min_span` | `"200 m"` | The smallest extent drawn; metres, or a string with `m` or `km` |
| `max_size` | `2048` | The largest side of a background request, in pixels; the map is the preview box or this, whichever is smaller |
| `max_vertices` | `200000` | Vertices drawn at most over the whole file |
| `max_shapes` | `20000` | Shapes drawn at most over the whole file |
| `fetch_timeout` | `"10s"` | The whole of one layer request |
| `max_layer_size` | `"8 MiB"` | A larger layer response is a failure |

### `[media.geojson.style]`

The default style, in the simplestyle-spec vocabulary; a feature's own
properties override it key by key.

| Key | Default |
|---|---|
| `stroke` | `"#FFD400"` |
| `stroke-width` | `4` |
| `stroke-opacity` | `1` |
| `fill` | `"#FFD400"` |
| `fill-opacity` | `0.35` |
| `marker-color` | `"#FFD400"` |
| `marker-size` | `"medium"` |

### `[[media.geojson.layers]]`

Background layers, composited bottom to top. None by default.

| Key | Default | Description |
|---|---|---|
| `url` | required | A WMS `GetMap` URL template with `{bbox}` (`minx,miny,maxx,maxy` in EPSG:3857), `{width}` and `{height}` (pixels) |
| `attribution` | none | Burned into a corner of the map when this is the first drawn layer that has one |
| `min_span` | none | Drawn only on an extent at least this wide |
| `max_span` | none | Drawn only on an extent narrower than this |

## `[jobs]`

| Key | Default | Description |
|---|---|---|
| `poll_interval` | `"2s"` | Fallback polling when a `NOTIFY` wakeup is missed |
| `lease_timeout` | `"10m"` | A job locked for longer is claimed again. Running workers renew it |
| `reap_interval` | `"1m"` | How often the reaper runs |
| `keep_failed` | `"7d"` | How long dead jobs are kept |
| `kinds` | every kind | The kinds this process runs workers for. `serve --kinds` takes precedence |

### `[jobs.concurrency]`

Workers per kind, per process. An unknown kind is a configuration error.

| Key | Default |
|---|---|
| `fetch` | `2` |
| `scan` | `4` |
| `render` | `4` |
| `rows` | `2` |
| `archive` | `1` |
| `document` | `2` |
| `purge` | `4` |
| `reap` | `1` |
| `notify` | `4` |

## `[webhook]`

| Key | Default | Description |
|---|---|---|
| `url` | none | Where events are posted. No webhook, and nothing enqueued, when unset |
| `secret` | none, **required** with `url` | `whsec_` followed by base64. A secret |
| `timeout` | `"10s"` | The whole request; anything but a `2xx` within it fails the attempt |
| `events` | every event | Which of `ready`, `infected`, `failed` and `expired` to send |
| `allow_insecure` | `false` | Accept an `http` URL |

## `[dashboard]`

| Key | Default | Description |
|---|---|---|
| `enabled` | `false` | Serve the dashboard under `/dashboard` on the private port, on `api` and `all` processes |
| `actions` | `false` | Show the retry and discard buttons. Off, the action routes answer `404` |

## Validation at startup

The process refuses to start when the configuration is inconsistent, with a
message naming the key:

| Message | Fix |
|---|---|
| `server.public_base_url must not end with a slash` | Remove the trailing slash |
| `server.private_hosts: "…" is not a host and port, like agrafe.internal:8080` | List bare `host:port` names, no scheme or path |
| `` signing.current_key `…` is not present in signing.keys `` | Add the key or fix the id |
| `` signing key id `…` must be non-empty and use only [A-Za-z0-9_-] `` | Rename the key |
| `` signing key `…` must be at least 32 bytes `` | Generate it with `openssl rand -base64 32` |
| `signing.window must be positive`, `signing.ttl.max must be positive` | Set a non-zero duration |
| `signing.ttl.read (…) exceeds signing.ttl.max (…)` | Lower the TTL or raise the cap |
| `encryption.current_kek is required` | Set it |
| `` encryption.current_kek `…` is not present in encryption.keks or encryption.keks_file `` | Add the KEK or fix the id |
| `` encryption KEK `…` must be 32 bytes `` | Generate it with `openssl rand -base64 32` |
| `` encryption KEK `…` is given both in encryption.keks and in encryption.keks_file `` | Keep it in one place |
| `` encryption.keks_file `…`: No such file or directory `` | Fix the path; a relative one is from the working directory |
| `` encryption.keks_file `…` must be a TOML table of KEK id to base64 key, `id = "…"` lines: … at line … `` | Fix that line. The message names the line, not its content, which would be a key |
| `encryption.chunk_size must be at most 16777216 bytes (16 MiB)` | Lower it |
| `blobs.default_expires_in must be at least 1h: …` | Raise it |
| `media.variant_version must be at least 1` | Raise it |
| `media.geojson.layers[0].url must contain {bbox}` | Add the placeholder; `{width}` and `{height}` likewise |
| `media.geojson.layers[0].url must be an http or https URL` | Fix the URL |
| `archive.max_size must be at most … bytes: …` | Lower it to 309 GiB or less |
| `jobs.kinds must name at least one kind, or be omitted for all` | Remove the empty list |
| `webhook.secret is required with webhook.url` | Set the secret |
| `webhook.secret must be whsec_ followed by base64: …` | Generate it with `echo "whsec_$(openssl rand -base64 24)"` |
| `webhook.url must be https: the body carries secrets; …` | Use `https`, or set `allow_insecure` for development |
| `webhook.events must name at least one event, or be omitted for all` | Remove the empty list |
| `invalid configuration: …` | A missing required key, an unknown variant or a malformed value |

Workers also probe `vips` and the helpers before starting, and exit with
`the renderer failed its startup probe: …` when one of them does not work.

## Annotated example

The repository's `config/example.toml`, with every key and its default:

```toml
{{#include ../../../config/example.toml}}
```

