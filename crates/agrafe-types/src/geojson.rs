//! The GeoJSON contract between the server and the `agrafe-render-geo`
//! helper: the content type and how a JSON head is told to be one, the
//! style vocabulary (simplestyle-spec), the spec the server writes for the
//! helper and the extent the helper writes back. The helper parses the
//! file; the server never does. Both read this module, so the two sides of
//! each file agree by construction.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// The content type of a GeoJSON file, RFC 7946.
pub const GEOJSON: &str = "application/geo+json";

/// Bytes of a JSON head looked at for the GeoJSON markers.
const HEAD_LEN: usize = 1024;

/// Whether a JSON document is a GeoJSON one: an object whose first KiB
/// carries `"type": "FeatureCollection"` or `"type": "Feature"`. A bare
/// geometry at the top level is not recognised here, only when declared.
pub fn looks_like_geojson(head: &[u8]) -> bool {
    let head = &head[..head.len().min(HEAD_LEN)];
    let body = head.strip_prefix(b"\xef\xbb\xbf").unwrap_or(head);
    let mut i = 0;
    while i < body.len() && body[i].is_ascii_whitespace() {
        i += 1;
    }
    if body.get(i) != Some(&b'{') {
        return false;
    }
    let mut rest = &body[i..];
    while let Some(at) = find(rest, b"\"type\"") {
        rest = &rest[at + b"\"type\"".len()..];
        let value = rest.trim_ascii_start();
        let Some(value) = value.strip_prefix(b":") else {
            continue;
        };
        let value = value.trim_ascii_start();
        if value.starts_with(b"\"FeatureCollection\"") || value.starts_with(b"\"Feature\"") {
            return true;
        }
    }
    false
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// An sRGB colour, parsed from the hex spelling simplestyle uses and
/// written back as `#rrggbb`, so nothing from a file reaches the SVG as
/// it came.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Color {
    /// The colour darkened, for the casing under a stroke.
    pub fn darkened(self) -> Self {
        let dark = |c: u8| (f32::from(c) * 0.55) as u8;
        Self {
            r: dark(self.r),
            g: dark(self.g),
            b: dark(self.b),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("`{0}` is not a hex colour of 3 or 6 digits")]
pub struct ColorError(String);

impl FromStr for Color {
    type Err = ColorError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let hex = s.trim().strip_prefix('#').unwrap_or(s.trim());
        let err = || ColorError(s.to_string());
        let digit = |c: char| c.to_digit(16).map(|d| d as u8).ok_or_else(err);
        let digits: Vec<u8> = hex.chars().map(digit).collect::<Result<_, _>>()?;
        match digits.as_slice() {
            [r, g, b] => Ok(Self {
                r: r * 17,
                g: g * 17,
                b: b * 17,
            }),
            [r1, r2, g1, g2, b1, b2] => Ok(Self {
                r: r1 * 16 + r2,
                g: g1 * 16 + g2,
                b: b1 * 16 + b2,
            }),
            _ => Err(err()),
        }
    }
}

impl fmt::Display for Color {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "#{:02x}{:02x}{:02x}", self.r, self.g, self.b)
    }
}

impl Serialize for Color {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Color {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// The three disc sizes of simplestyle's `marker-size`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MarkerSize {
    Small,
    Medium,
    Large,
}

impl MarkerSize {
    /// The disc's radius in pixels.
    pub fn radius(self) -> f64 {
        match self {
            MarkerSize::Small => 5.0,
            MarkerSize::Medium => 8.0,
            MarkerSize::Large => 12.0,
        }
    }
}

impl FromStr for MarkerSize {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "small" => Ok(MarkerSize::Small),
            "medium" => Ok(MarkerSize::Medium),
            "large" => Ok(MarkerSize::Large),
            _ => Err(()),
        }
    }
}

/// The widest stroke drawn, in pixels, so a file cannot paint the whole map.
pub const MAX_STROKE_WIDTH: f64 = 20.0;

/// How features are drawn, in the simplestyle-spec vocabulary: the keys
/// of `[media.geojson.style]` and of a feature's properties alike. Every
/// key is optional, so one feature's style can be laid over the default.
/// `marker-symbol`, `title` and `description` are not honoured.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", default, deny_unknown_fields)]
pub struct Style {
    pub stroke: Option<Color>,
    /// In pixels, clamped to `0..=MAX_STROKE_WIDTH`.
    pub stroke_width: Option<f64>,
    /// Clamped to `0..=1`.
    pub stroke_opacity: Option<f64>,
    pub fill: Option<Color>,
    /// Clamped to `0..=1`.
    pub fill_opacity: Option<f64>,
    pub marker_color: Option<Color>,
    pub marker_size: Option<MarkerSize>,
}

impl Style {
    /// This style laid over `base`: a key set here wins, a key unset
    /// here keeps the base's.
    pub fn over(&self, base: &Style) -> Style {
        Style {
            stroke: self.stroke.or(base.stroke),
            stroke_width: self.stroke_width.or(base.stroke_width),
            stroke_opacity: self.stroke_opacity.or(base.stroke_opacity),
            fill: self.fill.or(base.fill),
            fill_opacity: self.fill_opacity.or(base.fill_opacity),
            marker_color: self.marker_color.or(base.marker_color),
            marker_size: self.marker_size.or(base.marker_size),
        }
    }
}

