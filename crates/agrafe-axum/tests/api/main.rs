//! Integration tests against the real routers and a real Postgres database.
//!
//! Each test runs on a database of its own, created and migrated at its
//! start on the server `DATABASE_URL` names (the default is in
//! `.cargo/config.toml`) and dropped with the app, so tests run in
//! parallel, and so do runs: the database is named after a fresh UUID,
//! not the test. Each test stores its objects in a directory of its own,
//! removed when it ends.

mod support;

mod archive;
mod attachments;
mod audit;
mod blobs;
mod cache;
mod document;
mod fetch;
mod hosts;
mod jobs;
mod map;
mod original;
mod races;
mod reaper;
mod representations;
mod rows;
mod scan;
mod schema;
mod sealed;
mod trace;
mod upload;
mod webhook;
