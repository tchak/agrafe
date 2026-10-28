//! The schema against the code. Enum columns are `text` (or `smallint`)
//! with a CHECK list rather than Postgres enums, so nothing ties that list
//! to the Rust enum written into the column but this test.

use std::collections::{BTreeMap, BTreeSet};

use agrafe::blob::{BlobState, InputType, PreviewSource, RepresentationState, ScanState};
use agrafe::classify::Category;
use agrafe::jobs::{JobKind, Priority};
use sqlx::{PgPool, Postgres, QueryBuilder};

use crate::support::*;

type Column = (&'static str, &'static str);

/// Every column constrained to a list of values, with the values the code
/// writes there.
fn expected() -> BTreeMap<Column, BTreeSet<String>> {
    fn values<T: Copy>(all: &[T], f: impl Fn(T) -> String) -> BTreeSet<String> {
        all.iter().map(|&v| f(v)).collect()
    }
    let representation = values(RepresentationState::ALL, |v| v.as_str().into());
    let priority = values(Priority::ALL, |v| v.rank().to_string());
    BTreeMap::from([
        (
            ("blobs", "state"),
            values(BlobState::ALL, |v| v.as_str().into()),
        ),
        (
            ("blobs", "input_type"),
            values(InputType::ALL, |v| v.as_str().into()),
        ),
        (
            ("blobs", "scan_state"),
            values(ScanState::ALL, |v| v.as_str().into()),
        ),
        (
            ("blobs", "category"),
            values(Category::ALL, |v| v.as_str().into()),
        ),
        (("blobs", "thumbnail_state"), representation.clone()),
        (("blobs", "preview_state"), representation),
        (
            ("blobs", "preview_source"),
            values(PreviewSource::ALL, |v| v.as_str().into()),
        ),
        (("blobs", "priority"), priority.clone()),
        (("jobs", "priority"), priority),
        (
            ("jobs", "kind"),
            values(JobKind::ALL, |v| v.as_str().into()),
        ),
    ])
}

/// The value lists of the schema's single-column CHECK constraints, keyed
/// by table and column. Postgres prints `x IN (...)` back as
/// `x = ANY (ARRAY[...])` with casts on the literals, so the array is cut
/// out and handed back to Postgres to evaluate rather than parsed here.
async fn value_lists(pool: &PgPool) -> BTreeMap<(String, String), BTreeSet<String>> {
    let checks: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT c.conrelid::regclass::text, a.attname::text, pg_get_constraintdef(c.oid) \
         FROM pg_constraint c \
         JOIN pg_attribute a ON a.attrelid = c.conrelid AND a.attnum = c.conkey[1] \
         WHERE c.contype = 'c' AND cardinality(c.conkey) = 1 \
           AND c.connamespace = current_schema()::regnamespace",
    )
    .fetch_all(pool)
    .await
    .unwrap();

    let mut lists = BTreeMap::new();
    for (table, column, def) in checks {
        let Some(start) = def.find("= ANY (ARRAY[") else {
            continue;
        };
        let rest = &def[start + "= ANY (ARRAY[".len()..];
        let end = rest
            .rfind(']')
            .unwrap_or_else(|| panic!("{table}.{column}: {def}"));
        // The array comes from the catalog, not from input.
        let values: Vec<String> =
            QueryBuilder::<Postgres>::new(format!("SELECT unnest(ARRAY[{}])::text", &rest[..end]))
                .build_query_scalar()
                .fetch_all(pool)
                .await
                .unwrap();
        lists.insert((table, column), values.into_iter().collect());
    }
    lists
}

/// Each enum column's CHECK list is exactly its Rust enum's values, and
/// no constrained column is left out of `expected`.
#[tokio::test]
async fn check_constraints_match_the_enums() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let actual = value_lists(app.state.pool()).await;
    let expected: BTreeMap<(String, String), BTreeSet<String>> = expected()
        .into_iter()
        .map(|((t, c), v)| ((t.to_string(), c.to_string()), v))
        .collect();
    assert_eq!(actual, expected);
}

/// `Blob` reads every column of `blobs` but the column declaration, by
/// name: a column added to the table and forgotten in the list would never
/// reach the struct, and one dropped would fail every read.
#[tokio::test]
async fn blob_columns_are_the_table_but_the_declaration() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let table: BTreeSet<String> = sqlx::query_scalar(
        "SELECT column_name::text FROM information_schema.columns \
         WHERE table_schema = current_schema() AND table_name = 'blobs'",
    )
    .fetch_all(app.state.pool())
    .await
    .unwrap()
    .into_iter()
    .collect();
    let mut read: BTreeSet<String> = agrafe::blob::repo::BLOB_COLUMNS
        .split(',')
        .map(|c| c.trim().to_string())
        .collect();
    assert!(read.insert("source_columns".into()));
    assert_eq!(read, table);
}
