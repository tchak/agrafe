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
it yourself, wrap it in `agrafe_axum::host::refuse_unknown_host` and
`agrafe_axum::trace::private` as the binary does, since a layer on a router
does not reach routes nested after it.

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
finished.

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
method is meant to be called. The crate documentation is built with:

```sh
cargo doc -p agrafe -p agrafe-axum --open
```
