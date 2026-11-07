# Embedding in an axum application

agrafe runs standalone, but it is also a set of libraries. A Rust
application built on [axum](https://github.com/tokio-rs/axum) can host it
in-process:

- `agrafe` is the service as a library: the configuration, the database,
  the jobs, and the `Agrafe` handle, whose methods are the private API in
  Rust;
- `agrafe-axum` provides the two HTTP interfaces as routers, with their
  state applied, ready to nest;
- `agrafe-dashboard` provides the dashboard as a router of its own.

## Adding the crates

```sh
cargo add agrafe agrafe-axum
cargo add agrafe-dashboard   # for the dashboard
```

Every agrafe crate is released at the same version, and they depend on
each other at it: keep them on one version, and upgrade them together.
Before 1.0, `cargo update` stays within the minor version your
`Cargo.toml` names, since a minor version may break the Rust API; the
[changelog](../changelog.md) says how. The reference for each crate is on
docs.rs: [`agrafe`](https://docs.rs/agrafe),
[`agrafe-axum`](https://docs.rs/agrafe-axum) and
[`agrafe-dashboard`](https://docs.rs/agrafe-dashboard).

The libraries render through the same sandboxed helpers as the binary,
which are separate executables: install them where the workers run, from
the release your crates are at,

```sh
cargo install --locked agrafe-extract agrafe-render-text \
  agrafe-render-document agrafe-render-geo
```

and put them on the `PATH` or name them in `media.extract_command` and its
siblings. A process that serves the HTTP interfaces and runs no `render` or
`document` worker needs none of them; see
[Deployment](deployment.md#what-to-ship).

The migrations are embedded in `agrafe`: `db::migrate` below applies the
ones of the version you build with, so upgrading the crate is also
upgrading the schema, forward only, as [Upgrading](deployment.md#upgrading)
describes.

## Setting up the handle

```rust,ignore
use agrafe::{Agrafe, config::Config, db, jobs};

let config = Config::load(Some("agrafe.toml".as_ref()))?;
let pool = db::connect(&config.database).await?;
db::migrate(&pool, &config.database).await?;
let agrafe = Agrafe::new(config, pool)?;
```

`Config::load` reads the TOML file, if any, and the `AGRAFE_` environment
variables, exactly as the binary does. `Agrafe` is cheap to clone.

### Sharing the host's database

agrafe can live in the application's own database, in a schema of its own:

```toml
[database]
url = "postgres://localhost/my_app"
schema = "agrafe"
```

`db::connect` opens every connection with that schema as its `search_path`,
and `db::migrate` creates it when missing, so the tables, the migration
history (`_sqlx_migrations`, kept apart from the host's own if it uses
sqlx too) and the trigger function all land there. Open agrafe's pool with
`db::connect` rather than handing it the application's: a pool built
elsewhere searches the host's schema, and the handle would read and write
there.

## Nesting the routers

```rust,ignore
let app = axum::Router::new()
    .nest("/files", agrafe_axum::public_router(agrafe.clone()))
    .merge(my_routes);
```

Set `server.public_base_url` to the URL the nested router is reached under,
here `https://app.example/files`, so minted URLs point at it.

The private router can be nested on an internal listener the same way:

```rust,ignore
let internal = axum::Router::new()
    .nest("/agrafe", agrafe_axum::private_router(agrafe.clone()))
    .nest("/agrafe/dashboard", agrafe_dashboard::dashboard_router(agrafe.clone()));
```

The dashboard finds its prefix from the request, and embeds thumbnails from
the private API one segment up: `/agrafe/dashboard` expects the private
routes under `/agrafe`. Put it behind your own authentication. When nesting
it yourself, wrap it in `agrafe_axum::host::refuse_unknown_host` as the
binary does, since a layer on a router does not reach routes nested after
it. It brings its own request span: do not wrap it in
`agrafe_axum::trace::private` again.

## Running the workers

```rust,ignore
use futures::FutureExt;

// The runner hands a clone of the shutdown future to every worker.
let shutdown = async { tokio::signal::ctrl_c().await.ok(); }.shared();
tokio::spawn(jobs::runner::run(agrafe.clone(), shutdown));
```

The runner probes the renderers and the document compiler when it handles
their kinds, schedules the reaper, and starts the pools of `jobs.kinds`. It
returns once the shutdown future completes and the jobs in flight have
finished or, past `jobs.shutdown_grace`, gone back to the queue.

## Tracing

The library and the routers open spans and set no subscriber: the host's
applies. To export them, add its OpenTelemetry layer and tell agrafe to take a
request's or a job's ids from their span, so that what a job row stores names a
span the backend has, with `agrafe::trace::set_span_source`; the `agrafe`
binary's `telemetry/otel.rs` is an example. Keep what the [monitoring
guide](monitoring.md#what-stays-in-the-process) lists in the process: the
`path` span field holds filenames, and `last_error` what a job's failure
quotes. `http.route` is there to use instead.

## Calling the private API in Rust

Every private route is a method on the handle, returning its own error
enum:

```rust,ignore
use agrafe::blob::create::CreateBlobRequest;

let blob = agrafe.create_blob(request).await?;          // CreateError
let urls = agrafe.mint_urls(blob.id, &options).await?;  // MintError
let page = agrafe.list_blobs(params).await?;            // ListError
agrafe.delete_blob(blob.id).await?;                     // DeleteError
```

A job enqueued by a method records the trace context current in the
task, which the routers set from the request. Called from your own code,
run the call in your request's context so its jobs join your trace:

```rust,ignore
use agrafe::trace::TraceContext;

let context = TraceContext::continue_from(&my_traceparent)
    .unwrap_or_else(TraceContext::root);
let blob = context.scope(agrafe.create_blob(request)).await?;
```

The handlers in `agrafe-axum` are a line or two each over these methods,
and the mapping of each error onto a status code lives in one place,
`crates/agrafe-axum/src/error.rs`. Read them as the reference for how each
method is meant to be called, with the crate documentation on
[docs.rs](https://docs.rs/agrafe), or built locally, for the version in
your lock file, with:

```sh
cargo doc -p agrafe -p agrafe-axum --open
```
