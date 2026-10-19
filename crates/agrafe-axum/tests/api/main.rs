//! Integration tests against the real routers and a real Postgres database.
//!
//! Each test runs on a database of its own, created and migrated by
//! `#[sqlx::test]` on the server `DATABASE_URL` names (the default is in
//! `.cargo/config.toml`), so tests run in parallel. The database is named
//! after the test, so two runs on one server at once fail each other's
//! common tests. Each test stores its objects in a directory of its own,
//! removed when it ends.

mod support;

mod archive;
mod attachments;
mod blobs;
mod cache;
mod document;
mod fetch;
mod geojson;
mod hosts;
mod jobs;
mod original;
mod races;
mod reaper;
mod representations;
mod rows;
mod scan;
mod schema;
mod sealed;
mod upload;
mod webhook;
