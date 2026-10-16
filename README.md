# agrafe

File storage service: encrypted at rest, virus scanned, with WebP previews and
thumbnails. Rust, Postgres, any S3-compatible object store.

See [DESIGN.md](DESIGN.md) for the architecture and the decisions behind it.

## Running locally

Thumbnails and previews are rendered by [libvips](https://www.libvips.org/),
whose `vips` command must be on the `PATH` (`brew install vips`, or
`libvips-tools` on Debian). Virus scanning needs a reachable `clamd`.

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
override. They need `vips` on the `PATH` as well. The files under
`crates/agrafe/tests/fixtures` are built by `make.py` there (needs Pillow),
the placeholder icons by `scripts/icons.sh`; both outputs are committed.

Coverage needs [cargo-llvm-cov](https://github.com/taiki-e/cargo-llvm-cov)
(`cargo install cargo-llvm-cov`):

```sh
cargo coverage                        # summary per file
cargo coverage --show-missing-lines   # plus the uncovered lines
cargo coverage --html --open          # browsable report
```
