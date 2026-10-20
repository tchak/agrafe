//! Maps for GeoJSON files. Three steps, two of them in the sandbox: the
//! `agrafe-render-geo` helper reads the file and writes the geometry as
//! SVG paths with the extent it chose; this module, trusted code in the
//! worker, fetches one WMS `GetMap` image per background layer that
//! applies to that extent; `vips composite` stacks the layers and the SVG
//! into a raster that then goes through `to_webp` like any picture.
//!
//! The background comes from whatever WMS the configuration names, as a
//! URL template with the bounding box and the pixel size filled in; no
//! provider is named in the code. Every render sends the extent of a
//! user's geometry to that provider, which is why the layer list is empty
//! by default: then the geometry is drawn on a flat background and nothing
//! leaves the worker.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use agrafe_types::geojson::{Color, DEFAULT_COLOR, Extent, LayerRule, Spec, Style};
use agrafe_types::sniff::{LOADER_SNIFF_LEN, loader_for};
use bytesize::ByteSize;
use reqwest::Url;
use serde::Deserialize;
use tokio::io::AsyncWriteExt;
use tokio::sync::Semaphore;

use crate::vips::{self, Fit};
use crate::{Config, Error, Rejection, Verdict, helpers};

/// The helper's SVG in the work directory.
pub(crate) const PATHS: &str = "paths.svg";
/// The helper's extent in the work directory.
pub(crate) const EXTENT: &str = "extent.json";
/// The server's spec for the helper in the work directory.
pub(crate) const SPEC: &str = "spec.json";
/// The composited map in the work directory, in vips's native format.
pub(crate) const MAP: &str = "map.v";

/// Painted under the geometry when no layer applies.
const BACKGROUND: Color = Color {
    r: 0xe8,
    g: 0xe4,
    b: 0xdc,
};

/// Background requests in flight at once, per process: a
/// `variant_version` bump re-renders every GeoJSON blob and must not be a
/// burst against the provider.
const CONCURRENT_FETCHES: usize = 4;

/// `[media.geojson]`: how the map of a GeoJSON file is drawn and what it
/// is drawn over.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct GeoJson {
    /// Added around the geometry, as a fraction of its larger side.
    pub padding: f64,
    /// The smallest extent drawn, so a single point or a tiny parcel still
    /// gets a readable map.
    pub min_span: Metres,
    /// The largest side of a background request, in pixels: the map is
    /// rendered at the preview box or this, whichever is smaller.
    pub max_size: u32,
    /// Vertices drawn at most over the whole file, so a huge file cannot
    /// produce a huge SVG.
    pub max_vertices: usize,
    /// Shapes drawn at most over the whole file: a point costs one vertex
    /// but one element, and the rasteriser pays per element.
    pub max_shapes: usize,
    /// The whole of one layer request, connect to last byte.
    #[serde(with = "humantime_serde")]
    pub fetch_timeout: Duration,
    /// A layer response larger than this is a failure, not a bomb to
    /// decode.
    pub max_layer_size: ByteSize,
    /// The default style, in the simplestyle-spec vocabulary; a feature's
    /// own properties override it key by key.
    pub style: Style,
    /// Background layers, composited bottom to top. None by default.
    pub layers: Vec<Layer>,
}

impl Default for GeoJson {
    fn default() -> Self {
        let yellow = DEFAULT_COLOR;
        Self {
            padding: 0.1,
            min_span: Metres(200.0),
            max_size: 2048,
            max_vertices: 200_000,
            max_shapes: 20_000,
            fetch_timeout: Duration::from_secs(10),
            max_layer_size: ByteSize::mib(8),
            style: Style {
                stroke: Some(yellow),
                stroke_width: Some(4.0),
                stroke_opacity: Some(1.0),
                fill: Some(yellow),
                fill_opacity: Some(0.35),
                marker_color: Some(yellow),
                marker_size: Some(agrafe_types::geojson::MarkerSize::Medium),
            },
            layers: Vec::new(),
        }
    }
}

