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

fn attachment(record_type: &str, record_id: &str, name: &str) -> Value {
    json!({"record_type": record_type, "record_id": record_id, "name": name})
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
    let app = setup().await;
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

    let (status, first) = attach(&app, id, attachment("Dossier", "42", "piece")).await;
    assert_eq!(status, StatusCode::CREATED, "{first}");
    assert_eq!(first["blob_id"], id);
    assert_eq!(first["record_type"], "Dossier");
    assert_eq!(first["record_id"], "42");
    assert_eq!(first["name"], "piece");
    assert!(first["created_at"].is_string());
    let blob = get_blob(&app, id).await;
    assert!(blob["expires_at"].is_null(), "{blob}");
    assert_eq!(blob["attachments"].as_array().unwrap().len(), 1);
    assert_eq!(blob["attachments"][0]["id"], first["id"]);

    // Idempotent: the same triple hands back the same attachment.
    let (status, again) = attach(&app, id, attachment("Dossier", "42", "piece")).await;
    assert_eq!(status, StatusCode::OK, "{again}");
    assert_eq!(again["id"], first["id"]);
    assert_eq!(
        get_blob(&app, id).await["attachments"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    // Another record, another attachment.
    let (status, second) = attach(&app, id, attachment("User", "7", "avatar")).await;
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
    assert_eq!(got["record_id"], "42");

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
    let clamd = fake_clamd().await;
    let app = setup_tuned(clamd, |c| {
        c.blobs.default_expires_in = Duration::from_secs(600);
    })
    .await;
    let created = create(&app, raw_blob()).await;
    assert!(
        about(expires_at(&created).unwrap(), Duration::from_secs(600)),
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
    let app = setup().await;
    let mut blob = raw_blob();
    blob["attachment"] = attachment("Dossier", "42", "piece");
    let created = create(&app, blob).await;
    assert!(created["expires_at"].is_null(), "{created}");
    assert_eq!(created["attachments"].as_array().unwrap().len(), 1);
    assert_eq!(created["attachments"][0]["record_id"], "42");
    assert_eq!(created["attachments"][0]["blob_id"], created["id"]);

    // An expiry makes no sense on a blob created attached.
    let mut blob = raw_blob();
    blob["attachment"] = attachment("Dossier", "42", "piece");
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
    blob["attachment"] = attachment("Dossier", "", "piece");
    let (status, body) = call(&app.private, json_req(Method::POST, "/blobs", blob)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(
        body["error"]["message"],
        "attachment: record_id must not be empty"
    );
    let mut blob = raw_blob();
    blob["attachment"] =
        json!({"record_type": "Dossier", "record_id": "1", "name": "x", "extra": 1});
    let (status, _) = call(&app.private, json_req(Method::POST, "/blobs", blob)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let (_, all) = call(&app.private, req(Method::GET, "/blobs")).await;
    assert_eq!(all["items"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn attachment_fields_are_validated() {
    let app = setup().await;
    let created = create(&app, raw_blob()).await;
    let id = id_of(&created);
    for (body, message) in [
        (attachment("", "1", "x"), "record_type must not be empty"),
        (
            attachment("T", "1\n", "x"),
            "record_id must not contain control characters",
        ),
        (
            attachment("T", "1", &"n".repeat(256)),
            "name is too long (255 bytes at most)",
        ),
    ] {
        let (status, got) = attach(&app, id, body).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{got}");
        assert_eq!(got["error"]["code"], "unprocessable");
        assert_eq!(got["error"]["message"], message);
    }
    let (status, _) = attach(&app, id, json!({"record_type": "T", "record_id": "1"})).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "name is required");
    let (status, _) = attach(
        &app,
        id,
        json!({"record_type": "T", "record_id": "1", "name": "x", "blob_id": id}),
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
    let app = setup().await;
    let blob = ready_blob(&app, &png_bytes(100)).await;
    let id = id_of(&blob);
    let (status, attached) = attach(&app, id, attachment("Dossier", "1", "piece")).await;
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
    let app = setup().await;

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
    let app = setup().await;
    let created = create(&app, raw_blob()).await;
    let id = id_of(&created);
    let (status, attached) = attach(&app, id, attachment("Dossier", "1", "piece")).await;
    assert_eq!(status, StatusCode::CREATED, "{attached}");
    // Infected after the fact, as a deferred scan would leave it; the
    // attachment stays so the record's side can find out.
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
    let app = setup().await;
    let a = create(&app, raw_blob()).await;
    let b = create(&app, raw_blob()).await;
    let c = create(&app, raw_blob()).await;
    let d = create(&app, raw_blob()).await;
    let mut ids = Vec::new();
    for (blob, record_type, record_id, name) in [
        (&a, "Dossier", "1", "piece"),
        (&a, "Dossier", "2", "piece"),
        (&b, "Dossier", "1", "piece"),
        (&c, "User", "1", "avatar"),
    ] {
        let (status, got) =
            attach(&app, id_of(blob), attachment(record_type, record_id, name)).await;
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
    assert_eq!(all["items"][0]["record_type"], "User");
    assert!(all["next_cursor"].is_null());

    assert_eq!(
        items(&list(&app, "?record_type=Dossier").await),
        vec![ids[2].clone(), ids[1].clone(), ids[0].clone()]
    );
    assert_eq!(
        items(&list(&app, "?record_type=Dossier&record_id=1").await),
        vec![ids[2].clone(), ids[0].clone()]
    );
    assert_eq!(
        items(&list(&app, "?record_type=Dossier&record_id=1&name=piece").await).len(),
        2
    );
    assert_eq!(
        items(&list(&app, "?name=avatar").await),
        vec![ids[3].clone()]
    );
    assert_eq!(
        items(&list(&app, &format!("?blob_id={}", id_of(&a))).await),
        vec![ids[1].clone(), ids[0].clone()]
    );
    assert!(items(&list(&app, "?record_type=Nothing").await).is_empty());

    let page1 = list(&app, "?limit=3").await;
    assert_eq!(items(&page1).len(), 3);
    let cursor = page1["next_cursor"].as_str().unwrap().to_string();
    let page2 = list(&app, &format!("?limit=3&cursor={cursor}")).await;
    assert_eq!(items(&page2), vec![ids[0].clone()]);
    assert!(page2["next_cursor"].is_null());

    for query in ["record_id=1", "limit=0", "limit=201", "blob_id=nope"] {
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

#[tokio::test]
async fn the_public_port_never_sees_attachments() {
    let app = setup().await;
    let mut blob = raw_blob();
    blob["attachment"] = attachment("Dossier", "42", "piece");
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
    let app = setup().await;
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
                    record_type: "Dossier".into(),
                    record_id: "1".into(),
                    name: "piece".into(),
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

    let err = attaching.await.unwrap().unwrap_err();
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
    let app = setup().await;
    let created = create(&app, raw_blob()).await;
    let id = parse_id(id_of(&created));
    let (status, attached) =
        attach(&app, id_of(&created), attachment("Dossier", "1", "piece")).await;
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

    assert!(!detaching.await.unwrap().unwrap(), "nothing left to detach");
    assert_eq!(
        get_blob(&app, id_of(&created)).await["attachments"],
        json!([])
    );
}