/// A background layer's rule for the extent it is drawn on, from
/// `[[media.geojson.layers]]`; the URL stays with the server, which
/// fetches. Spans in metres, EPSG:3857.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LayerRule {
    pub min_span: Option<f64>,
    pub max_span: Option<f64>,
    pub attribution: Option<String>,
}

impl LayerRule {
    /// Whether the layer is drawn on an extent whose larger side is
    /// `span` metres.
    pub fn applies(&self, span: f64) -> bool {
        self.min_span.is_none_or(|min| span >= min) && self.max_span.is_none_or(|max| span < max)
    }
}

/// What the server asks the helper to draw, written as JSON next to the
/// file. The helper reads one input format and parses the default style
/// with the same code as a feature's properties.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Spec {
    /// The pixel box; the extent is widened to its aspect ratio.
    pub width: u32,
    pub height: u32,
    /// Added around the geometry, as a fraction of its larger side.
    pub padding: f64,
    /// The smallest extent drawn, in metres.
    pub min_span: f64,
    /// Vertices drawn at most, over the whole file.
    pub max_vertices: usize,
    /// The default style, under each feature's own.
    pub style: Style,
    /// Painted under the geometry when no layer applies.
    pub background: Color,
    /// The layers in their drawing order; which of them apply is decided
    /// by the extent, and the attribution shown is the first applicable
    /// layer's that has one.
    pub layers: Vec<LayerRule>,
}

/// What the helper chose, written as JSON next to the SVG: the bounding
/// box in EPSG:3857 metres, the pixel size, and the layers that apply.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Extent {
    pub minx: f64,
    pub miny: f64,
    pub maxx: f64,
    pub maxy: f64,
    pub width: u32,
    pub height: u32,
    /// Indices into the spec's layers, in drawing order.
    pub layers: Vec<usize>,
}

impl Extent {
    /// The larger side, in metres.
    pub fn span(&self) -> f64 {
        (self.maxx - self.minx).max(self.maxy - self.miny)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_a_geojson_head() {
        assert!(looks_like_geojson(
            br#"{"type": "FeatureCollection", "features": []}"#
        ));
        assert!(looks_like_geojson(
            b"\xef\xbb\xbf \n{\n  \"type\" :\n \"Feature\""
        ));
        // The marker may come after other keys.
        assert!(looks_like_geojson(
            br#"{"name": "parcelles", "crs": {"type": "name"}, "type": "FeatureCollection"}"#
        ));
        assert!(!looks_like_geojson(
            br#"{"type": "Point", "coordinates": [1, 2]}"#
        ));
        assert!(!looks_like_geojson(br#"{"kind": "Feature"}"#));
        assert!(!looks_like_geojson(br#"["Feature"]"#));
        assert!(!looks_like_geojson(b"type: Feature"));
        assert!(!looks_like_geojson(b""));
        // Past the first KiB is not looked at.
        let mut late = b"{\"a\": \"".to_vec();
        late.extend(std::iter::repeat_n(b'x', 1100));
        late.extend_from_slice(b"\", \"type\": \"Feature\"}");
        assert!(!looks_like_geojson(&late));
    }

    #[test]
    fn parses_hex_colours() {
        let gold: Color = "#FFD400".parse().unwrap();
        assert_eq!(
            gold,
            Color {
                r: 255,
                g: 212,
                b: 0
            }
        );
        assert_eq!(
            "fd0".parse::<Color>().unwrap(),
            Color {
                r: 255,
                g: 221,
                b: 0
            }
        );
        assert_eq!(gold.to_string(), "#ffd400");
        assert_eq!(gold.darkened().to_string(), "#8c7400");
        for bad in ["", "#", "red", "#ffd40", "#ggg", "#ffd4000"] {
            assert!(bad.parse::<Color>().is_err(), "{bad}");
        }
        assert_eq!(serde_json::to_string(&gold).unwrap(), "\"#ffd400\"");
        assert!(serde_json::from_str::<Color>("\"red\"").is_err());
    }

    #[test]
    fn lays_a_style_over_another() {
        let base = Style {
            stroke: Some("#000".parse().unwrap()),
            stroke_width: Some(4.0),
            fill: Some("#fff".parse().unwrap()),
            ..Style::default()
        };
        let own = Style {
            stroke: Some("#f00".parse().unwrap()),
            fill_opacity: Some(0.5),
            ..Style::default()
        };
        let merged = own.over(&base);
        assert_eq!(merged.stroke, own.stroke);
        assert_eq!(merged.stroke_width, Some(4.0));
        assert_eq!(merged.fill, base.fill);
        assert_eq!(merged.fill_opacity, Some(0.5));
        // The spelling of the keys is simplestyle's.
        let json = serde_json::to_value(&merged).unwrap();
        assert_eq!(json["stroke-width"], 4.0);
        assert_eq!(json["marker-size"], serde_json::Value::Null);
        assert!(serde_json::from_str::<Style>(r#"{"strokeWidth": 1}"#).is_err());
    }

    #[test]
    fn layers_apply_by_span() {
        let any = LayerRule::default();
        assert!(any.applies(0.0));
        let zoomed_out = LayerRule {
            min_span: Some(3000.0),
            ..LayerRule::default()
        };
        assert!(!zoomed_out.applies(2999.0));
        assert!(zoomed_out.applies(3000.0));
        let zoomed_in = LayerRule {
            max_span: Some(3000.0),
            ..LayerRule::default()
        };
        assert!(zoomed_in.applies(2999.0));
        assert!(!zoomed_in.applies(3000.0));
    }
}
