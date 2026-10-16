use std::path::PathBuf;

use agrafe::app::AppState;
use agrafe::config::{Config, Role};
use agrafe::{api, db, jobs};
use anyhow::{Context, Result};
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
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let cli = Cli::parse();
    if let Some(path) = &cli.config {
        anyhow::ensure!(path.exists(), "config file {} not found", path.display());
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
    }
}

async fn serve(config: Config) -> Result<()> {
    let role = config.server.role;
    let pool = db::connect(&config.database).await?;
    let state = AppState::new(config, pool)?;
    let shutdown = shutdown_signal().shared();

    let mut tasks = tokio::task::JoinSet::new();

    if role.runs_api() {
        let cfg = &state.config().server;
        let private = TcpListener::bind(cfg.private_addr)
            .await
            .with_context(|| format!("binding private interface on {}", cfg.private_addr))?;
        let public = TcpListener::bind(cfg.public_addr)
            .await
            .with_context(|| format!("binding public interface on {}", cfg.public_addr))?;
        tracing::info!(addr = %cfg.private_addr, "private interface listening");
        tracing::info!(addr = %cfg.public_addr, "public interface listening");

        let private_app = api::private_router(state.clone());
        let public_app = api::public_router(state.clone());
        let s = shutdown.clone();
        tasks.spawn(async move {
            axum::serve(private, private_app)
                .with_graceful_shutdown(s)
                .await
                .context("private interface")
        });
        let s = shutdown.clone();
        tasks.spawn(async move {
            axum::serve(public, public_app)
                .with_graceful_shutdown(s)
                .await
                .context("public interface")
        });
    }

    if role.runs_worker() {
        tasks.spawn(jobs::runner::run(state.clone(), shutdown.clone()));
    }

    while let Some(result) = tasks.join_next().await {
        result.context("task panicked")??;
    }
    tracing::info!("shutdown complete");
    Ok(())
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
