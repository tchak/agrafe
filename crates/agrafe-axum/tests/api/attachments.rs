//! Attachments: attach and detach, the expiry handed from the default to
//! the first attachment and back on the last detach, delete refusing an
//! attached blob, the listing and its filters, and the blob row lock that
//! orders an attach against an expiry, and `replace`, which leaves one
//! blob at an owner and role.

use agrafe::blob::repo;
use agrafe::jobs::reap;
use axum::http::{Method, StatusCode};
use jiff::{SignedDuration, Timestamp};
use serde_json::{Value, json};
use std::time::Duration;

use crate::support::*;

fn attachment(owner_kind: &str, owner_id: &str, role: &str) -> Value {
    json!({"owner_kind": owner_kind, "owner_id": owner_id, "role": role})
}

async fn attach(app: &TestApp, id: &str, body: Value) -> (StatusCode, Value) {
    call(
        &app.private,
        json_req(Method::POST, &format!("/blobs/{id}/attachments"), body),
    )
    .await
}

async fn detach(app: &TestApp, id: &str) -> StatusCode {
    call(
        &app.private,
        req(Method::DELETE, &format!("/attachments/{id}")),
    )
    .await
    .0
}

/// The blob's `expires_at` as the private port reports it.
fn expires_at(blob: &Value) -> Option<Timestamp> {
    blob["expires_at"].as_str().map(|s| s.parse().unwrap())
}

/// Whether `at` is about `after` from now.
fn about(at: Timestamp, after: Duration) -> bool {
    let expected = Timestamp::now() + SignedDuration::try_from(after).unwrap();
    at.duration_since(expected).abs() < SignedDuration::from_secs(5)
}

