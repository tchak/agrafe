//! Maps for GeoJSON, GPX, KML and KMZ files: the geometry over a flat
//! background or over the layers of a WMS, here a local stub; a provider
//! that fails; the JSON upload that turns out to be GeoJSON, the XML one
//! that turns out to be GPX or KML and the ZIP one that turns out to be
//! KMZ; the files that hold no map.

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
    /// A redirect to the same request on this host, under another path.
    Moved,
    /// A redirect to the same request on another host.
    Elsewhere(std::net::SocketAddr),
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
            .route(
                "/moved",
                get(get_map).with_state((Answer::Png, requests.clone())),
            )
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
    uri: axum::http::Uri,
) -> Response {
    requests.lock().unwrap().push(params.clone());
    let query = uri.query().unwrap_or_default();
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
        Answer::Moved => {
            axum::response::Redirect::temporary(&format!("/moved?{query}")).into_response()
        }
        Answer::Elsewhere(addr) => {
            axum::response::Redirect::temporary(&format!("http://{addr}/wms?{query}"))
                .into_response()
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
async fn map_blob(app: &TestApp, fixture_name: &str, content_type: &str) -> Value {
    ready_file(app, fixture_name, content_type, &fixture(fixture_name)).await
}

#[tokio::test]
async fn geojson_files_get_a_map_preview_and_thumbnail() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let parcel = map_blob(&app, "polygon-hole.geojson", "application/geo+json").await;
    let styled = map_blob(&app, "styled.geojson", "application/geo+json").await;
    let hostile = map_blob(&app, "hostile-style.geojson", "application/geo+json").await;
    let world = map_blob(&app, "world.geojson", "application/geo+json").await;
    let lambert = map_blob(&app, "lambert93.geojson", "application/geo+json").await;
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

#[tokio::test]
async fn the_map_is_capped_at_max_size() {
    let db = TestDatabase::new().await;
    let clamd = fake_clamd().await;
    let app = setup_tuned(db, clamd, |c| c.media.render.map.max_size = 300).await;
    let point = map_blob(&app, "point.geojson", "application/geo+json").await;
    render_all(&app).await;
    let id = id_of(&point);
    let size = rendered(&app, &format!("/blobs/{id}/preview/point.geojson")).await;
    assert_eq!(size, (300, 300));
}

#[tokio::test]
async fn the_background_comes_from_the_layers_that_apply() {
    let db = TestDatabase::new().await;
    let wms = Wms::start(Answer::Png).await;
    let clamd = fake_clamd().await;
    let app = setup_tuned(db, clamd, |c| {
        let photo = wms.layer();
        let names = Layer {
            attribution: None,
            min_span: Some(Metres(3000.0)),
            ..wms.layer()
        };
        c.media.render.map.layers = vec![photo, names];
    })
    .await;
    // A parcel is well under 3 km: one request.
    let parcel = map_blob(&app, "polygon-hole.geojson", "application/geo+json").await;
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

    // A box past the world square, the world's padded, asks for none.
    let world = map_blob(&app, "world.geojson", "application/geo+json").await;
    render_all(&app).await;
    assert_eq!(wms.requests().len(), 1);
    let id = id_of(&world);
    assert_eq!(get_blob(&app, id).await["preview_state"], "ready");
}

/// A layer that comes back at another size than asked still covers the
/// bounding box: it is fitted to the map, which keeps the size the drawing
/// was made for, instead of taking the layer's size.
#[tokio::test]
async fn a_layer_of_another_size_is_fitted_to_the_map() {
    let db = TestDatabase::new().await;
    let wms = Wms::start(Answer::Half).await;
    let clamd = fake_clamd().await;
    let app = setup_tuned(db, clamd, |c| {
        c.media.render.map.layers = vec![wms.layer()];
    })
    .await;
    let point = map_blob(&app, "point.geojson", "application/geo+json").await;
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
async fn provider_fault(db: TestDatabase, answer: Answer, expected: &str) {
    let wms = Wms::start(answer).await;
    let clamd = fake_clamd().await;
    let app = setup_tuned(db, clamd, |c| {
        c.media.render.map.layers = vec![wms.layer()];
        c.media.render.map.fetch_timeout = Duration::from_millis(500);
        c.media.render.map.max_layer_size = bytesize::ByteSize::kib(16);
    })
    .await;
    let point = map_blob(&app, "point.geojson", "application/geo+json").await;
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

/// An extent that does not parse is the helper's output gone wrong, so
/// the file's fault like one that fails the checks: the map is rejected,
/// not retried.
#[tokio::test]
async fn an_extent_that_does_not_parse_rejects_the_map() {
    let db = TestDatabase::new().await;
    use std::os::unix::fs::PermissionsExt;
    let helper = std::env::temp_dir().join(format!("agrafe-bad-extent-{}", uuid::Uuid::now_v7()));
    // --input $2 --spec $4 --svg $6 --extent $8
    std::fs::write(
        &helper,
        "#!/bin/sh\nprintf '<svg/>' > \"$6\"\nprintf 'not json' > \"$8\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755)).unwrap();
    let clamd = fake_clamd().await;
    let app = setup_tuned(db, clamd, |c| {
        c.media.render.render_geo_command = helper.to_string_lossy().into_owned();
    })
    .await;
    let point = map_blob(&app, "point.geojson", "application/geo+json").await;
    let id = id_of(&point);
    render_all(&app).await;
    let blob = get_blob(&app, id).await;
    assert_eq!(blob["preview_state"], "failed");
    assert_eq!(
        jobs_for(&app, id).await,
        Vec::<String>::new(),
        "not retried"
    );
    std::fs::remove_file(&helper).unwrap();
}

/// A provider may move a path on its own host, not send the worker to
/// another one: that redirect is its answer, and fails the attempt.
#[tokio::test]
async fn redirects_stay_on_the_layer_host() {
    let db = TestDatabase::new().await;
    let moved = Wms::start(Answer::Moved).await;
    let clamd = fake_clamd().await;
    let app = setup_tuned(db, clamd, |c| {
        c.media.render.map.layers = vec![moved.layer()];
    })
    .await;
    let point = map_blob(&app, "point.geojson", "application/geo+json").await;
    render_all(&app).await;
    assert_eq!(moved.requests().len(), 2, "the redirect and the move");
    assert_eq!(
        get_blob(&app, id_of(&point)).await["preview_state"],
        "ready"
    );

    let db = TestDatabase::new().await;
    let elsewhere = Wms::start(Answer::Png).await;
    let addr = elsewhere.url.split('/').nth(2).unwrap().parse().unwrap();
    provider_fault(
        db,
        Answer::Elsewhere(addr),
        "answered 307 Temporary Redirect",
    )
    .await;
    assert!(elsewhere.requests().is_empty(), "the other host was asked");
}

/// An extent that parses but fails the checks, a box of another size or
/// layers the rules do not give, is the helper's doing too: the map is
/// rejected, and the provider is never asked.
#[tokio::test]
async fn an_extent_that_fails_the_checks_rejects_the_map() {
    use std::os::unix::fs::PermissionsExt;
    let wms = Wms::start(Answer::Png).await;
    for (name, extent) in [
        (
            "size",
            r#"{"minx":0,"miny":0,"maxx":400,"maxy":400,"width":4000,"height":400,"layers":[0]}"#,
        ),
        (
            "layers",
            r#"{"minx":0,"miny":0,"maxx":400,"maxy":400,"width":400,"height":400,"layers":[]}"#,
        ),
    ] {
        let db = TestDatabase::new().await;
        let helper =
            std::env::temp_dir().join(format!("agrafe-bad-extent-{}", uuid::Uuid::now_v7()));
        // --input $2 --spec $4 --svg $6 --extent $8
        std::fs::write(
            &helper,
            format!("#!/bin/sh\nprintf '<svg/>' > \"$6\"\nprintf '%s' '{extent}' > \"$8\"\n"),
        )
        .unwrap();
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755)).unwrap();
        let clamd = fake_clamd().await;
        let app = setup_tuned(db, clamd, |c| {
            c.media.render.render_geo_command = helper.to_string_lossy().into_owned();
            c.media.render.map.layers = vec![wms.layer()];
        })
        .await;
        let point = map_blob(&app, "point.geojson", "application/geo+json").await;
        let id = id_of(&point);
        render_all(&app).await;
        let blob = get_blob(&app, id).await;
        assert_eq!(blob["preview_state"], "failed", "{name}");
        assert_eq!(
            jobs_for(&app, id).await,
            Vec::<String>::new(),
            "{name}: not retried"
        );
        std::fs::remove_file(&helper).unwrap();
    }
    assert!(wms.requests().is_empty(), "{:?}", wms.requests());
}

#[tokio::test]
async fn a_service_exception_fails_the_attempt() {
    let db = TestDatabase::new().await;
    provider_fault(db, Answer::Xml, "answered `text/xml`, not an image").await;
}

#[tokio::test]
async fn a_provider_timeout_fails_the_attempt() {
    let db = TestDatabase::new().await;
    provider_fault(db, Answer::Hang, "requesting layer 0 from 127.0.0.1").await;
}

#[tokio::test]
async fn an_undecodable_layer_fails_the_attempt() {
    let db = TestDatabase::new().await;
    provider_fault(db, Answer::Garbage, "an image vips could not decode").await;
}

#[tokio::test]
async fn an_oversized_layer_fails_the_attempt() {
    let db = TestDatabase::new().await;
    provider_fault(db, Answer::Huge, "answered more than 16.0 KiB").await;
}

#[tokio::test]
async fn a_geojson_declared_as_json_or_text_is_refined() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let as_json = map_blob(&app, "polygon-hole.geojson", "application/json").await;
    let as_text = map_blob(&app, "point.geojson", "text/plain").await;
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

#[tokio::test]
async fn gpx_files_get_a_map_preview_and_thumbnail() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let track = map_blob(&app, "track.gpx", "application/gpx+xml").await;
    render_all(&app).await;
    let id = id_of(&track);
    let blob = get_blob(&app, id).await;
    assert_eq!(blob["content_type"], "application/gpx+xml");
    assert_eq!(blob["category"], "text");
    assert_eq!(blob["preview_state"], "ready");
    assert_eq!(blob["preview_source"], "gpx");
    assert_eq!(blob["thumbnail_state"], "ready");
    let size = rendered(&app, &format!("/blobs/{id}/preview/track.gpx")).await;
    assert_eq!(size, (400, 400));
    let size = rendered(&app, &format!("/blobs/{id}/thumbnail/track.gpx")).await;
    assert_eq!(size, (256, 256));
    assert_eq!(jobs_for(&app, id).await, Vec::<String>::new());
    // The track and a GeoJSON file give two different maps.
    let point = map_blob(&app, "point.geojson", "application/geo+json").await;
    render_all(&app).await;
    let (_, _, a) = fetch(
        &app.private,
        get_with(&format!("/blobs/{id}/preview/track.gpx"), &[]),
    )
    .await;
    let (_, _, b) = fetch(
        &app.private,
        get_with(
            &format!("/blobs/{}/preview/point.geojson", id_of(&point)),
            &[],
        ),
    )
    .await;
    assert_ne!(a, b);
}

#[tokio::test]
async fn a_gpx_declared_as_xml_or_text_is_refined() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let as_xml = map_blob(&app, "track.gpx", "application/xml").await;
    let as_text = map_blob(&app, "empty.gpx", "text/plain").await;
    let plain_xml = ready_file(
        &app,
        "note.xml",
        "text/xml",
        b"<?xml version=\"1.0\"?><note/>\n",
    )
    .await;
    // The refinement is on the row as soon as the upload completes.
    assert_eq!(as_xml["content_type"], "application/gpx+xml");
    assert_eq!(as_text["content_type"], "application/gpx+xml");
    assert_eq!(plain_xml["content_type"], "text/xml");
    render_all(&app).await;
    let id = id_of(&as_xml);
    let blob = get_blob(&app, id).await;
    assert_eq!(blob["content_type"], "application/gpx+xml");
    assert_eq!(blob["category"], "text");
    assert_eq!(blob["preview_state"], "ready");
    assert_eq!(blob["preview_source"], "gpx");
    // The original is served with the refined type.
    let (status, headers, _) = fetch(
        &app.private,
        get_with(&format!("/blobs/{id}/original/track.gpx"), &[]),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "application/gpx+xml");
    let plain = get_blob(&app, id_of(&plain_xml)).await;
    assert_eq!(plain["preview_state"], "unavailable");
    assert_eq!(plain["thumbnail_state"], "ready");

    // Declared as something a GPX is not.
    let bytes = fixture("track.gpx");
    let mut blob = blob_for(&bytes, "application/geo+json", "raw");
    blob["skip_virus_scan"] = serde_json::json!(true);
    let created = create(&app, blob).await;
    let (status, body) = put_private(&app, id_of(&created), bytes).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(
        body["error"]["message"],
        "content looks like `application/gpx+xml`, not `application/geo+json`"
    );
}

#[tokio::test]
async fn kml_and_kmz_files_get_a_map_preview_and_thumbnail() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let kml = map_blob(&app, "places.kml", "application/vnd.google-earth.kml+xml").await;
    let kmz = map_blob(&app, "places.kmz", "application/vnd.google-earth.kmz").await;
    render_all(&app).await;
    for (blob, category) in [(&kml, "text"), (&kmz, "archive")] {
        let id = id_of(blob);
        let name = blob["filename"].as_str().unwrap();
        let blob = get_blob(&app, id).await;
        assert_eq!(blob["category"], category, "{name}");
        assert_eq!(blob["preview_state"], "ready", "{name}");
        assert_eq!(blob["preview_source"], "kml", "{name}");
        assert_eq!(blob["thumbnail_state"], "ready", "{name}");
        let size = rendered(&app, &format!("/blobs/{id}/preview/{name}")).await;
        assert_eq!(size, (400, 400), "{name}");
        let size = rendered(&app, &format!("/blobs/{id}/thumbnail/{name}")).await;
        assert_eq!(size, (256, 256), "{name}");
        assert_eq!(jobs_for(&app, id).await, Vec::<String>::new(), "{name}");
    }
    assert_eq!(kml["content_type"], "application/vnd.google-earth.kml+xml");
    assert_eq!(kmz["content_type"], "application/vnd.google-earth.kmz");
    // The document and its zipped self give the same map; a GeoJSON
    // file another.
    let preview_of = |blob: &Value| {
        format!(
            "/blobs/{}/preview/{}",
            id_of(blob),
            blob["filename"].as_str().unwrap()
        )
    };
    let point = map_blob(&app, "point.geojson", "application/geo+json").await;
    render_all(&app).await;
    let (_, _, a) = fetch(&app.private, get_with(&preview_of(&kml), &[])).await;
    let (_, _, b) = fetch(&app.private, get_with(&preview_of(&kmz), &[])).await;
    let (_, _, c) = fetch(&app.private, get_with(&preview_of(&point), &[])).await;
    assert_eq!(a, b);
    assert_ne!(a, c);
}

