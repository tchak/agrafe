//! The audit: the bucket listing against the `blobs` table. What it
//! reports, what it repairs on its own, and what only `--fix` deletes.

use std::num::NonZeroU32;
use std::time::Duration;

use agrafe::jobs::audit::{self, Finding, Options, Report};
use agrafe::jobs::{self, JobKind};
use axum::http::{Method, StatusCode};
use jiff::Timestamp;
use object_store::ObjectStoreExt as _;
use object_store::path::Path as ObjectPath;
use serde_json::json;

use crate::support::*;

/// An app whose rows are settled as soon as they are written: the audit
/// leaves a row alone for `upload.stale_after` after its last change,
/// since the listing may predate it, and these tests write and audit at
/// once.
async fn settled_setup(db: TestDatabase) -> TestApp {
    let clamd = fake_clamd().await;
    setup_tuned(db, clamd, |c| c.upload.stale_after = Duration::ZERO).await
}

async fn audit_with(app: &TestApp, options: Options) -> Report {
    app.state.audit(options).await.unwrap()
}

async fn audit_now(app: &TestApp) -> Report {
    audit_with(app, Options::default()).await
}

const DEEP: Options = Options {
    fix: false,
    delete_unknown: false,
    deep: Some(NonZeroU32::MIN),
};

const FIX: Options = Options {
    fix: true,
    delete_unknown: false,
    deep: None,
};

const DELETE_UNKNOWN: Options = Options {
    fix: false,
    delete_unknown: true,
    deep: None,
};

async fn delete_object(app: &TestApp, key: &str) {
    app.state
        .store()
        .delete(&ObjectPath::from(key))
        .await
        .unwrap();
}

async fn put_object(app: &TestApp, key: &str, bytes: Vec<u8>) {
    app.state
        .store()
        .put(&ObjectPath::from(key), bytes.into())
        .await
        .unwrap();
}

async fn object_bytes(app: &TestApp, key: &str) -> Vec<u8> {
    app.state
        .store()
        .get(&ObjectPath::from(key))
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap()
        .to_vec()
}

async fn object_keys(app: &TestApp) -> Vec<String> {
    let mut keys: Vec<String> = stored_objects(app)
        .await
        .into_iter()
        .map(|o| o.location.to_string())
        .collect();
    keys.sort();
    keys
}

/// Make every scheduled purge due and run them all.
async fn purge_all(app: &TestApp) {
    sqlx::query("UPDATE jobs SET run_at = now() WHERE kind = 'purge'")
        .execute(app.state.pool())
        .await
        .unwrap();
    while run_job(app, JobKind::Purge).await {}
}

async fn render_all(app: &TestApp) {
    while run_job(app, JobKind::Render).await {}
}

fn counts(report: &Report) -> Vec<(Finding, u64)> {
    Finding::ALL
        .iter()
        .map(|f| (*f, report.count(*f)))
        .filter(|(_, n)| *n > 0)
        .collect()
}

#[tokio::test]
async fn a_consistent_bucket_has_no_findings() {
    let db = TestDatabase::new().await;
    let app = settled_setup(db).await;
    let photo = ready_file(&app, "photo.jpg", "image/jpeg", &fixture("photo.jpg")).await;
    render_all(&app).await;
    // No bytes yet: nothing to expect, nothing to find.
    create(&app, raw_blob()).await;
    // Deleted and awaiting its purge: the prefix is the purge's.
    let going = ready_blob(&app, &png_bytes(100)).await;
    assert_eq!(delete(&app, id_of(&going)).await, StatusCode::NO_CONTENT);
    assert_eq!(
        get_blob(&app, id_of(&photo)).await["thumbnail_state"],
        "ready"
    );

    let report = audit_now(&app).await;
    assert_eq!(counts(&report), vec![], "{report}");
    assert_eq!(
        report.objects, 4,
        "original, thumbnail, preview, the deleted one"
    );
    assert_eq!(report.blobs, 2);
    assert_eq!((report.deep, report.deleted), (0, 0));

    let (status, body) = call(&app.private, req(Method::GET, "/metrics")).await;
    assert_eq!(status, StatusCode::OK);
    let text = body.as_str().unwrap();
    assert!(
        text.contains("agrafe_audit_findings{kind=\"orphan\"} 0"),
        "{text}"
    );
    assert!(
        text.contains("agrafe_audit_findings{kind=\"missing_original\"} 0"),
        "{text}"
    );
    assert!(text.contains("agrafe_audit_objects 4"), "{text}");
    assert!(text.contains("agrafe_audit_blobs 2"), "{text}");
    let at = text
        .lines()
        .find_map(|l| l.strip_prefix("agrafe_audit_last_run_timestamp_seconds "))
        .unwrap()
        .parse::<i64>()
        .unwrap();
    assert!(at >= Timestamp::now().as_second() - 60, "{at}");
}

