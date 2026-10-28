//! Attachments: attach and detach, the expiry handed from the default to
//! the first attachment and back on the last detach, delete refusing an
//! attached blob, the listing and its filters, and the blob row lock that
//! orders an attach against an expiry.

use agrafe::blob::repo;
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
        items(&list(&app, "?role=avatar").await),
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

    for query in ["owner_id=1", "limit=0", "limit=201", "blob_id=nope"] {
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
                },
            )
            .await
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!attaching.is_finished(), "the attach waits on the row lock");

    // The reaper, as of a moment past the blob's expiry.
    let expired = repo::expire(&mut tx, Timestamp::now() + SignedDuration::from_secs(10))
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