#[tokio::test]
async fn the_first_attach_clears_the_expiry_and_the_last_detach_restores_it() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let created = create(&app, raw_blob()).await;
    let id = id_of(&created);
    assert!(
        about(
            expires_at(&created).unwrap(),
            Duration::from_secs(24 * 3600)
        ),
        "created with the default expiry: {created}"
    );
    assert_eq!(created["attachments"], json!([]));

    let (status, first) = attach(&app, id, attachment("expense", "42", "receipt")).await;
    assert_eq!(status, StatusCode::CREATED, "{first}");
    assert_eq!(first["blob_id"], id);
    assert_eq!(first["owner_kind"], "expense");
    assert_eq!(first["owner_id"], "42");
    assert_eq!(first["role"], "receipt");
    assert!(first["created_at"].is_string());
    let blob = get_blob(&app, id).await;
    assert!(blob["expires_at"].is_null(), "{blob}");
    assert_eq!(blob["attachments"].as_array().unwrap().len(), 1);
    assert_eq!(blob["attachments"][0]["id"], first["id"]);

    // Idempotent: the same triple hands back the same attachment.
    let (status, again) = attach(&app, id, attachment("expense", "42", "receipt")).await;
    assert_eq!(status, StatusCode::OK, "{again}");
    assert_eq!(again["id"], first["id"]);
    assert_eq!(
        get_blob(&app, id).await["attachments"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    // Another owner, another attachment.
    let (status, second) = attach(&app, id, attachment("user", "7", "avatar")).await;
    assert_eq!(status, StatusCode::CREATED, "{second}");
    assert_ne!(second["id"], first["id"]);
    let blob = get_blob(&app, id).await;
    assert_eq!(blob["attachments"].as_array().unwrap().len(), 2);
    assert_eq!(blob["attachments"][0]["id"], first["id"], "oldest first");

    let (status, got) = call(
        &app.private,
        req(
            Method::GET,
            &format!("/attachments/{}", first["id"].as_str().unwrap()),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{got}");
    assert_eq!(got["owner_id"], "42");

    // Detaching one of two leaves the blob attached.
    assert_eq!(
        detach(&app, first["id"].as_str().unwrap()).await,
        StatusCode::NO_CONTENT
    );
    let blob = get_blob(&app, id).await;
    assert!(blob["expires_at"].is_null(), "{blob}");
    assert_eq!(blob["attachments"].as_array().unwrap().len(), 1);

    // The last detach re-arms the default expiry, from now.
    assert_eq!(
        detach(&app, second["id"].as_str().unwrap()).await,
        StatusCode::NO_CONTENT
    );
    let blob = get_blob(&app, id).await;
    assert!(
        about(expires_at(&blob).unwrap(), Duration::from_secs(24 * 3600)),
        "{blob}"
    );
    assert_eq!(blob["attachments"], json!([]));

    // Detaching twice is a 404, as is reading what is gone.
    assert_eq!(
        detach(&app, second["id"].as_str().unwrap()).await,
        StatusCode::NOT_FOUND
    );
    let (status, _) = call(
        &app.private,
        req(
            Method::GET,
            &format!("/attachments/{}", second["id"].as_str().unwrap()),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, _) = attach(
        &app,
        &uuid::Uuid::now_v7().to_string(),
        attachment("A", "1", "x"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn the_default_expiry_is_configurable() {
    let db = TestDatabase::new().await;
    let clamd = fake_clamd().await;
    let app = setup_tuned(db, clamd, |c| {
        c.blobs.default_expires_in = Duration::from_secs(3 * 3600);
    })
    .await;
    let created = create(&app, raw_blob()).await;
    assert!(
        about(expires_at(&created).unwrap(), Duration::from_secs(3 * 3600)),
        "{created}"
    );
    let mut own = raw_blob();
    own["expires_in"] = json!("2h");
    let created = create(&app, own).await;
    assert!(
        about(expires_at(&created).unwrap(), Duration::from_secs(7200)),
        "the request's own expiry wins: {created}"
    );
}

#[tokio::test]
async fn a_blob_created_attached_has_no_expiry() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let mut blob = raw_blob();
    blob["attachment"] = attachment("expense", "42", "receipt");
    let created = create(&app, blob).await;
    assert!(created["expires_at"].is_null(), "{created}");
    assert_eq!(created["attachments"].as_array().unwrap().len(), 1);
    assert_eq!(created["attachments"][0]["owner_id"], "42");
    assert_eq!(created["attachments"][0]["blob_id"], created["id"]);

    // An expiry makes no sense on a blob created attached.
    let mut blob = raw_blob();
    blob["attachment"] = attachment("expense", "42", "receipt");
    blob["expires_in"] = json!("1h");
    let (status, body) = call(&app.private, json_req(Method::POST, "/blobs", blob)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("created attached"),
        "{body}"
    );

    // The attachment is validated with the rest, and nothing is created.
    let mut blob = raw_blob();
    blob["attachment"] = attachment("expense", "", "receipt");
    let (status, body) = call(&app.private, json_req(Method::POST, "/blobs", blob)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(
        body["error"]["message"],
        "attachment: owner_id must not be empty"
    );
    let mut blob = raw_blob();
    blob["attachment"] = json!({"owner_kind": "expense", "owner_id": "1", "role": "x", "extra": 1});
    let (status, _) = call(&app.private, json_req(Method::POST, "/blobs", blob)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let (_, all) = call(&app.private, req(Method::GET, "/blobs")).await;
    assert_eq!(all["items"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn attachment_fields_are_validated() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let created = create(&app, raw_blob()).await;
    let id = id_of(&created);
    for (body, message) in [
        (attachment("", "1", "x"), "owner_kind must not be empty"),
        (
            attachment("T", "1\n", "x"),
            "owner_id must not contain control characters",
        ),
        (
            attachment("T", "1", &"n".repeat(256)),
            "role is too long (255 bytes at most)",
        ),
    ] {
        let (status, got) = attach(&app, id, body).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{got}");
        assert_eq!(got["error"]["code"], "unprocessable");
        assert_eq!(got["error"]["message"], message);
    }
    let (status, _) = attach(&app, id, json!({"owner_kind": "T", "owner_id": "1"})).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "role is required");
    let (status, _) = attach(
        &app,
        id,
        json!({"owner_kind": "T", "owner_id": "1", "role": "x", "blob_id": id}),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "unknown fields are refused"
    );
    assert_eq!(get_blob(&app, id).await["attachments"], json!([]));
}

#[tokio::test]
async fn delete_refuses_an_attached_blob() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let blob = ready_blob(&app, &png_bytes(100)).await;
    let id = id_of(&blob);
    let (status, attached) = attach(&app, id, attachment("expense", "1", "receipt")).await;
    assert_eq!(status, StatusCode::CREATED, "{attached}");

    let (status, body) = call(&app.private, req(Method::DELETE, &format!("/blobs/{id}"))).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"]["code"], "conflict");
    assert_eq!(
        body["error"]["message"],
        "blob is attached and cannot be deleted; detach it first"
    );
    let blob = get_blob(&app, id).await;
    assert_eq!(blob["state"], "ready");
    assert!(blob["deleted_at"].is_null());
    assert_eq!(jobs_for(&app, id).await, vec!["render"], "no purge");

    // Detach, then delete: the backend in a hurry.
    assert_eq!(
        detach(&app, attached["id"].as_str().unwrap()).await,
        StatusCode::NO_CONTENT
    );
    assert_eq!(delete(&app, id).await, StatusCode::NO_CONTENT);
    assert_eq!(get_blob(&app, id).await["state"], "deleted");
}

#[tokio::test]
async fn attach_is_refused_on_terminal_blobs() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;

    // Past its expiry, whether or not the reaper has recorded it.
    let created = create(&app, raw_blob()).await;
    sqlx::query("UPDATE blobs SET expires_at = now() - interval '1 second' WHERE id = $1")
        .bind(parse_id(id_of(&created)))
        .execute(app.state.pool())
        .await
        .unwrap();
    let (status, body) = attach(&app, id_of(&created), attachment("T", "1", "x")).await;
    assert_eq!(status, StatusCode::GONE, "{body}");
    assert_eq!(body["error"]["code"], "gone");
    assert_eq!(
        body["error"]["message"],
        "blob is expired and cannot be attached"
    );

    for (state, status) in [
        ("expired", StatusCode::GONE),
        ("deleted", StatusCode::GONE),
        ("infected", StatusCode::CONFLICT),
    ] {
        let created = create(&app, raw_blob()).await;
        sqlx::query("UPDATE blobs SET state = $2 WHERE id = $1")
            .bind(parse_id(id_of(&created)))
            .bind(state)
            .execute(app.state.pool())
            .await
            .unwrap();
        let (got, body) = attach(&app, id_of(&created), attachment("T", "1", "x")).await;
        assert_eq!(got, status, "{state}: {body}");
        assert_eq!(
            body["error"]["message"],
            format!("blob is {state} and cannot be attached")
        );
        assert_eq!(
            get_blob(&app, id_of(&created)).await["attachments"],
            json!([])
        );
    }
}

#[tokio::test]
async fn detaching_an_infected_blob_starts_its_purge_clock() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let created = create(&app, raw_blob()).await;
    let id = id_of(&created);
    let (status, attached) = attach(&app, id, attachment("expense", "1", "receipt")).await;
    assert_eq!(status, StatusCode::CREATED, "{attached}");
    // Infected after the fact, as a deferred scan would leave it; the
    // attachment stays so the owner's side can find out.
    sqlx::query("UPDATE blobs SET state = 'infected', storage_key = 'k/1' WHERE id = $1")
        .bind(parse_id(id))
        .execute(app.state.pool())
        .await
        .unwrap();
    let blob = get_blob(&app, id).await;
    assert!(blob["expires_at"].is_null(), "{blob}");
    assert_eq!(blob["attachments"].as_array().unwrap().len(), 1);

    assert_eq!(
        detach(&app, attached["id"].as_str().unwrap()).await,
        StatusCode::NO_CONTENT
    );
    let blob = get_blob(&app, id).await;
    assert_eq!(blob["state"], "infected");
    assert!(
        about(expires_at(&blob).unwrap(), Duration::from_secs(24 * 3600)),
        "{blob}"
    );
}

#[tokio::test]
async fn listing_filters_and_pages() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let a = create(&app, raw_blob()).await;
    let b = create(&app, raw_blob()).await;
    let c = create(&app, raw_blob()).await;
    let d = create(&app, raw_blob()).await;
    let mut ids = Vec::new();
    for (blob, owner_kind, owner_id, role) in [
        (&a, "expense", "1", "receipt"),
        (&a, "expense", "2", "receipt"),
        (&b, "expense", "1", "receipt"),
        (&c, "user", "1", "avatar"),
    ] {
        let (status, got) = attach(&app, id_of(blob), attachment(owner_kind, owner_id, role)).await;
        assert_eq!(status, StatusCode::CREATED, "{got}");
        ids.push(got["id"].as_str().unwrap().to_string());
    }
    sqlx::query("UPDATE blobs SET state = 'expired' WHERE id = $1")
        .bind(parse_id(id_of(&c)))
        .execute(app.state.pool())
        .await
        .unwrap();

    async fn list(app: &TestApp, query: &str) -> Value {
        let uri = format!("/attachments{query}");
        let (status, body) = call(&app.private, req(Method::GET, &uri)).await;
        assert_eq!(status, StatusCode::OK, "{uri}: {body}");
        body
    }
    let items = |body: &Value| -> Vec<String> {
        body["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a["id"].as_str().unwrap().to_string())
            .collect()
    };

    let all = list(&app, "").await;
    assert_eq!(
        items(&all),
        ids.iter().rev().cloned().collect::<Vec<_>>(),
        "newest first"
    );
    assert_eq!(all["items"][0]["blob_state"], "expired", "{all}");
    assert_eq!(all["items"][1]["blob_state"], "created");
    assert_eq!(all["items"][0]["owner_kind"], "user");
    assert!(all["next_cursor"].is_null());

    assert_eq!(
        items(&list(&app, "?owner_kind=expense").await),
        vec![ids[2].clone(), ids[1].clone(), ids[0].clone()]
    );
    assert_eq!(
        items(&list(&app, "?owner_kind=expense&owner_id=1").await),
        vec![ids[2].clone(), ids[0].clone()]
    );
    assert_eq!(
        items(&list(&app, "?owner_kind=expense&owner_id=1&role=receipt").await).len(),
        2
    );
    assert_eq!(
        items(&list(&app, "?owner_kind=user&role=avatar").await),
        vec![ids[3].clone()]
    );
    assert_eq!(
        items(&list(&app, &format!("?blob_id={}", id_of(&a))).await),
        vec![ids[1].clone(), ids[0].clone()]
    );
    assert!(items(&list(&app, "?owner_kind=Nothing").await).is_empty());

    let page1 = list(&app, "?limit=3").await;
    assert_eq!(items(&page1).len(), 3);
    let cursor = page1["next_cursor"].as_str().unwrap().to_string();
    let page2 = list(&app, &format!("?limit=3&cursor={cursor}")).await;
    assert_eq!(items(&page2), vec![ids[0].clone()]);
    assert!(page2["next_cursor"].is_null());

    for query in [
        "owner_id=1",
        "role=receipt",
        "limit=0",
        "limit=201",
        "blob_id=nope",
    ] {
        let (status, body) = call(
            &app.private,
            req(Method::GET, &format!("/attachments?{query}")),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{query}: {body}");
        assert_eq!(body["error"]["code"], "bad_request", "{query}");
    }

    // Blobs by whether anything uses them.
    let (status, attached) = call(&app.private, req(Method::GET, "/blobs?attached=true")).await;
    assert_eq!(status, StatusCode::OK, "{attached}");
    let names: Vec<&str> = attached["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["id"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec![id_of(&c), id_of(&b), id_of(&a)]);
    assert_eq!(
        attached["items"][2]["attachments"]
            .as_array()
            .unwrap()
            .len(),
        2,
        "{attached}"
    );
    let (_, free) = call(&app.private, req(Method::GET, "/blobs?attached=false")).await;
    assert_eq!(free["items"].as_array().unwrap().len(), 1);
    assert_eq!(free["items"][0]["id"], id_of(&d));
    let (status, _) = call(&app.private, req(Method::GET, "/blobs?attached=maybe")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// `attached` is answered by the blob row on an index of its own, never
/// by asking `attachments`: an anti-join there walks every attached blob
/// to fill a page of unattached ones. Sequential and bitmap scans are off
/// so that the planner, on a table this small, shows the index it would
/// walk in the cursor's order, with no sort.
#[tokio::test]
async fn the_attached_filter_walks_an_index_of_blobs() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let mut conn = app.state.pool().acquire().await.unwrap();
    for off in ["SET enable_seqscan = off", "SET enable_bitmapscan = off"] {
        sqlx::query(off).execute(&mut *conn).await.unwrap();
    }
    for (attached, index) in [
        (true, "blobs_attached_idx"),
        (false, "blobs_unattached_idx"),
    ] {
        for cursor in [None, Some(uuid::Uuid::now_v7())] {
            let params = repo::ListParams {
                attached: Some(attached),
                cursor,
                limit: repo::DEFAULT_LIMIT,
                ..Default::default()
            };
            let plan = repo::explain_list(&mut *conn, &params).await.unwrap();
            assert!(plan.contains(index), "attached={attached}: {plan}");
            assert!(!plan.contains("attachments"), "attached={attached}: {plan}");
            assert!(!plan.contains("Sort"), "attached={attached}: {plan}");
        }
    }
}

/// A blob shared by many owners inlines the first hundred, says it was
/// cut short, and the listing by blob pages through them all.
#[tokio::test]
async fn a_widely_shared_blob_inlines_its_first_attachments() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let mut blob = raw_blob();
    blob["attachment"] = attachment("expense", "0", "receipt");
    let created = create(&app, blob).await;
    let id = id_of(&created);
    assert_eq!(created["attachments_truncated"], false);
    sqlx::query(
        "INSERT INTO attachments (id, blob_id, owner_kind, owner_id, role) \
         SELECT gen_random_uuid(), $1, 'expense', n::text, 'receipt' \
         FROM generate_series(1, 100) AS n",
    )
    .bind(parse_id(id))
    .execute(app.state.pool())
    .await
    .unwrap();

    let got = get_blob(&app, id).await;
    assert_eq!(got["attachments"].as_array().unwrap().len(), 100);
    assert_eq!(got["attachments_truncated"], true);
    let mut listed = 0;
    let mut cursor = String::new();
    loop {
        let uri = format!("/attachments?blob_id={id}&limit=50{cursor}");
        let (status, page) = call(&app.private, req(Method::GET, &uri)).await;
        assert_eq!(status, StatusCode::OK, "{page}");
        listed += page["items"].as_array().unwrap().len();
        match page["next_cursor"].as_str() {
            Some(next) => cursor = format!("&cursor={next}"),
            None => break,
        }
    }
    assert_eq!(listed, 101);
}

/// Every filter shape the listing accepts walks an index of
/// `attachments`: `role` comes only with `owner_kind` or `blob_id`, since
/// no index leads with it. Sequential and bitmap scans are off so that,
/// on a table this small, the planner shows the index it would walk.
#[tokio::test]
async fn every_attachment_listing_walks_an_index() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let mut conn = app.state.pool().acquire().await.unwrap();
    for off in ["SET enable_seqscan = off", "SET enable_bitmapscan = off"] {
        sqlx::query(off).execute(&mut *conn).await.unwrap();
    }
    let kind = || Some("expense".to_owned());
    let id = || Some("1".to_owned());
    let role = || Some("receipt".to_owned());
    let blob = || Some(uuid::Uuid::now_v7());
    let shapes = [
        (kind(), None, None, None),
        (kind(), id(), None, None),
        (kind(), None, role(), None),
        (kind(), id(), role(), None),
        (None, None, None, blob()),
        (None, None, role(), blob()),
    ];
    for (owner_kind, owner_id, role, blob_id) in shapes {
        let params = agrafe::attachment::ListParams {
            owner_kind,
            owner_id,
            role,
            blob_id,
            limit: agrafe::attachment::DEFAULT_LIMIT,
            ..Default::default()
        };
        let plan = agrafe::attachment::explain_list(&mut *conn, &params)
            .await
            .unwrap();
        assert!(plan.contains("attachments_"), "{params:?}: {plan}");
        assert!(
            !plan.contains("Seq Scan on attachments"),
            "{params:?}: {plan}"
        );
    }
}

#[tokio::test]
async fn the_public_port_never_sees_attachments() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let mut blob = raw_blob();
    blob["attachment"] = attachment("expense", "42", "receipt");
    let created = create(&app, blob).await;
    let (status, body) = call(
        &app.public,
        req(Method::GET, &public_url(&created, "metadata")),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.get("attachments").is_none(), "{body}");
    assert!(body.get("expires_at").is_none(), "{body}");
    let (status, _) = call(&app.public, req(Method::GET, "/attachments")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// An attach and the reaper's expiry contend for the blob row: whichever
/// locks it first wins, and the other sees the outcome. Here the expiry
/// holds the lock while the attach waits, then finds the blob expired,
/// although it was still alive when the attach began.
#[tokio::test]
async fn attach_waits_for_the_expiry_lock_and_sees_its_outcome() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let mut blob = raw_blob();
    blob["expires_in"] = json!("2s");
    let created = create(&app, blob).await;
    let id = parse_id(id_of(&created));

    let mut tx = app.state.pool().begin().await.unwrap();
    sqlx::query("SELECT id FROM blobs WHERE id = $1 FOR UPDATE")
        .bind(id)
        .execute(&mut *tx)
        .await
        .unwrap();

    let state = app.state.clone();
    let attaching = tokio::spawn(async move {
        state
            .attach_blob(
                id,
                agrafe::attachment::AttachmentRequest {
                    owner_kind: "expense".into(),
                    owner_id: "1".into(),
                    role: "receipt".into(),
                    replace: false,
                },
            )
            .await
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!attaching.is_finished(), "the attach waits on the row lock");

    // The reaper, as of a moment past the blob's expiry.
    let expired = repo::expire(
        &mut tx,
        Timestamp::now() + SignedDuration::from_secs(10),
        reap::BATCH,
    )
    .await
    .unwrap();
    assert_eq!(expired, vec![id]);
    tx.commit().await.unwrap();

    let err = within("the attach", attaching).await.unwrap().unwrap_err();
    assert!(
        matches!(
            err,
            agrafe::attachment::AttachError::Terminal(agrafe::blob::BlobState::Expired)
        ),
        "{err}"
    );
    let blob = get_blob(&app, id_of(&created)).await;
    assert_eq!(blob["state"], "expired");
    assert_eq!(blob["attachments"], json!([]));
}

/// Two detaches of one attachment contend for the blob row: the one that
/// waits finds the attachment gone and says so, rather than claiming a
/// detach it did not do.
#[tokio::test]
async fn a_detach_that_lost_the_race_finds_nothing() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let created = create(&app, raw_blob()).await;
    let id = parse_id(id_of(&created));
    let (status, attached) =
        attach(&app, id_of(&created), attachment("expense", "1", "receipt")).await;
    assert_eq!(status, StatusCode::CREATED, "{attached}");
    let attachment_id = parse_id(attached["id"].as_str().unwrap());

    let mut tx = app.state.pool().begin().await.unwrap();
    sqlx::query("SELECT id FROM blobs WHERE id = $1 FOR UPDATE")
        .bind(id)
        .execute(&mut *tx)
        .await
        .unwrap();

    let state = app.state.clone();
    let detaching = tokio::spawn(async move { state.detach_blob(attachment_id).await });
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!detaching.is_finished(), "the detach waits on the row lock");

    // The detach holding the lock takes the row.
    sqlx::query("DELETE FROM attachments WHERE id = $1")
        .bind(attachment_id)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();

    assert!(
        !within("the detach", detaching).await.unwrap().unwrap(),
        "nothing left to detach"
    );
    assert_eq!(
        get_blob(&app, id_of(&created)).await["attachments"],
        json!([])
    );
}

/// The facets are the distinct owner kinds and roles, sorted, each role
/// once however many kinds use it, from a walk capped at `MAX_FACETS`
/// pairs.
#[tokio::test]
async fn facets_walk_the_distinct_pairs() {
    let db = TestDatabase::new().await;
    use agrafe::attachment::MAX_FACETS;

    let app = setup(db).await;
    let facets = app.state.attachment_facets().await.unwrap();
    assert!(facets.owner_kinds.is_empty() && facets.roles.is_empty());

    let a = create(&app, raw_blob()).await;
    let b = create(&app, raw_blob()).await;
    for (blob, owner_kind, owner_id, role) in [
        (&a, "user", "1", "avatar"),
        (&a, "expense", "1", "receipt"),
        (&b, "expense", "2", "receipt"),
        (&b, "expense", "2", "avatar"),
        (&b, "message", "9", "attachment"),
    ] {
        let (status, got) = attach(&app, id_of(blob), attachment(owner_kind, owner_id, role)).await;
        assert_eq!(status, StatusCode::CREATED, "{got}");
    }
    let facets = app.state.attachment_facets().await.unwrap();
    assert_eq!(facets.owner_kinds, ["expense", "message", "user"]);
    assert_eq!(facets.roles, ["attachment", "avatar", "receipt"]);

    // One kind with more roles than the cap: the walk stops at the cap,
    // in order, and the kinds after it are not seen.
    sqlx::query(
        "INSERT INTO attachments (id, blob_id, owner_kind, owner_id, role) \
         SELECT gen_random_uuid(), $1, 'bulk', '1', 'n' || lpad(i::text, 4, '0') \
         FROM generate_series(1, $2) AS i",
    )
    .bind(parse_id(id_of(&a)))
    .bind(MAX_FACETS as i32 + 10)
    .execute(app.state.pool())
    .await
    .unwrap();
    let facets = app.state.attachment_facets().await.unwrap();
    assert_eq!(facets.owner_kinds, ["bulk"]);
    assert_eq!(facets.roles.len(), MAX_FACETS);
    assert_eq!(facets.roles[0], "n0001");
}

fn replacing(owner_kind: &str, owner_id: &str, role: &str) -> Value {
    let mut body = attachment(owner_kind, owner_id, role);
    body["replace"] = json!(true);
    body
}

/// The blob ids attached at an owner and role, as the listing has them.
async fn blobs_at(app: &TestApp, owner_kind: &str, owner_id: &str, role: &str) -> Vec<String> {
    let (status, page) = call(
        &app.private,
        req(
            Method::GET,
            &format!("/attachments?owner_kind={owner_kind}&owner_id={owner_id}&role={role}"),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    let mut ids: Vec<String> = page["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["blob_id"].as_str().unwrap().to_string())
        .collect();
    ids.sort();
    ids
}

#[tokio::test]
async fn replace_leaves_one_blob_at_the_address() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let a = create(&app, raw_blob()).await;
    let b = create(&app, raw_blob()).await;
    let c = create(&app, raw_blob()).await;
    let (_, at_a) = attach(&app, id_of(&a), attachment("expense", "42", "receipt")).await;
    let (_, at_b) = attach(&app, id_of(&b), attachment("expense", "42", "receipt")).await;
    // `a` is used elsewhere too, `b` only here.
    attach(&app, id_of(&a), attachment("user", "1", "avatar")).await;
    // The same role of another owner is another address.
    attach(&app, id_of(&b), attachment("expense", "43", "receipt")).await;
    let (status, plain) = attach(&app, id_of(&b), attachment("expense", "44", "receipt")).await;
    assert_eq!(status, StatusCode::CREATED);
    assert!(
        plain.get("detached").is_none(),
        "only with replace: {plain}"
    );

    let (status, got) = attach(&app, id_of(&c), replacing("expense", "42", "receipt")).await;
    assert_eq!(status, StatusCode::CREATED, "{got}");
    assert_eq!(got["blob_id"], c["id"]);
    let detached: Vec<&Value> = got["detached"].as_array().unwrap().iter().collect();
    assert_eq!(detached.len(), 2, "{got}");
    assert_eq!(detached[0]["id"], at_a["id"], "oldest first");
    assert_eq!(detached[1]["id"], at_b["id"]);
    assert_eq!(
        blobs_at(&app, "expense", "42", "receipt").await,
        vec![id_of(&c).to_string()]
    );
    assert_eq!(blobs_at(&app, "expense", "43", "receipt").await.len(), 1);
    assert_eq!(blobs_at(&app, "user", "1", "avatar").await.len(), 1);

    // The last detach of each re-arms its expiry; one still used elsewhere
    // keeps none, and the new blob has none.
    assert!(get_blob(&app, id_of(&a)).await["expires_at"].is_null());
    assert!(get_blob(&app, id_of(&b)).await["expires_at"].is_null());
    assert!(get_blob(&app, id_of(&c)).await["expires_at"].is_null());
    let (_, got) = attach(&app, id_of(&c), replacing("expense", "43", "receipt")).await;
    assert_eq!(got["detached"].as_array().unwrap().len(), 1, "{got}");
    let (_, got) = attach(&app, id_of(&c), replacing("expense", "44", "receipt")).await;
    assert_eq!(got["detached"].as_array().unwrap().len(), 1, "{got}");
    let blob = get_blob(&app, id_of(&b)).await;
    assert!(
        about(expires_at(&blob).unwrap(), Duration::from_secs(24 * 3600)),
        "{blob}"
    );

    // Idempotent: the blob already alone at the address is kept as is.
    let (status, again) = attach(&app, id_of(&c), replacing("expense", "42", "receipt")).await;
    assert_eq!(status, StatusCode::OK, "{again}");
    assert_eq!(again["detached"], json!([]));
    // Already there among others: kept, and the others go.
    attach(&app, id_of(&a), attachment("expense", "42", "receipt")).await;
    let (status, kept) = attach(&app, id_of(&c), replacing("expense", "42", "receipt")).await;
    assert_eq!(status, StatusCode::OK, "{kept}");
    assert_eq!(kept["id"], again["id"]);
    assert_eq!(kept["detached"][0]["blob_id"], a["id"]);
    assert_eq!(
        blobs_at(&app, "expense", "42", "receipt").await,
        vec![id_of(&c).to_string()]
    );
}

#[tokio::test]
async fn a_refused_replace_detaches_nothing() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let old = create(&app, raw_blob()).await;
    attach(&app, id_of(&old), attachment("expense", "42", "receipt")).await;

    let infected = create(&app, raw_blob()).await;
    sqlx::query("UPDATE blobs SET state = 'infected' WHERE id = $1")
        .bind(parse_id(id_of(&infected)))
        .execute(app.state.pool())
        .await
        .unwrap();
    let (status, body) = attach(
        &app,
        id_of(&infected),
        replacing("expense", "42", "receipt"),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    let (status, _) = attach(
        &app,
        &uuid::Uuid::now_v7().to_string(),
        replacing("expense", "42", "receipt"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = attach(&app, id_of(&old), replacing("expense", "", "receipt")).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        blobs_at(&app, "expense", "42", "receipt").await,
        vec![id_of(&old).to_string()]
    );
}

#[tokio::test]
async fn a_blob_created_attached_can_replace() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let old = create(&app, raw_blob()).await;
    attach(&app, id_of(&old), attachment("user", "7", "avatar")).await;

    let mut blob = raw_blob();
    blob["attachment"] = replacing("user", "7", "avatar");
    let created = create(&app, blob).await;
    assert!(created["expires_at"].is_null(), "{created}");
    assert_eq!(
        blobs_at(&app, "user", "7", "avatar").await,
        vec![id_of(&created).to_string()]
    );
    let old = get_blob(&app, id_of(&old)).await;
    assert_eq!(old["attachments"], json!([]));
    assert!(
        about(expires_at(&old).unwrap(), Duration::from_secs(24 * 3600)),
        "{old}"
    );
}

/// Past `MAX_REPLACED` other blobs, a replace is refused rather than
/// made unbounded, on attach and on creation alike, and nothing changes.
#[tokio::test]
async fn replace_is_capped() {
    let db = TestDatabase::new().await;
    use agrafe::attachment::MAX_REPLACED;

    let app = setup(db).await;
    let template = create(&app, raw_blob()).await;
    // Copies of one blob row, each attached at the address.
    sqlx::query(
        "WITH copies AS ( \
             INSERT INTO blobs \
             SELECT (jsonb_populate_record(b, jsonb_build_object('id', gen_random_uuid()))).* \
             FROM blobs b, generate_series(1, $2) WHERE b.id = $1 \
             RETURNING id \
         ) \
         INSERT INTO attachments (id, blob_id, owner_kind, owner_id, role) \
         SELECT gen_random_uuid(), id, 'gallery', '1', 'photo' FROM copies",
    )
    .bind(parse_id(id_of(&template)))
    .bind(MAX_REPLACED as i32 + 1)
    .execute(app.state.pool())
    .await
    .unwrap();

    let (status, body) = attach(&app, id_of(&template), replacing("gallery", "1", "photo")).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(
        body["error"]["message"],
        format!("more than {MAX_REPLACED} blobs to replace at this owner and role")
    );
    let mut blob = raw_blob();
    blob["attachment"] = replacing("gallery", "1", "photo");
    let (status, body) = call(&app.private, json_req(Method::POST, "/blobs", blob)).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(
        blobs_at(&app, "gallery", "1", "photo").await.len(),
        agrafe::attachment::DEFAULT_LIMIT,
        "a full first page, nothing detached"
    );
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM attachments")
        .fetch_one(app.state.pool())
        .await
        .unwrap();
    assert_eq!(count, MAX_REPLACED as i64 + 1);
}

/// Two replaces of one address are serialised: each finds the other's
/// blob, if it came first, and detaches it, so one blob is left. Without
/// the address lock, both would read the address before either inserted
/// and both blobs would stay.
#[tokio::test]
async fn two_replaces_of_one_address_leave_one_blob() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let old = create(&app, raw_blob()).await;
    attach(&app, id_of(&old), attachment("user", "7", "avatar")).await;
    let first = create(&app, raw_blob()).await;
    let second = create(&app, raw_blob()).await;

    // Hold the old blob's row: the first replace takes the address, then
    // waits on that row; the second waits on the address.
    let mut tx = app.state.pool().begin().await.unwrap();
    sqlx::query("SELECT id FROM blobs WHERE id = $1 FOR UPDATE")
        .bind(parse_id(id_of(&old)))
        .execute(&mut *tx)
        .await
        .unwrap();
    let replace = |blob: &Value| {
        let state = app.state.clone();
        let id = parse_id(id_of(blob));
        tokio::spawn(async move {
            state
                .attach_blob(
                    id,
                    agrafe::attachment::AttachmentRequest {
                        owner_kind: "user".into(),
                        owner_id: "7".into(),
                        role: "avatar".into(),
                        replace: true,
                    },
                )
                .await
        })
    };
    let one = replace(&first);
    tokio::time::sleep(Duration::from_millis(200)).await;
    let two = replace(&second);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!one.is_finished() && !two.is_finished(), "both wait");
    tx.commit().await.unwrap();

    within("the first replace", one).await.unwrap().unwrap();
    let two = within("the second replace", two).await.unwrap().unwrap();
    assert_eq!(two.detached.len(), 1, "the second detaches the first");
    assert_eq!(two.detached[0].blob_id, parse_id(id_of(&first)));
    assert_eq!(
        blobs_at(&app, "user", "7", "avatar").await,
        vec![id_of(&second).to_string()]
    );
    for blob in [&old, &first] {
        assert!(get_blob(&app, id_of(blob)).await["expires_at"].is_string());
    }
}

/// A replace reads the address, then waits for the blob rows. A blob
/// detached and attached again there meanwhile has a new attachment row;
/// its attach committed before the replace took its row, so the replace
/// detaches it all the same, by address rather than by the rows it read.
#[tokio::test]
async fn a_replace_detaches_a_blob_reattached_while_it_waited() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let old = create(&app, raw_blob()).await;
    let (_, first_row) = attach(&app, id_of(&old), attachment("user", "7", "avatar")).await;
    let new = create(&app, raw_blob()).await;

    // Hold the old blob's row; the replace reads the address, then waits
    // on it. Meanwhile, what a detach and an attach of the old blob at
    // the same address do: its row goes, a new one comes.
    let mut tx = app.state.pool().begin().await.unwrap();
    sqlx::query("SELECT id FROM blobs WHERE id = $1 FOR UPDATE")
        .bind(parse_id(id_of(&old)))
        .execute(&mut *tx)
        .await
        .unwrap();
    let router = app.private.clone();
    let new_id = id_of(&new).to_string();
    let replace = tokio::spawn(async move {
        call(
            &router,
            json_req(
                Method::POST,
                &format!("/blobs/{new_id}/attachments"),
                replacing("user", "7", "avatar"),
            ),
        )
        .await
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!replace.is_finished(), "waits on the row");
    sqlx::query("DELETE FROM attachments WHERE id = $1")
        .bind(parse_id(first_row["id"].as_str().unwrap()))
        .execute(&mut *tx)
        .await
        .unwrap();
    let second_row: uuid::Uuid = sqlx::query_scalar(
        "INSERT INTO attachments (id, blob_id, owner_kind, owner_id, role) \
         VALUES ($1, $2, 'user', '7', 'avatar') RETURNING id",
    )
    .bind(uuid::Uuid::now_v7())
    .bind(parse_id(id_of(&old)))
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let (status, got) = within("the replace", replace).await.unwrap();
    assert_eq!(status, StatusCode::CREATED, "{got}");
    assert_eq!(got["detached"].as_array().unwrap().len(), 1, "{got}");
    assert_eq!(got["detached"][0]["id"], second_row.to_string());
    assert_eq!(
        blobs_at(&app, "user", "7", "avatar").await,
        vec![id_of(&new).to_string()]
    );
    assert!(get_blob(&app, id_of(&old)).await["expires_at"].is_string());
}
