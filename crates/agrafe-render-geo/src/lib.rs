//! Maps for GeoJSON files: the geometry projected to Web Mercator, fitted
//! into a pixel box with some margin, simplified to the pixel, and drawn as
//! SVG paths in the simplestyle-spec colours of the file or the default.
//! The file is untrusted, so this runs in the `agrafe-render-geo` helper,
//! in the sandbox; the server never parses a GeoJSON. The helper writes the
//! SVG and the extent it chose; the server fetches the background for that
//! extent and `vips` composites the two.
//!
//! The contract with the server, [`Spec`] in and [`Extent`] out, lives in
//! `agrafe_types::geojson`, along with the style vocabulary.

use std::fmt::Write as _;

use agrafe_types::geojson::{
    Color, DEFAULT_COLOR, Extent, MAX_STROKE_WIDTH, MarkerSize, Spec, Style,
};
use geojson::{GeoJson, Geometry, GeometryValue, JsonObject, Position};

/// The Earth's radius of the spherical Mercator, EPSG:3857, in metres.
const EARTH_RADIUS: f64 = 6_378_137.0;
/// Half the width of the Web Mercator square, in metres: the x of 180°.
const HALF_WORLD: f64 = std::f64::consts::PI * EARTH_RADIUS;
/// The latitude the projection stops at, where the square map ends.
const MAX_LATITUDE: f64 = 85.051_128_78;
/// How far a drawn vertex may stray from the exact line, in pixels.
const SIMPLIFY_TOLERANCE: f64 = 0.5;
/// What the casing adds around a stroke, in pixels.
const CASING: f64 = 3.0;
/// The ring around a point's disc, in pixels.
const MARKER_RING: f64 = 2.0;

/// A coordinate that is not on Earth in the file's coordinate system:
/// the file is in some projected system it does not declare, Lambert-93
/// as French administrations export it, or has a hole in it. Either way
/// there is no map to draw from it, and the file declines like one with
/// no geometry.
#[derive(Debug, Clone, Copy, PartialEq)]
struct OffEarth {
    x: f64,
    y: f64,
}

/// The coordinate system of a file. RFC 7946 allows only longitude and
/// latitude in degrees, but the 2008 specification had a `crs` member,
/// and GDAL and QGIS still write it for a layer in another system; the
/// few that French data comes in are honoured, anything else declines.
#[derive(Debug, Clone, PartialEq)]
enum Crs {
    /// WGS 84 longitude and latitude in degrees, EPSG:4326 or CRS84, the
    /// default and the only conformant one.
    Degrees,
    /// RGF93 / Lambert-93, EPSG:2154, the legal system of metropolitan
    /// France: eastings and northings in metres.
    Lambert93(Lcc),
    /// Web Mercator, EPSG:3857, the projection drawn in: metres.
    WebMercator,
}

impl Crs {
    /// The system the `crs` member among `foreign_members` declares, or
    /// degrees when there is none or it is `null`. `None` when it names
    /// a system that is not handled or is not a named CRS.
    fn of(foreign_members: Option<&JsonObject>) -> Option<Crs> {
        let crs = match foreign_members.and_then(|members| members.get("crs")) {
            None | Some(serde_json::Value::Null) => return Some(Crs::Degrees),
            Some(crs) => crs,
        };
        if crs.get("type").and_then(serde_json::Value::as_str) != Some("name") {
            return None;
        }
        let name = crs.get("properties")?.get("name")?.as_str()?;
        Crs::named(name)
    }

    /// The system `name` spells, in any of the spellings in use:
    /// `urn:ogc:def:crs:EPSG::2154`, `EPSG:2154`,
    /// `urn:ogc:def:crs:OGC:1.3:CRS84`, an OpenGIS URL.
    fn named(name: &str) -> Option<Crs> {
        let name = name.trim().to_ascii_lowercase();
        if name.ends_with("crs84") {
            return Some(Crs::Degrees);
        }
        match name.rsplit([':', '/']).next()?.parse::<u32>().ok()? {
            4326 => Some(Crs::Degrees),
            2154 => Some(Crs::Lambert93(Lcc::lambert93())),
            3857 | 900_913 | 102_100 => Some(Crs::WebMercator),
            _ => None,
        }
    }
}

/// A Lambert conformal conic projection with two standard parallels on
/// an ellipsoid, EPSG method 9802, reduced to what its inverse needs.
#[derive(Debug, Clone, PartialEq)]
struct Lcc {
    /// The eccentricity of the ellipsoid.
    e: f64,
    /// The cone constant.
    n: f64,
    /// The semi-major axis times the mapping radius at the equator.
    af: f64,
    /// The radius of the latitude of origin.
    r0: f64,
    lon0: f64,
    x0: f64,
    y0: f64,
}

