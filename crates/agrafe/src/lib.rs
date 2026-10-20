//! agrafe: encrypted, scanned file storage with previews.
//!
//! This crate is the service without its HTTP layer. A host application
//! loads a [`config::Config`] with [`Config::load`](config::Config::load),
//! opens a pool with [`db::connect`] and applies the migrations with
//! [`db::migrate`], then builds an [`Agrafe`] from the two. The methods on
//! the handle are the private API: blobs ([`Agrafe::create_blob`],
//! [`Agrafe::find_blob`], [`Agrafe::delete_blob`]), uploads
//! ([`Agrafe::upload`], [`Agrafe::upload_part`], [`Agrafe::finish_source`]),
//! URLs ([`Agrafe::urls`], [`Agrafe::mint_urls`]), attachments
//! ([`Agrafe::attach_blob`], [`Agrafe::detach_blob`]) and jobs
//! ([`Agrafe::list_jobs`], [`Agrafe::retry_job`]). The workers that scan,
//! render and build run with [`jobs::runner::run`], in the same process or
//! another one sharing the database.
//!
//! The `agrafe-axum` crate turns the same handle into the private and
//! public routers, and `agrafe-server` is the binary.
//!
//! ```no_run
//! use agrafe::{Agrafe, config::Config, db, jobs};
//!
//! # async fn start() -> Result<(), Box<dyn std::error::Error>> {
//! let config = Config::load(Some("config/agrafe.toml".as_ref()))?;
//! let pool = db::connect(&config.database).await?;
//! db::migrate(&pool).await?;
//! let agrafe = Agrafe::new(config, pool)?;
//! // Hand `agrafe.clone()` to the routers of `agrafe-axum`, then:
//! jobs::runner::run(agrafe, std::future::pending()).await?;
//! # Ok(())
//! # }
//! ```

pub mod app;
pub mod attachment;
pub mod blob;
pub mod clamav;
pub mod config;
pub mod db;
pub mod encryption;
pub mod jobs;
pub mod metrics;
pub mod signing;
pub mod source;
pub mod storage;
pub mod upload;

pub use agrafe_render::icons;
pub use agrafe_types::{ParseEnumError, classify, sniff, text};
pub use app::Agrafe;

#[cfg(test)]
mod vectors;
