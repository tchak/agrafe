# agrafe

Rust file storage service. See `DESIGN.md` for architecture and decisions; read
it before changing behaviour, and update it when a decision changes.

## Version control: jj, not git

This repository is managed with **jj** (Jujutsu), colocated with git. Always use
`jj` commands, never `git` commands, for status, diff, log, describe, new,
squash, bookmarks and pushing. `jj st`, `jj diff`, `jj log`, `jj describe -m`,
`jj new`, `jj bookmark`.

## Layout

See "Crates" in `DESIGN.md` for the dependency graph.

- `crates/agrafe` — the library: domain, jobs, config, and the `Agrafe`
  handle whose methods are the private API. No axum anywhere in it.
- `crates/agrafe-axum` — the HTTP interfaces as routers, `ApiError`, and the
  integration tests under `tests/api`.
- `crates/agrafe-dashboard` — the HTML dashboard as a router: askama
  templates under `templates/`, the stylesheet under `assets/` (daisyUI
  components assembled by `scripts/daisyui.sh`; regenerate, never edit),
  its own integration tests under `tests/`. No `ApiError`: it renders
  `DashboardError` as a page.
- `crates/agrafe-server` — the clap entry point; its binary is named
  `agrafe`.
- `crates/agrafe-crypto` — streaming AEAD and key wrapping, reviewed in
  isolation; no dependency on the server crate.
- `crates/agrafe-types` — content types, classification, sniffing, the
  text-sample contract, `str_enum!`. Pure; shared by every other crate.
- `crates/agrafe-sandbox` — subprocesses with resource limits.
- `crates/agrafe-extract`, `crates/agrafe-render-text`,
  `crates/agrafe-render-document` — the sandboxed helpers, each a library
  plus a binary of the same name. The server does not link them; it finds
  the binaries on `PATH`. The last one embeds the typst compiler.
- `crates/agrafe-render` — what renders and how: `vips`, the helper
  drivers, the icons, the probe, the `[media]` tool config.
- `fixtures/` — sample files for tests, with the `make.py` that builds them.
- `migrations/` — sqlx migrations, plain SQL, one file per table. Nothing
  is deployed yet, so a schema change edits the table's file; no `ALTER`
  migrations and no backfills until the first deployment. A changed file
  needs a fresh database (`DROP SCHEMA public CASCADE; CREATE SCHEMA
  public;`), since sqlx checksums applied migrations.
- `config/example.toml` — documented configuration reference.
- `docs/` — the user documentation, an mdBook (`mdbook build docs`,
  `mdbook serve docs`), deployed to GitHub Pages by
  `.github/workflows/docs.yml` on push to `main`. Update it with the
  README when a feature lands or a route, key or error changes; the
  configuration reference includes `config/example.toml` verbatim.

## Local development

- Copy `config/example.toml` to `config/agrafe.toml` (gitignored), set real
  keys, and point `database.url` at `agrafe_dev`. Ports 8080/8081 are taken on
  this machine; the local config uses 18080/18081.
- `cargo run --bin agrafe -- --config config/agrafe.toml migrate`, then
  `cargo run --bin agrafe -- --config config/agrafe.toml serve` (the
  workspace has several binaries, so `--bin` is needed). The workers need the
  helper binaries: `cargo build --workspace` and point
  `media.extract_command`, `media.render_text_command` and
  `media.render_document_command` at the binaries under `target/debug`, or
  put `target/debug` on `PATH`.
- Tests need a `agrafe_test` database reachable as the current OS user
  with `CREATEDB`, or `DATABASE_URL`, and `vips` on the `PATH` (`brew
  install vips`). Run `cargo nextest run --workspace`, `cargo clippy
  --all-targets`, `cargo fmt` before committing. nextest kills a test
  still running after a minute (`.config/nextest.toml`); `cargo test`
  works too. The integration tests live in `agrafe-axum` and run the
  helper binaries from `target/`; a workspace run builds them, `-p
  agrafe-axum` builds them on demand.
- Test fixtures and the renderer probe files in
  `crates/agrafe-render/assets/probe` come from `fixtures/make.py`
  (Pillow), the thumbnail icons from `scripts/icons.sh`; regenerate rather
  than edit the outputs. The probe's typst workspace under
  `probe/document` is hand-written text and is edited directly.
- `cargo coverage` (an alias in `.cargo/config.toml`, needs cargo-llvm-cov)
  reports line coverage; add `--show-missing-lines` to see what is uncovered.

## Conventions

- Modules use named files, never `mod.rs`: `src/blob.rs` declares the module
  and its children live in `src/blob/`.
- Queries use `sqlx::query_as` with `FromRow` (no compile-time macros) so the
  build does not need a database.
- Blob state transitions are conditional `UPDATE ... WHERE state = $from`.
  Never set `state` unconditionally.
- Public routes verify the signature before touching the database.
- Nothing decodes or renders user files in-process: `vips` and the
  `agrafe-extract` and `agrafe-render-text` binaries run through
  `agrafe_sandbox`. The parsers (`lopdf`, `zip`, `quick-xml`, `ab_glyph`)
  are dependencies of the helper crates only; never add them to `agrafe`
  or `agrafe-render`.
- Errors are `thiserror` enums, one per module that fails in its own way
  (`ConfigError`, `ReadError`, `JobError`, `UploadError`, …). No `anyhow`.
  Messages inline their cause (`"reading the object: {0}"`), so log them
  with `%err`. `ApiError::internal(e)` is the only type-erased boundary.
- HTTP stays in `agrafe-axum` and `agrafe-dashboard`. The library never
  names a status code; a new operation is a method on `Agrafe` returning
  its own error enum, and the mapping to `ApiError` goes in
  `crates/agrafe-axum/src/error.rs` (to `DashboardError` in
  `crates/agrafe-dashboard/src/error.rs`). Handlers parse the request,
  call the handle and shape the response, nothing more.
- `blobs.updated_at` is set by a trigger (`migrations/0001_blobs.sql`). Do not set it in
  queries.
- Database tests are `#[sqlx::test(migrator = "MIGRATOR")]` taking a
  `PgPool`, handed to `setup`: each gets a fresh, migrated database of its
  own on the server `DATABASE_URL` names (default in `.cargo/config.toml`:
  `agrafe_test` as the current OS user), so they run in parallel. Never
  share state between tests through the database. The database is named
  after the test: two test runs against one server at once (another
  session, another workspace) fail each other, so run one at a time.