impl Lcc {
    /// RGF93 / Lambert-93 (EPSG:2154): GRS 80, parallels at 44° and 49°,
    /// origin at 46.5° N 3° E, false origin (700 000, 6 600 000).
    fn lambert93() -> Lcc {
        Lcc::new(
            6_378_137.0,
            1.0 / 298.257_222_101,
            46.5_f64.to_radians(),
            3.0_f64.to_radians(),
            44.0_f64.to_radians(),
            49.0_f64.to_radians(),
            700_000.0,
            6_600_000.0,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn new(a: f64, f: f64, lat0: f64, lon0: f64, lat1: f64, lat2: f64, x0: f64, y0: f64) -> Lcc {
        let e = (2.0 * f - f * f).sqrt();
        let m = |lat: f64| lat.cos() / (1.0 - e * e * lat.sin() * lat.sin()).sqrt();
        let t = |lat: f64| Lcc::t(e, lat);
        let n = (m(lat1).ln() - m(lat2).ln()) / (t(lat1).ln() - t(lat2).ln());
        let big_f = m(lat1) / (n * t(lat1).powf(n));
        let af = a * big_f;
        Lcc {
            e,
            n,
            af,
            r0: af * t(lat0).powf(n),
            lon0,
            x0,
            y0,
        }
    }

    /// The isometric latitude function of the method.
    fn t(e: f64, lat: f64) -> f64 {
        let es = e * lat.sin();
        (std::f64::consts::FRAC_PI_4 - lat / 2.0).tan() / ((1.0 - es) / (1.0 + es)).powf(e / 2.0)
    }

    /// Longitude and latitude in degrees of the easting and northing
    /// `x`, `y` in metres.
    fn inverse(&self, x: f64, y: f64) -> (f64, f64) {
        let (dx, dy) = (x - self.x0, self.r0 - (y - self.y0));
        let r = (dx * dx + dy * dy).sqrt().copysign(self.n);
        let t = (r / self.af).powf(1.0 / self.n);
        let lon = dx.atan2(dy) / self.n + self.lon0;
        let mut lat = std::f64::consts::FRAC_PI_2 - 2.0 * t.atan();
        for _ in 0..10 {
            let es = self.e * lat.sin();
            lat = std::f64::consts::FRAC_PI_2
                - 2.0 * (t * ((1.0 - es) / (1.0 + es)).powf(self.e / 2.0)).atan();
        }
        (lon.to_degrees(), lat.to_degrees())
    }
}

/// The SVG and the extent it is drawn on.
#[derive(Debug, Clone, PartialEq)]
pub struct Rendered {
    pub svg: String,
    pub extent: Extent,
}

/// A projected point, in metres.
#[derive(Debug, Clone, Copy, PartialEq)]
struct P {
    x: f64,
    y: f64,
}

/// One drawn thing, projected, with the style it is drawn in.
#[derive(Debug)]
enum Shape {
    Point(P),
    Line(Vec<P>),
    /// The outer ring first, then the holes.
    Polygon(Vec<Vec<P>>),
}

/// Draw `bytes` as `spec` asks. `None` when the file is not a GeoJSON
/// document, holds no geometry, or has a coordinate that is not on Earth
/// in degrees: then there is nothing to draw and the file gets whatever a
/// text file gets. Nothing about the file is an error: a file that
/// cannot be drawn today would not be drawn tomorrow either.
pub fn render(bytes: &[u8], spec: &Spec) -> Option<Rendered> {
    let geojson = serde_json::from_slice::<GeoJson>(bytes).ok()?;
    let crs = Crs::of(match &geojson {
        GeoJson::Geometry(geometry) => geometry.foreign_members.as_ref(),
        GeoJson::Feature(feature) => feature.foreign_members.as_ref(),
        GeoJson::FeatureCollection(collection) => collection.foreign_members.as_ref(),
    })?;
    let mut shapes: Vec<(Shape, Style)> = Vec::new();
    let mut collect = |geometry: Option<&Geometry>, properties: Option<&JsonObject>| {
        let style = properties
            .map(style_of)
            .unwrap_or_default()
            .over(&spec.style);
        if let Some(geometry) = geometry {
            let mut out = Vec::new();
            project_geometry(geometry, &crs, &mut out)?;
            shapes.extend(out.into_iter().map(|shape| (shape, style.clone())));
        }
        Ok::<(), OffEarth>(())
    };
    let projected = match &geojson {
        GeoJson::Geometry(geometry) => collect(Some(geometry), None),
        GeoJson::Feature(feature) => {
            collect(feature.geometry.as_ref(), feature.properties.as_ref())
        }
        GeoJson::FeatureCollection(collection) => {
            collection.features.iter().try_for_each(|feature| {
                collect(feature.geometry.as_ref(), feature.properties.as_ref())
            })
        }
    };
    if projected.is_err() || shapes.is_empty() {
        return None;
    }
    let extent = extent_of(shapes.iter().map(|(shape, _)| shape), spec);
    let svg = draw(&shapes, &extent, spec);
    Some(Rendered { svg, extent })
}

/// The style a feature's properties ask for. A value of the wrong type or
/// out of the vocabulary is ignored, never an error: nothing from the file
/// reaches the SVG as it came.
pub fn style_of(properties: &JsonObject) -> Style {
    let color = |key: &str| {
        properties
            .get(key)
            .and_then(serde_json::Value::as_str)
            .and_then(|s| s.parse::<Color>().ok())
    };
    let number = |key: &str| properties.get(key).and_then(serde_json::Value::as_f64);
    Style {
        stroke: color("stroke"),
        stroke_width: number("stroke-width"),
        stroke_opacity: number("stroke-opacity"),
        fill: color("fill"),
        fill_opacity: number("fill-opacity"),
        marker_color: color("marker-color"),
        marker_size: properties
            .get("marker-size")
            .and_then(serde_json::Value::as_str)
            .and_then(|s| s.parse::<MarkerSize>().ok()),
    }
}

/// A position in `crs` projected to Web Mercator. The latitude is
/// clamped to where the projection ends.
fn project(position: &Position, crs: &Crs) -> Result<P, OffEarth> {
    let (x, y) = match position.as_slice() {
        [x, y, ..] => (*x, *y),
        _ => (f64::NAN, f64::NAN),
    };
    let off = OffEarth { x, y };
    if !x.is_finite() || !y.is_finite() {
        return Err(off);
    }
    let (lon, lat) = match crs {
        Crs::Degrees => (x, y),
        Crs::Lambert93(lcc) => lcc.inverse(x, y),
        Crs::WebMercator => {
            if x.abs() > HALF_WORLD * (1.0 + 1e-9) {
                return Err(off);
            }
            return Ok(P {
                x,
                y: y.clamp(-HALF_WORLD, HALF_WORLD),
            });
        }
    };
    if !lon.is_finite() || !lat.is_finite() || lon.abs() > 180.0 || lat.abs() > 90.0 {
        return Err(off);
    }
    let lat = lat.clamp(-MAX_LATITUDE, MAX_LATITUDE);
    Ok(P {
        x: EARTH_RADIUS * lon.to_radians(),
        y: EARTH_RADIUS
            * (std::f64::consts::FRAC_PI_4 + lat.to_radians() / 2.0)
                .tan()
                .ln(),
    })
}

fn project_all(positions: &[Position], crs: &Crs) -> Result<Vec<P>, OffEarth> {
    positions.iter().map(|p| project(p, crs)).collect()
}

/// The shapes of `geometry`, projected, onto `out`. A line or a ring
/// without positions is nothing to draw and is dropped, so that a file
/// made of them declines instead of getting a map of nowhere.
fn project_geometry(geometry: &Geometry, crs: &Crs, out: &mut Vec<Shape>) -> Result<(), OffEarth> {
    let line = |positions: &[Position]| -> Result<Option<Shape>, OffEarth> {
        let line = project_all(positions, crs)?;
        Ok((!line.is_empty()).then_some(Shape::Line(line)))
    };
    let polygon = |rings: &[Vec<Position>]| -> Result<Option<Shape>, OffEarth> {
        let rings: Vec<Vec<P>> = rings
            .iter()
            .filter(|ring| !ring.is_empty())
            .map(|ring| project_all(ring, crs))
            .collect::<Result<_, _>>()?;
        Ok((!rings.is_empty()).then_some(Shape::Polygon(rings)))
    };
    match &geometry.value {
        GeometryValue::Point { coordinates } => out.push(Shape::Point(project(coordinates, crs)?)),
        GeometryValue::MultiPoint { coordinates } => {
            for p in coordinates {
                out.push(Shape::Point(project(p, crs)?));
            }
        }
        GeometryValue::LineString { coordinates } => out.extend(line(coordinates)?),
        GeometryValue::MultiLineString { coordinates } => {
            for positions in coordinates {
                out.extend(line(positions)?);
            }
        }
        GeometryValue::Polygon { coordinates } => out.extend(polygon(coordinates)?),
        GeometryValue::MultiPolygon { coordinates } => {
            for rings in coordinates {
                out.extend(polygon(rings)?);
            }
        }
        GeometryValue::GeometryCollection { geometries } => {
            for geometry in geometries {
                project_geometry(geometry, crs, out)?;
            }
        }
    }
    Ok(())
}

/// The box the map shows: the geometry's bounding box, padded, grown to
/// the smallest span, then widened to the aspect ratio of the pixel box.
/// The extent is widened, never letterboxed: the image is the whole box.
fn extent_of<'a>(shapes: impl Iterator<Item = &'a Shape>, spec: &Spec) -> Extent {
    let (mut minx, mut miny, mut maxx, mut maxy) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
    let mut take = |p: &P| {
        minx = minx.min(p.x);
        miny = miny.min(p.y);
        maxx = maxx.max(p.x);
        maxy = maxy.max(p.y);
    };
    for shape in shapes {
        match shape {
            Shape::Point(p) => take(p),
            Shape::Line(line) => line.iter().for_each(&mut take),
            Shape::Polygon(rings) => rings.iter().flatten().for_each(&mut take),
        }
    }
    let pad = spec.padding.max(0.0) * (maxx - minx).max(maxy - miny);
    let (cx, cy) = ((minx + maxx) / 2.0, (miny + maxy) / 2.0);
    // At least a metre a side, so a degenerate spec still yields a box.
    let floor = spec.min_span.max(1.0);
    let mut w = (maxx - minx + 2.0 * pad).max(floor);
    let mut h = (maxy - miny + 2.0 * pad).max(floor);
    let (width, height) = (spec.width.max(1), spec.height.max(1));
    let ratio = f64::from(width) / f64::from(height);
    if w / h < ratio {
        w = h * ratio;
    } else {
        h = w / ratio;
    }
    let span = w.max(h);
    let layers = spec
        .layers
        .iter()
        .enumerate()
        .filter(|(_, rule)| rule.applies(span))
        .map(|(i, _)| i)
        .collect();
    Extent {
        minx: cx - w / 2.0,
        miny: cy - h / 2.0,
        maxx: cx + w / 2.0,
        maxy: cy + h / 2.0,
        width,
        height,
        layers,
    }
}

/// Douglas–Peucker: the points of `line` that keep it within
/// `SIMPLIFY_TOLERANCE` of the original.
fn simplify(line: &[P]) -> Vec<P> {
    fn recurse(line: &[P], keep: &mut Vec<bool>, offset: usize) {
        let (first, last) = (line[0], line[line.len() - 1]);
        let (dx, dy) = (last.x - first.x, last.y - first.y);
        let length = (dx * dx + dy * dy).sqrt();
        let distance = |p: &P| {
            if length == 0.0 {
                ((p.x - first.x).powi(2) + (p.y - first.y).powi(2)).sqrt()
            } else {
                ((p.x - first.x) * dy - (p.y - first.y) * dx).abs() / length
            }
        };
        let (index, farthest) = line
            .iter()
            .enumerate()
            .skip(1)
            .take(line.len().saturating_sub(2))
            .map(|(i, p)| (i, distance(p)))
            .fold((0, 0.0), |best, candidate| {
                if candidate.1 > best.1 {
                    candidate
                } else {
                    best
                }
            });
        if farthest > SIMPLIFY_TOLERANCE {
            keep[offset + index] = true;
            recurse(&line[..=index], keep, offset);
            recurse(&line[index..], keep, offset + index);
        }
    }
    if line.len() < 3 {
        return line.to_vec();
    }
    let mut keep = vec![false; line.len()];
    keep[0] = true;
    keep[line.len() - 1] = true;
    recurse(line, &mut keep, 0);
    line.iter()
        .zip(keep)
        .filter_map(|(p, keep)| keep.then_some(*p))
        .collect()
}

/// The SVG: a flat background when no layer applies, each shape in its
/// style, the attribution in a corner. Shapes are drawn in file order
/// until `max_vertices` are placed or `max_shapes` are drawn: the
/// rasteriser pays per vertex and per element alike, and a point is one
/// vertex but one element.
fn draw(shapes: &[(Shape, Style)], extent: &Extent, spec: &Spec) -> String {
    let (width, height) = (f64::from(extent.width), f64::from(extent.height));
    let to_pixels = |p: &P| P {
        x: (p.x - extent.minx) / (extent.maxx - extent.minx) * width,
        y: (extent.maxy - p.y) / (extent.maxy - extent.miny) * height,
    };
    let mut svg = String::new();
    let _ = write!(
        svg,
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="{w}" height="{h}" viewBox="0 0 {w} {h}">"##,
        w = extent.width,
        h = extent.height,
    );
    svg.push('\n');
    if extent.layers.is_empty() {
        let _ = writeln!(
            svg,
            r##"<rect width="100%" height="100%" fill="{}"/>"##,
            spec.background
        );
    }
    let mut vertices = 0usize;
    for (count, (shape, style)) in shapes.iter().enumerate() {
        if count >= spec.max_shapes {
            break;
        }
        let drawn = Drawn::new(style);
        match shape {
            Shape::Point(p) => {
                vertices += 1;
                if vertices > spec.max_vertices {
                    break;
                }
                let p = to_pixels(p);
                let _ = writeln!(
                    svg,
                    r##"<circle cx="{:.1}" cy="{:.1}" r="{}" fill="{}" stroke="{}" stroke-width="{}"/>"##,
                    p.x,
                    p.y,
                    drawn.marker_size.radius(),
                    drawn.marker,
                    drawn.marker.darkened(),
                    MARKER_RING,
                );
            }
            Shape::Line(line) => {
                let line = simplify(&line.iter().map(to_pixels).collect::<Vec<_>>());
                vertices += line.len();
                if vertices > spec.max_vertices {
                    break;
                }
                let d = path_of(&[line], false);
                drawn.casing(&mut svg, &d, false);
                drawn.stroke(&mut svg, &d);
            }
            Shape::Polygon(rings) => {
                let rings: Vec<Vec<P>> = rings
                    .iter()
                    .map(|ring| simplify(&ring.iter().map(to_pixels).collect::<Vec<_>>()))
                    .collect();
                vertices += rings.iter().map(Vec::len).sum::<usize>();
                if vertices > spec.max_vertices {
                    break;
                }
                let d = path_of(&rings, true);
                drawn.casing(&mut svg, &d, true);
                drawn.stroke(&mut svg, &d);
            }
        }
    }
    if let Some(attribution) = extent
        .layers
        .iter()
        .filter_map(|&i| spec.layers.get(i))
        .find_map(|rule| rule.attribution.as_deref())
    {
        let _ = writeln!(
            svg,
            r##"<text x="{:.1}" y="{:.1}" text-anchor="end" font-family="sans-serif" font-size="12" fill="#222222" stroke="#ffffff" stroke-width="3" stroke-linejoin="round" paint-order="stroke">{}</text>"##,
            width - 6.0,
            height - 6.0,
            escape(attribution),
        );
    }
    svg.push_str("</svg>\n");
    svg
}

/// The `d` of a path through `parts`, each closed when `close`.
fn path_of(parts: &[Vec<P>], close: bool) -> String {
    let mut d = String::new();
    for part in parts {
        for (i, p) in part.iter().enumerate() {
            let _ = write!(
                d,
                "{}{:.1} {:.1}",
                if i == 0 { "M" } else { " L" },
                p.x,
                p.y
            );
        }
        if close {
            d.push('Z');
        }
        d.push(' ');
    }
    d.trim_end().to_string()
}

/// A style resolved to what is drawn: every key set, clamped.
struct Drawn {
    stroke: Color,
    stroke_width: f64,
    stroke_opacity: f64,
    fill: Color,
    fill_opacity: f64,
    marker: Color,
    marker_size: MarkerSize,
}

impl Drawn {
    fn new(style: &Style) -> Self {
        let clamp = |value: Option<f64>, max: f64, default: f64| match value {
            Some(v) if v.is_finite() => v.clamp(0.0, max),
            _ => default,
        };
        Self {
            stroke: style.stroke.unwrap_or(DEFAULT_COLOR),
            stroke_width: clamp(style.stroke_width, MAX_STROKE_WIDTH, 4.0),
            stroke_opacity: clamp(style.stroke_opacity, 1.0, 1.0),
            fill: style.fill.unwrap_or(DEFAULT_COLOR),
            fill_opacity: clamp(style.fill_opacity, 1.0, 0.35),
            marker: style.marker_color.unwrap_or(DEFAULT_COLOR),
            marker_size: style.marker_size.unwrap_or(MarkerSize::Medium),
        }
    }

