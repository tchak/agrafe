# agrafe

[![crates.io](https://img.shields.io/crates/v/agrafe.svg)](https://crates.io/crates/agrafe)
[![docs.rs](https://img.shields.io/docsrs/agrafe)](https://docs.rs/agrafe)
[![CI](https://github.com/tchak/agrafe/actions/workflows/ci.yml/badge.svg)](https://github.com/tchak/agrafe/actions/workflows/ci.yml)
[![Docs](https://github.com/tchak/agrafe/actions/workflows/docs.yml/badge.svg)](https://tchak.github.io/agrafe/)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)
[![Rust 1.94+](https://img.shields.io/badge/rust-1.94%2B-orange.svg)](https://www.rust-lang.org)
[![Edition 2024](https://img.shields.io/badge/edition-2024-orange.svg)](https://doc.rust-lang.org/edition-guide/rust-2024/)

A file storage service: encrypted at rest, virus scanned, with previews and
thumbnails (WebP, or JPEG XL), and server-side generation of spreadsheets, ZIP archives and
PDF, DOCX and ODT documents. Rust, Postgres, and an object store: S3-compatible, Azure
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
  it, and expires on its own otherwise; `replace` swaps an owner's file
  in one request.
- **Generated files**: spreadsheets (CSV, XLSX, ODS), ZIP archives and
  [typst](https://typst.app) documents, as PDF, or, with an optional
  pandoc, as DOCX and ODT from the same template, built by jobs.
- **Webhooks** when a blob is ready or failed, signed the Standard
  Webhooks way.
- **Operations**: a Postgres job queue with per-kind workers, Prometheus
  metrics, tracing with optional Sentry and OpenTelemetry exporters that keep
  signatures and filenames in the process, a bucket audit and an optional HTML
  dashboard.

## Install

Every release is published to [crates.io](https://crates.io/crates/agrafe)
and as Docker images, `ghcr.io/tchak/agrafe`. A deployment runs the
`agrafe` binary, from the `agrafe-server` crate, with its four sandboxed
helpers next to it, all from the same release:

```sh
cargo install --locked agrafe-server agrafe-extract agrafe-render-text \
  agrafe-render-document agrafe-render-geo
```

```sh
docker pull ghcr.io/tchak/agrafe:latest          # or :latest-office, with LibreOffice and pandoc
```

To host agrafe inside an axum application instead, add the libraries:
`cargo add agrafe agrafe-axum` (and `agrafe-dashboard` for the dashboard);
see [Embedding](https://tchak.github.io/agrafe/guide/embedding.html).
Before 1.0, a minor version may break the configuration, the HTTP APIs or
the Rust API, and the [changelog](CHANGELOG.md) says how.

## Quick start

You need Postgres, [libvips](https://www.libvips.org/) with its `vips`
command on the `PATH`, and ClamAV's `clamd` (`brew install vips clamav`, or
`apt install libvips-tools clamav-daemon`). With the binaries installed as
above:

```sh
createdb agrafe_dev
curl -fsSL -o agrafe.toml \
  "https://raw.githubusercontent.com/tchak/agrafe/v$(agrafe -V | cut -d' ' -f2)/config/example.toml"
# set database.url, and replace the two keys with: openssl rand -base64 32

agrafe --config agrafe.toml migrate
agrafe --config agrafe.toml serve
```

The private interface listens on `127.0.0.1:8080`, the public one on
`127.0.0.1:8081`. To run a release's Docker image instead, see
[Deployment](https://tchak.github.io/agrafe/guide/deployment.html),
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
git clone https://github.com/tchak/agrafe.git && cd agrafe
createdb agrafe_test
cargo nextest run --workspace
cargo clippy --all-targets
cargo fmt
```

Tests connect as the current OS user to `agrafe_test` (set `DATABASE_URL`
to override), with `CREATEDB`: each integration test creates a database of
its own. They need `vips` on the `PATH`; ClamAV is faked. The office
thumbnail tests skip without `soffice`. Test fixtures are built by
`fixtures/make.py`; regenerate rather than edit them. To run a build of
the checkout, `cargo build --workspace --release` and put `target/release`
on the `PATH`: the server finds the helpers there.

A schema change is a new migration under `crates/agrafe/migrations/`:
a migration that shipped in a release is never edited, and CI refuses a
change to one; see [DESIGN.md](DESIGN.md#migrations).

Releases go through a pull request like any change: `cargo xtask release
<version>` opens it, and once it is merged `cargo xtask tag <version>`
tags the merged commit, which starts the release workflow; see
[DESIGN.md](DESIGN.md#releases).

## Security

Report a vulnerability privately, through GitHub's [private vulnerability
reporting](https://github.com/tchak/agrafe/security/advisories/new); see
[SECURITY.md](SECURITY.md) for what is in scope and which versions receive
fixes.

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
