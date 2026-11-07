# agrafe

Rust file storage service. See `DESIGN.md` for architecture and decisions; read
it before changing behaviour, and update it when a decision changes.

## Version control: jj, not git

This repository is managed with **jj** (Jujutsu), colocated with git. Always use
`jj` commands, never `git` commands, for status, diff, log, describe, new,
squash, bookmarks and pushing. `jj st`, `jj diff`, `jj log`, `jj describe -m`,
`jj new`, `jj bookmark`.

## Scale: billions of blobs

agrafe is designed for billions of blobs (see "Scale" in `DESIGN.md`).
Design every feature, query, job and review for that size: no
unbounded scans, every query on an index, whole-table work as a
resumable batched job, and per-blob costs weighed at a billion times.
A test passing on a small database proves nothing about this.

## Layout

See "Crates" in `DESIGN.md` for the dependency graph. Every published
crate has a `README.md` of its own, its crates.io page, but
`agrafe-server`, whose page is the root README; a crate's README changes
with what it offers.

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
  `agrafe`. It alone holds the Sentry and OpenTelemetry exporters
  (`telemetry`), behind cargo features.
- `crates/agrafe-crypto` — streaming AEAD and key wrapping, reviewed in
  isolation; no dependency on the server crate.
- `crates/agrafe-types` — content types, classification, sniffing, the
  text-sample contract, `str_enum!`. Pure; shared by `agrafe`,
  `agrafe-render` and the helpers but the document one.
- `crates/agrafe-sandbox` — subprocesses with resource limits.
- `crates/agrafe-extract`, `crates/agrafe-render-text`,
  `crates/agrafe-render-document`, `crates/agrafe-render-geo` — the
  sandboxed helpers, each a library plus a binary of the same name. The
  server does not link them; it finds the binaries on `PATH`. The document
  one embeds the typst compiler; the geo one parses GeoJSON, GPX, KML,
  KMZ, GML and zipped shapefiles and writes SVG.
- `crates/agrafe-render` — what renders and how: `vips`, the helper
  drivers, the icons, the probe, the `[media]` tool config, and the
  fetcher of map backgrounds (`geo`), the one network client in it.
- `fixtures/` — sample files for tests, with the `make.py` that builds them.
- `crates/agrafe/migrations/` — sqlx migrations, plain SQL, inside the
  crate so that `cargo publish` packages them. Append-only since 0.1.0
  (see "Migrations" in `DESIGN.md`): a migration in a release tag is
  never edited, renamed or removed, and a schema change is a new file
  numbered after the last (`0005_<what>.sql`), `ALTER`s and all; CI
  fails on a change to a released file. One added since the newest tag
  may still be edited, which needs a fresh database (`DROP SCHEMA public
  CASCADE; CREATE SCHEMA public;`), since sqlx checksums applied
  migrations. No backfills yet: nothing is in production.
- `config/example.toml` — documented configuration reference.
- `config/alerts.yml` — the Prometheus rules the monitoring guide includes
  verbatim; `config/alerts.test.yml` tests them with `promtool test rules`,
  and `config/alerts.days.test.yml` the staleness alert, evaluated hourly
  since its tests span weeks.
- `Dockerfile`, `docker/` — the release images: a trixie build stage, a
  trixie runtime with or without LibreOffice (`VARIANT=office`), and the
  configuration file each variant gets.
