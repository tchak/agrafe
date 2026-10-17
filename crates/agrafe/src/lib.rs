//! agrafe: encrypted, scanned file storage with previews.
//!
//! This crate is the service without its HTTP layer: build an [`Agrafe`]
//! from a [`config::Config`] and a pool, call the private API on it, run
//! the workers with [`jobs::runner::run`]. The `agrafe-axum` crate turns
//! the same handle into the two routers, and `agrafe-server` is the binary.

pub mod app;
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