    /// The darker, wider line under the stroke, carrying the fill when
    /// the shape is `filled`, a polygon.
    fn casing(&self, svg: &mut String, d: &str, filled: bool) {
        let fill_attributes = if filled {
            format!(
                r##"fill="{}" fill-opacity="{}" fill-rule="evenodd""##,
                self.fill, self.fill_opacity
            )
        } else {
            r##"fill="none""##.to_string()
        };
        if self.stroke_width == 0.0 {
            // No line: the fill alone, or nothing.
            if filled {
                let _ = writeln!(svg, r##"<path d="{d}" {fill_attributes}/>"##);
            }
            return;
        }
        let _ = writeln!(
            svg,
            r##"<path d="{d}" {fill_attributes} stroke="{}" stroke-width="{}" stroke-opacity="{}" stroke-linejoin="round" stroke-linecap="round"/>"##,
            self.stroke.darkened(),
            self.stroke_width + CASING,
            self.stroke_opacity,
        );
    }

    fn stroke(&self, svg: &mut String, d: &str) {
        if self.stroke_width == 0.0 {
            return;
        }
        let _ = writeln!(
            svg,
            r##"<path d="{d}" fill="none" stroke="{}" stroke-width="{}" stroke-opacity="{}" stroke-linejoin="round" stroke-linecap="round"/>"##,
            self.stroke, self.stroke_width, self.stroke_opacity,
        );
    }
}

/// `text` as XML character data.
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use agrafe_types::geojson::LayerRule;

