//! Database pool and migrations.

use anyhow::{Context, Result};
use sqlx::decode::Decode;
use sqlx::error::BoxDynError;
use sqlx::postgres::{PgPoolOptions, PgTypeInfo, PgValueRef};
use sqlx::{PgPool, Postgres, Type};

use crate::config::DatabaseConfig;

pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../../migrations");

pub async fn connect(config: &DatabaseConfig) -> Result<PgPool> {
    PgPoolOptions::new()
        .max_connections(config.max_connections)
        .connect(config.url.expose())
        .await
        .context("connecting to the database")
}

pub async fn migrate(pool: &PgPool) -> Result<()> {
    MIGRATOR.run(pool).await.context("running migrations")
}

/// A nullable `timestamptz` column read straight into `Option<jiff::Timestamp>`.
///
/// `jiff-sqlx` provides a wrapper for the non-null case, which `FromRow` can
/// convert through `#[sqlx(try_from = "jiff_sqlx::Timestamp")]`. The derive
/// has no way to lift that through `Option`, so nullable columns use
/// `#[sqlx(try_from = "crate::db::NullableTimestamp")]` instead.
pub struct NullableTimestamp(Option<jiff_sqlx::Timestamp>);

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