#[tokio::test]
async fn a_missing_original_is_stamped_on_the_row() {
    let db = TestDatabase::new().await;
    let app = settled_setup(db).await;
    let blob = ready_blob(&app, &png_bytes(100)).await;
    let id = id_of(&blob);
    delete_object(&app, &storage_key(&app, id).await).await;
    // A terminal blob's original is the purge's business, not a finding.
    let going = ready_blob(&app, &png_bytes(100)).await;
    delete_object(&app, &storage_key(&app, id_of(&going)).await).await;
    assert_eq!(delete(&app, id_of(&going)).await, StatusCode::NO_CONTENT);

    let report = audit_now(&app).await;
    assert_eq!(
        counts(&report),
        vec![(Finding::MissingOriginal, 1)],
        "{report}"
    );
    assert_eq!(report.objects, 0);
    assert_eq!(report.blobs, 1, "the one row that should have an object");

    let got = get_blob(&app, id).await;
    assert_eq!(got["state"], "ready");
    assert_eq!(
        got["last_error"],
        "audit: the original is missing from the store"
    );
    let stamped = got["failed_at"].as_str().unwrap().to_string();
    let (_, failed) = call(&app.private, req(Method::GET, "/blobs?failed=true")).await;
    assert_eq!(failed["items"][0]["id"], id, "{failed}");

    // Found again on the next run; the first verdict stands.
    let report = audit_now(&app).await;
    assert_eq!(report.count(Finding::MissingOriginal), 1);
    assert_eq!(get_blob(&app, id).await["failed_at"], stamped);
}

#[tokio::test]
async fn a_truncated_original_is_damaged() {
    let db = TestDatabase::new().await;
    let app = settled_setup(db).await;
    let blob = ready_blob(&app, &png_bytes(3000)).await;
    let id = id_of(&blob);
    let key = storage_key(&app, id).await;
    let mut bytes = object_bytes(&app, &key).await;
    let whole = bytes.len();
    bytes.truncate(whole - 1);
    put_object(&app, &key, bytes).await;

    let report = audit_now(&app).await;
    assert_eq!(
        counts(&report),
        vec![(Finding::DamagedOriginal, 1)],
        "{report}"
    );
    assert_eq!(report.deep, 0, "the listing's size is enough");
    let got = get_blob(&app, id).await;
    assert_eq!(
        got["last_error"],
        format!(
            "audit: the original is {} bytes, not the {whole} its size calls for",
            whole - 1
        )
    );
    assert!(got["failed_at"].is_string(), "{got}");
}

#[tokio::test]
async fn the_deep_check_reads_the_originals_back() {
    let db = TestDatabase::new().await;
    let app = settled_setup(db).await;
    let sound = ready_blob(&app, &png_bytes(3000)).await;
    let corrupt = ready_blob(&app, &png_bytes(3000)).await;
    let key = storage_key(&app, id_of(&corrupt)).await;
    let mut bytes = object_bytes(&app, &key).await;
    bytes[1500] ^= 0xff;
    put_object(&app, &key, bytes).await;

    // Same length: the listing sees nothing.
    let report = audit_now(&app).await;
    assert_eq!(counts(&report), vec![], "{report}");

    let report = audit_with(&app, DEEP).await;
    assert_eq!(
        counts(&report),
        vec![(Finding::DamagedOriginal, 1)],
        "{report}"
    );
    assert_eq!(report.deep, 2);
    let got = get_blob(&app, id_of(&corrupt)).await;
    let error = got["last_error"].as_str().unwrap();
    assert!(
        error.starts_with("audit: the original does not read back: opening segment 1"),
        "{error}"
    );
    assert_eq!(
        get_blob(&app, id_of(&sound)).await["failed_at"],
        json!(null)
    );
}