/// One `[[media.geojson.layers]]`: a WMS `GetMap` as a URL template.
#[derive(Debug, Clone, Deserialize)]
pub struct Layer {
    /// With `{bbox}` (`minx,miny,maxx,maxy` in EPSG:3857), `{width}` and
    /// `{height}` (pixels) to fill in.
    pub url: String,
    /// Burned into a corner of the map when this layer is the first drawn
    /// that has one.
    #[serde(default)]
    pub attribution: Option<String>,
    /// Drawn only on an extent at least this wide.
    #[serde(default)]
    pub min_span: Option<Metres>,
    /// Drawn only on an extent narrower than this.
    #[serde(default)]
    pub max_span: Option<Metres>,
}

impl Layer {
    fn rule(&self) -> LayerRule {
        LayerRule {
            min_span: self.min_span.map(|m| m.0),
            max_span: self.max_span.map(|m| m.0),
            attribution: self.attribution.clone(),
        }
    }

    /// The URL for `extent`.
    fn url_for(&self, extent: &Extent) -> String {
        self.url
            .replace(
                "{bbox}",
                &format!(
                    "{},{},{},{}",
                    extent.minx, extent.miny, extent.maxx, extent.maxy
                ),
            )
            .replace("{width}", &extent.width.to_string())
            .replace("{height}", &extent.height.to_string())
    }
}

impl GeoJson {
    /// What is wrong with the section, if anything: a layer URL without
    /// its placeholders, or that is not an HTTP URL once they are filled.
    pub fn validate(&self) -> Result<(), String> {
        for (i, layer) in self.layers.iter().enumerate() {
            for placeholder in ["{bbox}", "{width}", "{height}"] {
                if !layer.url.contains(placeholder) {
                    return Err(format!(
                        "media.geojson.layers[{i}].url must contain {placeholder}"
                    ));
                }
            }
            let probe = Extent {
                minx: 0.0,
                miny: 0.0,
                maxx: 1.0,
                maxy: 1.0,
                width: 1,
                height: 1,
                layers: Vec::new(),
            };
            let url = Url::parse(&layer.url_for(&probe))
                .map_err(|err| format!("media.geojson.layers[{i}].url is not a URL: {err}"))?;
            if !matches!(url.scheme(), "http" | "https") {
                return Err(format!(
                    "media.geojson.layers[{i}].url must be an http or https URL"
                ));
            }
        }
        if !self.padding.is_finite() || self.padding < 0.0 {
            return Err("media.geojson.padding must be zero or more".into());
        }
        if self.max_size == 0 {
            return Err("media.geojson.max_size must be at least 1".into());
        }
        Ok(())
    }

    /// The spec for the helper, drawing at `fit` or `max_size`, whichever
    /// is smaller.
    pub(crate) fn spec(&self, fit: Fit) -> Spec {
        Spec {
            width: fit.width.min(self.max_size),
            height: fit.height.min(self.max_size),
            padding: self.padding,
            min_span: self.min_span.0,
            max_vertices: self.max_vertices,
            max_shapes: self.max_shapes,
            style: self.style.clone(),
            background: BACKGROUND,
            layers: self.layers.iter().map(Layer::rule).collect(),
        }
    }
}

/// A length in metres, from a number or a string with a unit: `200`,
/// `"200 m"`, `"3 km"`.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct Metres(pub f64);

impl<'de> Deserialize<'de> for Metres {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Number(f64),
            Text(String),
        }
        let value = match Raw::deserialize(deserializer)? {
            Raw::Number(n) => n,
            Raw::Text(s) => s.parse::<Metres>().map_err(serde::de::Error::custom)?.0,
        };
        if !value.is_finite() || value < 0.0 {
            return Err(serde::de::Error::custom(
                "a length must be zero or more metres",
            ));
        }
        Ok(Metres(value))
    }
}

