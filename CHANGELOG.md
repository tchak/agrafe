# Changelog

Every crate of the workspace is released together, at one version. The
format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and the versions [Semantic Versioning](https://semver.org/spec/v2.0.0.html):
before 1.0, a minor version may break the configuration, the HTTP APIs or
the Rust API, and says how in its entry.

A change that a user, an operator or an embedding application would notice
gets a line under `Unreleased` in the same commit, in one of `Added`,
`Changed`, `Deprecated`, `Removed`, `Fixed` or `Security`.
`cargo xtask release` turns that section into the release's, and its text
becomes the GitHub release's notes.

## [Unreleased]

The first release.

### Added

- **Two interfaces**: a private port for the backend that owns the files
  (blobs, uploads, attachments, signed URL minting, jobs, metrics) and a
  public port for browsers on which every URL carries an HMAC signature,
  with key rotation, rounded expiries and a ceiling on their lifetime.
- **CORS** on the public port, `[server.cors]`: the origins allowed to
  upload and read from a page, or any, never with credentials. Off by
  default.
- **Uploads** streamed in one pass through size and checksum checks,
  SHA-256, content sniffing, ClamAV and encryption into a multipart
  upload; raw or multipart bodies, write-once blobs, accepted types,
  expiry, and `url` blobs that a job fetches from a presigned URL.
- **Attachments**: a blob lives as long as an owner in the backend uses
  it, with tombstones and no purge of a blob still in use.
- **Encryption at rest**: envelope encryption with streaming AEAD (Tink's
  AES-GCM-HKDF-STREAMING) and wrapped data keys, KEK rotation, and
  `Range` reads of the plaintext.
- **Previews and thumbnails** in WebP, rendered by `vips` and four helper
  binaries in a sandbox of Landlock, seccomp and resource limits, or
  bubblewrap where the host grants it: images, PDF first pages, embedded
  images, text, maps of GeoJSON, GPX, KML and KMZ files, and, with
  LibreOffice, office documents; category icons for the rest.
- **Generated files**: spreadsheets, ZIP archives and PDF documents
  compiled from typst, built by jobs from parts sent in pieces.
- **Operations**: a Postgres job queue with per-kind worker pools, split
  API and worker roles, the audit of objects against the database, a
  signed webhook, Prometheus metrics, request-to-job tracing, the jobs
  API, the HTML dashboard and the reaper.
- **Storage** on the local disk, S3-compatible stores, Azure Blob Storage
  and Google Cloud Storage, objects keyed `<shard>/<blob id>/<name>` with
  a random two-digit shard so stores that split a bucket by key range
  spread the writes; one TOML file, overridable from the environment.
- **Embedding**: the service as a library (`agrafe`) with a Rust private
  API, and its HTTP interfaces as axum routers (`agrafe-axum`,
  `agrafe-dashboard`).
- **Docker images** on the GitHub Container Registry, with and without
  LibreOffice.

[Unreleased]: https://github.com/tchak/agrafe/commits/main