#[tokio::test]
async fn a_missing_representation_is_rendered_again() {
    let db = TestDatabase::new().await;
    let app = settled_setup(db).await;
    let photo = ready_file(&app, "photo.jpg", "image/jpeg", &fixture("photo.jpg")).await;
    let id = id_of(&photo);
    render_all(&app).await;
    assert_eq!(jobs_for(&app, id).await, Vec::<String>::new());
    delete_object(&app, &object_key(id, "thumbnail")).await;
    put_object(&app, &object_key(id, "preview"), vec![0; 10]).await;

    let report = audit_now(&app).await;
    assert_eq!(
        counts(&report),
        vec![(Finding::MissingRepresentation, 2)],
        "{report}"
    );
    let got = get_blob(&app, id).await;
    assert_eq!(got["thumbnail_state"], "pending");
    assert_eq!(got["preview_state"], "pending");
    assert_eq!(got["failed_at"], json!(null), "repaired, not stuck");
    assert_eq!(jobs_for(&app, id).await, vec!["render"]);

    render_all(&app).await;
    let got = get_blob(&app, id).await;
    assert_eq!(got["thumbnail_state"], "ready");
    assert_eq!(got["preview_state"], "ready");
    let report = audit_now(&app).await;
    assert_eq!(counts(&report), vec![], "{report}");
    assert_eq!(report.objects, 3);
}

/// The merge: rows the listing has nothing for sit between listed
/// prefixes, before the first and after the last, and each is found.
#[tokio::test]
async fn rows_between_listed_prefixes_are_checked() {
    let db = TestDatabase::new().await;
    let app = settled_setup(db).await;
    let mut ids = Vec::new();
    for _ in 0..5 {
        ids.push(id_of(&ready_blob(&app, &png_bytes(100)).await).to_string());
    }
    let mut sorted = ids.clone();
    sorted.sort();
    assert_eq!(ids, sorted, "ids are ordered by time");
    for i in [0, 2, 4] {
        delete_object(&app, &storage_key(&app, &ids[i]).await).await;
    }
    // A prefix with no row between two that have one.
    let unknown = uuid::Uuid::now_v7();
    put_object(&app, &agrafe::blob::key::original(unknown), vec![1]).await;

    let report = audit_now(&app).await;
    assert_eq!(
        counts(&report),
        vec![(Finding::MissingOriginal, 3), (Finding::Unknown, 1)],
        "{report}"
    );
    assert_eq!(
        report.unknown,
        Some(audit::Unknown {
            prefixes: 1,
            first: unknown,
            last: unknown
        })
    );
    assert_eq!((report.objects, report.blobs), (3, 5));
    for (i, id) in ids.iter().enumerate() {
        let stamped = get_blob(&app, id).await["failed_at"].is_string();
        assert_eq!(stamped, i % 2 == 0, "blob {i}");
    }
}

/// What `plant_orphans` left in the bucket.
struct Planted {
    /// A live blob, whose prefix two of the orphans are under.
    id: String,
    /// The objects under known prefixes that their rows do not account
    /// for: junk and a source part under the live blob, the original of a
    /// purged one.
    orphans: Vec<String>,
    /// The id of a prefix no row has, with an `original` under it.
    unknown: uuid::Uuid,
}

/// The objects nothing accounts for, and two outside the layout: a key
/// without a blob id, and a blob id without a name.
async fn plant_orphans(app: &TestApp) -> Planted {
    let blob = ready_blob(app, &png_bytes(100)).await;
    let id = id_of(&blob).to_string();
    let tombstone = ready_blob(app, &png_bytes(100)).await;
    assert_eq!(delete(app, id_of(&tombstone)).await, StatusCode::NO_CONTENT);
    purge_all(app).await;
    assert!(
        get_blob(app, id_of(&tombstone)).await["purged_at"].is_string(),
        "purged"
    );
    let orphans = vec![
        object_key(&id, "junk"),
        object_key(&id, "source/1"),
        object_key(id_of(&tombstone), "original"),
    ];
    for key in &orphans {
        put_object(app, key, vec![1, 2, 3]).await;
    }
    let unknown = uuid::Uuid::now_v7();
    put_object(app, &agrafe::blob::key::original(unknown), vec![1, 2, 3]).await;
    put_object(app, "README", vec![1, 2, 3]).await;
    put_object(app, &uuid::Uuid::now_v7().to_string(), vec![1, 2, 3]).await;
    Planted {
        id,
        orphans,
        unknown,
    }
}

