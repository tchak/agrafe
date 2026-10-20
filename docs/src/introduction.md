# Introduction

**agrafe** is a file storage service. It sits between your backend and an
object store, and takes care of everything that happens to a file between
the moment a user picks it in a form and the moment someone downloads it
again:

- it **streams** the bytes through size checks, SHA-256 and MD5 hashing,
  content-type sniffing and a ClamAV scan, without ever holding a whole file
  in memory;
- it **encrypts** every object at rest with a key per file, wrapped by keys
  only your deployment holds, so the storage provider cannot read anything;
- it renders a **WebP preview and thumbnail** in sandboxed subprocesses, so
  no user file is ever decoded inside the server;
- it serves everything through **signed, expiring URLs**, so a browser never
  needs credentials and a leaked link dies on its own;
- it **generates files** server side: CSV, XLSX and ODS spreadsheets from
  rows, ZIP archives from existing files, and PDF documents compiled from
  [typst](https://typst.app) templates.

It is written in Rust, keeps its metadata and its job queue in Postgres, and
stores bytes on the local filesystem, in any S3-compatible bucket, or on
Azure Blob Storage or Google Cloud Storage.

## Who it is for

agrafe is built for a backend that owns files on behalf of its users: a
public service collecting supporting documents, an application exporting
reports, a platform migrating away from Active Storage. The backend keeps
its own records and talks to agrafe over a private, trusted network. Users'
browsers talk to agrafe directly, but only through URLs the backend handed
them.

```text
            ┌──────────────────────────── trusted network ───────────────┐
 browser    │                                                             │
   │        │   backend ──── private port (:8080) ────┐                   │
   │        │      ▲                                   ▼                   │
   │        │      └──── webhook (ready, failed…) ── agrafe ── Postgres    │
   │        │                                          │                   │
   └── public port (:8081), signed URLs only ──────────┤                   │
            │                                          ▼                   │
            │                                    object store (S3)         │
            └─────────────────────────────────────────────────────────────┘
```

## Two interfaces

agrafe listens on two ports.

- The **private port** is for your backend. It has no authentication and must
  only be reachable from a trusted network. Everything happens here: creating
  blobs, uploading, attaching them to records, minting URLs, inspecting jobs,
  scraping metrics.
- The **public port** is for browsers. Every route carries an HMAC signature
  bound to the HTTP method, the path and the query string. A blob that was
  deleted, has expired or was found infected answers `404` there, even with a
  valid signature.

## What a typical flow looks like

1. A user opens a form. The backend calls `POST /blobs` on the private port
   with the file's name, type, size and SHA-256, and gets back a blob id and
   a signed upload URL.
2. The browser `PUT`s the file to that URL. agrafe scans it, encrypts it and
   stores it. The blob becomes `ready`, and a render job produces the
   thumbnail.
3. The user submits the form. The backend attaches the blob to its record,
   which keeps the file for as long as the attachment exists.
4. Later, the backend asks for download URLs, or mints them itself with the
   shared signing key, and puts them in its pages.

Files nobody attaches expire on their own after a day, so abandoned uploads
clean themselves up.

## How this book is organised

- The **Guide** starts with a [quick start](guide/quick-start.md), explains
  the [concepts](guide/concepts.md), then walks through every feature in its
  own chapter: uploads, fetches, attachments, serving, representations,
  [minting signed URLs from another project](guide/signed-urls.md),
  generated files, webhooks, jobs, security, deployment, embedding and
  [monitoring](guide/monitoring.md).
- The **Reference** lists every [configuration key](reference/configuration.md),
  the [command line](reference/cli.md), every route of the
  [public](reference/public-api.md) and [private](reference/private-api.md)
  HTTP APIs, and every [error](reference/errors.md).

The reasoning behind each decision lives in
[`DESIGN.md`](https://github.com/tchak/agrafe/blob/main/DESIGN.md) in the
repository. This book describes what agrafe does and how to use it; the
design document explains why.

## License

agrafe is licensed under either of the Apache License, Version 2.0 or the
MIT license, at your option.
