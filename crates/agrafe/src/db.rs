//! Database pool and migrations.

use sqlx::AssertSqlSafe;
use sqlx::decode::Decode;
use sqlx::error::BoxDynError;
use sqlx::migrate::MigrateError;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions, PgTypeInfo, PgValueRef};
use sqlx::{PgPool, Postgres, Type};

use crate::config::DatabaseConfig;

/// The migrations under `crates/agrafe/migrations/`, embedded at build
/// time.
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// A pool of at most `max_connections` connections to `url`, the first one
/// opened before returning. With a `schema`, every connection searches it
/// alone, so the unqualified table names in the queries and the migrations
/// resolve there.
pub async fn connect(config: &DatabaseConfig) -> sqlx::Result<PgPool> {
    let options: PgConnectOptions = config.url.expose().parse()?;
    PgPoolOptions::new()
        .max_connections(config.max_connections)
        .connect_with(with_schema(options, config))
        .await
}

/// `options` with its `search_path` set to `config.schema`, when there is
/// one. A startup parameter rather than a `SET` after connecting, so a
/// connection the pool recycles cannot lose it.
fn with_schema(options: PgConnectOptions, config: &DatabaseConfig) -> PgConnectOptions {
    match &config.schema {
        Some(schema) => options.options([("search_path", schema.as_str())]),
        None => options,
    }
}

/// Applies the migrations `pool`'s database has not yet run, after creating
/// `config.schema` when one is set and missing. Fails on a migration whose
/// file changed since it was applied, as sqlx checksums them.
pub async fn migrate(pool: &PgPool, config: &DatabaseConfig) -> Result<(), MigrateError> {
    if let Some(schema) = &config.schema {
        // Interpolated, as DDL takes no parameters: `Config::validate`
        // admits a plain lowercase identifier and nothing else.
        sqlx::raw_sql(AssertSqlSafe(format!(
            "CREATE SCHEMA IF NOT EXISTS {schema}"
        )))
        .execute(pool)
        .await?;
    }
    MIGRATOR.run(pool).await
}

/// A nullable `timestamptz` column read straight into `Option<jiff::Timestamp>`.
///
/// `jiff-sqlx` provides a wrapper for the non-null case, which `FromRow` can
/// convert through `#[sqlx(try_from = "jiff_sqlx::Timestamp")]`. The derive
/// has no way to lift that through `Option`, so nullable columns use
/// `#[sqlx(try_from = "crate::db::NullableTimestamp")]` instead.
pub(crate) struct NullableTimestamp(Option<jiff_sqlx::Timestamp>);

impl Type<Postgres> for NullableTimestamp {
    fn type_info() -> PgTypeInfo {
        <jiff_sqlx::Timestamp as Type<Postgres>>::type_info()
    }

    fn compatible(ty: &PgTypeInfo) -> bool {
        <jiff_sqlx::Timestamp as Type<Postgres>>::compatible(ty)
    }
}

impl<'r> Decode<'r, Postgres> for NullableTimestamp {
    fn decode(value: PgValueRef<'r>) -> Result<Self, BoxDynError> {
        <Option<jiff_sqlx::Timestamp> as Decode<'r, Postgres>>::decode(value).map(Self)
    }
}

impl From<NullableTimestamp> for Option<jiff::Timestamp> {
    fn from(value: NullableTimestamp) -> Self {
        value.0.map(jiff_sqlx::Timestamp::to_jiff)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(schema: &str) -> DatabaseConfig {
        DatabaseConfig {
            url: String::from("unused: the test hands its own connect options").into(),
            schema: Some(schema.into()),
            max_connections: 2,
        }
    }

    /// With `database.schema`, the tables, the migration history and the
    /// trigger function land in that schema and the queries find them
    /// there; `public` stays untouched for the host's own tables.
    #[sqlx::test(migrations = false)]
    async fn migrates_into_the_configured_schema(
        _pool_options: PgPoolOptions,
        connect_options: PgConnectOptions,
    ) {
        let config = config("agrafe_files");
        let pool = PgPoolOptions::new()
            .connect_with(with_schema(connect_options, &config))
            .await
            .unwrap();

        migrate(&pool, &config).await.unwrap();
        // Running it again finds the schema and the migrations in place.
        migrate(&pool, &config).await.unwrap();

        let tables: Vec<(String, String)> = sqlx::query_as(
            "SELECT table_schema::text, table_name::text FROM information_schema.tables \
             WHERE table_schema NOT IN ('pg_catalog', 'information_schema') \
             ORDER BY table_name",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        let expected: Vec<(String, String)> = ["_sqlx_migrations", "attachments", "blobs", "jobs"]
            .into_iter()
            .map(|t| ("agrafe_files".to_owned(), t.to_owned()))
            .collect();
        assert_eq!(tables, expected);

        let functions: Vec<String> = sqlx::query_scalar(
            "SELECT n.nspname::text FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace \
             WHERE p.proname = 'set_updated_at'",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(functions, ["agrafe_files"]);

        // Unqualified names resolve through the search path, and the
        // trigger fires: it was bound to the function at creation.
        let (schema, count): (String, i64) =
            sqlx::query_as("SELECT current_schema()::text, count(*) FROM blobs")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(schema, "agrafe_files");
        assert_eq!(count, 0);
    }
}
