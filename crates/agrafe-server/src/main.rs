use std::net::SocketAddr;
use std::path::PathBuf;

use agrafe::Agrafe;
use agrafe::app::InitError;
use agrafe::config::{Config, ConfigError, Role};
use agrafe::jobs::JobKind;
use agrafe::{db, jobs};
use agrafe_axum as api;
use clap::{Parser, Subcommand};
use futures::FutureExt;
use tokio::net::TcpListener;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(
    name = "agrafe",
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
        /// Overrides jobs.kinds: the job kinds this process runs workers
        /// for, comma separated (fetch, scan, render, rows, archive, purge, reap).
        #[arg(long, value_delimiter = ',')]
        kinds: Option<Vec<JobKind>>,
    },
    /// Apply pending database migrations.
    Migrate,
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
}

/// `main` reports an error through `Debug`; print the message instead.
impl std::fmt::Debug for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self, f)
    }
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    let cli = Cli::parse();
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into());
    // The HTTP stack under the fetch client logs request targets at debug
    // and trace, and a fetched URL is a presigned secret: those crates stay
    // at info whatever the filter says, unless it names them itself.
    let filter =
        ["hyper", "hyper_util", "h2", "reqwest"]
            .into_iter()
            .fold(filter, |filter, target| {
                filter.add_directive(format!("{target}=info").parse().expect("a valid directive"))
            });
    tracing_subscriber::fmt().with_env_filter(filter).init();

    if let Some(path) = &cli.config
        && !path.exists()
    {
        return Err(Error::ConfigNotFound(path.clone()));
    }
    let mut config = Config::load(cli.config.as_deref())?;

    match cli.command {
        Command::Migrate => {
            let pool = db::connect(&config.database).await?;
            db::migrate(&pool).await?;
            tracing::info!("migrations applied");
            Ok(())
        }
        Command::Serve { role, kinds } => {
            if let Some(role) = role {
                config.server.role = role;
            }
            if let Some(kinds) = kinds {
                config.jobs.kinds = Some(kinds);
            }
            serve(config).await
        }
    }
}

async fn serve(config: Config) -> Result<(), Error> {
    let role = config.server.role;
    let pool = db::connect(&config.database).await?;
    let state = Agrafe::new(config, pool)?;
    let shutdown = shutdown_signal().shared();

    let mut tasks = tokio::task::JoinSet::new();

    if role.runs_api() {
        let cfg = &state.config().server;
        let private = bind("private", cfg.private_addr).await?;
        let public = bind("public", cfg.public_addr).await?;
        tracing::info!(addr = %cfg.private_addr, "private interface listening");
        tracing::info!(addr = %cfg.public_addr, "public interface listening");

        let private_app = api::private_router(state.clone());
        let public_app = api::public_router(state.clone());
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

    if role.runs_worker() {
        if !role.runs_api() {
            // No API, but still something to probe and scrape.
            let addr = state.config().server.private_addr;
            let private = bind("private", addr).await?;
            tracing::info!(addr = %addr, "private interface listening (healthz and metrics only)");
            let app = api::ops_router(state.clone());
            let s = shutdown.clone();
            tasks.spawn(async move {
                axum::serve(private, app)
                    .with_graceful_shutdown(s)
                    .await
                    .map_err(|source| Error::Serve {
                        interface: "private",
                        source,
                    })
            });
        }
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
