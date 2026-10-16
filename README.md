# agrafe

File storage service: encrypted at rest, virus scanned, with WebP previews and
thumbnails. Rust, Postgres, any S3-compatible object store.

See [DESIGN.md](DESIGN.md) for the architecture and the decisions behind it.

## Running locally

```sh
createdb agrafe_dev
cp config/example.toml config/agrafe.toml   # then set database.url and real keys
cargo run -- --config config/agrafe.toml migrate
cargo run -- --config config/agrafe.toml serve
```

Generate keys with `openssl rand -base64 32`.

## Tests

```sh
createdb agrafe_test
cargo test
```

Tests connect as the current OS user to `agrafe_test`; set `DATABASE_URL` to
override.
