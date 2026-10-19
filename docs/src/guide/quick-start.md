# Quick start

This chapter gets agrafe running on your machine, uploads a file and
downloads it back through a signed URL. It takes about ten minutes, most of
it compiling.

## Prerequisites

| Dependency | Why | Install |
|---|---|---|
| Rust 1.94 or newer | Building agrafe | [rustup.rs](https://rustup.rs) |
| Postgres | Metadata and the job queue | `brew install postgresql` / `apt install postgresql` |
| libvips 8.15 or newer, with its `vips` command | Rendering previews and thumbnails | `brew install vips` / `apt install libvips-tools` |
| ClamAV's `clamd` | Virus scanning | `brew install clamav` / `apt install clamav-daemon` |

ClamAV is optional for a first try. When `clamd` cannot be reached, uploads
still succeed and their scan is deferred to a job that retries until `clamd`
answers. Until then the blob stays `uploaded` and cannot be read, so you
will want it running before you download anything.

> **Production note.** libvips 8.15 is the minimum the renderer's options
> need, not what is safe to expose to untrusted uploads. In production run
> upstream libvips 8.18.3 or newer, or a distribution build that backports
> the security fixes. See [Deployment](deployment.md#libvips).

## Build

```sh
git clone https://github.com/tchak/agrafe.git
cd agrafe
cargo build --workspace --release
export PATH="$PWD/target/release:$PATH"
```

The workspace produces four binaries that ship together: `agrafe`, the
server, and three sandboxed helpers it runs as subprocesses,
`agrafe-extract`, `agrafe-render-text` and `agrafe-render-document`. The
server finds the helpers on the `PATH`.

## Configure

```sh
createdb agrafe_dev
cp config/example.toml config/agrafe.toml
```

Edit `config/agrafe.toml`:

- set `database.url`, for example `postgres://localhost/agrafe_dev`;
- replace the signing key `signing.keys.k1` and the key-encryption key
  `encryption.keks.kek1` with fresh random values:

```sh
openssl rand -base64 32   # once for signing.keys.k1
openssl rand -base64 32   # once for encryption.keks.kek1
```

Every key can also come from the environment, which is how secrets should
be passed in production:

```sh
export AGRAFE_DATABASE__URL=postgres://localhost/agrafe_dev
export AGRAFE_SIGNING__KEYS__K1=$(openssl rand -base64 32)
export AGRAFE_ENCRYPTION__KEKS__KEK1=$(openssl rand -base64 32)
```

## Run

```sh
agrafe --config config/agrafe.toml migrate
agrafe --config config/agrafe.toml serve
```

The private port listens on `127.0.0.1:8080` and the public one on
`127.0.0.1:8081`. Before the workers start, the renderer probes `vips` and
the helpers with sample files, and the process refuses to start if one of
them is missing or broken. Check that it is up:

```sh
curl -s localhost:8080/healthz   # ok
```

## Upload a file

The examples use `curl` and `jq` against the private port. First create the
blob with the file's checksum and size, then send the bytes.

```sh
FILE=report.pdf
CHECKSUM=$(openssl dgst -sha256 -binary "$FILE" | base64)
SIZE=$(wc -c < "$FILE" | tr -d ' ')

ID=$(curl -s localhost:8080/blobs \
  -H 'content-type: application/json' \
  -d "{\"filename\": \"$FILE\", \"content_type\": \"application/pdf\",
       \"input_type\": \"raw\", \"checksum\": \"$CHECKSUM\", \"size\": $SIZE}" \
  | jq -r .id)

curl -s -X PUT "localhost:8080/blobs/$ID" --data-binary "@$FILE" | jq .state
```

The `PUT` answers with the blob's metadata. Its `state` is `ready` when the
scan ran inline and found nothing, or `uploaded` while a deferred scan is
pending.

## Download it

The blob's metadata carries ready-made signed URLs on the public port:

```sh
curl -s "localhost:8080/blobs/$ID" | jq .urls
```

```json
{
  "upload": "http://127.0.0.1:8081/blobs/v1.k1.1792310400.3q2-7w…/0199f2…",
  "metadata": "http://127.0.0.1:8081/blobs/v1.k1.1792393200.Yk9…/0199f2…",
  "original": "http://127.0.0.1:8081/blobs/v1.k1.1792393200.Pp1…/0199f2…/original/report.pdf?e=AQAE…",
  "preview": "http://127.0.0.1:8081/blobs/v1.k1.1792393200.c2V…/0199f2…/preview/report.pdf?e=AQAE…",
  "thumbnail": "http://127.0.0.1:8081/blobs/v1.k1.1792393200.Zm9…/0199f2…/thumbnail/report.pdf?e=AQAE…",
  "expires_at": 1792393200
}
```

`upload` is still listed, but blobs are write-once: now that the bytes are
in, a `PUT` to it answers `409`. Open the `original` URL in a browser, or
fetch the thumbnail:

```sh
curl -s "$(curl -s localhost:8080/blobs/$ID | jq -r .urls.thumbnail)" -o thumb.webp
```

## Keep it

A blob nobody uses expires a day after its creation. To keep it, attach it
to a record of yours:

```sh
curl -s "localhost:8080/blobs/$ID/attachments" \
  -H 'content-type: application/json' \
  -d '{"record_type": "Dossier", "record_id": "42", "name": "piece_justificative"}'
```

## Watch it

Turn on the dashboard to see the queue, the blobs and the attachments in a
browser:

```toml
[dashboard]
enabled = true
```

Restart, then open <http://localhost:8080/dashboard>.

## Where to go next

- [Concepts](concepts.md) explains blobs, states and the two ports.
- [Uploading files](uploads.md) covers browser uploads and multipart forms.
- [Minting signed URLs from another project](signed-urls.md) shows how a
  backend builds download links without calling agrafe.
- [Deployment](deployment.md) covers S3, ClamAV limits and splitting the API
  from the workers.