impl std::str::FromStr for Metres {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        let (number, factor) = if let Some(n) = s.strip_suffix("km") {
            (n, 1000.0)
        } else if let Some(n) = s.strip_suffix('m') {
            (n, 1.0)
        } else {
            (s, 1.0)
        };
        number
            .trim()
            .parse::<f64>()
            .map(|n| Metres(n * factor))
            .map_err(|_| format!("`{s}` is not a length, like \"200 m\" or \"3 km\""))
    }
}

/// Why a background layer could not be fetched. The provider's fault or
/// the network's, never the file's: the attempt fails and is retried.
#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    /// The request failed or timed out before a whole response came.
    #[error("requesting layer {layer} from {host}: {source}")]
    Request {
        /// The layer's index in `[[media.geojson.layers]]`.
        layer: usize,
        /// The provider's host; the rest of the URL carries the extent.
        host: String,
        /// The client's error, stripped of the URL.
        source: reqwest::Error,
    },
    /// The provider answered with a status other than a success.
    #[error("layer {layer} from {host} answered {status}")]
    Status {
        /// The layer's index in `[[media.geojson.layers]]`.
        layer: usize,
        /// The provider's host; the rest of the URL carries the extent.
        host: String,
        /// The status it answered.
        status: reqwest::StatusCode,
    },
    /// The response's `Content-Type` is not `image/*`, as a WMS
    /// `ServiceException` is.
    #[error("layer {layer} from {host} answered `{content_type}`, not an image")]
    NotAnImage {
        /// The layer's index in `[[media.geojson.layers]]`.
        layer: usize,
        /// The provider's host; the rest of the URL carries the extent.
        host: String,
        /// The `Content-Type` it answered, empty when it sent none.
        content_type: String,
    },
    /// The response grew past `media.geojson.max_layer_size`.
    #[error("layer {layer} from {host} answered more than {max}")]
    TooLarge {
        /// The layer's index in `[[media.geojson.layers]]`.
        layer: usize,
        /// The provider's host; the rest of the URL carries the extent.
        host: String,
        /// The configured `max_layer_size`.
        max: ByteSize,
    },
    /// The bytes do not sniff as PNG, JPEG, WebP, GIF, TIFF or BMP,
    /// whatever the `Content-Type` said.
    #[error("layer {layer} from {host} answered bytes that are not an image the renderer may load")]
    Unloadable {
        /// The layer's index in `[[media.geojson.layers]]`.
        layer: usize,
        /// The provider's host; the rest of the URL carries the extent.
        host: String,
    },
    /// The image passed the sniff but `vips` failed to decode it at the
    /// map's size.
    #[error("layer {layer} from {host} answered an image vips could not decode: {why}")]
    Undecodable {
        /// The layer's index in `[[media.geojson.layers]]`.
        layer: usize,
        /// The provider's host; the rest of the URL carries the extent.
        host: String,
        /// The rejection `vips` ended with.
        why: String,
    },
    /// Writing the response into the work directory failed.
    #[error("writing a layer: {0}")]
    Io(#[from] std::io::Error),
}

/// The HTTP client background layers are fetched through: a timeout on
/// the whole request, a few redirects, and at most
/// `CONCURRENT_FETCHES` requests in flight per process. One per
/// process, built from `[media.geojson]`.
#[derive(Clone)]
pub struct Client {
    inner: reqwest::Client,
    slots: Arc<Semaphore>,
}

