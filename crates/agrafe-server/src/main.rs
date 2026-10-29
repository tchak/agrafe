//! The `agrafe` binary. `agrafe migrate` applies the pending database
//! migrations; `agrafe serve` runs what `server.role` names: the HTTP
//! interfaces `server.interfaces` lists (private, public or both, the
//! dashboard under `/dashboard` on the private one when enabled), the job
//! workers, or both; `agrafe audit` checks the bucket against the `blobs`
//! table once, on demand. Every process binds the private address: one
//! that does not serve the private API answers `/healthz` and `/metrics`
//! on it alone.

use std::net::SocketAddr;
use std::num::NonZeroU32;
use std::path::PathBuf;

use agrafe::Agrafe;
use agrafe::app::InitError;
use agrafe::config::{Config, ConfigError, Interface, Role};
use agrafe::jobs::JobKind;
use agrafe::jobs::audit::{self, AuditError};
use agrafe::{db, jobs};
use agrafe_axum as api;
use clap::{Parser, Subcommand};
use futures::FutureExt;
use tokio::net::TcpListener;
use tracing::Instrument as _;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(
    name = "agrafe",
    version,
    about = "Encrypted, scanned file storage with previews"
)]
struct Cli {
    /// Path to the TOML configuration file.
    #[arg(long, env = "AGRAFE_CONFIG", global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the HTTP interfaces and/or the workers.
    Serve {
        /// Overrides server.role from the configuration: api, worker or
        /// all.
        #[arg(long)]
        role: Option<Role>,
        /// Overrides server.interfaces: the HTTP interfaces an api process
        /// serves, comma separated (private, public).
        #[arg(long, value_delimiter = ',')]
        interfaces: Option<Vec<Interface>>,
        /// Overrides jobs.kinds: the job kinds this process runs workers
        /// for, comma separated (fetch, scan, render, rows, archive,
        /// document, purge, reap, notify, audit).
        #[arg(long, value_delimiter = ',')]
        kinds: Option<Vec<JobKind>>,
    },
    /// Apply pending database migrations.
    Migrate,
    /// Check the bucket against the blobs table once, as the weekly audit
    /// job does: one log line per mismatch on stderr, the tally on stdout, and a
    /// non-zero exit when anything was found.
    Audit {
        /// Delete the orphan objects older than upload.stale_after: the
        /// objects under a known blob that its row does not account for.
        /// Without it they are reported and left alone. Never touches a
        /// prefix no row has.
        #[arg(long)]
        fix: bool,
        /// Delete the objects under a prefix no row has, older than
        /// upload.stale_after. After a database restored from backup these
        /// are the blobs the backup missed, rows to recover rather than
        /// objects to delete: do not use it until the database is as
        /// recovered as it will be.
        #[arg(long)]
        delete_unknown: bool,
        /// Decrypt the originals and compare their SHA-256 with the rows',
        /// which reads the whole bucket; the listing alone catches a
        /// missing or truncated object.
        #[arg(long)]
        deep: bool,
        /// With --deep: read one original in N, the same ones every run.
        #[arg(long, value_name = "N", requires = "deep")]
        sample: Option<NonZeroU32>,
        /// Save the sweep's place to FILE every minute and resume from it
        /// when it is there, so that an interrupted sweep of a large
        /// bucket carries on rather than starting over. Removed once the
        /// sweep is done; a file saved with other options is refused.
        #[arg(long, value_name = "FILE")]
        progress: Option<std::path::PathBuf>,
    },
}

