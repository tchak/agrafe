//! A database of its own for each test, on the server `DATABASE_URL`
//! names.
//!
//! `#[sqlx::test]` names each test's database after the test, so two runs
//! against one server at once, from two sessions or two workspaces, drop
//! each other's databases. [`TestDatabase`] names its database after a
//! fresh UUID instead: any number of runs share a server. A database is
//! dropped when its handle is, unless the test is failing, in which case
//! it is kept for a look and its name printed; the next run of any test
//! ten minutes later or more sweeps it.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sqlx::postgres::{PgConnectOptions, PgConnection, PgPoolOptions};
use sqlx::{AssertSqlSafe, Connection, Executor, PgPool};
use uuid::Uuid;

use super::MIGRATOR;

/// What every test database's name starts with: the UUID follows.
const PREFIX: &str = "agrafe_test_";

/// A database older than this with no handle left is a failed test's,
/// kept for a look, or an aborted run's; the sweep drops it. Above the
/// minute nextest allows a test, so a live one is never swept.
const STALE: Duration = Duration::from_secs(10 * 60);

/// Whether this process swept the stale databases yet: once is enough.
static SWEPT: AtomicBool = AtomicBool::new(false);

/// A handle on a database created for one test, dropped with the last
/// clone of the handle.
#[derive(Clone)]
pub struct TestDatabase(Arc<Inner>);

struct Inner {
    name: String,
    master: PgConnectOptions,
    pool: PgPool,
}

impl TestDatabase {
    /// A fresh database, migrated.
    pub async fn new() -> Self {
        let db = Self::empty().await;
        MIGRATOR
            .run(db.pool())
            .await
            .expect("migrating the test database");
        db
    }

    /// A fresh database with nothing in it.
    pub async fn empty() -> Self {
        let url = std::env::var("DATABASE_URL")
            .expect("DATABASE_URL must name the server the tests create their databases on");
        let master: PgConnectOptions = url.parse().expect("parsing DATABASE_URL");
        let mut conn = PgConnection::connect_with(&master)
            .await
            .expect("connecting to the server DATABASE_URL names");
        if !SWEPT.swap(true, Ordering::SeqCst) {
            sweep(&mut conn).await;
        }
        let name = format!("{PREFIX}{}", Uuid::now_v7().simple());
        conn.execute(AssertSqlSafe(format!("CREATE DATABASE {name:?}")))
            .await
            .expect("creating the test database");
        conn.close().await.expect("closing the master connection");
        // Shown with the output of a failed test, whose database is kept.
        eprintln!("test database: {name}");
        let pool = PgPoolOptions::new()
            // A test wanting more is doing too much at once.
            .max_connections(5)
            .connect_lazy_with(master.clone().database(&name));
        Self(Arc::new(Inner { name, master, pool }))
    }

    /// The pool on the database.
    pub fn pool(&self) -> &PgPool {
        &self.0.pool
    }

    /// How to connect to the database, for a pool with other options.
    pub fn connect_options(&self) -> PgConnectOptions {
        self.0.master.clone().database(&self.0.name)
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!("test database {} kept for inspection", self.name);
            return;
        }
        let master = self.master.clone();
        let name = self.name.clone();
        // A runtime of its own on a thread of its own: the test's runtime
        // may be shutting down, and nothing async runs in a destructor.
        std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("building a runtime to drop the test database")
                .block_on(async {
                    let mut conn = PgConnection::connect_with(&master).await?;
                    drop_database(&mut conn, &name).await?;
                    conn.close().await
                })
        })
        .join()
        .expect("the thread dropping the test database panicked")
        .unwrap_or_else(|err| eprintln!("could not drop test database {}: {err}", self.name));
    }
}

/// `DROP DATABASE` and whatever still holds a connection to it: the pool
/// of a handle just dropped may not have closed its connections yet.
async fn drop_database(conn: &mut PgConnection, name: &str) -> sqlx::Result<()> {
    conn.execute(AssertSqlSafe(format!(
        "DROP DATABASE IF EXISTS {name:?} WITH (FORCE)"
    )))
    .await?;
    Ok(())
}

/// Drops the test databases created more than [`STALE`] ago, read off
/// the UUIDv7 in their names. Two runs may sweep the same one at once:
/// one of them fails, harmlessly.
async fn sweep(conn: &mut PgConnection) {
    let names: Vec<String> = sqlx::query_scalar(
        "SELECT datname::text FROM pg_database WHERE datname LIKE $1 ESCAPE '\\'",
    )
    .bind(format!("{}%", PREFIX.replace('_', "\\_")))
    .fetch_all(&mut *conn)
    .await
    .expect("listing the test databases");
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("the clock is after 1970");
    for name in names {
        let Some(created) = name
            .strip_prefix(PREFIX)
            .and_then(|id| Uuid::parse_str(id).ok())
            .and_then(|id| id.get_timestamp())
        else {
            continue;
        };
        let (secs, _) = created.to_unix();
        if now.saturating_sub(Duration::from_secs(secs)) < STALE {
            continue;
        }
        if let Err(err) = drop_database(conn, &name).await {
            eprintln!("could not sweep test database {name}: {err}");
        }
    }
}
