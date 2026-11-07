# agrafe-axum

[![crates.io](https://img.shields.io/crates/v/agrafe-axum.svg)](https://crates.io/crates/agrafe-axum)
[![docs.rs](https://img.shields.io/docsrs/agrafe-axum)](https://docs.rs/agrafe-axum)

The HTTP interfaces of [agrafe](https://github.com/tchak/agrafe), the file
storage service, as [axum](https://github.com/tokio-rs/axum) routers, to
serve or to nest in another application:

- `public_router`, for browsers: every route verifies its signature before
  touching the database;
- `private_router`, for the backend that owns the files, unsigned and
  trusted, on an internal listener;
- `ops_router`, the health check and metrics alone, for a process that runs
  workers only.

Each router comes with its state applied, an `agrafe::Agrafe` handle, so
nesting needs no state plumbing. Every failure answers as an `ApiError`, a
JSON body with a stable `code`.

## Usage

```sh
cargo add agrafe agrafe-axum
```

```rust,ignore
let app = axum::Router::new()
    .nest("/files", agrafe_axum::public_router(agrafe.clone()))
    .merge(my_routes);

let internal = axum::Router::new()
    .nest("/agrafe", agrafe_axum::private_router(agrafe.clone()));
```

Set `server.public_base_url` to the URL the public router is reached
under, here `https://app.example/files`, so that the URLs agrafe mints
point at it. Every agrafe crate is released together, at one version, and they depend
on each other at it: keep them on the same version and upgrade them
together. Before 1.0 a minor version may break the Rust API, and the
[changelog](https://github.com/tchak/agrafe/blob/main/CHANGELOG.md) says
how.

See [Embedding in an axum application](https://tchak.github.io/agrafe/guide/embedding.html),
the [public](https://tchak.github.io/agrafe/reference/public-api.html) and
[private](https://tchak.github.io/agrafe/reference/private-api.html) HTTP
APIs and the [errors](https://tchak.github.io/agrafe/reference/errors.html).

## License

Licensed under either of the [Apache License, Version 2.0](https://github.com/tchak/agrafe/blob/main/LICENSE-APACHE)
or the [MIT license](https://github.com/tchak/agrafe/blob/main/LICENSE-MIT),
at your option.
