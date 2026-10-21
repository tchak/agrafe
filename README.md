# agrafe

[![CI](https://github.com/tchak/agrafe/actions/workflows/ci.yml/badge.svg)](https://github.com/tchak/agrafe/actions/workflows/ci.yml)
[![Docs](https://github.com/tchak/agrafe/actions/workflows/docs.yml/badge.svg)](https://tchak.github.io/agrafe/)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)
[![Rust 1.94+](https://img.shields.io/badge/rust-1.94%2B-orange.svg)](https://www.rust-lang.org)
[![Edition 2024](https://img.shields.io/badge/edition-2024-orange.svg)](https://doc.rust-lang.org/edition-guide/rust-2024/)

A file storage service: encrypted at rest, virus scanned, with WebP previews
and thumbnails, and server-side generation of spreadsheets, ZIP archives and
PDF documents. Rust, Postgres, and an object store: S3-compatible, Azure
Blob Storage or Google Cloud Storage.

agrafe sits between your backend and a bucket. Your backend creates a blob,
hands the browser a signed upload URL, and gets back signed URLs for the
original, a preview and a thumbnail. agrafe streams the bytes through
hashing, type sniffing, ClamAV and encryption on the way in, renders the
derived images in sandboxed workers, and never keeps a whole file in memory.

It runs as a standalone service (the `agrafe` binary), or inside another
[axum](https://github.com/tokio-rs/axum) application.

**Documentation: <https://tchak.github.io/agrafe/>**, a guide and a
reference for the configuration, both HTTP APIs and the errors. See
[DESIGN.md](DESIGN.md) for the architecture and the reasoning behind every
decision.

## Features

- **Two interfaces**: a private port for your backend, on a trusted
  network, and a public port for browsers where every URL is signed and
  expires, with CORS for the origins you allow.
- **Streaming uploads**, from a backend or straight from a browser, with
  checksum, size and content type checked on the way in, or
  [fetched from a URL](https://tchak.github.io/agrafe/guide/fetch.html).
- **Envelope encryption at rest** and **ClamAV scanning**: nothing is
  rendered or served before it is clean.
- **Previews and thumbnails** for images, PDFs, text, maps and, with an
  optional LibreOffice, office files; an icon for everything else. No user
  file is decoded in the server process: every renderer runs
  [sandboxed](https://tchak.github.io/agrafe/guide/security.html).
- **Attachments**: a blob lives as long as an owner in your backend uses
  it, and expires on its own otherwise.
- **Generated files**: spreadsheets (CSV, XLSX, ODS), ZIP archives and
  [typst](https://typst.app) PDF documents, built by jobs.
- **Webhooks** when a blob is ready or failed, signed the Standard
  Webhooks way.
- **Operations**: a Postgres job queue with per-kind workers, Prometheus
  metrics, tracing, a bucket audit and an optional HTML dashboard.

## Quick start

You need Postgres, [libvips](https://www.libvips.org/) with its `vips`
command on the `PATH`, and ClamAV's `clamd` (`brew install vips clamav`, or
`apt install libvips-tools clamav-daemon`).

```sh
createdb agrafe_dev
cp config/example.toml config/agrafe.toml
# set database.url, and replace the two keys with: openssl rand -base64 32

cargo build --workspace --release
export PATH="$PWD/target/release:$PATH"   # the helper binaries live here

agrafe --config config/agrafe.toml migrate
agrafe --config config/agrafe.toml serve
```

The private interface listens on `127.0.0.1:8080`, the public one on
`127.0.0.1:8081`. Or run a release's Docker image, `ghcr.io/tchak/agrafe`;
see [Deployment](https://tchak.github.io/agrafe/guide/deployment.html),
which also covers the libvips version to run in production.

Upload a file from the backend, then get its signed URLs:

```sh
FILE=report.pdf
ID=$(curl -s localhost:8080/blobs \
  -H 'content-type: application/json' \
  -d "{\"filename\": \"$FILE\", \"content_type\": \"application/pdf\",
       \"input_type\": \"raw\",
       \"checksum\": \"$(openssl dgst -sha256 -binary "$FILE" | base64)\",
       \"size\": $(wc -c < "$FILE" | tr -d ' ')}" | jq -r .id)

curl -s -X PUT "localhost:8080/blobs/$ID" --data-binary "@$FILE" | jq .state
curl -s "localhost:8080/blobs/$ID" | jq .urls
```

A blob expires a day after its creation unless something uses it; attach
it to its owner to keep it:

```sh
curl -s "localhost:8080/blobs/$ID/attachments" \
  -H 'content-type: application/json' \
  -d '{"owner_kind": "expense", "owner_id": "42", "role": "receipt"}'
```

The [quick start](https://tchak.github.io/agrafe/guide/quick-start.html)
walks through this in full, and the guide goes on to browser uploads,
spreadsheets, archives, documents and webhooks.

## Crates

`agrafe` is the service as a library, with a Rust private API;
`agrafe-axum` provides its two HTTP interfaces as routers to nest in your
application, and `agrafe-dashboard` the HTML dashboard; `agrafe-server`
is the `agrafe` binary. The rest are its building blocks and the sandboxed
helper binaries a deployment ships alongside it. See
[Embedding](https://tchak.github.io/agrafe/guide/embedding.html) and
[DESIGN.md](DESIGN.md#crates).

## Development

```sh
createdb agrafe_test
cargo nextest run --workspace
cargo clippy --all-targets
cargo fmt
```

Tests connect as the current OS user to `agrafe_test` (set `DATABASE_URL`
to override), with `CREATEDB`: each integration test creates a database of
its own. They need `vips` on the `PATH`; ClamAV is faked. The office
thumbnail tests skip without `soffice`. Test fixtures are built by
`fixtures/make.py`; regenerate rather than edit them.

Releases are cut with `cargo xtask release <version>`; see
[DESIGN.md](DESIGN.md#releases).

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or
  <https://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or
  <https://opensource.org/licenses/MIT>)

at your option.

The DejaVu Sans Mono font embedded in `agrafe-render-text` is distributed
under its own license, in
[crates/agrafe-render-text/assets/fonts/LICENSE](crates/agrafe-render-text/assets/fonts/LICENSE).

### Contribution

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in the work by you, as defined in the Apache-2.0
license, shall be dual licensed as above, without any additional terms or
conditions.
