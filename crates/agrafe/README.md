# agrafe

[![crates.io](https://img.shields.io/crates/v/agrafe.svg)](https://crates.io/crates/agrafe)
[![docs.rs](https://img.shields.io/docsrs/agrafe?label=docs.rs)](https://docs.rs/agrafe)

The library of [agrafe](https://github.com/tchak/agrafe), a file storage
service: encrypted at rest, virus scanned, with previews and thumbnails,
and server-side generation of spreadsheets, ZIP archives and documents.
Rust, Postgres, and an object store.

This crate is the service without its HTTP layer: the configuration, the
database and its migrations, the upload pipeline, the jobs, and the
`Agrafe` handle, whose methods are the private API in Rust. Use it to host
agrafe inside your own application; to run it standalone, install
[agrafe-server](https://crates.io/crates/agrafe-server), the `agrafe`
binary.

## Usage

```sh
cargo add agrafe agrafe-axum
```

```rust,ignore
use agrafe::{Agrafe, config::Config, db, jobs};

let config = Config::load(Some("agrafe.toml".as_ref()))?;
let pool = db::connect(&config.database).await?;
db::migrate(&pool, &config.database).await?;
let agrafe = Agrafe::new(config, pool)?;

// The private API, in Rust:
let blob = agrafe.create_blob(request).await?;
let urls = agrafe.mint_urls(blob.id, &options).await?;

// The workers that scan, render and build, here or in another process
// sharing the database:
tokio::spawn(jobs::runner::run(agrafe.clone(), shutdown));
```

[agrafe-axum](https://crates.io/crates/agrafe-axum) turns the same handle
into the HTTP routers, and
[agrafe-dashboard](https://crates.io/crates/agrafe-dashboard) into the HTML
dashboard. The workers run the sandboxed helper binaries, installed apart:

```sh
cargo install --locked agrafe-extract agrafe-render-text \
  agrafe-render-document agrafe-render-geo
```

The migrations are embedded: `db::migrate` applies those of the version
you build with, forward only, so upgrading the crate upgrades the schema.
Every agrafe crate is released together, at one version, and they depend
on each other at it: keep them on the same version and upgrade them
together. Before 1.0 a minor version may break the Rust API, and the
[changelog](https://github.com/tchak/agrafe/blob/main/CHANGELOG.md) says
how.

See [Embedding in an axum application](https://tchak.github.io/agrafe/guide/embedding.html)
and the [configuration reference](https://tchak.github.io/agrafe/reference/configuration.html).

## License

Licensed under either of the [Apache License, Version 2.0](https://github.com/tchak/agrafe/blob/main/LICENSE-APACHE)
or the [MIT license](https://github.com/tchak/agrafe/blob/main/LICENSE-MIT),
at your option.