    fn fixture(name: &str) -> Vec<u8> {
        let path = format!("{}/../../fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
        std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
    }

    fn spec() -> Spec {
        Spec {
            width: 400,
            height: 300,
            padding: 0.1,
            min_span: 200.0,
            max_vertices: 200_000,
            max_shapes: 20_000,
            style: Style {
                stroke: Some("#ffd400".parse().unwrap()),
                stroke_width: Some(4.0),
                stroke_opacity: Some(1.0),
                fill: Some("#ffd400".parse().unwrap()),
                fill_opacity: Some(0.35),
                marker_color: Some("#ffd400".parse().unwrap()),
                marker_size: Some(MarkerSize::Medium),
            },
            background: "#e8e4dc".parse().unwrap(),
            layers: vec![
                LayerRule {
                    attribution: Some("© IGN & co <Géoplateforme>".into()),
                    ..LayerRule::default()
                },
                LayerRule {
                    min_span: Some(3000.0),
                    ..LayerRule::default()
                },
            ],
        }
    }

    fn rendered(name: &str, spec: &Spec) -> Rendered {
        render(&fixture(name), spec).expect("rendered")
    }

    #[test]
    fn projects_to_web_mercator() {
        let at = |lon: f64, lat: f64| project(&Position::from([lon, lat]), &Crs::Degrees).unwrap();
        let origin = at(0.0, 0.0);
        assert!(origin.x.abs() < 1e-6 && origin.y.abs() < 1e-6, "{origin:?}");
        let paris = at(2.3522, 48.8566);
        assert!((paris.x - 261_845.7).abs() < 1.0, "{}", paris.x);
        assert!((paris.y - 6_250_564.3).abs() < 1.0, "{}", paris.y);
        // The poles are clamped to the edge of the square.
        let pole = at(0.0, 90.0);
        let edge = at(0.0, MAX_LATITUDE);
        assert_eq!(pole.y, edge.y);
        assert!((pole.y - std::f64::consts::PI * EARTH_RADIUS).abs() < 1.0);
        for bad in [
            vec![200.0, 0.0],
            vec![0.0, -91.0],
            vec![f64::NAN, 0.0],
            vec![0.0, f64::INFINITY],
            vec![1.0],
            vec![],
        ] {
            assert!(
                project(&Position::from(bad.clone()), &Crs::Degrees).is_err(),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn inverts_lambert_93() {
        // Reference values from PROJ (`cs2cs EPSG:4326 EPSG:2154`).
        let lcc = Lcc::lambert93();
        for (x, y, lon, lat) in [
            (700_000.0, 6_600_000.0, 3.0, 46.5),
            (652_469.023, 6_862_035.259, 2.3522, 48.8566),
            (146_632.979, 6_836_262.327, -4.4861, 48.3904),
            (1_050_362.695, 6_840_899.647, 7.7521, 48.5734),
            (1_226_126.037, 6_228_981.055, 9.45, 42.9667),
        ] {
            let (got_lon, got_lat) = lcc.inverse(x, y);
            assert!((got_lon - lon).abs() < 2e-8, "{x} {y}: {got_lon} vs {lon}");
            assert!((got_lat - lat).abs() < 2e-8, "{x} {y}: {got_lat} vs {lat}");
        }
    }

    #[test]
    fn honours_a_declared_crs() {
        let spec = spec();
        let degrees = rendered("point.geojson", &spec);
        let centre = |map: &Rendered| {
            (
                (map.extent.minx + map.extent.maxx) / 2.0,
                (map.extent.miny + map.extent.maxy) / 2.0,
            )
        };
        // Paris in Lambert-93, as QGIS exports it, lands on Paris.
        let lambert = rendered("lambert93.geojson", &spec);
        let (dx, dy) = (
            centre(&lambert).0 - centre(&degrees).0,
            centre(&lambert).1 - centre(&degrees).1,
        );
        assert!(dx.abs() < 0.5 && dy.abs() < 0.5, "{dx} {dy}");
        assert_eq!(lambert.svg.matches("<circle").count(), 1);
        assert_eq!(lambert.svg.matches("<path").count(), 2);
        // Web Mercator metres are taken as they are.
        let (x, y) = (centre(&degrees).0, centre(&degrees).1);
        let point_in = |crs: &str| {
            format!(
                r##"{{"type": "Feature", "crs": {crs}, "properties": null, "geometry": {{"type": "Point", "coordinates": [{x}, {y}]}}}}"##
            )
        };
        let named =
            |name: &str| format!(r##"{{"type": "name", "properties": {{"name": "{name}"}}}}"##);
        let mercator = render(point_in(&named("EPSG:3857")).as_bytes(), &spec).expect("rendered");
        assert_eq!(mercator.extent, degrees.extent);
        // Degrees under any of their names; null is degrees too.
        for name in [
            "urn:ogc:def:crs:OGC:1.3:CRS84",
            "urn:ogc:def:crs:EPSG::4326",
            "EPSG:4326",
            "http://www.opengis.net/def/crs/EPSG/0/4326",
        ] {
            assert_eq!(Crs::named(name), Some(Crs::Degrees), "{name}");
        }
        assert_eq!(
            Crs::named("urn:ogc:def:crs:EPSG::2154"),
            Some(Crs::Lambert93(Lcc::lambert93()))
        );
        assert_eq!(Crs::of(None), Some(Crs::Degrees));
        assert!(
            render(point_in("null").as_bytes(), &spec).is_none(),
            "metres are not degrees"
        );
        // A system that is not handled, or not a named one, declines.
        for crs in [
            named("EPSG:27572"),
            named("urn:ogc:def:crs:EPSG::32631"),
            named("Lambert 93"),
            r##"{"type": "link", "properties": {"href": "https://example.org/crs"}}"##.to_string(),
            "5".to_string(),
        ] {
            assert!(render(point_in(&crs).as_bytes(), &spec).is_none(), "{crs}");
        }
        // Off the globe in the declared system declines as well.
        let far = r##"{"type": "Point", "crs": {"type": "name", "properties": {"name": "EPSG:3857"}}, "coordinates": [30000000, 0]}"##;
        assert!(render(far.as_bytes(), &spec).is_none());
    }

    #[test]
    fn a_point_gets_the_smallest_extent_at_the_box_ratio() {
        let map = rendered("point.geojson", &spec());
        let extent = &map.extent;
        assert_eq!((extent.width, extent.height), (400, 300));
        let (w, h) = (extent.maxx - extent.minx, extent.maxy - extent.miny);
        assert!((h - 200.0).abs() < 1e-6, "{h}");
        assert!((w - 200.0 * 400.0 / 300.0).abs() < 1e-6, "{w}");
        // The point sits at the centre of the box.
        assert!(
            map.svg.contains(r##"<circle cx="200.0" cy="150.0" r="8""##),
            "{}",
            map.svg
        );
        assert!(map.svg.contains(r##"fill="#ffd400" stroke="#8c7400""##));
        // Both layers' rules: the first applies, the second wants 3 km.
        assert_eq!(extent.layers, vec![0]);
        assert!(!map.svg.contains("<rect"));
        assert!(
            map.svg
                .contains("© IGN &amp; co &lt;Géoplateforme&gt;</text>")
        );
    }

    #[test]
    fn a_polygon_keeps_its_hole_and_gets_a_casing() {
        let map = rendered("polygon-hole.geojson", &spec());
        let paths: Vec<&str> = map.svg.lines().filter(|l| l.starts_with("<path")).collect();
        assert_eq!(paths.len(), 2, "{}", map.svg);
        // The casing carries the fill, with the even-odd rule for the hole,
        // and a darker, wider stroke than the line on top.
        assert!(paths[0].contains(r##"fill="#ffd400" fill-opacity="0.35" fill-rule="evenodd""##));
        assert!(paths[0].contains(r##"stroke="#8c7400" stroke-width="7""##));
        assert!(paths[1].contains(r##"fill="none" stroke="#ffd400" stroke-width="4""##));
        // Two rings, both closed.
        assert_eq!(paths[0].matches('Z').count(), 2);
        assert_eq!(paths[0].matches('M').count(), 2);
        // Padded by a tenth of the larger side on each side.
        let extent = &map.extent;
        let span = extent.span();
        assert!(span > 200.0, "{span}");
        assert_eq!(extent.layers, vec![0], "the second layer wants 3 km");
        assert!(!map.svg.contains("<rect"));
        // Without a layer to draw on, the background is flat.
        let bare = Spec {
            layers: Vec::new(),
            ..spec()
        };
        let map = rendered("polygon-hole.geojson", &bare);
        assert!(map.extent.layers.is_empty());
        assert!(
            map.svg
                .contains(r##"<rect width="100%" height="100%" fill="#e8e4dc"/>"##),
            "{}",
            map.svg
        );
        assert!(!map.svg.contains("<text"));
    }

    #[test]
    fn lines_and_collections_are_drawn_in_order() {
        let map = rendered("multiline.geojson", &spec());
        let paths: Vec<&str> = map.svg.lines().filter(|l| l.starts_with("<path")).collect();
        // Two lines, a casing and a stroke each; a line has no fill.
        assert_eq!(paths.len(), 4, "{}", map.svg);
        assert!(paths.iter().all(|p| p.contains(r##"fill="none""##)));
        assert!(!map.svg.contains('Z'));

        let map = rendered("collection.geojson", &spec());
        let circles = map.svg.matches("<circle").count();
        let paths = map.svg.matches("<path").count();
        assert_eq!((circles, paths), (1, 4), "{}", map.svg);
    }

    #[test]
    fn a_world_spanning_collection_fits_the_box() {
        let map = rendered("world.geojson", &spec());
        let extent = &map.extent;
        let half_world = std::f64::consts::PI * EARTH_RADIUS;
        assert!(extent.maxx - extent.minx > 1.5 * half_world, "{extent:?}");
        assert_eq!(extent.layers, vec![0, 1]);
        // Every pixel coordinate lands inside the box.
        for number in map
            .svg
            .split(['"', ' ', 'M', 'L', 'Z'])
            .filter_map(|s| s.parse::<f64>().ok())
        {
            assert!((-1.0..=401.0).contains(&number), "{number} in {}", map.svg);
        }
    }

    #[test]
    fn declines_what_has_no_geometry() {
        let spec = spec();
        assert_eq!(render(&fixture("empty.geojson"), &spec), None);
        assert_eq!(render(br##"{"a": 1}"##, &spec), None);
        assert_eq!(render(b"not json", &spec), None);
        assert_eq!(
            render(
                br##"{"type": "Feature", "properties": {}, "geometry": null}"##,
                &spec
            ),
            None
        );
        assert_eq!(
            render(
                br##"{"type": "GeometryCollection", "geometries": []}"##,
                &spec
            ),
            None
        );
        // A line or a ring without positions is nothing to draw, not a
        // map of nowhere.
        for empty in [
            br##"{"type": "LineString", "coordinates": []}"## as &[u8],
            br##"{"type": "MultiLineString", "coordinates": [[]]}"##,
            br##"{"type": "Polygon", "coordinates": []}"##,
            br##"{"type": "Polygon", "coordinates": [[]]}"##,
            br##"{"type": "MultiPolygon", "coordinates": [[[]], []]}"##,
            br##"{"type": "MultiPoint", "coordinates": []}"##,
        ] {
            assert_eq!(
                render(empty, &spec),
                None,
                "{}",
                String::from_utf8_lossy(empty)
            );
        }
        // An empty hole is dropped, the polygon around it stays.
        let map = render(
            br##"{"type": "Polygon", "coordinates": [[[0, 0], [0.01, 0], [0.01, 0.01], [0, 0]], []]}"##,
            &spec,
        )
        .expect("rendered");
        assert_eq!(map.svg.matches('M').count(), 2, "{}", map.svg);
        // Off the globe, in degrees, is nothing to draw either: a Lambert-93
        // file, as French administrations export them, or a broken one.
        assert_eq!(render(&fixture("off-earth.geojson"), &spec), None);
        assert_eq!(
            render(
                br##"{"type": "Point", "coordinates": [652469.0, 6862035.3]}"##,
                &spec
            ),
            None
        );
    }

    #[test]
    fn honours_simplestyle_properties() {
        let map = rendered("styled.geojson", &spec());
        // A red polygon with a half-transparent green fill, cased in dark red.
        assert!(
            map.svg.contains(r##"fill="#00ff00" fill-opacity="0.5" fill-rule="evenodd" stroke="#8c0000" stroke-width="5""##),
            "{}",
            map.svg
        );
        assert!(
            map.svg
                .contains(r##"fill="none" stroke="#ff0000" stroke-width="2""##)
        );
        // A large blue marker.
        assert!(
            map.svg
                .contains(r##"r="12" fill="#0000ff" stroke="#00008c""##)
        );
        // The title and the symbol never reach the SVG.
        assert!(!map.svg.contains("star"));
        assert!(!map.svg.contains("Parcelle"));
    }

    #[test]
    fn ignores_hostile_style_values() {
        let map = rendered("hostile-style.geojson", &spec());
        // The colour that is not hex and the opacity of 7 fall back to the
        // default; the 10 000 px stroke is clamped.
        assert!(
            map.svg
                .contains(r##"stroke="#ffd400" stroke-width="20" stroke-opacity="1""##),
            "{}",
            map.svg
        );
        assert!(map.svg.contains(r##"fill-opacity="1""##));
        assert!(!map.svg.contains("red"));
        assert!(!map.svg.contains("10000"));
        assert!(map.svg.contains(r##"r="8" fill="#ffd400""##));
        let props: JsonObject = serde_json::from_str(
            r##"{"stroke": 12, "stroke-width": "4", "fill": "#12345", "marker-size": 3, "stroke-opacity": null}"##,
        )
        .unwrap();
        assert_eq!(style_of(&props), Style::default());
    }

    #[test]
    fn simplifies_to_the_pixel_and_stops_at_the_budget() {
        let straight: Vec<P> = (0..100)
            .map(|i| P {
                x: f64::from(i),
                y: f64::from(i) * 0.001,
            })
            .collect();
        assert_eq!(simplify(&straight).len(), 2);
        let zigzag: Vec<P> = (0..100)
            .map(|i| P {
                x: f64::from(i),
                y: if i % 2 == 0 { 0.0 } else { 3.0 },
            })
            .collect();
        assert_eq!(simplify(&zigzag).len(), 100);
        assert_eq!(simplify(&zigzag[..2]).len(), 2);

        // Two lines of four and two vertices, a budget for the first only.
        let tight = Spec {
            max_vertices: 5,
            ..spec()
        };
        let map = rendered("multiline.geojson", &tight);
        let paths = map.svg.matches("<path").count();
        assert_eq!(paths, 2, "{}", map.svg);
        let none = Spec {
            max_vertices: 0,
            ..spec()
        };
        let map = rendered("multiline.geojson", &none);
        assert_eq!(map.svg.matches("<path").count(), 0);
        // Shapes are budgeted too: a point is one vertex but one element.
        let one_shape = Spec {
            max_shapes: 1,
            ..spec()
        };
        let map = rendered("collection.geojson", &one_shape);
        assert_eq!(
            (
                map.svg.matches("<circle").count(),
                map.svg.matches("<path").count()
            ),
            (1, 0),
            "{}",
            map.svg
        );
        let map = rendered("multiline.geojson", &one_shape);
        assert_eq!(map.svg.matches("<path").count(), 2, "{}", map.svg);
    }

    #[test]
    fn a_degenerate_box_still_renders() {
        let tiny = Spec {
            width: 0,
            height: 0,
            padding: -1.0,
            min_span: -5.0,
            ..spec()
        };
        let map = rendered("point.geojson", &tiny);
        assert_eq!((map.extent.width, map.extent.height), (1, 1));
        assert!(map.extent.maxx > map.extent.minx);
    }
}