- `xtask/` — the workspace's own tasks, `cargo xtask <task>`, never
  published: `release <version>` opens a release's pull request, `tag
  <version>` tags the commit it was merged as (see "Releases" in
  `DESIGN.md`), and that tag runs `.github/workflows/release.yml`, which
  takes the GitHub release's text from `notes <version>`. `mdbook` is
  the book's preprocessor, which mdbook runs (`docs/book.toml`).
- `docs/` — the user documentation, an mdBook (`mdbook build docs`,
  `mdbook serve docs`), deployed to GitHub Pages once per release:
  `release.yml` calls `.github/workflows/docs.yml` on the tag, after the
  GitHub release, and not for a pre-release; a pull request only builds
  it. The published book is the newest release's, so a docs fix reaches
  it with the next release. A version in the book is a placeholder,
  `{{version}}`, `{{series}}` (the image's `0.1` tag), `{{next_minor}}` or
  `{{next_patch}}`, filled in from `Cargo.toml` by its preprocessor,
  `cargo xtask mdbook`; never write a version number by hand. Update it with the README when a feature
  lands or a route, key or error changes, in the same pull request; the
  configuration reference includes `config/example.toml` verbatim.
- `CHANGELOG.md` — Keep a Changelog, one version for every crate. A
  change a user, operator or embedder would notice adds its line under
  `Unreleased` in the same commit; the book includes the file verbatim.
  Every crate but `xtask` is published to crates.io and the images to
  `ghcr.io/tchak/agrafe`, so a breaking change (allowed before 1.0, in a
  minor version) says in its line what breaks and what to do, and a new
  migration says whether the previous release survives it. CI's `Semver`
  job (`cargo semver-checks`) runs on a release's pull request and tag
  only, and fails a Rust API break the bumped version does not allow.

## Local development

- Copy `config/example.toml` to `config/agrafe.toml` (gitignored), set real
  keys, and point `database.url` at `agrafe_dev`. Ports 8080/8081 are taken on
  this machine; the local config uses 18080/18081.
- `cargo run --bin agrafe -- --config config/agrafe.toml migrate`, then
  `cargo run --bin agrafe -- --config config/agrafe.toml serve` (the
  workspace has several binaries, so `--bin` is needed). The workers need the
  helper binaries: `cargo build --workspace` and point
  `media.extract_command`, `media.render_text_command`,
  `media.render_document_command` and `media.render_geo_command` at the binaries under `target/debug`, or
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
- `AGRAFE_TEST_PROMTOOL=$(which promtool)` runs the test that lints
  `/metrics` with Prometheus' linter; it skips without the variable, and CI
  sets it.
- `cargo coverage` (an alias in `.cargo/config.toml`, needs cargo-llvm-cov)
  reports line coverage; add `--show-missing-lines` to see what is uncovered.
- Linux-only code (the `agrafe-sandbox` confinement, bubblewrap) runs
  locally in a [Lima](https://lima-vm.io) VM, `scripts/lima.yaml`:
  Ubuntu 26.04 like CI, its stock kernel (Landlock, AppArmor restricting
  unprivileged user namespaces), vips, Postgres with the test database,
  rust and nextest. Create it once from the checkout with `limactl start
  --name agrafe --mount-only "$PWD:w" scripts/lima.yaml`, then
  `limactl shell --workdir "$PWD" agrafe env AGRAFE_SANDBOX_REQUIRED=1
  CARGO_TARGET_DIR=target/linux cargo nextest run --workspace` (its own
  target directory: macOS and Linux artifacts do not mix). On a cold
  target run `cargo build --workspace --all-targets` first, as CI does,
  or the integration tests time out waiting for the helpers to build.
  `/usr/bin/bwrap` gets its user namespace from Ubuntu's packaged
  `bwrap-userns-restrict` AppArmor profile; a copy at another path is
  refused, which is the unprofiled host. CI (`ubuntu-26.04`) sets
  `AGRAFE_SANDBOX_REQUIRED=1` too, so a pushed bookmark with a draft PR
  is the same run. `cargo clippy -p agrafe-sandbox --all-targets --target
  x86_64-unknown-linux-gnu` (target installed) catches type and cfg
  errors in the sandbox crate without the VM. Only that crate: the
  others pull in `aws-lc-sys` through rustls, whose C build needs a
  Linux cross compiler macOS does not have, so they wait for the VM or
  CI.

## Conventions

- Modules use named files, never `mod.rs`: `src/blob.rs` declares the module
  and its children live in `src/blob/`.
- Queries use `sqlx::query_as` with `FromRow` (no compile-time macros) so the
  build does not need a database.
- Blob state transitions are conditional `UPDATE ... WHERE state = $from`.
  Never set `state` unconditionally.
- Public routes verify the signature before touching the database.
- Nothing decodes or renders user files in-process: `vips` and the
  `agrafe-extract`, `agrafe-render-text`, `agrafe-render-document` and
  `agrafe-render-geo` binaries run through `agrafe_sandbox`. The parsers
  (`lopdf`, `zip`, `quick-xml`, `ab_glyph`, `typst`, `geojson`) are
  dependencies of the helper crates only; never add them to `agrafe` or
  `agrafe-render`. What a helper and the server exchange (the text sample,
  the map spec and extent) is typed in `agrafe-types`.
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
- `blobs.updated_at` is set by a trigger
  (`crates/agrafe/migrations/0001_blobs.sql`). Do not set it in queries.
- Database tests are `#[tokio::test]` and start with `let db =
  TestDatabase::new().await`, handed to `setup`: each gets a fresh,
  migrated database of its own on the server `DATABASE_URL` names
  (default in `.cargo/config.toml`: `agrafe_test` as the current OS
  user), named after a UUID and dropped with the app, so tests run in
  parallel and so do runs (another session, another workspace). Never
  share state between tests through the database. The helper is
  `agrafe::db::test`, behind the `test-support` feature.
