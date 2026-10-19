//! Creating, listing, minting URLs for and deleting blobs; the public
//! port's view of them.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::json;

use crate::support::*;

#[sqlx::test(migrator = "MIGRATOR")]
async fn create_and_get_blob(pool: PgPool) {
    let app = setup(pool).await;
    let (status, body) = call(&app.private, json_req(Method::POST, "/blobs", raw_blob())).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["state"], "created");
    assert_eq!(body["category"], "image");
    assert_eq!(body["scan_state"], "pending");
    assert_eq!(body["public_upload_allowed"], true);
    assert!(body["urls"]["upload"].is_string());
    assert!(
        body["urls"]["original"]
            .as_str()
            .unwrap()
            .ends_with("/original/photo%20%C3%A9t%C3%A9.jpg")
    );

    let id = id_of(&body);
    let (status, got) = call(&app.private, req(Method::GET, &format!("/blobs/{id}"))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(got["id"], body["id"]);
    assert_eq!(got["checksum"], STANDARD.encode([1u8; 32]));

    let (status, _) = call(
        &app.private,
        req(Method::GET, &format!("/blobs/{}", uuid::Uuid::now_v7())),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn validation_errors(pool: PgPool) {
    let app = setup(pool).await;
    let mut no_checksum = raw_blob();
    no_checksum.as_object_mut().unwrap().remove("checksum");
    let (status, body) = call(&app.private, json_req(Method::POST, "/blobs", no_checksum)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["error"]["code"], "unprocessable");

    let (status, _) = call(
        &app.private,
        json_req(
            Method::POST,
            "/blobs",
            json!({"filename": "x", "content_type": "bad", "input_type": "raw"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    let (status, _) = call(
        &app.private,
        json_req(
            Method::POST,
            "/blobs",
            json!({"filename": "export.csv", "content_type": "text/csv", "input_type": "rows", "size": 10, "columns": [{"name": "a"}]}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    let (status, body) = call(
        &app.private,
        json_req(
            Method::POST,
            "/blobs",
            json!({"filename": "export.csv", "content_type": "text/csv", "input_type": "rows", "columns": [{"name": "a"}]}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["category"], "spreadsheet");
    assert_eq!(body["public_upload_allowed"], false);
    assert!(body["urls"]["upload"].is_null());
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn private_only_blobs_get_no_public_upload(pool: PgPool) {
    let app = setup(pool).await;
    let mut b = raw_blob();
    b["skip_virus_scan"] = json!(true);
    let (status, body) = call(&app.private, json_req(Method::POST, "/blobs", b)).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["public_upload_allowed"], false);
    assert!(body["urls"]["upload"].is_null());
    assert_eq!(body["scan_state"], "skipped");

    // A forged upload URL for such a blob is refused by the handler too.
    let id = id_of(&body);
    let token = app
        .state
        .signer()
        .sign(&Method::PUT, unix_now() + 3600, &format!("/{id}"));
    let (status, body) = call(
        &app.public,
        req(Method::PUT, &format!("/blobs/{token}/{id}")),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn public_signed_metadata(pool: PgPool) {
    let app = setup(pool).await;
    let (_, created) = call(&app.private, json_req(Method::POST, "/blobs", raw_blob())).await;
    let metadata_url = public_url(&created, "metadata");

    let (status, body) = call(&app.public, req(Method::GET, &metadata_url)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["state"], "created");
    assert!(
        body.get("filename").is_none(),
        "public metadata must not leak filenames"
    );

    // HEAD is accepted with a GET signature.
    let (status, _) = call(&app.public, req(Method::HEAD, &metadata_url)).await;
    assert_eq!(status, StatusCode::OK);

    // Tampered path: 403.
    let other = uuid::Uuid::now_v7();
    let tampered = format!("{}/{other}", metadata_url.rsplit_once('/').unwrap().0);
    let (status, body) = call(&app.public, req(Method::GET, &tampered)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    // Added query parameter: 403.
    let (status, _) = call(
        &app.public,
        req(Method::GET, &format!("{metadata_url}?x=1")),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // Wrong method for the signature: 403.
    let (status, _) = call(&app.public, req(Method::PUT, &metadata_url)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // No signature at all: the route does not even exist on the public port.
    let id = id_of(&created);
    let (status, _) = call(&app.public, req(Method::GET, &format!("/blobs/{id}"))).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Expired: 410.
    let expired = app.state.signer().sign(&Method::GET, 1, &format!("/{id}"));
    let (status, _) = call(
        &app.public,
        req(Method::GET, &format!("/blobs/{expired}/{id}")),
    )
    .await;
    assert_eq!(status, StatusCode::GONE);

    // Valid signature on a missing blob: 404.
    let tok = app
        .state
        .signer()
        .sign(&Method::GET, unix_now() + 3600, &format!("/{other}"));
    let (status, _) = call(
        &app.public,
        req(Method::GET, &format!("/blobs/{tok}/{other}")),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Signed with the key, but for longer than signing.ttl.max allows:
    // refused, so the ceiling binds whoever holds the key.
    let tok = app
        .state
        .signer()
        .sign(&Method::GET, unix_now() + 9 * 24 * 3600, &format!("/{id}"));
    let (status, body) = call(&app.public, req(Method::GET, &format!("/blobs/{tok}/{id}"))).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("not valid yet"),
        "{body}"
    );
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn terminal_blobs_are_404_on_the_public_port(pool: PgPool) {
    let app = setup(pool).await;
    for state in ["expired", "infected"] {
        let (_, created) = call(&app.private, json_req(Method::POST, "/blobs", raw_blob())).await;
        let id = parse_id(id_of(&created));
        sqlx::query("UPDATE blobs SET state = $2 WHERE id = $1")
            .bind(id)
            .bind(state)
            .execute(app.state.pool())
            .await
            .unwrap();

        let metadata_url = public_url(&created, "metadata");
        let (status, body) = call(&app.public, req(Method::GET, &metadata_url)).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{state}: {body}");
        let upload_url = public_url(&created, "upload");
        let (status, _) = call(&app.public, req(Method::PUT, &upload_url)).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{state}");

        // The private port still reports the real state.
        let (status, body) = call(&app.private, req(Method::GET, &format!("/blobs/{id}"))).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["state"], state);
    }
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn blobs_past_expires_at_are_expired_before_the_reaper_runs(pool: PgPool) {
    let app = setup(pool).await;
    let ready = ready_blob(&app, &png_bytes(100)).await;
    let created = create(&app, raw_blob()).await;
    for blob in [&ready, &created] {
        sqlx::query("UPDATE blobs SET expires_at = now() - interval '1 second' WHERE id = $1")
            .bind(parse_id(id_of(blob)))
            .execute(app.state.pool())
            .await
            .unwrap();
    }

    // The public port treats them as gone.
    for url in [
        public_url(&ready, "metadata"),
        public_url(&created, "metadata"),
    ] {
        let (status, body) = call(&app.public, req(Method::GET, &url)).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{url}: {body}");
    }
    // The sealed original URL minted before the expiry still answers until
    // the purge takes the object: nothing on that path checks the row. One
    // minted now expires with the blob, which is already.
    let (status, _) = call(
        &app.public,
        req(Method::GET, &public_url(&ready, "original")),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let ready = get_blob(&app, id_of(&ready)).await;
    assert!(ready["envelope"].is_null());
    let url = public_url(&ready, "original");
    assert!(!url.contains("e="), "no envelope once expired: {url}");
    let (status, body) = call(&app.public, req(Method::GET, &url)).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{url}: {body}");
    let (status, _) = call(
        &app.public,
        put_raw(&public_url(&created, "upload"), vec![0; 1024]),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // The private port refuses to read or upload them, and says why.
    let id = id_of(&ready);
    let (status, body) = call(
        &app.private,
        req(Method::GET, &format!("/blobs/{id}/original/photo.png")),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"]["message"], "blob is expired and not readable");
    let (status, body) = put_private(&app, id_of(&created), vec![0; 1024]).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(
        body["error"]["message"],
        "blob is expired and cannot be uploaded"
    );

    // Metadata keeps reporting the stored state until the reaper records it.
    let (status, body) = call(&app.private, req(Method::GET, &format!("/blobs/{id}"))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["state"], "ready");
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn mint_urls_with_options(pool: PgPool) {
    let app = setup(pool).await;
    let (_, created) = call(&app.private, json_req(Method::POST, "/blobs", raw_blob())).await;
    let id = id_of(&created);

    let (status, body) = call(
        &app.private,
        json_req(
            Method::POST,
            &format!("/blobs/{id}/urls"),
            json!({"ttl": "10m", "query": {"disposition": "attachment"}}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // The TTL counts from the next 1h window boundary.
    let expires = body["expires_at"].as_u64().unwrap();
    let now = unix_now();
    assert!(
        (now + 600..=now + 600 + 3600).contains(&expires),
        "{expires} for now {now}"
    );
    assert!(
        created["urls"]["expires_at"].as_u64().unwrap() >= now + 24 * 3600,
        "the default read TTL is longer"
    );
    let original = body["original"].as_str().unwrap();
    assert!(original.ends_with("?disposition=attachment"), "{original}");
    // Representation routes accept the signed query string; the blob has no
    // bytes yet, so the answer is a conflict rather than a signature error.
    let (status, _) = call(&app.public, req(Method::GET, &public_path(original))).await;
    assert_eq!(status, StatusCode::CONFLICT);

    // The TTL is capped by signing.ttl.max, seven days by default, and
    // there is no way to ask for a permanent URL.
    for opts in [json!({"ttl": "8d"}), json!({"permanent": true})] {
        let (status, body) = call(
            &app.private,
            json_req(Method::POST, &format!("/blobs/{id}/urls"), opts.clone()),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{opts}: {body}");
    }
    let (status, body) = call(
        &app.private,
        json_req(
            Method::POST,
            &format!("/blobs/{id}/urls"),
            json!({"ttl": "7d"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // Query parameters are checked when minted, so a signed URL never
    // answers a client error for its own query.
    for (query, needle) in [
        (json!({"disposition": "weird"}), "disposition"),
        (json!({"filename": "a/b.pdf"}), "filename"),
        (json!({"filename": ""}), "filename"),
        (json!({"wait": "soon"}), "wait"),
        (json!({"wait": "60000"}), "wait"),
        (json!({"ttl": "500"}), "unknown query parameter `ttl`"),
    ] {
        let (status, body) = call(
            &app.private,
            json_req(
                Method::POST,
                &format!("/blobs/{id}/urls"),
                json!({"query": query}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{query}: {body}");
        let message = body["error"]["message"].as_str().unwrap();
        assert!(message.contains(needle), "{query}: {body}");
    }

    // Minting is stable inside a window.
    let (_, a) = call(
        &app.private,
        req(Method::POST, &format!("/blobs/{id}/urls")),
    )
    .await;
    let (_, b) = call(
        &app.private,
        req(Method::POST, &format!("/blobs/{id}/urls")),
    )
    .await;
    assert_eq!(a, b);
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn list_filters_and_pagination(pool: PgPool) {
    let app = setup(pool).await;
    for i in 0..5 {
        let mut b = raw_blob();
        b["filename"] = json!(format!("f{i}.jpg"));
        if i % 2 == 0 {
            b["content_type"] = json!("application/pdf");
        }
        let (status, _) = call(&app.private, json_req(Method::POST, "/blobs", b)).await;
        assert_eq!(status, StatusCode::CREATED);
    }

    let (status, page1) = call(&app.private, req(Method::GET, "/blobs?limit=2")).await;
    assert_eq!(status, StatusCode::OK, "{page1}");
    assert_eq!(page1["items"].as_array().unwrap().len(), 2);
    assert_eq!(page1["items"][0]["filename"], "f4.jpg", "newest first");
    let cursor = page1["next_cursor"].as_str().unwrap();

    let (_, page2) = call(
        &app.private,
        req(Method::GET, &format!("/blobs?limit=2&cursor={cursor}")),
    )
    .await;
    assert_eq!(page2["items"][0]["filename"], "f2.jpg");
    let cursor = page2["next_cursor"].as_str().unwrap();
    let (_, page3) = call(
        &app.private,
        req(Method::GET, &format!("/blobs?limit=2&cursor={cursor}")),
    )
    .await;
    assert_eq!(page3["items"].as_array().unwrap().len(), 1);
    assert!(page3["next_cursor"].is_null());

    let (_, pdfs) = call(&app.private, req(Method::GET, "/blobs?category=document")).await;
    assert_eq!(pdfs["items"].as_array().unwrap().len(), 3);
    let (_, jpgs) = call(
        &app.private,
        req(Method::GET, "/blobs?content_type=image%2Fjpeg"),
    )
    .await;
    assert_eq!(jpgs["items"].as_array().unwrap().len(), 2);
    let (_, none) = call(&app.private, req(Method::GET, "/blobs?state=ready")).await;
    assert_eq!(none["items"].as_array().unwrap().len(), 0);
    let (status, body) = call(&app.private, req(Method::GET, "/blobs?state=bogus")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "bad_request", "{body}");

    // Out-of-range or malformed filters are rejected, not silently adjusted.
    for query in ["limit=0", "limit=201", "content_type=bogus"] {
        let (status, body) = call(&app.private, req(Method::GET, &format!("/blobs?{query}"))).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{query}: {body}");
        assert_eq!(body["error"]["code"], "bad_request", "{query}");
    }
    let (status, _) = call(&app.private, req(Method::GET, "/blobs?limit=200")).await;
    assert_eq!(status, StatusCode::OK);
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn every_error_uses_the_json_envelope(pool: PgPool) {
    let app = setup(pool).await;

    // Malformed JSON body.
    let (status, body) = call(
        &app.private,
        Request::builder()
            .method(Method::POST)
            .uri("/blobs")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from("{not json"))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"]["code"], "bad_request");

    // Missing content type.
    let (status, body) = call(&app.private, req(Method::POST, "/blobs")).await;
    assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE, "{body}");
    assert_eq!(body["error"]["code"], "unsupported_media_type");

    // Unknown field: serde data error.
    let (status, body) = call(
        &app.private,
        json_req(Method::POST, "/blobs", json!({"bogus": 1})),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"]["code"], "unprocessable");

    // Path parameter that is not a UUID.
    let (status, body) = call(&app.private, req(Method::GET, "/blobs/not-a-uuid")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"]["code"], "bad_request");

    // Unknown route and wrong method, on both ports.
    for router in [&app.private, &app.public] {
        let (status, body) = call(router, req(Method::GET, "/nope")).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
        assert_eq!(body["error"]["code"], "not_found");
        let (status, body) = call(router, req(Method::DELETE, "/healthz")).await;
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED, "{body}");
        assert_eq!(body["error"]["code"], "method_not_allowed");
    }
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn delete_is_soft_and_idempotent(pool: PgPool) {
    let app = setup(pool).await;
    let (_, created) = call(&app.private, json_req(Method::POST, "/blobs", raw_blob())).await;
    let id = id_of(&created);

    assert_eq!(delete(&app, id).await, StatusCode::NO_CONTENT);
    assert_eq!(delete(&app, id).await, StatusCode::NO_CONTENT);
    let (status, _) = call(
        &app.private,
        req(Method::DELETE, &format!("/blobs/{}", uuid::Uuid::now_v7())),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, body) = call(&app.private, req(Method::GET, &format!("/blobs/{id}"))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["state"], "deleted");
    assert!(body["deleted_at"].is_string());

    // Deleted blobs vanish from the public port.
    let metadata_url = public_url(&created, "metadata");
    let (status, _) = call(&app.public, req(Method::GET, &metadata_url)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let upload_url = public_url(&created, "upload");
    let (status, _) = call(&app.public, req(Method::PUT, &upload_url)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Upload is refused.
    let (status, _) = call(&app.private, req(Method::PUT, &format!("/blobs/{id}"))).await;
    assert_eq!(status, StatusCode::CONFLICT);

    // Nothing was stored, so no purge job.
    let jobs: i64 = sqlx::query_scalar("SELECT count(*) FROM jobs")
        .fetch_one(app.state.pool())
        .await
        .unwrap();
    assert_eq!(jobs, 0);
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn delete_enqueues_purge_when_stored(pool: PgPool) {
    let app = setup(pool).await;
    let mut blob = raw_blob();
    blob["priority"] = json!("high");
    let (_, created) = call(&app.private, json_req(Method::POST, "/blobs", blob)).await;
    let id = parse_id(id_of(&created));
    sqlx::query("UPDATE blobs SET state = 'ready', storage_key = 'k/1' WHERE id = $1")
        .bind(id)
        .execute(app.state.pool())
        .await
        .unwrap();

    assert_eq!(delete(&app, id).await, StatusCode::NO_CONTENT);
    let (kind, blob_id, priority, in_secs): (String, uuid::Uuid, i16, f64) = sqlx::query_as(
        "SELECT kind, blob_id, priority, EXTRACT(EPOCH FROM run_at - now())::float8 FROM jobs",
    )
    .fetch_one(app.state.pool())
    .await
    .unwrap();
    assert_eq!(kind, "purge");
    assert_eq!(blob_id, id);
    assert_eq!(
        priority, 0,
        "a purge runs at normal whatever the blob's priority"
    );
    assert!((1700.0..=1800.0).contains(&in_secs), "{in_secs}");
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn state_machine_transitions(pool: PgPool) {
    use agrafe::blob::BlobState;
    use agrafe::blob::repo::{self, Transition};

    let app = setup(pool).await;
    let (_, created) = call(&app.private, json_req(Method::POST, "/blobs", raw_blob())).await;
    let id = parse_id(id_of(&created));
    let pool = app.state.pool();
    let blob = repo::find(pool, id).await.unwrap().unwrap();

    let mut conn = pool.acquire().await.unwrap();
    let Transition::Done(uploading) = repo::begin_upload(&mut conn, &blob).await.unwrap() else {
        panic!("first upload should start");
    };
    assert_eq!(uploading.state, BlobState::Uploading);
    assert!(uploading.upload_started_at.is_some());
    assert!(
        uploading.updated_at > blob.updated_at,
        "trigger bumps updated_at"
    );

    // A concurrent PUT loses.
    assert!(matches!(
        repo::begin_upload(&mut conn, &blob).await.unwrap(),
        Transition::Conflict(_)
    ));

    // Failure hands the write back to `created`, but only the upload that
    // began it may do so: `upload_started_at` is its claim on the row.
    let started = uploading.upload_started_at.unwrap();
    let stranger = started - std::time::Duration::from_secs(1);
    assert!(matches!(
        repo::abort_upload(&mut conn, id, stranger).await.unwrap(),
        Transition::Conflict(_)
    ));
    assert!(!repo::touch_upload(&mut *conn, id, stranger).await.unwrap());
    assert!(repo::touch_upload(&mut *conn, id, started).await.unwrap());
    let Transition::Done(after_abort) = repo::abort_upload(&mut conn, id, started).await.unwrap()
    else {
        panic!("abort should apply");
    };
    assert_eq!(after_abort.state, BlobState::Created);
    assert!(matches!(
        repo::begin_upload(&mut conn, &blob).await.unwrap(),
        Transition::Done(_)
    ));

    // Once ready, no upload may start: blobs are write-once.
    repo::transition(&mut conn, id, &[BlobState::Uploading], BlobState::Uploaded)
        .await
        .unwrap();
    repo::transition(&mut conn, id, &[BlobState::Uploaded], BlobState::Ready)
        .await
        .unwrap();
    let ready = repo::find(pool, id).await.unwrap().unwrap();
    assert!(matches!(
        repo::begin_upload(&mut conn, &ready).await.unwrap(),
        Transition::Conflict(_)
    ));
    assert!(matches!(
        repo::abort_upload(&mut conn, id, started).await.unwrap(),
        Transition::Conflict(_)
    ));

    assert!(matches!(
        repo::transition(
            &mut conn,
            uuid::Uuid::now_v7(),
            &[BlobState::Created],
            BlobState::Deleted
        )
        .await
        .unwrap(),
        Transition::Missing
    ));
}

/// The repair from the blob's side: `POST /blobs/:id/retry` enqueues
/// whatever the blob's state waits on, supersedes the dead job of that
/// kind and clears the failure on the row. It needs no job id, so it works
/// once the dead job is pruned, and says `409` when there is nothing to do.
#[sqlx::test(migrator = "MIGRATOR")]
async fn retry_enqueues_what_the_blob_waits_on(pool: PgPool) {
    let app = setup(pool).await;
    let pool = app.state.pool();
    async fn retry(app: &TestApp, id: String) -> (StatusCode, serde_json::Value) {
        call(
            &app.private,
            req(Method::POST, &format!("/blobs/{id}/retry")),
        )
        .await
    }

    // A `created` blob waits on no job; an unknown id is unknown.
    let created = create(&app, raw_blob()).await;
    let id = id_of(&created).to_string();
    let (status, body) = retry(&app, id.clone()).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"]["code"], "conflict");
    let (status, _) = retry(&app, uuid::Uuid::now_v7().to_string()).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // An `uploaded` blob whose scan died, as the runner leaves it.
    sqlx::query(
        "UPDATE blobs SET state = 'uploaded', failed_at = now(), last_error = 'clamd: boom' \
         WHERE id = $1",
    )
    .bind(parse_id(&id))
    .execute(pool)
    .await
    .unwrap();
    let dead: i64 = sqlx::query_scalar(
        "INSERT INTO jobs (kind, blob_id, attempts, failed_at, locked_at, locked_by, last_error) \
         VALUES ('scan', $1, 5, now(), now(), 'w', 'clamd: boom') RETURNING id",
    )
    .bind(parse_id(&id))
    .fetch_one(pool)
    .await
    .unwrap();

    // The retry queues a scan, drops the dead job and clears the verdict.
    let (status, body) = retry(&app, id.clone()).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(body["state"], "uploaded");
    assert!(body["failed_at"].is_null(), "{body}");
    assert!(body["last_error"].is_null(), "{body}");
    let jobs: Vec<(i64, Option<jiff_sqlx::Timestamp>)> =
        sqlx::query_as("SELECT id, failed_at FROM jobs WHERE blob_id = $1")
            .bind(parse_id(&id))
            .fetch_all(pool)
            .await
            .unwrap();
    assert_eq!(jobs.len(), 1, "{jobs:?}");
    assert_ne!(jobs[0].0, dead, "the dead job is gone");
    assert!(jobs[0].1.is_none(), "the new one is pending");
    assert_eq!(jobs_for(&app, &id).await, vec!["scan"]);

    // With that job pending, a second retry has nothing to add; nor while
    // it runs, when the pending slot is free but the work is under way.
    let (status, body) = retry(&app, id.clone()).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    sqlx::query("UPDATE jobs SET locked_at = now(), locked_by = 'w' WHERE blob_id = $1")
        .bind(parse_id(&id))
        .execute(pool)
        .await
        .unwrap();
    let (status, body) = retry(&app, id.clone()).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(jobs_for(&app, &id).await, vec!["scan"], "no twin");

    // A `ready` blob with a representation pending and no job to render
    // it, the dead render pruned long ago: the retry is what gets it going.
    let ready = ready_blob(&app, &png_bytes(100)).await;
    let ready_id = id_of(&ready).to_string();
    assert_eq!(ready["thumbnail_state"], "pending");
    sqlx::query("DELETE FROM jobs WHERE blob_id = $1")
        .bind(parse_id(&ready_id))
        .execute(pool)
        .await
        .unwrap();
    let (status, body) = retry(&app, ready_id.clone()).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(jobs_for(&app, &ready_id).await, vec!["render"]);

    // A `ready` blob with its representations settled waits on nothing.
    let bytes = png_bytes(100);
    let mut settled = blob_for(&bytes, "image/png", "raw");
    settled["skip_virus_scan"] = json!(true);
    settled["representations"] = json!([]);
    let settled = create(&app, settled).await;
    let settled_id = id_of(&settled).to_string();
    let (status, body) = put_private(&app, &settled_id, bytes).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["state"], "ready");
    let (status, body) = retry(&app, settled_id).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");

    // Gone is gone, whether or not the reaper has recorded it.
    let expired = create(&app, raw_blob()).await;
    let expired_id = id_of(&expired).to_string();
    sqlx::query(
        "UPDATE blobs SET state = 'uploaded', expires_at = now() - interval '1 second' \
         WHERE id = $1",
    )
    .bind(parse_id(&expired_id))
    .execute(pool)
    .await
    .unwrap();
    let (status, body) = retry(&app, expired_id).await;
    assert_eq!(status, StatusCode::GONE, "{body}");
}