#[tokio::test]
async fn a_kml_declared_as_xml_or_text_and_a_kmz_declared_as_zip_are_refined() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    let as_xml = map_blob(&app, "places.kml", "application/xml").await;
    let as_text = map_blob(&app, "empty.kml", "text/plain").await;
    let as_zip = map_blob(&app, "places.kmz", "application/zip").await;
    let plain_zip = ready_file(&app, "icons.zip", "application/zip", &zip_of_icon()).await;
    // The refinement is on the row as soon as the upload completes.
    assert_eq!(
        as_xml["content_type"],
        "application/vnd.google-earth.kml+xml"
    );
    assert_eq!(
        as_text["content_type"],
        "application/vnd.google-earth.kml+xml"
    );
    assert_eq!(as_zip["content_type"], "application/vnd.google-earth.kmz");
    assert_eq!(plain_zip["content_type"], "application/zip");
    render_all(&app).await;
    for (blob, category) in [(&as_xml, "text"), (&as_zip, "archive")] {
        let id = id_of(blob);
        let name = blob["filename"].as_str().unwrap();
        let blob = get_blob(&app, id).await;
        assert_eq!(blob["category"], category, "{name}");
        assert_eq!(blob["preview_state"], "ready", "{name}");
        assert_eq!(blob["preview_source"], "kml", "{name}");
        // The original is served with the refined type.
        let (status, headers, _) = fetch(
            &app.private,
            get_with(&format!("/blobs/{id}/original/{name}"), &[]),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{name}");
        assert_eq!(
            headers["content-type"],
            blob["content_type"].as_str().unwrap(),
            "{name}"
        );
    }
    let plain = get_blob(&app, id_of(&plain_zip)).await;
    assert_eq!(plain["preview_state"], "unavailable");
    assert_eq!(plain["thumbnail_state"], "unavailable");

    // Declared as something a KML or a KMZ is not.
    for (fixture_name, declared, sniffed) in [
        (
            "places.kml",
            "application/geo+json",
            "application/vnd.google-earth.kml+xml",
        ),
        (
            "places.kmz",
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            "application/vnd.google-earth.kmz",
        ),
    ] {
        let bytes = fixture(fixture_name);
        let mut blob = blob_for(&bytes, declared, "raw");
        blob["skip_virus_scan"] = serde_json::json!(true);
        let created = create(&app, blob).await;
        let (status, body) = put_private(&app, id_of(&created), bytes).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        assert_eq!(
            body["error"]["message"],
            format!("content looks like `{sniffed}`, not `{declared}`")
        );
    }
}