#[tokio::test]
async fn orphans_are_reported_and_left_alone() {
    let db = TestDatabase::new().await;
    let app = settled_setup(db).await;
    let planted = plant_orphans(&app).await;
    let before = object_keys(&app).await;
    assert_eq!(before.len(), 7);

    let report = audit_now(&app).await;
    assert_eq!(
        counts(&report),
        vec![
            (Finding::Orphan, 3),
            (Finding::Unknown, 1),
            (Finding::Foreign, 2)
        ],
        "{report}"
    );
    assert_eq!(
        report.unknown,
        Some(audit::Unknown {
            prefixes: 1,
            first: planted.unknown,
            last: planted.unknown
        })
    );
    assert_eq!(report.deleted, 0);
    assert_eq!(object_keys(&app).await, before);
    assert_eq!(get_blob(&app, &planted.id).await["failed_at"], json!(null));
}

/// An object younger than `upload.stale_after` that no row accounts for
/// is what an upload or a render in flight looks like, its row not yet
/// updated: not a finding, not deleted, left for the next run.
#[tokio::test]
async fn young_orphans_are_left_for_the_next_run() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let planted = plant_orphans(&app).await;
    let before = object_keys(&app).await;

    let options = Options {
        fix: true,
        delete_unknown: true,
        deep: None,
    };
    let report = audit_with(&app, options).await;
    assert_eq!(counts(&report), vec![(Finding::Foreign, 2)], "{report}");
    assert_eq!(report.young, 4);
    assert_eq!(report.unknown, None, "nothing old enough under it");
    assert_eq!(report.unsettled, 1, "the live row, just written: {report}");
    assert_eq!(report.deleted, 0);
    assert_eq!(object_keys(&app).await, before);
    assert_eq!(get_blob(&app, &planted.id).await["failed_at"], json!(null));
}

#[tokio::test]
async fn fix_deletes_the_orphans_older_than_stale_after() {
    let db = TestDatabase::new().await;
    let app = settled_setup(db).await;
    let planted = plant_orphans(&app).await;

    let report = audit_with(&app, FIX).await;
    assert_eq!(
        counts(&report),
        vec![
            (Finding::Orphan, 3),
            (Finding::Unknown, 1),
            (Finding::Foreign, 2)
        ],
        "{report}"
    );
    assert_eq!(report.deleted, 3);
    let left = object_keys(&app).await;
    for key in &planted.orphans {
        assert!(!left.contains(key), "{key} in {left:?}");
    }
    assert!(
        left.contains(&object_key(&planted.id, "original")),
        "{left:?}"
    );
    assert!(
        left.contains(&agrafe::blob::key::original(planted.unknown)),
        "a prefix no row has is not --fix's to delete: {left:?}"
    );
    assert!(
        left.contains(&"README".to_string()),
        "never deleted: {left:?}"
    );
    assert_eq!(left.len(), 4);

    let report = audit_now(&app).await;
    assert_eq!(
        counts(&report),
        vec![(Finding::Unknown, 1), (Finding::Foreign, 2)],
        "{report}"
    );
    assert_eq!(
        report.mismatches(),
        1,
        "the unknown prefix counts, foreign does not"
    );
}

/// The objects under a prefix no row has go only with `--delete-unknown`,
/// which leaves the orphans under known prefixes to `--fix`.
#[tokio::test]
async fn delete_unknown_deletes_the_prefixes_no_row_has() {
    let db = TestDatabase::new().await;
    let app = settled_setup(db).await;
    let planted = plant_orphans(&app).await;

    let report = audit_with(&app, DELETE_UNKNOWN).await;
    assert_eq!(
        counts(&report),
        vec![
            (Finding::Orphan, 3),
            (Finding::Unknown, 1),
            (Finding::Foreign, 2)
        ],
        "{report}"
    );
    assert_eq!(report.deleted, 1);
    let left = object_keys(&app).await;
    assert!(
        !left.contains(&agrafe::blob::key::original(planted.unknown)),
        "{left:?}"
    );
    for key in &planted.orphans {
        assert!(left.contains(key), "{key} not in {left:?}");
    }
    assert_eq!(left.len(), 6);

    let report = audit_now(&app).await;
    assert_eq!(
        counts(&report),
        vec![(Finding::Orphan, 3), (Finding::Foreign, 2)],
        "{report}"
    );
    assert_eq!(report.unknown, None);
}