impl Client {
    /// A client with `fetch_timeout` on the connection and on the whole
    /// request. Fails when reqwest cannot build the client.
    pub fn from_config(config: &GeoJson) -> Result<Self, reqwest::Error> {
        let inner = reqwest::Client::builder()
            .user_agent(concat!("agrafe/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(config.fetch_timeout)
            .timeout(config.fetch_timeout)
            .redirect(reqwest::redirect::Policy::limited(3))
            .referer(false)
            .build()?;
        Ok(Self {
            inner,
            slots: Arc::new(Semaphore::new(CONCURRENT_FETCHES)),
        })
    }

    /// Fetch layer `index` of `config` for `extent` into `path`. The
    /// response is an image only if its `Content-Type` says so and the
    /// loader sniff agrees: a WMS answers an XML `ServiceException` with
    /// a `200`.
    async fn fetch(
        &self,
        config: &GeoJson,
        index: usize,
        extent: &Extent,
        path: &Path,
    ) -> Result<(), FetchError> {
        let url = config.layers[index].url_for(extent);
        let host = host_of(&url);
        // reqwest's errors carry the URL, and the URL carries the extent.
        let fail = |source: reqwest::Error| FetchError::Request {
            layer: index,
            host: host.clone(),
            source: source.without_url(),
        };
        let _slot = self
            .slots
            .acquire()
            .await
            .expect("the semaphore is never closed");
        let mut response = self.inner.get(url).send().await.map_err(fail)?;
        let status = response.status();
        if !status.is_success() {
            return Err(FetchError::Status {
                layer: index,
                host,
                status,
            });
        }
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let is_image = content_type
            .trim_start()
            .get(..6)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("image/"));
        if !is_image {
            return Err(FetchError::NotAnImage {
                layer: index,
                host,
                content_type,
            });
        }
        let max = config.max_layer_size;
        let mut file = tokio::fs::File::create(path).await?;
        let mut head = Vec::with_capacity(LOADER_SNIFF_LEN);
        let mut received = 0u64;
        while let Some(chunk) = response.chunk().await.map_err(fail)? {
            received += chunk.len() as u64;
            if received > max.as_u64() {
                return Err(FetchError::TooLarge {
                    layer: index,
                    host,
                    max,
                });
            }
            if head.len() < LOADER_SNIFF_LEN {
                let take = (LOADER_SNIFF_LEN - head.len()).min(chunk.len());
                head.extend_from_slice(&chunk[..take]);
            }
            file.write_all(&chunk).await?;
        }
        file.flush().await?;
        if !matches!(
            loader_for(&head),
            Some("png" | "jpeg" | "webp" | "gif" | "tiff" | "bmp")
        ) {
            return Err(FetchError::Unloadable { layer: index, host });
        }
        Ok(())
    }
}

/// The file of layer `index` in the work directory, as it came.
fn layer_file(index: usize) -> String {
    format!("layer-{index}.png")
}

/// The file of layer `index` decoded at the map's size, for the composite.
fn fitted_file(index: usize) -> String {
    format!("layer-{index}.v")
}

/// What is wrong with the extent the helper wrote for `spec`, if
/// anything. The helper is the untrusted side: its pixel box must be the
/// one asked for, its layers must be the spec's, in order, and its box
/// must be a box, or the server would fetch, index and composite on a
/// compromised helper's say-so.
fn check_extent(extent: &Extent, spec: &Spec) -> Result<(), String> {
    if (extent.width, extent.height) != (spec.width, spec.height) {
        return Err(format!(
            "a {}x{} box, not the {}x{} asked for",
            extent.width, extent.height, spec.width, spec.height
        ));
    }
    if !(extent.minx < extent.maxx && extent.miny < extent.maxy) {
        return Err("an empty or inverted bounding box".into());
    }
    let in_order = extent.layers.windows(2).all(|pair| pair[0] < pair[1]);
    if !in_order || extent.layers.iter().any(|&i| i >= spec.layers.len()) {
        return Err(format!(
            "layers {:?}, of the {} configured",
            extent.layers,
            spec.layers.len()
        ));
    }
    Ok(())
}

/// Render the map of the GeoJSON at `original` into `MAP` under
/// `workdir`, at `fit` or `media.geojson.max_size`, whichever is smaller:
/// the helper, the fetch, each layer fitted to the box, the composite.
/// `Declined` when the file holds no geometry, `Rejected` when the helper
/// or vips fail on it, an error when a layer cannot be fetched or
/// decoded.
pub async fn render_map(
    config: &Config,
    client: &Client,
    original: &Path,
    workdir: &Path,
    fit: Fit,
) -> Result<Verdict<PathBuf>, Error> {
    let geojson = &config.geojson;
    let spec = geojson.spec(fit);
    let spec_path = workdir.join(SPEC);
    tokio::fs::write(
        &spec_path,
        serde_json::to_vec(&spec).expect("a spec serialises"),
    )
    .await
    .map_err(Error::Input)?;
    let svg = workdir.join(PATHS);
    let extent_path = workdir.join(EXTENT);
    match helpers::render_geo(config, original, &spec_path, &svg, &extent_path).await? {
        Verdict::Rendered(()) => {}
        Verdict::Declined => return Ok(Verdict::Declined),
        Verdict::Rejected(rejection) => return Ok(Verdict::Rejected(rejection)),
    }
    // The helper is the untrusted side: an extent that does not parse is
    // the file's doing like one that does not check, not ours to retry.
    let bytes = tokio::fs::read(&extent_path).await.map_err(Error::Output)?;
    let extent: Extent = match serde_json::from_slice(&bytes) {
        Ok(extent) => extent,
        Err(e) => {
            return Ok(Verdict::Rejected(Rejection::Output(format!(
                "an extent that does not parse: {e}"
            ))));
        }
    };
    if let Err(what) = check_extent(&extent, &spec) {
        return Ok(Verdict::Rejected(Rejection::Output(format!(
            "an extent with {what}"
        ))));
    }
    let fetches = extent.layers.iter().map(|&index| {
        let path = workdir.join(layer_file(index));
        let extent = &extent;
        async move { client.fetch(geojson, index, extent, &path).await }
    });
    futures::future::try_join_all(fetches).await?;
    // Each layer decoded at the box's size, one at a time: a layer that
    // passed the sniff but does not decode is the provider's fault, not
    // the file's, and the attempt is retried.
    for &index in &extent.layers {
        let fitted = vips::fit_layer(
            config,
            workdir,
            &layer_file(index),
            &fitted_file(index),
            extent.width,
            extent.height,
        )
        .await?;
        if let Verdict::Rejected(rejection) = fitted {
            return Err(Error::Fetch(FetchError::Undecodable {
                layer: index,
                host: host_of(&geojson.layers[index].url_for(&extent)),
                why: rejection.to_string(),
            }));
        }
    }
    tracing::debug!(
        span = extent.span(),
        layers = extent.layers.len(),
        "compositing the map"
    );
    let layers: Vec<String> = extent.layers.iter().map(|&i| fitted_file(i)).collect();
    let verdict = vips::composite(config, workdir, &layers, PATHS, MAP).await?;
    Ok(verdict.map(|()| workdir.join(MAP)))
}

/// The host of `url`, for logs: the rest of a layer URL carries the
/// extent of a user's geometry.
fn host_of(url: &str) -> String {
    Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_lengths() {
        #[derive(Deserialize)]
        struct Doc {
            span: Metres,
        }
        let parse = |toml: &str| toml::from_str::<Doc>(toml).map(|d| d.span.0);
        assert_eq!(parse("span = 200").unwrap(), 200.0);
        assert_eq!(parse("span = 2.5").unwrap(), 2.5);
        assert_eq!(parse("span = \"200 m\"").unwrap(), 200.0);
        assert_eq!(parse("span = \"3km\"").unwrap(), 3000.0);
        assert_eq!(parse("span = \" 1.5 km \"").unwrap(), 1500.0);
        for bad in [
            "span = \"far\"",
            "span = \"3 miles\"",
            "span = -1",
            "span = \"-2 m\"",
        ] {
            assert!(parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn validates_layer_urls() {
        let mut config = GeoJson::default();
        assert_eq!(config.validate(), Ok(()));
        let layer = |url: &str| Layer {
            url: url.to_string(),
            attribution: None,
            min_span: None,
            max_span: None,
        };
        config.layers = vec![layer(
            "https://wms.example/?BBOX={bbox}&WIDTH={width}&HEIGHT={height}",
        )];
        assert_eq!(config.validate(), Ok(()));
        config.layers = vec![layer("https://wms.example/?BBOX={bbox}&WIDTH={width}")];
        assert_eq!(
            config.validate().unwrap_err(),
            "media.geojson.layers[0].url must contain {height}"
        );
        config.layers = vec![layer("wms.example/?{bbox}{width}{height}")];
        assert!(
            config
                .validate()
                .unwrap_err()
                .starts_with("media.geojson.layers[0].url is not a URL")
        );
        config.layers = vec![layer("ftp://wms.example/?{bbox}{width}{height}")];
        assert_eq!(
            config.validate().unwrap_err(),
            "media.geojson.layers[0].url must be an http or https URL"
        );
        config.layers.clear();
        config.padding = -0.1;
        assert!(config.validate().is_err());
    }

    #[test]
    fn fills_the_template() {
        let layer = Layer {
            url: "https://wms.example/?BBOX={bbox}&WIDTH={width}&HEIGHT={height}&FORMAT=image/png"
                .into(),
            attribution: None,
            min_span: None,
            max_span: None,
        };
        let extent = Extent {
            minx: 1.5,
            miny: -2.0,
            maxx: 3.0,
            maxy: 4.25,
            width: 640,
            height: 480,
            layers: vec![0],
        };
        assert_eq!(
            layer.url_for(&extent),
            "https://wms.example/?BBOX=1.5,-2,3,4.25&WIDTH=640&HEIGHT=480&FORMAT=image/png"
        );
    }

    #[test]
    fn checks_the_extent_against_the_spec() {
        let config = GeoJson {
            layers: vec![
                Layer {
                    url: "https://wms.example/?{bbox}{width}{height}".into(),
                    attribution: None,
                    min_span: None,
                    max_span: None,
                };
                2
            ],
            ..GeoJson::default()
        };
        let spec = config.spec(Fit {
            width: 400,
            height: 300,
            quality: 80,
        });
        let good = Extent {
            minx: 0.0,
            miny: 0.0,
            maxx: 400.0,
            maxy: 300.0,
            width: 400,
            height: 300,
            layers: vec![0, 1],
        };
        assert_eq!(check_extent(&good, &spec), Ok(()));
        let none = Extent {
            layers: Vec::new(),
            ..good.clone()
        };
        assert_eq!(check_extent(&none, &spec), Ok(()));
        let bad = [
            Extent {
                width: 4000,
                ..good.clone()
            },
            Extent {
                height: 1,
                ..good.clone()
            },
            Extent {
                maxx: 0.0,
                ..good.clone()
            },
            Extent {
                miny: 300.0,
                ..good.clone()
            },
            Extent {
                layers: vec![2],
                ..good.clone()
            },
            Extent {
                layers: vec![1, 0],
                ..good.clone()
            },
            Extent {
                layers: vec![0, 0],
                ..good.clone()
            },
        ];
        for extent in bad {
            assert!(check_extent(&extent, &spec).is_err(), "{extent:?}");
        }
    }

    #[test]
    fn the_spec_follows_the_box_and_the_cap() {
        let config = GeoJson {
            max_size: 1024,
            ..GeoJson::default()
        };
        let fit = Fit {
            width: 2048,
            height: 2048,
            quality: 85,
        };
        let spec = config.spec(fit);
        assert_eq!((spec.width, spec.height), (1024, 1024));
        assert_eq!(spec.style, config.style);
        assert!(spec.layers.is_empty());
        let small = Fit {
            width: 256,
            height: 128,
            quality: 80,
        };
        let spec = config.spec(small);
        assert_eq!((spec.width, spec.height), (256, 128));
    }
}
