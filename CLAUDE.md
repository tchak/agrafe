# files

Rust file storage service. See `DESIGN.md` for architecture and decisions; read
it before changing behaviour, and update it when a decision changes.

## Version control: jj, not git

This repository is managed with **jj** (Jujutsu), colocated with git. Always use
`jj` commands, never `git` commands, for status, diff, log, describe, new,
squash, bookmarks and pushing. `jj st`, `jj diff`, `jj log`, `jj describe -m`,
`jj new`, `jj bookmark`.

## Layout

- `crates/files` — the server binary and library.
- `migrations/` — sqlx migrations, plain SQL, forward only.
- `config/example.toml` — documented configuration reference.

## Local development

- Copy `config/example.toml` to `config/files.toml` (gitignored), set real
  keys, and point `database.url` at `files_dev`. Ports 8080/8081 are taken on
  this machine; the local config uses 18080/18081.
- `cargo run -- --config config/files.toml migrate`, then
  `cargo run -- --config config/files.toml serve`.
- Tests need a `files_test` database reachable as the current OS user, or
  `DATABASE_URL`. Run `cargo test`, `cargo clippy --all-targets`, `cargo fmt`
  before committing.

## Conventions

- Queries use `sqlx::query_as` with `FromRow` (no compile-time macros) so the
  build does not need a database.
- Blob state transitions are conditional `UPDATE ... WHERE state = $from`.
  Never set `state` unconditionally.
- Public routes verify the signature before touching the database.
- Tests run against the `files_test` database (`DATABASE_URL` in `.env.test` or
  env). Integration tests share one database and serialise through a mutex.
