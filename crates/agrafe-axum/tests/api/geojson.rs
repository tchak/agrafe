//! Maps for GeoJSON files: the geometry over a flat background or over
//! the layers of a WMS, here a local stub; a provider that fails; the JSON
//! upload that turns out to be GeoJSON; the files that hold no map.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agrafe::jobs::JobKind;
use agrafe_render::geo::{Layer, Metres};
use axum::Router;
use axum::extract::{Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde_json::Value;
use tokio::net::TcpListener;

use crate::support::*;

/// Run the render job for every blob that has one queued.
async fn render_all(app: &TestApp) {
    while run_job(app, JobKind::Render).await {}
}

/// How the stub WMS answers a `GetMap`.
#[derive(Clone, Copy)]
enum Answer {
    /// A black PNG of the requested size.
    Png,
    /// A `ServiceException`, with a `200`, as the Géoplateforme does.
    Xml,
    /// Nothing, for longer than the fetch timeout.
    Hang,
    /// `image/png` whose bytes start as a PNG and end as noise.
    Garbage,
    /// A PNG far larger than `max_layer_size`.
    Huge,
    /// A black PNG of half the requested size, as a WMS that caps or
    /// rounds the size answers.
    Half,
}

type Requests = Arc<Mutex<Vec<HashMap<String, String>>>>;

/// A WMS stub: the URL template to configure, and what it was asked.
struct Wms {
    url: String,
    requests: Requests,
}

impl Wms {
    async fn start(answer: Answer) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let requests: Requests = Arc::default();
        let app = Router::new()
            .route("/wms", get(get_map))
            .with_state((answer, requests.clone()));
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self {
            url: format!(
                "http://{addr}/wms?SERVICE=WMS&REQUEST=GetMap&CRS=EPSG:3857&BBOX={{bbox}}&WIDTH={{width}}&HEIGHT={{height}}"
            ),
            requests,
        }
    }

    fn layer(&self) -> Layer {
        Layer {
            url: self.url.clone(),
            attribution: Some("© stub".into()),
            min_span: None,
            max_span: None,
        }
    }

    fn requests(&self) -> Vec<HashMap<String, String>> {
        self.requests.lock().unwrap().clone()
    }
}

async fn get_map(
    State((answer, requests)): State<(Answer, Requests)>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    requests.lock().unwrap().push(params.clone());
    let (width, height) = (params["WIDTH"].as_str(), params["HEIGHT"].as_str());
    match answer {
        Answer::Png => {
            ([(header::CONTENT_TYPE, "image/png")], png(width, height).await).into_response()
        }
        Answer::Xml => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "text/xml")],
            r#"<?xml version="1.0"?><ServiceExceptionReport><ServiceException code="LayerNotDefined"/></ServiceExceptionReport>"#,
        )
            .into_response(),
        Answer::Hang => {
            tokio::time::sleep(Duration::from_secs(5)).await;
            ([(header::CONTENT_TYPE, "image/png")], png(width, height).await).into_response()
        }
        Answer::Garbage => {
            let mut bytes = png("4", "4").await;
            bytes.truncate(24);
            bytes.extend((0..200u32).map(|i| (i * 37 % 251) as u8));
            ([(header::CONTENT_TYPE, "image/png")], bytes).into_response()
        }
        Answer::Huge => {
            let mut bytes = png("4", "4").await;
            bytes.extend(std::iter::repeat_n(0u8, 100 * 1024));
            ([(header::CONTENT_TYPE, "image/png")], bytes).into_response()
        }
        Answer::Half => {
            let half = |side: &str| (side.parse::<u32>().unwrap() / 2).to_string();
            (
                [(header::CONTENT_TYPE, "image/png")],
                png(&half(width), &half(height)).await,
            )
                .into_response()
        }
    }
}

