# agrafe

File storage service: encrypted at rest, virus scanned, with WebP previews and
thumbnails. Rust, Postgres, any S3-compatible object store.

See [DESIGN.md](DESIGN.md) for the architecture and the decisions behind it.

## Running locally

Thumbnails and previews are rendered by [libvips](https://www.libvips.org/)
8.15 or newer, whose `vips` command must be on the `PATH` (`brew install
vips`, or `libvips-tools` on Debian trixie and later; bookworm's 8.14 is too
old). Virus scanning needs a reachable `clamd`. Embedded images and text
thumbnails come from two helper binaries built by this workspace,
`agrafe-extract` and `agrafe-render-text`, which the workers look up on the
`PATH` (or at `media.extract_command` and `media.render_text_command`).

```sh
createdb agrafe_dev
cp config/example.toml config/agrafe.toml   # then set database.url and real keys
cargo build --workspace                     # the helpers land in target/debug
cargo run -- --config config/agrafe.toml migrate
PATH="$PWD/target/debug:$PATH" cargo run -- --config config/agrafe.toml serve
```

Generate keys with `openssl rand -base64 32`.

## Tests

```sh
createdb agrafe_test
cargo test
```

Tests connect as the current OS user to `agrafe_test`; set `DATABASE_URL` to
override. They need `vips` on the `PATH` as well, and run the helper
binaries from `target/`, building them if a `cargo test -p agrafe` has not.
The files under `fixtures/` and the renderer probe files under
`crates/agrafe-render/assets/probe` are built by `fixtures/make.py` (needs
Pillow), the placeholder icons by `scripts/icons.sh`; all outputs are
committed.

Coverage needs [cargo-llvm-cov](https://github.com/taiki-e/cargo-llvm-cov)
(`cargo install cargo-llvm-cov`):

```sh
cargo coverage                        # summary per file
cargo coverage --show-missing-lines   # plus the uncovered lines
cargo coverage --html --open          # browsable report
```
