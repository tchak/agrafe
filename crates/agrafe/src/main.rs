use std::net::SocketAddr;
use std::path::PathBuf;

use agrafe::app::{AppState, InitError};
use agrafe::config::{Config, ConfigError, Role};
use agrafe::media::{extract, text};
use agrafe::{api, db, jobs};
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
        /// Overrides server.role from the configuration.
        #[arg(long, value_enum)]
        role: Option<Role>,
    },
    /// Apply pending database migrations.
    Migrate,
    /// Pull the embedded image out of a document. The workers run this in
    /// a sandbox; it is not meant to be called by hand.
    #[command(hide = true)]
    Extract(ExtractArgs),
    /// Rasterise the first lines of a text file. Same deal as `extract`.
    #[command(hide = true)]
    RenderText(RenderTextArgs),
}

#[derive(clap::Args)]
struct RenderTextArgs {
    #[arg(long)]
    input: PathBuf,
    /// A PGM, written only when the input looks like text.
    #[arg(long)]
    output: PathBuf,
    #[arg(long)]
    width: u32,
    #[arg(long)]
    height: u32,
}

#[derive(clap::Args)]
struct ExtractArgs {
    #[arg(long)]
    input: PathBuf,
    /// Written only when an image was found.
    #[arg(long)]
    output: PathBuf,
    #[arg(long)]
    content_type: String,
    #[arg(long)]
    min_coverage: f32,
    #[arg(long)]
    max_text_chars: u32,
    /// Largest decoded buffer, in bytes.
    #[arg(long)]
    max_output: usize,
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
    #[error("reading or writing a helper file: {0}")]
    Helper(std::io::Error),
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
    // The helpers read no configuration and write no logs: they run in a
    // sandbox with nothing but their two files.
    match cli.command {
        Command::Extract(args) => return extract(args),
        Command::RenderText(args) => return render_text(args),
        _ => {}
    }

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
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
            db::migrate(&pool).await?;
            tracing::info!("migrations applied");
            Ok(())
        }
        Command::Serve { role } => {
            if let Some(role) = role {
                config.server.role = role;
            }
            serve(config).await
        }
        Command::Extract(_) | Command::RenderText(_) => {
            unreachable!("handled before loading the configuration")
        }
    }
}

fn render_text(args: RenderTextArgs) -> Result<(), Error> {
    let bytes = std::fs::read(&args.input).map_err(Error::Helper)?;
    if let Some(pgm) = text::render(&bytes, args.width, args.height) {
        std::fs::write(&args.output, pgm).map_err(Error::Helper)?;
    }
    Ok(())
}

fn extract(args: ExtractArgs) -> Result<(), Error> {
    let bytes = std::fs::read(&args.input).map_err(Error::Helper)?;
    let thresholds = extract::Thresholds {
        min_coverage: args.min_coverage,
        max_text_chars: args.max_text_chars,
    };
    if let Some(found) = extract::extract(&args.content_type, &bytes, &thresholds, args.max_output)
    {
        std::fs::write(&args.output, found.bytes).map_err(Error::Helper)?;
    }
    Ok(())
}

async fn serve(config: Config) -> Result<(), Error> {
    let role = config.server.role;
    let pool = db::connect(&config.database).await?;
    let state = AppState::new(config, pool)?;
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
