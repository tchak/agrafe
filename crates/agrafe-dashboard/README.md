# agrafe-dashboard

[![crates.io](https://img.shields.io/crates/v/agrafe-dashboard.svg)](https://crates.io/crates/agrafe-dashboard)
[![docs.rs](https://img.shields.io/docsrs/agrafe-dashboard?label=docs.rs)](https://docs.rs/agrafe-dashboard)

The HTML dashboard of [agrafe](https://github.com/tchak/agrafe), the file
storage service, as an [axum](https://github.com/tokio-rs/axum) router: the
job queue, the blobs and the attachments, for operators. Read-only but for
the retry and discard actions, which the configuration enables.
Compile-time templates and one vendored stylesheet, no JavaScript: the
binary carries everything it serves.

## Usage

```sh
cargo add agrafe agrafe-axum agrafe-dashboard
```

```rust,ignore
let internal = axum::Router::new()
    .nest("/agrafe", agrafe_axum::private_router(agrafe.clone()))
    .nest("/agrafe/dashboard", agrafe_dashboard::dashboard_router(agrafe.clone()));
```

The dashboard embeds thumbnails from the private API one segment up, so
`/agrafe/dashboard` expects the private routes under `/agrafe`. It has no
authentication of its own: put it behind yours, on the internal listener,
and wrap it in `agrafe_axum::host::refuse_unknown_host` as the `agrafe`
binary does. Every agrafe crate is released together, at one version, and they depend
on each other at it: keep them on the same version and upgrade them
together. Before 1.0 a minor version may break the Rust API, and the
[changelog](https://github.com/tchak/agrafe/blob/main/CHANGELOG.md) says
how.

See [Embedding in an axum application](https://tchak.github.io/agrafe/guide/embedding.html)
and [Monitoring](https://tchak.github.io/agrafe/guide/monitoring.html).

## License

Licensed under either of the [Apache License, Version 2.0](https://github.com/tchak/agrafe/blob/main/LICENSE-APACHE)
or the [MIT license](https://github.com/tchak/agrafe/blob/main/LICENSE-MIT),
at your option.