#[tokio::test]
async fn a_source_part_the_build_still_needs_is_checked() {
    let db = TestDatabase::new().await;
    let app = settled_setup(db).await;
    let blob = create(
        &app,
        json!({
            "filename": "export.csv",
            "content_type": "text/csv",
            "input_type": "rows",
            "columns": [{"name": "Name"}],
        }),
    )
    .await;
    let id = id_of(&blob);
    let (status, body) = call(
        &app.private,
        put_raw(
            &format!("/blobs/{id}/parts/1"),
            b"[\"Lovelace\"]\n".to_vec(),
        ),
    )
    .await;
    assert!(status.is_success(), "{status} {body}");
    assert_eq!(get_blob(&app, id).await["state"], "created");

    let report = audit_now(&app).await;
    assert_eq!(counts(&report), vec![], "{report}");
    assert_eq!(report.objects, 1);

    delete_object(&app, &object_key(id, "source/1")).await;
    let report = audit_now(&app).await;
    assert_eq!(
        counts(&report),
        vec![(Finding::MissingSource, 1)],
        "{report}"
    );
    assert_eq!(
        get_blob(&app, id).await["failed_at"],
        json!(null),
        "the build says so itself"
    );

    // Truncated to a length no stream has.
    put_object(&app, &object_key(id, "source/1"), vec![0; 5]).await;
    let report = audit_now(&app).await;
    assert_eq!(
        counts(&report),
        vec![(Finding::MissingSource, 1)],
        "{report}"
    );
}

/// A row that changed within `upload.stale_after` of the listing passing
/// it may name an object the listing could not have shown, an upload
/// finishing as the audit went by: nothing is required of it until the
/// next run, and nothing is written on it.
#[tokio::test]
async fn a_row_that_just_changed_is_left_for_the_next_run() {
    let db = TestDatabase::new().await;
    let clamd = fake_clamd().await;
    let app = setup_tuned(db, clamd, |c| c.upload.stale_after = Duration::from_secs(2)).await;
    let blob = ready_blob(&app, &png_bytes(100)).await;
    let id = id_of(&blob);
    delete_object(&app, &storage_key(&app, id).await).await;

    let report = audit_now(&app).await;
    assert_eq!(counts(&report), vec![], "{report}");
    assert_eq!((report.blobs, report.unsettled), (1, 1));
    assert_eq!(get_blob(&app, id).await["failed_at"], json!(null));

    tokio::time::sleep(Duration::from_millis(2100)).await;
    let report = audit_now(&app).await;
    assert_eq!(
        counts(&report),
        vec![(Finding::MissingOriginal, 1)],
        "{report}"
    );
    assert_eq!(report.unsettled, 0);
    assert!(get_blob(&app, id).await["failed_at"].is_string());
}

/// A job whose payload holds a `Progress` carries on from it: the rows up
/// to its cursor and the keys up to its last one are done, the tally so
/// far is added to.
#[tokio::test]
async fn the_job_resumes_from_its_progress() {
    let db = TestDatabase::new().await;
    let app = settled_setup(db).await;
    let mut ids = Vec::new();
    for _ in 0..3 {
        ids.push(id_of(&ready_blob(&app, &png_bytes(100)).await).to_string());
    }
    // The sweep runs in key order, the shard then the id, not in time order.
    ids.sort_by_key(|id| agrafe::blob::key::order(parse_id(id)));
    for id in &ids {
        delete_object(&app, &storage_key(&app, id).await).await;
    }
    // Planted as the one before the cursor: it is never listed again.
    put_object(&app, &object_key(&ids[0], "junk"), vec![1]).await;

    let done = audit::Progress {
        after: Some(object_key(&ids[0], "junk")),
        cursor: ids[1].parse().unwrap(),
        report: {
            let mut report = Report::default();
            report.objects = 7;
            report.blobs = 2;
            report
        },
    };
    sqlx::query("INSERT INTO jobs (kind, payload, run_at) VALUES ('audit', $1, now())")
        .bind(serde_json::to_value(&done).unwrap())
        .execute(app.state.pool())
        .await
        .unwrap();
    assert!(run_job(&app, JobKind::Audit).await);

    for (i, id) in ids.iter().enumerate() {
        let stamped = get_blob(&app, id).await["failed_at"].is_string();
        assert_eq!(stamped, i == 2, "blob {i}: only the one after the cursor");
    }
    let (_, body) = call(&app.private, req(Method::GET, "/metrics")).await;
    let text = body.as_str().unwrap();
    assert!(text.contains("agrafe_audit_objects 7"), "{text}");
    assert!(text.contains("agrafe_audit_blobs 3"), "{text}");
    assert!(
        text.contains("agrafe_audit_findings{kind=\"missing_original\"} 1"),
        "{text}"
    );
    assert!(
        text.contains("agrafe_audit_findings{kind=\"orphan\"} 0"),
        "the planted key is behind the resumed listing: {text}"
    );
}