/// A ZIP whose first entry is an icon, not a KMZ to the sniffer.
fn zip_of_icon() -> Vec<u8> {
    use std::io::Write as _;
    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default();
    writer.start_file("files/icon.png", options).unwrap();
    writer.write_all(&fixture("photo.png")).unwrap();
    writer.finish().unwrap().into_inner()
}

#[tokio::test]
async fn files_without_a_map_get_what_a_text_file_gets() {
    let db = TestDatabase::new().await;
    let app = setup(db).await;
    // No geometry at all: nothing to draw, the text sample as thumbnail.
    let empty = map_blob(&app, "empty.geojson", "application/geo+json").await;
    // A coordinate that is not degrees on Earth, as in a Lambert-93 file:
    // nothing this helper can place, so the same as no geometry.
    let off = map_blob(&app, "off-earth.geojson", "application/geo+json").await;
    // A GPX file with a track of no points; a KML document with a
    // placemark without geometry.
    let no_points = map_blob(&app, "empty.gpx", "application/gpx+xml").await;
    let no_placemark = map_blob(&app, "empty.kml", "application/vnd.google-earth.kml+xml").await;
    // The same document zipped: an archive, so the icon rather than the
    // text thumbnail.
    let zipped = map_blob(&app, "empty.kmz", "application/vnd.google-earth.kmz").await;
    render_all(&app).await;
    let id = id_of(&zipped);
    let blob = get_blob(&app, id).await;
    assert_eq!(blob["preview_state"], "unavailable");
    assert_eq!(blob["preview_source"], Value::Null);
    assert_eq!(blob["thumbnail_state"], "unavailable");
    let (status, headers, body) = fetch(
        &app.private,
        get_with(&format!("/blobs/{id}/thumbnail/empty.kmz"), &[]),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "image/webp");
    assert_eq!(headers["x-representation-state"], "unavailable");
    assert_eq!(webp_size(&body), (256, 256));
    assert_eq!(jobs_for(&app, id).await, Vec::<String>::new());
    for (blob, preview) in [
        (&empty, "unavailable"),
        (&off, "unavailable"),
        (&no_points, "unavailable"),
        (&no_placemark, "unavailable"),
    ] {
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