/// Why the process stopped.
#[derive(thiserror::Error)]
enum Error {
    #[error("config file {} not found", .0.display())]
    ConfigNotFound(PathBuf),
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error("connecting to the database: {0}")]
    Connect(#[from] sqlx::Error),
    #[error("running migrations: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),
    #[error("the database schema: {0}")]
    Schema(#[from] db::SchemaError),
    #[error(transparent)]
    Init(#[from] InitError),
    #[error("binding the {interface} interface on {addr}: {source}")]
    Bind {
        interface: &'static str,
        addr: SocketAddr,
        source: std::io::Error,
    },
    #[error("{interface} interface: {source}")]
    Serve {
        interface: &'static str,
        source: std::io::Error,
    },
    #[error(transparent)]
    Workers(#[from] jobs::runner::Error),
    #[error("task panicked: {0}")]
    Panicked(#[from] tokio::task::JoinError),
    #[error(transparent)]
    Audit(#[from] AuditError),
    #[error("the audit found {0} mismatches, listed above; foreign objects do not count")]
    Findings(u64),
    #[error("the audit could not act on {0} of its findings, see the log")]
    Unacted(u64),
}

/// `main` reports an error through `Debug`; print the message instead.
impl std::fmt::Debug for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self, f)
    }
}

/// The crates under the fetch client, which log request targets at debug
/// and trace. A fetched URL is a presigned secret.
const HTTP_TARGETS: [&str; 4] = ["hyper", "hyper_util", "h2", "reqwest"];

/// Hold the HTTP stack at `info` whatever the global level says, unless
/// the filter names one of those crates itself: then the operator asked
/// and gets it. The check is on the filter string, since a directive added
/// on a target replaces the one the filter already had on it; a directive
/// on a module below, `hyper::proto=trace`, is more specific and wins over
/// the cap on its own.
/// The filter `env`, the value of `RUST_LOG`, spells, `info` when it is
/// empty or names nothing valid. A directive it cannot parse is reported on stderr and
/// left out, the others kept: one typo must not silently turn the debug
/// lines an operator asked for back into `info`.
fn env_filter(env: &str) -> EnvFilter {
    EnvFilter::builder()
        .with_default_directive(tracing_subscriber::filter::LevelFilter::INFO.into())
        .parse_lossy(env)
}

fn cap_http_targets(filter: EnvFilter, env: &str) -> EnvFilter {
    HTTP_TARGETS
        .into_iter()
        .filter(|target| !names_target(env, target))
        .fold(filter, |filter, target| {
            filter.add_directive(format!("{target}=info").parse().expect("a valid directive"))
        })
}

/// Whether a filter string has a directive on exactly `target`:
/// `hyper=debug`, `hyper[span]=trace`, or a bare `hyper`.
fn names_target(env: &str, target: &str) -> bool {
    env.split(',')
        .map(str::trim)
        .any(|directive| directive.split(['=', '[']).next() == Some(target))
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    let cli = Cli::parse();
    let env = std::env::var(EnvFilter::DEFAULT_ENV).unwrap_or_default();
    let filter = cap_http_targets(env_filter(&env), &env);
    // Logs on stderr: stdout is the audit's report, to keep or pipe apart.
    // Coloured on a terminal only, not in a log collector's input.
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .init();

    if let Some(path) = &cli.config
        && !path.exists()
    {
        return Err(Error::ConfigNotFound(path.clone()));
    }
    let mut config = Config::load(cli.config.as_deref())?;

    match cli.command {
        Command::Migrate => {
            let pool = db::connect(&config.database).await?;
            db::migrate(&pool, &config.database).await?;
            tracing::info!("migrations applied");
            Ok(())
        }
        Command::Serve {
            role,
            interfaces,
            kinds,
        } => {
            if let Some(role) = role {
                config.server.role = role;
            }
            if let Some(interfaces) = interfaces {
                config.server.interfaces = interfaces;
            }
            if let Some(kinds) = kinds {
                config.jobs.kinds = Some(kinds);
            }
            serve(config).await
        }
        Command::Audit {
            fix,
            delete_unknown,
            deep,
            sample,
            progress,
        } => {
            let pool = db::connect(&config.database).await?;
            db::check_migrated(&pool).await?;
            let state = Agrafe::new(config, pool)?;
            let options = audit::Options {
                fix,
                delete_unknown,
                deep: deep.then(|| sample.unwrap_or(NonZeroU32::MIN)),
            };
            // One trace for the sweep, as the weekly job has: the renders it
            // requests join it, and its log lines carry the id.
            let context = agrafe::trace::TraceContext::root();
            let span = tracing::info_span!("audit", trace_id = %context.trace_id());
            let sweep = async {
                match progress {
                    Some(path) => state.audit_resumable(options, &path).await,
                    None => state.audit(options).await,
                }
            };
            let report = context.scope(sweep).instrument(span).await?;
            println!("{report}");
            match (report.errors, report.mismatches()) {
                (0, 0) => Ok(()),
                (0, n) => Err(Error::Findings(n)),
                (n, _) => Err(Error::Unacted(n)),
            }
        }
    }
}

async fn serve(config: Config) -> Result<(), Error> {
    // First, so that the logs of a fleet midway through an upgrade say
    // which release each process runs.
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        role = %config.server.role,
        "agrafe starting"
    );
    let pool = db::connect(&config.database).await?;
    // Before anything listens: a process against a schema it was not
    // written for would answer `/healthz` and fail everything else.
    db::check_migrated(&pool).await?;
    let state = Agrafe::new(config, pool)?;
    let shutdown = shutdown_signal().shared();
    let cfg = &state.config().server;

    let mut tasks = tokio::task::JoinSet::new();

    // The private address is always bound: the private API when this
    // process serves it, `/healthz` and `/metrics` alone otherwise, so
    // that every process has something to probe and scrape.
    let private = bind("private", cfg.private_addr).await?;
    let private_app = if cfg.serves(Interface::Private) {
        tracing::info!(addr = %cfg.private_addr, "private interface listening");
        let mut app = api::private_router(state.clone());
        if state.config().dashboard.enabled {
            tracing::info!(addr = %cfg.private_addr, "dashboard at /dashboard");
            if cfg.private_hosts.is_empty() {
                tracing::warn!(
                    "dashboard enabled without server.private_hosts: a browser on the \
                     trusted network is exposed to DNS rebinding, see DESIGN.md"
                );
            }
            // Nested after the private router's layers, so it gets the host
            // check of its own; it brings its request span, and the
            // cross-site check is its extractor's.
            let dashboard = agrafe_dashboard::dashboard_router(state.clone()).layer(
                axum::middleware::from_fn_with_state(state.clone(), api::host::refuse_unknown_host),
            );
            app = app.nest("/dashboard", dashboard);
        }
        app
    } else {
        tracing::info!(addr = %cfg.private_addr, "private interface listening (healthz and metrics only)");
        api::ops_router(state.clone())
    };
    let s = shutdown.clone();
    tasks.spawn(async move {
        axum::serve(private, private_app)
            .with_graceful_shutdown(s)
            .await
            .map_err(|source| Error::Serve {
                interface: "private",
                source,
            })
    });

    if cfg.serves(Interface::Public) {
        let public = bind("public", cfg.public_addr).await?;
        tracing::info!(addr = %cfg.public_addr, "public interface listening");
        let public_app = api::public_router(state.clone());
        let s = shutdown.clone();
        tasks.spawn(async move {
            axum::serve(public, public_app)
                .with_graceful_shutdown(s)
                .await
                .map_err(|source| Error::Serve {
                    interface: "public",
                    source,
                })
        });
    }

    if cfg.role.runs_worker() {
        let (state, s) = (state.clone(), shutdown.clone());
        tasks.spawn(async move { Ok(jobs::runner::run(state, s).await?) });
    }

    while let Some(result) = tasks.join_next().await {
        result??;
    }
    tracing::info!("shutdown complete");
    Ok(())
}

async fn bind(interface: &'static str, addr: SocketAddr) -> Result<TcpListener, Error> {
    TcpListener::bind(addr).await.map_err(|source| Error::Bind {
        interface,
        addr,
        source,
    })
}

async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler");
        tokio::select! {
            _ = ctrl_c => {},
            _ = term.recv() => {},
        }
    }
    #[cfg(not(unix))]
    {
        let _ = ctrl_c.await;
    }
    tracing::info!("shutdown signal received");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_http_stack_is_capped_unless_named() {
        assert!(!names_target("debug", "hyper"));
        assert!(!names_target("hyper_util=trace", "hyper"), "another crate");
        assert!(
            !names_target("hyper::proto=trace", "hyper"),
            "a module below"
        );
        assert!(names_target("info,hyper=debug", "hyper"));
        assert!(names_target("hyper[span]=trace", "hyper"));
        assert!(names_target(" hyper ", "hyper"));

        let capped = cap_http_targets(EnvFilter::new("debug"), "debug").to_string();
        assert!(capped.contains("hyper=info"), "{capped}");
        assert!(capped.contains("reqwest=info"), "{capped}");
        let asked = cap_http_targets(EnvFilter::new("hyper=trace"), "hyper=trace").to_string();
        assert!(asked.contains("hyper=trace"), "{asked}");
        assert!(!asked.contains("hyper=info"), "{asked}");
        assert!(asked.contains("hyper_util=info"), "{asked}");
    }

    #[test]
    fn a_bad_directive_leaves_the_others() {
        let filter = env_filter("debug,hyper=trce").to_string();
        assert!(filter.contains("debug"), "{filter}");
        assert!(!filter.contains("trce"), "{filter}");
        let unset = env_filter("").to_string();
        assert!(unset.contains("info"), "{unset}");
        let targeted = env_filter("agrafe=debug").to_string();
        assert!(targeted.contains("agrafe=debug"), "{targeted}");
    }
}