#[tokio::test]
async fn the_job_schedules_the_next_run_and_yields_to_a_running_one() {
    let db = TestDatabase::new().await;
    let app = settled_setup(db).await;
    ready_blob(&app, &png_bytes(100)).await;
    assert!(
        audit::schedule(app.state.pool(), Timestamp::now())
            .await
            .unwrap()
    );
    assert!(run_job(&app, JobKind::Audit).await);
    let due_in: f64 = sqlx::query_scalar(
        "SELECT EXTRACT(EPOCH FROM run_at - now())::float8 FROM jobs WHERE kind = 'audit'",
    )
    .fetch_one(app.state.pool())
    .await
    .unwrap();
    let week = 7.0 * 24.0 * 3600.0;
    assert!(due_in > week - 60.0 && due_in <= week, "{due_in}");

    // Another session holds the lock: the API says so, the job steps aside.
    let mut holder = app.state.pool().acquire().await.unwrap();
    assert!(jobs::try_lock(&mut holder, audit::LOCK).await.unwrap());
    let err = app.state.audit(Options::default()).await.unwrap_err();
    assert!(matches!(err, audit::AuditError::Running), "{err}");
    sqlx::query("UPDATE jobs SET run_at = now() WHERE kind = 'audit'")
        .execute(app.state.pool())
        .await
        .unwrap();
    assert!(run_job(&app, JobKind::Audit).await);
    let (dead, pending): (i64, i64) = sqlx::query_as(
        "SELECT count(*) FILTER (WHERE failed_at IS NOT NULL), \
                count(*) FILTER (WHERE failed_at IS NULL AND locked_at IS NULL) \
         FROM jobs WHERE kind = 'audit'",
    )
    .fetch_one(app.state.pool())
    .await
    .unwrap();
    assert_eq!(dead, 0, "skipping is not failing");
    assert_eq!(pending, 1, "stepping aside still schedules the next run");
    jobs::unlock(&mut holder, audit::LOCK).await.unwrap();
    drop(holder);

    // The runner enqueues the first one an interval out, once.
    sqlx::query("DELETE FROM jobs")
        .execute(app.state.pool())
        .await
        .unwrap();
    let at = Timestamp::now() + app.state.config().jobs.audit_interval;
    assert!(audit::schedule(app.state.pool(), at).await.unwrap());
    assert!(!audit::schedule(app.state.pool(), at).await.unwrap());
    let pending = app
        .state
        .list_jobs(&jobs::ListParams {
            kind: Some(JobKind::Audit),
            limit: 10,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(pending.items.len(), 1);
}

/// A representation object the row does not expect: one a render is about
/// to replace is left alone, one the row says failed is an orphan.
#[tokio::test]
async fn representation_objects_are_checked_against_the_row_state() {
    let db = TestDatabase::new().await;
    let app = settled_setup(db).await;
    let photo = ready_file(&app, "photo.jpg", "image/jpeg", &fixture("photo.jpg")).await;
    let id = id_of(&photo);
    render_all(&app).await;

    sqlx::query(
        "UPDATE blobs SET thumbnail_state = 'pending', preview_state = 'failed' WHERE id = $1",
    )
    .bind(parse_id(id))
    .execute(app.state.pool())
    .await
    .unwrap();
    let report = audit_now(&app).await;
    assert_eq!(counts(&report), vec![(Finding::Orphan, 1)], "{report}");
    assert_eq!(report.deleted, 0, "reported, not deleted");
    assert_eq!(
        object_keys(&app).await.len(),
        3,
        "the original and both representations stay"
    );
}

/// An original that decrypts whole, to a file that is not the one
/// recorded.
#[tokio::test]
async fn the_deep_check_compares_the_digest() {
    let db = TestDatabase::new().await;
    let app = settled_setup(db).await;
    let blob = ready_blob(&app, &png_bytes(100)).await;
    let id = id_of(&blob);
    sqlx::query("UPDATE blobs SET checksum_sha256 = $2 WHERE id = $1")
        .bind(parse_id(id))
        .bind(vec![0u8; 32])
        .execute(app.state.pool())
        .await
        .unwrap();

    let report = audit_with(&app, DEEP).await;
    assert_eq!(
        counts(&report),
        vec![(Finding::DamagedOriginal, 1)],
        "{report}"
    );
    let got = get_blob(&app, id).await;
    let error = got["last_error"].as_str().unwrap();
    assert!(
        error.starts_with("audit: the original decrypts to a file with SHA-256 "),
        "{error}"
    );
    assert!(error.ends_with(", not the one recorded"), "{error}");
}

/// A payload that is not a `Progress` starts the audit over rather than
/// failing the job.
#[tokio::test]
async fn the_job_starts_over_on_a_progress_it_cannot_read() {
    let db = TestDatabase::new().await;
    let app = settled_setup(db).await;
    let blob = ready_blob(&app, &png_bytes(100)).await;
    let id = id_of(&blob);
    delete_object(&app, &storage_key(&app, id).await).await;
    sqlx::query("INSERT INTO jobs (kind, payload, run_at) VALUES ('audit', $1, now())")
        .bind(json!({"cursor": 5}))
        .execute(app.state.pool())
        .await
        .unwrap();

    assert!(run_job(&app, JobKind::Audit).await);
    assert!(get_blob(&app, id).await["failed_at"].is_string());
}

/// The last attempt of a failing audit schedules the next one: a dead
/// job does not end the weekly sweep. An earlier attempt does not, its
/// retry carrying the progress on.
#[cfg(unix)]
#[tokio::test]
async fn a_failed_last_attempt_still_schedules_the_next_run() {
    use std::os::unix::fs::PermissionsExt;

    let db = TestDatabase::new().await;
    let app = settled_setup(db).await;
    let blob = ready_blob(&app, &png_bytes(100)).await;
    let agrafe::config::StorageConfig::Local { local } = &app.state.config().storage else {
        panic!("the tests store locally");
    };
    // A prefix the listing cannot enter.
    let prefix = std::path::Path::new(&local.path).join(object_key(id_of(&blob), ""));
    std::fs::set_permissions(&prefix, std::fs::Permissions::from_mode(0o000)).unwrap();
    let restore = || std::fs::set_permissions(&prefix, std::fs::Permissions::from_mode(0o755));

    let jobs = |app: &TestApp| {
        let pool = app.state.pool().clone();
        async move {
            sqlx::query_as::<_, (i64, i64)>(
                "SELECT count(*) FILTER (WHERE failed_at IS NOT NULL), \
                        count(*) FILTER (WHERE failed_at IS NULL) \
                 FROM jobs WHERE kind = 'audit'",
            )
            .fetch_one(&pool)
            .await
            .unwrap()
        }
    };

    sqlx::query(
        "INSERT INTO jobs (kind, payload, run_at, max_attempts) VALUES ('audit', '{}', now(), 2)",
    )
    .execute(app.state.pool())
    .await
    .unwrap();
    assert!(run_job(&app, JobKind::Audit).await);
    let (dead, pending) = jobs(&app).await;
    assert_eq!((dead, pending), (0, 1), "a retry, and nothing else");

    sqlx::query("UPDATE jobs SET run_at = now() WHERE kind = 'audit'")
        .execute(app.state.pool())
        .await
        .unwrap();
    assert!(run_job(&app, JobKind::Audit).await);
    restore().unwrap();
    let (dead, pending) = jobs(&app).await;
    assert_eq!((dead, pending), (1, 1), "dead, and the next one scheduled");
    let error: String = sqlx::query_scalar(
        "SELECT last_error FROM jobs WHERE kind = 'audit' AND failed_at IS NOT NULL",
    )
    .fetch_one(app.state.pool())
    .await
    .unwrap();
    assert!(error.contains("object store"), "{error}");
    let due_in: f64 = sqlx::query_scalar(
        "SELECT EXTRACT(EPOCH FROM run_at - now())::float8 FROM jobs \
         WHERE kind = 'audit' AND failed_at IS NULL",
    )
    .fetch_one(app.state.pool())
    .await
    .unwrap();
    let week = 7.0 * 24.0 * 3600.0;
    assert!(due_in > week - 60.0 && due_in <= week, "{due_in}");
}