/// A black PNG of `width` by `height`, from vips, which the tests need
/// on the `PATH` anyway.
async fn png(width: &str, height: &str) -> Vec<u8> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("layer.png");
    let status = tokio::process::Command::new("vips")
        .arg("black")
        .arg(&path)
        .args([width, height, "--bands", "3"])
        .status()
        .await
        .unwrap();
    assert!(status.success());
    tokio::fs::read(&path).await.unwrap()
}

/// The rendered representation: `200`, WebP, with its size.
async fn rendered(app: &TestApp, uri: &str) -> (u32, u32) {
    let (status, headers, body) = fetch(&app.private, get_with(uri, &[])).await;
    assert_eq!(status, StatusCode::OK, "{uri}");
    assert_eq!(headers["content-type"], "image/webp");
    assert_eq!(headers["x-representation-state"], "ready");
    webp_size(&body)
}

/// A ready blob of `fixture`, declared as `content_type`.
async fn geojson_blob(app: &TestApp, fixture_name: &str, content_type: &str) -> Value {
    ready_file(app, fixture_name, content_type, &fixture(fixture_name)).await
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn geojson_files_get_a_map_preview_and_thumbnail(pool: PgPool) {
    let app = setup(pool).await;
    let parcel = geojson_blob(&app, "polygon-hole.geojson", "application/geo+json").await;
    let styled = geojson_blob(&app, "styled.geojson", "application/geo+json").await;
    let hostile = geojson_blob(&app, "hostile-style.geojson", "application/geo+json").await;
    let world = geojson_blob(&app, "world.geojson", "application/geo+json").await;
    let lambert = geojson_blob(&app, "lambert93.geojson", "application/geo+json").await;
    render_all(&app).await;
    for blob in [&parcel, &styled, &hostile, &world, &lambert] {
        let id = id_of(blob);
        let name = blob["filename"].as_str().unwrap();
        let blob = get_blob(&app, id).await;
        assert_eq!(blob["content_type"], "application/geo+json", "{name}");
        assert_eq!(blob["category"], "text", "{name}");
        assert_eq!(blob["preview_state"], "ready", "{name}");
        assert_eq!(blob["preview_source"], "geojson", "{name}");
        assert_eq!(blob["thumbnail_state"], "ready", "{name}");
        // The map is the preview box, square, at the test's 400 pixels,
        // since no layer caps it lower; the thumbnail is fitted from it.
        let size = rendered(&app, &format!("/blobs/{id}/preview/{name}")).await;
        assert_eq!(size, (400, 400), "{name}");
        let size = rendered(&app, &format!("/blobs/{id}/thumbnail/{name}")).await;
        assert_eq!(size, (256, 256), "{name}");
        assert_eq!(jobs_for(&app, id).await, Vec::<String>::new(), "{name}");
    }
    // Two different files give two different maps.
    let preview_of = |blob: &Value| {
        format!(
            "/blobs/{}/preview/{}",
            id_of(blob),
            blob["filename"].as_str().unwrap()
        )
    };
    let (_, _, a) = fetch(&app.private, get_with(&preview_of(&parcel), &[])).await;
    let (_, _, b) = fetch(&app.private, get_with(&preview_of(&world), &[])).await;
    assert_ne!(a, b);
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn the_map_is_capped_at_max_size(pool: PgPool) {
    let clamd = fake_clamd().await;
    let app = setup_tuned(pool, clamd, |c| c.media.render.geojson.max_size = 300).await;
    let point = geojson_blob(&app, "point.geojson", "application/geo+json").await;
    render_all(&app).await;
    let id = id_of(&point);
    let size = rendered(&app, &format!("/blobs/{id}/preview/point.geojson")).await;
    assert_eq!(size, (300, 300));
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn the_background_comes_from_the_layers_that_apply(pool: PgPool) {
    let wms = Wms::start(Answer::Png).await;
    let clamd = fake_clamd().await;
    let app = setup_tuned(pool, clamd, |c| {
        let photo = wms.layer();
        let names = Layer {
            attribution: None,
            min_span: Some(Metres(3000.0)),
            ..wms.layer()
        };
        c.media.render.geojson.layers = vec![photo, names];
    })
    .await;
    // A parcel is well under 3 km: one request. The world: two.
    let parcel = geojson_blob(&app, "polygon-hole.geojson", "application/geo+json").await;
    render_all(&app).await;
    let requests = wms.requests();
    assert_eq!(requests.len(), 1, "{requests:?}");
    let request = &requests[0];
    assert_eq!(request["WIDTH"], "400");
    assert_eq!(request["HEIGHT"], "400");
    assert_eq!(request["CRS"], "EPSG:3857");
    let bbox: Vec<f64> = request["BBOX"]
        .split(',')
        .map(|n| n.parse().unwrap())
        .collect();
    assert_eq!(bbox.len(), 4, "{request:?}");
    // Lyon, in Web Mercator metres, padded: a square a few hundred metres wide.
    assert!(bbox[0] > 530_000.0 && bbox[2] < 545_000.0, "{bbox:?}");
    assert!(bbox[1] > 5_735_000.0 && bbox[3] < 5_750_000.0, "{bbox:?}");
    let span = bbox[2] - bbox[0];
    assert!((200.0..3000.0).contains(&span), "{span}");
    assert!((span - (bbox[3] - bbox[1])).abs() < 1e-3, "square");
    let id = id_of(&parcel);
    let blob = get_blob(&app, id).await;
    assert_eq!(blob["preview_state"], "ready");
    assert_eq!(blob["preview_source"], "geojson");
    rendered(&app, &format!("/blobs/{id}/preview/polygon-hole.geojson")).await;

    let world = geojson_blob(&app, "world.geojson", "application/geo+json").await;
    render_all(&app).await;
    assert_eq!(wms.requests().len(), 3);
    let id = id_of(&world);
    assert_eq!(get_blob(&app, id).await["preview_state"], "ready");
}

/// A layer that comes back at another size than asked still covers the
/// bounding box: it is fitted to the map, which keeps the size the drawing
/// was made for, instead of taking the layer's size.
#[sqlx::test(migrator = "MIGRATOR")]
async fn a_layer_of_another_size_is_fitted_to_the_map(pool: PgPool) {
    let wms = Wms::start(Answer::Half).await;
    let clamd = fake_clamd().await;
    let app = setup_tuned(pool, clamd, |c| {
        c.media.render.geojson.layers = vec![wms.layer()];
    })
    .await;
    let point = geojson_blob(&app, "point.geojson", "application/geo+json").await;
    render_all(&app).await;
    let requests = wms.requests();
    assert_eq!(requests.len(), 1, "{requests:?}");
    assert_eq!(requests[0]["WIDTH"], "400");
    let id = id_of(&point);
    assert_eq!(get_blob(&app, id).await["preview_state"], "ready");
    let size = rendered(&app, &format!("/blobs/{id}/preview/point.geojson")).await;
    assert_eq!(size, (400, 400));
}

/// A provider that fails is not the file's fault: the attempt fails, the
/// representations stay pending and the job is retried.
async fn provider_fault(pool: PgPool, answer: Answer, expected: &str) {
    let wms = Wms::start(answer).await;
    let clamd = fake_clamd().await;
    let app = setup_tuned(pool, clamd, |c| {
        c.media.render.geojson.layers = vec![wms.layer()];
        c.media.render.geojson.fetch_timeout = Duration::from_millis(500);
        c.media.render.geojson.max_layer_size = bytesize::ByteSize::kib(16);
    })
    .await;
    let point = geojson_blob(&app, "point.geojson", "application/geo+json").await;
    let id = id_of(&point);
    assert!(run_job(&app, JobKind::Render).await);
    let blob = get_blob(&app, id).await;
    assert_eq!(blob["preview_state"], "pending");
    assert_eq!(blob["thumbnail_state"], "pending");
    assert_eq!(jobs_for(&app, id).await, ["render"]);
    let error = last_error(&app, id).await;
    assert!(error.contains(expected), "{error}");
    assert!(error.contains("from 127.0.0.1"), "{error}");
    // The extent never reaches the log line.
    assert!(!error.contains("BBOX"), "{error}");
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn a_service_exception_fails_the_attempt(pool: PgPool) {
    provider_fault(pool, Answer::Xml, "answered `text/xml`, not an image").await;
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn a_provider_timeout_fails_the_attempt(pool: PgPool) {
    provider_fault(pool, Answer::Hang, "requesting layer 0 from 127.0.0.1").await;
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn an_undecodable_layer_fails_the_attempt(pool: PgPool) {
    provider_fault(pool, Answer::Garbage, "an image vips could not decode").await;
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn an_oversized_layer_fails_the_attempt(pool: PgPool) {
    provider_fault(pool, Answer::Huge, "answered more than 16.0 KiB").await;
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn a_geojson_declared_as_json_or_text_is_refined(pool: PgPool) {
    let app = setup(pool).await;
    let as_json = geojson_blob(&app, "polygon-hole.geojson", "application/json").await;
    let as_text = geojson_blob(&app, "point.geojson", "text/plain").await;
    let plain_json = ready_file(&app, "config.json", "application/json", b"{\"a\": 1}\n").await;
    // The refinement is on the row as soon as the upload completes.
    assert_eq!(as_json["content_type"], "application/geo+json");
    assert_eq!(as_text["content_type"], "application/geo+json");
    assert_eq!(plain_json["content_type"], "application/json");
    render_all(&app).await;
    for blob in [&as_json, &as_text] {
        let id = id_of(blob);
        let blob = get_blob(&app, id).await;
        assert_eq!(blob["content_type"], "application/geo+json");
        assert_eq!(blob["category"], "text");
        assert_eq!(blob["preview_state"], "ready");
        assert_eq!(blob["preview_source"], "geojson");
    }
    let plain = get_blob(&app, id_of(&plain_json)).await;
    assert_eq!(plain["preview_state"], "unavailable");
    assert_eq!(plain["thumbnail_state"], "ready");
    // The original is served with the refined type.
    let id = id_of(&as_json);
    let (status, headers, _) = fetch(
        &app.private,
        get_with(&format!("/blobs/{id}/original/polygon-hole.geojson"), &[]),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "application/geo+json");

    // Declared as something a GeoJSON is not.
    let bytes = fixture("point.geojson");
    let mut blob = blob_for(&bytes, "image/png", "raw");
    blob["skip_virus_scan"] = serde_json::json!(true);
    let created = create(&app, blob).await;
    let (status, body) = put_private(&app, id_of(&created), bytes).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(
        body["error"]["message"],
        "content looks like `application/geo+json`, not `image/png`"
    );
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn files_without_a_map_get_what_a_text_file_gets(pool: PgPool) {
    let app = setup(pool).await;
    // No geometry at all: nothing to draw, the text sample as thumbnail.
    let empty = geojson_blob(&app, "empty.geojson", "application/geo+json").await;
    // A coordinate that is not degrees on Earth, as in a Lambert-93 file:
    // nothing this helper can place, so the same as no geometry.
    let off = geojson_blob(&app, "off-earth.geojson", "application/geo+json").await;
    render_all(&app).await;
    for (blob, preview) in [(&empty, "unavailable"), (&off, "unavailable")] {
        let id = id_of(blob);
        let name = blob["filename"].as_str().unwrap();
        let blob = get_blob(&app, id).await;
        assert_eq!(blob["preview_state"], preview, "{name}");
        assert_eq!(blob["preview_source"], Value::Null, "{name}");
        assert_eq!(blob["thumbnail_state"], "ready", "{name}");
        let size = rendered(&app, &format!("/blobs/{id}/thumbnail/{name}")).await;
        assert_eq!(size, (256, 256), "{name}");
        assert_eq!(jobs_for(&app, id).await, Vec::<String>::new(), "{name}");
    }
}
