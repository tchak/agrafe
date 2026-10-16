# agrafe

Rust file storage service. See `DESIGN.md` for architecture and decisions; read
it before changing behaviour, and update it when a decision changes.

## Version control: jj, not git

This repository is managed with **jj** (Jujutsu), colocated with git. Always use
`jj` commands, never `git` commands, for status, diff, log, describe, new,
squash, bookmarks and pushing. `jj st`, `jj diff`, `jj log`, `jj describe -m`,
`jj new`, `jj bookmark`.

## Layout

- `crates/agrafe` — the server binary and library.
- `crates/agrafe-crypto` — streaming AEAD and key wrapping, reviewed in
  isolation; no dependency on the server crate.
- `migrations/` — sqlx migrations, plain SQL, forward only.
- `config/example.toml` — documented configuration reference.

## Local development

- Copy `config/example.toml` to `config/agrafe.toml` (gitignored), set real
  keys, and point `database.url` at `agrafe_dev`. Ports 8080/8081 are taken on
  this machine; the local config uses 18080/18081.
- `cargo run -- --config config/agrafe.toml migrate`, then
  `cargo run -- --config config/agrafe.toml serve`.
- Tests need a `agrafe_test` database reachable as the current OS user, or
  `DATABASE_URL`. Run `cargo test`, `cargo clippy --all-targets`, `cargo fmt`
  before committing.
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
- Errors are `thiserror` enums, one per module that fails in its own way
  (`ConfigError`, `ReadError`, `JobError`, …). No `anyhow`. Messages inline
  their cause (`"reading the object: {0}"`), so log them with `%err`.
  `ApiError::internal(e)` is the only type-erased boundary.
- `blobs.updated_at` is set by a trigger (migration 0002). Do not set it in
  queries.
- Tests run against the `agrafe_test` database: `DATABASE_URL` from the
  environment, else `postgres://$USER@localhost/agrafe_test`. Integration
  tests share one database and serialise through a mutex.
