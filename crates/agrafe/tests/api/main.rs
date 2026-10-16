//! Integration tests against the real routers and a real Postgres database.
//!
//! `DATABASE_URL` selects the database, else `postgres://$USER@localhost/agrafe_test`.
//! Tests share it and serialise through a mutex; each stores its objects in
//! a directory of its own, removed when it ends.

mod support;

mod blobs;
mod jobs;
mod original;
mod scan;
mod upload;
