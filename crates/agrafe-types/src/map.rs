//! The map contract between the server and the `agrafe-render-geo`
//! helper: the input formats the helper reads, the style vocabulary
//! (simplestyle-spec), the spec the server writes for the helper and the
//! extent the helper writes back. The helper parses the file; the server
//! never does. Both read this module, so the two sides of each file agree
//! by construction. How a file is told to be in one of the formats is in
//! [`crate::geojson`], [`crate::gpx`], [`crate::kml`],
//! [`crate::shapefile`] and [`crate::gml`].

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// An input format of the helper. The server knows the content type and
/// tells the helper the format in the spec; the helper does not sniff.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Format {
    /// RFC 7946 GeoJSON, `application/geo+json`.
    GeoJson,
    /// GPX, the GPS exchange format, `application/gpx+xml`.
    Gpx,
    /// KML, Google Earth's XML, `application/vnd.google-earth.kml+xml`.
    Kml,
    /// KMZ, a KML document zipped with its icons and overlays,
    /// `application/vnd.google-earth.kmz`.
    Kmz,
    /// A zipped shapefile, its `.shp` layers each in the system its
    /// `.prj` names, `application/x-shapefile`.
    Shapefile,
    /// GML 2 or 3, what a WFS serves, its geometries each in the system
    /// its `srsName` names, `application/gml+xml`.
    Gml,
}

crate::str_enum!(Format {
    GeoJson => "geojson",
    Gpx => "gpx",
    Kml => "kml",
    Kmz => "kmz",
    Shapefile => "shapefile",
    Gml => "gml",
});

/// An sRGB colour, parsed from the hex spelling simplestyle uses and
/// written back as `#rrggbb`, so nothing from a file reaches the SVG as
/// it came.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Color {
    /// The red channel.
    pub r: u8,
    /// The green channel.
    pub g: u8,
    /// The blue channel.
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

/// Error returned when a string is not a hex colour of 3 or 6 digits, with
/// or without its `#`.
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
    /// A radius of 5 pixels.
    Small,
    /// A radius of 8 pixels, the default.
    Medium,
    /// A radius of 12 pixels.
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

/// The colour of a feature that sets none, and the configured default's
/// default: the yellow a form's map field prints in.
pub const DEFAULT_COLOR: Color = Color {
    r: 0xff,
    g: 0xd4,
    b: 0x00,
};

/// How features are drawn, in the simplestyle-spec vocabulary: the keys
/// of `[media.map.style]` and of a GeoJSON feature's properties alike.
/// Every key is optional, so one feature's style can be laid over the
/// default. `marker-symbol`, `title` and `description` are not honoured.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", default, deny_unknown_fields)]
pub struct Style {
    /// The colour of lines and of polygon outlines; their casing is it
    /// darkened.
    pub stroke: Option<Color>,
    /// In pixels, clamped to `0..=MAX_STROKE_WIDTH`.
    pub stroke_width: Option<f64>,
    /// Clamped to `0..=1`.
    pub stroke_opacity: Option<f64>,
    /// The colour inside polygons.
    pub fill: Option<Color>,
    /// Clamped to `0..=1`.
    pub fill_opacity: Option<f64>,
    /// The colour of the disc drawn for a point.
    pub marker_color: Option<Color>,
    /// The size of the disc drawn for a point.
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
/// `[[media.map.layers]]`; the URL stays with the server, which
/// fetches. Spans in metres, EPSG:3857.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LayerRule {
    /// The layer is drawn only on an extent at least this wide.
    pub min_span: Option<f64>,
    /// The layer is drawn only on an extent narrower than this.
    pub max_span: Option<f64>,
    /// The credit burned into a corner of the map when this is the first
    /// applicable layer that has one.
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
/// file. The helper reads the file in the format named here and parses
/// the default style with the same code as a GeoJSON feature's
/// properties.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Spec {
    /// The format the file is in.
    pub format: Format,
    /// The pixel box; the extent is widened to its aspect ratio.
    pub width: u32,
    /// The height of the pixel box.
    pub height: u32,
    /// Added around the geometry, as a fraction of its larger side.
    pub padding: f64,
    /// The smallest extent drawn, in metres.
    pub min_span: f64,
    /// Vertices drawn at most, over the whole file.
    pub max_vertices: usize,
    /// Shapes drawn at most, over the whole file: a point costs one
    /// vertex but one element, and the rasteriser pays per element.
    pub max_shapes: usize,
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
    /// The western edge, in metres.
    pub minx: f64,
    /// The southern edge, in metres.
    pub miny: f64,
    /// The eastern edge, in metres.
    pub maxx: f64,
    /// The northern edge, in metres.
    pub maxy: f64,
    /// The width of the SVG, in pixels.
    pub width: u32,
    /// The height of the SVG, in pixels.
    pub height: u32,
    /// Indices into the spec's layers, in drawing order.
    pub layers: Vec<usize>,
}

/// Half the side of the Web Mercator square, in metres: the x of 180°
/// and the y of the latitude the projection stops at.
pub const HALF_WORLD: f64 = std::f64::consts::PI * 6_378_137.0;

impl Extent {
    /// The larger side, in metres.
    pub fn span(&self) -> f64 {
        (self.maxx - self.minx).max(self.maxy - self.miny)
    }

    /// Whether the box lies within the Web Mercator square. One padded
    /// past it, a world-spanning file's, is a request a WMS may refuse
    /// every time.
    pub fn within_world(&self) -> bool {
        -HALF_WORLD <= self.minx
            && self.maxx <= HALF_WORLD
            && -HALF_WORLD <= self.miny
            && self.maxy <= HALF_WORLD
    }

    /// The indices of the `layers` drawn on this box, in order: those whose
    /// rule applies to its span, and none on a box outside the world. The
    /// helper lists them and the server checks the list against its own.
    pub fn applicable(&self, layers: &[LayerRule]) -> Vec<usize> {
        if !self.within_world() {
            return Vec::new();
        }
        let span = self.span();
        layers
            .iter()
            .enumerate()
            .filter(|(_, rule)| rule.applies(span))
            .map(|(i, _)| i)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_are_spelled_the_same_in_json_and_as_text() {
        for format in Format::ALL {
            let json = serde_json::to_string(format).unwrap();
            assert_eq!(json, format!("\"{format}\""));
            assert_eq!(serde_json::from_str::<Format>(&json).unwrap(), *format);
            assert_eq!(format.as_str().parse::<Format>().unwrap(), *format);
        }
        assert_eq!(Format::Gpx.as_str(), "gpx");
        assert_eq!(Format::Kmz.as_str(), "kmz");
        assert_eq!(Format::Shapefile.as_str(), "shapefile");
        assert_eq!(Format::Gml.as_str(), "gml");
        assert!("wkt".parse::<Format>().is_err());
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
