//! The KML reader: placemarks as geometries, each with the style its
//! document gives it, always in WGS 84 degrees by the specification. A
//! `Point`, a `LineString`, a `LinearRing`, a `Polygon` with its outer
//! and inner boundaries, a `MultiGeometry` (flattened, like a geometry
//! collection) and a `gx:Track` of `gx:coord` elements, which is how
//! Google Earth writes a GPS track. `<coordinates>` is
//! whitespace-separated `lon,lat[,alt]` tuples. `Document`, `Folder` and
//! `Placemark` nest; `NetworkLink`, `GroundOverlay`, `ExtendedData`,
//! `Schema`, and every element the reader does not know, are skipped.
//!
//! Styles: a `<Style id>` holds `LineStyle`, `PolyStyle` and `IconStyle`;
//! a placemark names one with `<styleUrl>#id</styleUrl>` or holds one
//! inline, the inline one laid over the named one; a `<StyleMap id>`
//! holds `normal` and `highlight` pairs, of which `normal` is taken, and
//! may point at another map, so resolution is bounded to a few hops.
//! Shared styles may be declared after their use, so placemarks are
//! resolved once the document is read. The vocabulary maps onto
//! simplestyle: a colour is `aabbggrr`, alpha first and bytes reversed,
//! and its alpha is the opacity; a width is pixels; `fill` and `outline`
//! at 0 zero the fill and, for polygons, the outline; an icon colour
//! tints the marker, white being untinted; an icon scale below 0.8 is a
//! small marker, above 1.3 a large one. The icon's `href` is not
//! honoured, for the reason `marker-symbol` is not. A value that does
//! not parse is ignored, never an error: nothing from the file reaches
//! the SVG verbatim.
//!
//! A KMZ is a ZIP whose first `.kml` entry, `doc.kml` by convention, is
//! the document; the icons and overlays beside it are ignored.

use std::collections::HashMap;

use agrafe_types::map::{Color, MarkerSize, Style};
use geojson::{Geometry, GeometryValue, Position};
use quick_xml::Reader;
use quick_xml::events::{BytesStart, Event};

use crate::archive;

/// The KML document of a KMZ read at most, inflated: the size a bare
/// document may be, and well under the sandbox's memory.
const MAX_DOCUMENT: u64 = archive::MAX_INFLATED;
/// How many `styleUrl` references are followed from a placemark to a
/// style: a map pointing at a map pointing at a style is two.
const MAX_STYLE_HOPS: usize = 8;
/// The icon colour that is no tint: Google Earth's default, which leaves
/// the configured marker colour to apply.
const WHITE: Color = Color {
    r: 0xff,
    g: 0xff,
    b: 0xff,
};

/// The geometries of the KML document `bytes` with the style each is
/// drawn in, in document order. The style is what the document says and
/// nothing more, to be laid over the configured default. `None` when the
/// document is not KML, is not well-formed, or holds no placemark with
/// a geometry: nothing to draw. A document that ends before its elements
/// close, between two of them or inside a tag, is drawn as far as it
/// goes, the placemark it was reading included.
pub(crate) fn placemarks(bytes: &[u8]) -> Option<Vec<(Geometry, Style)>> {
    let mut document = Parser::default().parse(bytes)?;
    // The placemarks are moved out, not cloned: their coordinates are
    // most of a large document's memory, and the styles they name live
    // in `shared`, which stays.
    let placemarks = std::mem::take(&mut document.placemarks);
    let mut out = Vec::new();
    for placemark in placemarks {
        let named = placemark
            .url
            .as_deref()
            .and_then(|url| document.resolve_url(url));
        let inline = placemark
            .inline
            .as_ref()
            .and_then(|target| document.resolve(target));
        let style = match (inline, named) {
            (Some(inline), Some(named)) => inline.over(&named),
            (Some(style), None) | (None, Some(style)) => style,
            (None, None) => KmlStyle::default(),
        };
        for geometry in placemark.geometries {
            let polygon = matches!(geometry.value, GeometryValue::Polygon { .. });
            out.push((geometry, style.style(polygon)));
        }
    }
    (!out.is_empty()).then_some(out)
}

/// The KML document in the KMZ archive `bytes`: its first `.kml` entry,
/// skipping the `__MACOSX/` sidecars a Mac zips in. `None` when the
/// bytes are not a ZIP, no entry is a `.kml`, or the document is larger
/// than a bare one may be.
pub(crate) fn document_of_kmz(bytes: &[u8]) -> Option<Vec<u8>> {
    document_of_kmz_within(bytes, MAX_DOCUMENT)
}

fn document_of_kmz_within(bytes: &[u8], max_document: u64) -> Option<Vec<u8>> {
    let mut zip = archive::open(bytes)?;
    let index = archive::names(&zip)
        .find(|(_, name)| name.ends_with(".kml"))?
        .0;
    archive::read(&mut zip, index, max_document)
}

/// A style as a `<Style>` element spells it, every field optional: a
/// colour with the opacity its alpha gives, a width in pixels, the
/// `fill` and `outline` switches, the marker tint and scale.
#[derive(Debug, Clone, Default, PartialEq)]
struct KmlStyle {
    line_color: Option<(Color, f64)>,
    line_width: Option<f64>,
    poly_color: Option<(Color, f64)>,
    poly_fill: Option<bool>,
    poly_outline: Option<bool>,
    icon_color: Option<Color>,
    icon_scale: Option<f64>,
}

impl KmlStyle {
    /// This style laid over `base`, key by key, as a placemark's inline
    /// style is laid over the shared one it names.
    fn over(&self, base: &KmlStyle) -> KmlStyle {
        KmlStyle {
            line_color: self.line_color.or(base.line_color),
            line_width: self.line_width.or(base.line_width),
            poly_color: self.poly_color.or(base.poly_color),
            poly_fill: self.poly_fill.or(base.poly_fill),
            poly_outline: self.poly_outline.or(base.poly_outline),
            icon_color: self.icon_color.or(base.icon_color),
            icon_scale: self.icon_scale.or(base.icon_scale),
        }
    }

    /// The simplestyle equivalent, for a `polygon` or for a point or a
    /// line: `outline` is a polygon's switch, so it only zeroes the
    /// stroke there.
    fn style(&self, polygon: bool) -> Style {
        let (stroke, stroke_opacity) = self.line_color.unzip();
        let (fill, fill_opacity) = self.poly_color.unzip();
        Style {
            stroke,
            stroke_width: self.line_width,
            stroke_opacity: if polygon && self.poly_outline == Some(false) {
                Some(0.0)
            } else {
                stroke_opacity
            },
            fill,
            fill_opacity: if self.poly_fill == Some(false) {
                Some(0.0)
            } else {
                fill_opacity
            },
            marker_color: self.icon_color,
            marker_size: self.icon_scale.map(|scale| {
                if scale < 0.8 {
                    MarkerSize::Small
                } else if scale > 1.3 {
                    MarkerSize::Large
                } else {
                    MarkerSize::Medium
                }
            }),
        }
    }
}

/// What a `styleUrl` or a `StyleMap` pair points at.
#[derive(Debug, Clone, PartialEq)]
enum Target {
    /// `#id`, a shared style or map of this document; anything else is
    /// another file, not resolved.
    Url(String),
    /// A style written inline.
    Style(KmlStyle),
}

/// A shared style, by its `id`.
#[derive(Debug, Clone, PartialEq)]
enum Shared {
    Style(KmlStyle),
    /// A `StyleMap`'s `normal` pair, when it has one.
    Map(Option<Target>),
}

/// One `<Placemark>`: its geometries and what styles it.
#[derive(Debug, Default)]
struct Placemark {
    geometries: Vec<Geometry>,
    url: Option<String>,
    inline: Option<Target>,
}

/// The document once read: its placemarks and the shared styles they
/// name, which may be declared after them.
#[derive(Debug, Default)]
struct Document {
    placemarks: Vec<Placemark>,
    shared: HashMap<String, Shared>,
}

impl Document {
    /// The style `target` ends at: itself when written inline, else what
    /// its URL resolves to.
    fn resolve(&self, target: &Target) -> Option<KmlStyle> {
        match target {
            Target::Style(style) => Some(style.clone()),
            Target::Url(url) => self.resolve_url(url),
        }
    }

    /// The style the `styleUrl` `url` ends at, through at most
    /// `MAX_STYLE_HOPS` maps. `None` when it points outside the document,
    /// at nothing, or round in a circle.
    fn resolve_url<'a>(&'a self, mut url: &'a str) -> Option<KmlStyle> {
        for _ in 0..MAX_STYLE_HOPS {
            let id = match url.trim().split_once('#') {
                Some(("", id)) => id,
                Some(_) => return None,
                None => url.trim(),
            };
            match self.shared.get(id)? {
                Shared::Style(style) => return Some(style.clone()),
                Shared::Map(normal) => match normal.as_ref()? {
                    Target::Style(style) => return Some(style.clone()),
                    Target::Url(next) => url = next,
                },
            }
        }
        None
    }
}

/// Which of a `<Style>`'s parts a `<color>` or a `<width>` is in.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Section {
    Line,
    Poly,
    Icon,
}

#[derive(Debug, Default)]
struct StyleBuilder {
    id: Option<String>,
    style: KmlStyle,
    section: Option<Section>,
}

#[derive(Debug, Default)]
struct PairBuilder {
    key: Option<String>,
    target: Option<Target>,
}

#[derive(Debug, Default)]
struct MapBuilder {
    id: Option<String>,
    normal: Option<Target>,
    pair: Option<PairBuilder>,
}

/// The geometry element whose coordinates are being read.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Geom {
    Point,
    Line,
    Ring,
    Track,
}

/// Which boundary of a `<Polygon>` a `<LinearRing>` is.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Role {
    Outer,
    Inner,
}

#[derive(Debug, Default)]
struct PolygonBuilder {
    outer: Option<Vec<Position>>,
    inners: Vec<Vec<Position>>,
}

/// The reader's state: what is open, from the placemark down to the
/// text of the leaf being read.
#[derive(Debug, Default)]
struct Parser {
    document: Document,
    placemark: Option<Placemark>,
    style: Option<StyleBuilder>,
    map: Option<MapBuilder>,
    geom: Option<(Geom, Vec<Position>)>,
    polygon: Option<PolygonBuilder>,
    role: Option<Role>,
    /// The text of the leaf element being read, when it is one whose
    /// text is used.
    text: Option<String>,
}

impl Parser {
    fn parse(mut self, bytes: &[u8]) -> Option<Document> {
        let mut reader = Reader::from_reader(bytes);
        let mut buf = Vec::new();
        let mut root_seen = false;
        loop {
            let event = match reader.read_event_into(&mut buf) {
                Ok(event) => event,
                Err(err)
                    if root_seen
                        && crate::cut_short(&err, reader.buffer_position(), bytes.len()) =>
                {
                    break;
                }
                Err(_) => return None,
            };
            match event {
                Event::Start(start) | Event::Empty(start) if !root_seen => {
                    if start.local_name().as_ref() != "kml" {
                        return None;
                    }
                    root_seen = true;
                }
                Event::Start(start) => self.open(&start),
                Event::Text(text) => {
                    if let Some(buffer) = &mut self.text {
                        buffer.push_str(&text);
                    }
                }
                Event::CData(text) => {
                    if let Some(buffer) = &mut self.text {
                        buffer.push_str(&text);
                    }
                }
                Event::End(end) => self.close(end.local_name().as_ref()),
                Event::Eof => break,
                _ => {}
            }
            buf.clear();
        }
        self.cut();
        Some(self.document)
    }

    /// Close what the document left open at its end, innermost first, as
    /// its end tags would have: the coordinates being read, less a tuple
    /// the cut may have split, the geometry, the polygon, the placemark.
    /// A style left open styles nothing: its placemark is the one cut.
    fn cut(&mut self) {
        if let Some(mut text) = self.text.take()
            && let Some((geom, _)) = &self.geom
        {
            if !text.ends_with(char::is_whitespace) {
                text.truncate(text.rfind(char::is_whitespace).unwrap_or(0));
            }
            let leaf = if matches!(geom, Geom::Track) {
                "coord"
            } else {
                "coordinates"
            };
            self.leaf(leaf, &text);
        }
        self.geometry_end();
        if self.polygon.is_some() {
            self.close("Polygon");
        }
        if self.placemark.is_some() {
            self.close("Placemark");
        }
    }

    fn open(&mut self, start: &BytesStart<'_>) {
        let id = || {
            start
                .attributes()
                .flatten()
                .find(|a| a.key.local_name().as_ref() == "id")
                .map(|a| a.value.trim().to_string())
        };
        match start.local_name().as_ref() {
            "Placemark" => self.placemark = Some(Placemark::default()),
            "Style" => {
                self.style = Some(StyleBuilder {
                    id: id(),
                    ..StyleBuilder::default()
                });
            }
            "StyleMap" => {
                self.map = Some(MapBuilder {
                    id: id(),
                    ..MapBuilder::default()
                });
            }
            "Pair" => {
                if let Some(map) = &mut self.map {
                    map.pair = Some(PairBuilder::default());
                }
            }
            "LineStyle" => self.section(Section::Line),
            "PolyStyle" => self.section(Section::Poly),
            "IconStyle" => self.section(Section::Icon),
            "Point" => self.geom = Some((Geom::Point, Vec::new())),
            "LineString" => self.geom = Some((Geom::Line, Vec::new())),
            "LinearRing" => self.geom = Some((Geom::Ring, Vec::new())),
            "Track" => self.geom = Some((Geom::Track, Vec::new())),
            "Polygon" => self.polygon = Some(PolygonBuilder::default()),
            "outerBoundaryIs" => self.role = Some(Role::Outer),
            "innerBoundaryIs" => self.role = Some(Role::Inner),
            "coordinates" if self.geom.is_some() => self.text = Some(String::new()),
            "coord" if matches!(self.geom, Some((Geom::Track, _))) => {
                self.text = Some(String::new());
            }
            "color" | "width" | "fill" | "outline" | "scale"
                if self.style.as_ref().is_some_and(|s| s.section.is_some()) =>
            {
                self.text = Some(String::new());
            }
            "key" if self.in_pair() => self.text = Some(String::new()),
            "styleUrl" if self.in_pair() || self.placemark.is_some() => {
                self.text = Some(String::new());
            }
            _ => {}
        }
    }

    fn close(&mut self, name: &str) {
        if let Some(text) = self.text.take() {
            self.leaf(name, &text);
        }
        match name {
            "Placemark" => {
                if let Some(placemark) = self.placemark.take()
                    && !placemark.geometries.is_empty()
                {
                    self.document.placemarks.push(placemark);
                }
            }
            "Style" => {
                if let Some(built) = self.style.take() {
                    if let Some(pair) = self.map.as_mut().and_then(|m| m.pair.as_mut()) {
                        pair.target = Some(Target::Style(built.style.clone()));
                    } else if let Some(placemark) = &mut self.placemark {
                        placemark.inline = Some(Target::Style(built.style.clone()));
                    }
                    if let Some(id) = built.id {
                        self.document.shared.insert(id, Shared::Style(built.style));
                    }
                }
            }
            "StyleMap" => {
                if let Some(built) = self.map.take() {
                    if let Some(placemark) = &mut self.placemark {
                        placemark.inline = built.normal.clone();
                    }
                    if let Some(id) = built.id {
                        self.document.shared.insert(id, Shared::Map(built.normal));
                    }
                }
            }
            "Pair" => {
                if let Some(map) = &mut self.map
                    && let Some(pair) = map.pair.take()
                {
                    // A pair without a key is `normal`, unless a keyed one
                    // says otherwise.
                    match pair.key.as_deref() {
                        Some("normal") => map.normal = pair.target,
                        None if map.normal.is_none() => map.normal = pair.target,
                        _ => {}
                    }
                }
            }
            "LineStyle" | "PolyStyle" | "IconStyle" => self.section_end(),
            "Point" | "LineString" | "LinearRing" | "Track" => self.geometry_end(),
            "Polygon" => {
                if let Some(polygon) = self.polygon.take()
                    && let Some(outer) = polygon.outer
                {
                    let mut coordinates = vec![outer];
                    coordinates.extend(polygon.inners);
                    self.push(GeometryValue::Polygon { coordinates });
                }
            }
            "outerBoundaryIs" | "innerBoundaryIs" => self.role = None,
            _ => {}
        }
    }

    /// The text of a leaf element, once it closes.
    fn leaf(&mut self, name: &str, text: &str) {
        match name {
            "coordinates" => {
                if let Some((_, positions)) = &mut self.geom {
                    positions.extend(tuples(text));
                }
            }
            "coord" => {
                if let Some((Geom::Track, positions)) = &mut self.geom {
                    let mut parts = text.split_whitespace();
                    if let (Some(lon), Some(lat)) = (number(parts.next()), number(parts.next())) {
                        positions.push(Position::from([lon, lat]));
                    }
                }
            }
            "color" | "width" | "fill" | "outline" | "scale" => {
                if let Some(built) = &mut self.style
                    && let Some(section) = built.section
                {
                    let style = &mut built.style;
                    match (section, name) {
                        (Section::Line, "color") => style.line_color = argb(text),
                        (Section::Line, "width") => style.line_width = number(Some(text)),
                        (Section::Poly, "color") => style.poly_color = argb(text),
                        (Section::Poly, "fill") => style.poly_fill = switch(text),
                        (Section::Poly, "outline") => style.poly_outline = switch(text),
                        (Section::Icon, "color") => {
                            // White is no tint: the configured colour applies.
                            style.icon_color = argb(text).map(|(c, _)| c).filter(|&c| c != WHITE);
                        }
                        (Section::Icon, "scale") => style.icon_scale = number(Some(text)),
                        _ => {}
                    }
                }
            }
            "key" => {
                if let Some(pair) = self.map.as_mut().and_then(|m| m.pair.as_mut()) {
                    pair.key = Some(text.trim().to_string());
                }
            }
            "styleUrl" => {
                if let Some(pair) = self.map.as_mut().and_then(|m| m.pair.as_mut()) {
                    pair.target = Some(Target::Url(text.to_string()));
                } else if let Some(placemark) = &mut self.placemark {
                    placemark.url = Some(text.to_string());
                }
            }
            _ => {}
        }
    }

    fn in_pair(&self) -> bool {
        self.map.as_ref().is_some_and(|m| m.pair.is_some())
    }

    fn section(&mut self, section: Section) {
        if let Some(built) = &mut self.style {
            built.section = Some(section);
        }
    }

    fn section_end(&mut self) {
        if let Some(built) = &mut self.style {
            built.section = None;
        }
    }

    /// A geometry element closed: what it holds goes to the polygon
    /// being built, or to the placemark. A line of fewer than two points
    /// is not a line, and an empty ring is nothing.
    fn geometry_end(&mut self) {
        let Some((geom, positions)) = self.geom.take() else {
            return;
        };
        match geom {
            Geom::Point => {
                if let Some(position) = positions.into_iter().next() {
                    self.push(GeometryValue::Point {
                        coordinates: position,
                    });
                }
            }
            Geom::Ring if self.polygon.is_some() && self.role.is_some() => {
                if positions.is_empty() {
                    return;
                }
                let polygon = self.polygon.as_mut().expect("checked");
                match self.role {
                    Some(Role::Outer) if polygon.outer.is_none() => {
                        polygon.outer = Some(positions);
                    }
                    Some(Role::Inner) => polygon.inners.push(positions),
                    _ => {}
                }
            }
            // A bare ring, or a line or a track, is a line.
            Geom::Line | Geom::Ring | Geom::Track => {
                if positions.len() >= 2 {
                    self.push(GeometryValue::LineString {
                        coordinates: positions,
                    });
                }
            }
        }
    }

    fn push(&mut self, value: GeometryValue) {
        if let Some(placemark) = &mut self.placemark {
            placemark.geometries.push(Geometry::new(value));
        }
    }
}

/// The positions in a `<coordinates>` text: whitespace-separated
/// `lon,lat[,alt]` tuples. A tuple that is not two numbers is skipped.
fn tuples(text: &str) -> impl Iterator<Item = Position> + '_ {
    text.split_whitespace().filter_map(|tuple| {
        let mut parts = tuple.split(',');
        let lon = number(parts.next())?;
        let lat = number(parts.next())?;
        Some(Position::from([lon, lat]))
    })
}

/// A finite number, or nothing.
fn number(text: Option<&str>) -> Option<f64> {
    text?.trim().parse::<f64>().ok().filter(|n| n.is_finite())
}

/// A KML colour, `aabbggrr` in hex, as the colour and the opacity its
/// alpha gives. Anything else is nothing.
fn argb(text: &str) -> Option<(Color, f64)> {
    let hex = text.trim();
    if hex.len() != 8 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let byte = |at: usize| u8::from_str_radix(&hex[at..at + 2], 16).ok();
    let (a, b, g, r) = (byte(0)?, byte(2)?, byte(4)?, byte(6)?);
    Some((Color { r, g, b }, f64::from(a) / 255.0))
}

/// A KML boolean: `0` or `false` is off, `1` or `true` is on.
fn switch(text: &str) -> Option<bool> {
    match text.trim() {
        "0" | "false" => Some(false),
        "1" | "true" => Some(true),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(bytes: &[u8]) -> Option<Vec<String>> {
        placemarks(bytes).map(|placemarks| {
            placemarks
                .iter()
                .map(|(g, _)| match &g.value {
                    GeometryValue::Point { coordinates } => {
                        format!("point {:?}", coordinates.as_slice())
                    }
                    GeometryValue::LineString { coordinates } => {
                        format!("line of {}", coordinates.len())
                    }
                    GeometryValue::Polygon { coordinates } => format!(
                        "polygon of {:?}",
                        coordinates.iter().map(Vec::len).collect::<Vec<_>>()
                    ),
                    other => panic!("{other:?}"),
                })
                .collect()
        })
    }

    fn styles(bytes: &[u8]) -> Vec<Style> {
        placemarks(bytes)
            .unwrap()
            .into_iter()
            .map(|(_, style)| style)
            .collect()
    }

    fn color(hex: &str) -> Option<Color> {
        Some(hex.parse().unwrap())
    }

    #[test]
    fn reads_every_geometry_in_order() {
        let doc = br#"<?xml version="1.0" encoding="UTF-8"?>
<kml xmlns="http://www.opengis.net/kml/2.2" xmlns:gx="http://www.google.com/kml/ext/2.2">
<Document>
  <name>Lyon</name>
  <Folder>
    <Placemark>
      <name>Fourviere</name>
      <Point><coordinates>4.8225,45.7623,287</coordinates></Point>
    </Placemark>
    <Placemark>
      <LineString><tessellate>1</tessellate><coordinates>
        4.8320,45.7578 4.8357,45.7640
        4.8290,45.7700,0
      </coordinates></LineString>
    </Placemark>
  </Folder>
  <Placemark>
    <Polygon>
      <outerBoundaryIs><LinearRing><coordinates>4.80,45.75 4.86,45.75 4.86,45.78 4.80,45.78 4.80,45.75</coordinates></LinearRing></outerBoundaryIs>
      <innerBoundaryIs><LinearRing><coordinates>4.82,45.76 4.84,45.76 4.84,45.77 4.82,45.76</coordinates></LinearRing></innerBoundaryIs>
    </Polygon>
  </Placemark>
  <Placemark>
    <MultiGeometry>
      <Point><coordinates>4.0,46.0</coordinates></Point>
      <MultiGeometry><LineString><coordinates>4.0,46.0 4.1,46.1</coordinates></LineString></MultiGeometry>
      <LinearRing><coordinates>4.0,46.0 4.1,46.0 4.1,46.1 4.0,46.0</coordinates></LinearRing>
    </MultiGeometry>
  </Placemark>
  <Placemark>
    <gx:Track>
      <when>2026-05-10T08:10:00Z</when>
      <when>2026-05-10T08:12:00Z</when>
      <gx:coord>4.8000 45.7500 170</gx:coord>
      <gx:coord>4.8100 45.7550 172</gx:coord>
      <gx:coord>4.8200 45.7520</gx:coord>
    </gx:Track>
  </Placemark>
  <Placemark>
    <gx:MultiTrack>
      <gx:Track><gx:coord>4.84 45.77 0</gx:coord><gx:coord>4.85 45.775 0</gx:coord></gx:Track>
      <gx:Track><gx:coord>4.86 45.78 0</gx:coord></gx:Track>
    </gx:MultiTrack>
  </Placemark>
</Document>
</kml>"#;
        assert_eq!(
            kinds(doc).unwrap(),
            [
                "point [4.8225, 45.7623]",
                "line of 3",
                "polygon of [5, 4]",
                "point [4.0, 46.0]",
                "line of 2",
                "line of 4",
                "line of 3",
                "line of 2",
            ]
        );
    }

    #[test]
    fn resolves_shared_inline_and_mapped_styles() {
        let doc = br##"<kml xmlns="http://www.opengis.net/kml/2.2">
<Document>
  <Placemark>
    <styleUrl>#late</styleUrl>
    <Point><coordinates>4,45</coordinates></Point>
  </Placemark>
  <Placemark>
    <styleUrl>#map</styleUrl>
    <LineString><coordinates>4,45 5,46</coordinates></LineString>
  </Placemark>
  <Placemark>
    <styleUrl>#red</styleUrl>
    <Style><LineStyle><width>2</width></LineStyle><PolyStyle><color>8000ff00</color></PolyStyle></Style>
    <Polygon><outerBoundaryIs><LinearRing><coordinates>4,45 5,45 5,46 4,45</coordinates></LinearRing></outerBoundaryIs></Polygon>
  </Placemark>
  <Placemark>
    <Style><IconStyle><color>ff0000ff</color><scale>1.5</scale></IconStyle></Style>
    <Point><coordinates>4,45</coordinates></Point>
  </Placemark>
  <Placemark>
    <styleUrl>#nowhere</styleUrl>
    <Point><coordinates>4,45</coordinates></Point>
  </Placemark>
  <Placemark>
    <styleUrl>red</styleUrl>
    <Point><coordinates>4,45</coordinates></Point>
  </Placemark>
  <Style id="red">
    <LineStyle><color>ff0000ff</color><width>5</width></LineStyle>
    <PolyStyle><color>7f00ff00</color></PolyStyle>
    <IconStyle><color>ff0000ff</color><scale>0.5</scale><Icon><href>http://maps.google.com/x.png</href></Icon></IconStyle>
    <LabelStyle><color>ff00ff00</color><scale>3</scale></LabelStyle>
  </Style>
  <Style id="late"><IconStyle><color>ffff0000</color></IconStyle></Style>
  <StyleMap id="map">
    <Pair><key>highlight</key><styleUrl>#late</styleUrl></Pair>
    <Pair><key>normal</key><styleUrl>#chain</styleUrl></Pair>
  </StyleMap>
  <StyleMap id="chain">
    <Pair><key>normal</key><Style><LineStyle><color>80ffffff</color></LineStyle></Style></Pair>
  </StyleMap>
</Document>
</kml>"##;
        let styles = styles(doc);
        assert_eq!(styles.len(), 6);
        // Declared after its use: a blue marker, in KML's bytes order.
        assert_eq!(styles[0].marker_color, color("#0000ff"));
        assert_eq!(styles[0].stroke, None);
        // Through two maps to an inline style: a half-transparent white.
        assert_eq!(styles[1].stroke, color("#ffffff"));
        assert!((styles[1].stroke_opacity.unwrap() - 128.0 / 255.0).abs() < 1e-9);
        // Inline over shared: the width and the fill from the inline, the
        // stroke from the shared.
        assert_eq!(styles[2].stroke, color("#ff0000"));
        assert_eq!(styles[2].stroke_width, Some(2.0));
        assert_eq!(styles[2].fill, color("#00ff00"));
        assert!((styles[2].fill_opacity.unwrap() - 128.0 / 255.0).abs() < 1e-9);
        assert_eq!(styles[2].marker_size, Some(MarkerSize::Small));
        // Inline only: a large red marker.
        assert_eq!(styles[3].marker_color, color("#ff0000"));
        assert_eq!(styles[3].marker_size, Some(MarkerSize::Large));
        // A name that is not declared: nothing.
        assert_eq!(styles[4], Style::default());
        // Without the `#`, as some writers spell it.
        assert_eq!(styles[5].stroke, color("#ff0000"));
        assert_eq!(styles[5].stroke_width, Some(5.0));
        // The label's colour and scale never reach a marker.
        assert_eq!(styles[5].marker_color, color("#ff0000"));
        assert_eq!(styles[5].marker_size, Some(MarkerSize::Small));
    }

    #[test]
    fn maps_the_switches_and_the_tint() {
        let doc = br##"<kml>
  <Style id="s">
    <LineStyle><color>ff00ff00</color><width>3</width></LineStyle>
    <PolyStyle><fill>0</fill><color>ffff0000</color><outline>false</outline></PolyStyle>
    <IconStyle><color>ffffffff</color><scale>1</scale></IconStyle>
  </Style>
  <Placemark><styleUrl>#s</styleUrl><Polygon><outerBoundaryIs><LinearRing><coordinates>4,45 5,45 5,46 4,45</coordinates></LinearRing></outerBoundaryIs></Polygon></Placemark>
  <Placemark><styleUrl>#s</styleUrl><LineString><coordinates>4,45 5,46</coordinates></LineString></Placemark>
  <Placemark><styleUrl>#s</styleUrl><Point><coordinates>4,45</coordinates></Point></Placemark>
  <Style id="on"><PolyStyle><fill>1</fill><outline>1</outline><color>40ff0000</color></PolyStyle></Style>
  <Placemark><styleUrl>#on</styleUrl><Polygon><outerBoundaryIs><LinearRing><coordinates>4,45 5,45 5,46 4,45</coordinates></LinearRing></outerBoundaryIs></Polygon></Placemark>
</kml>"##;
        let styles = styles(doc);
        // The polygon: no fill, no outline.
        assert_eq!(styles[0].fill, color("#0000ff"));
        assert_eq!(styles[0].fill_opacity, Some(0.0));
        assert_eq!(styles[0].stroke_opacity, Some(0.0));
        assert_eq!(styles[0].stroke, color("#00ff00"));
        // The line keeps its stroke: `outline` is a polygon's switch.
        assert_eq!(styles[1].stroke_opacity, Some(1.0));
        assert_eq!(styles[1].stroke_width, Some(3.0));
        // White is untinted; a scale of 1 is the medium marker.
        assert_eq!(styles[2].marker_color, None);
        assert_eq!(styles[2].marker_size, Some(MarkerSize::Medium));
        // The switches on leave the alpha as it is.
        assert!((styles[3].fill_opacity.unwrap() - 64.0 / 255.0).abs() < 1e-9);
        assert_eq!(styles[3].stroke_opacity, None);
    }

    #[test]
    fn ignores_what_it_does_not_know_or_cannot_read() {
        let doc = br##"<k:kml xmlns:k="http://www.opengis.net/kml/2.2">
  <k:NetworkLink><k:Link><k:href>http://example.org/x.kml</k:href></k:Link></k:NetworkLink>
  <k:GroundOverlay><k:Icon><k:href>x.png</k:href></k:Icon><k:LatLonBox><k:north>46</k:north></k:LatLonBox></k:GroundOverlay>
  <k:Style id="bad">
    <k:LineStyle><k:color>red</k:color><k:width>wide</k:width></k:LineStyle>
    <k:PolyStyle><k:color>ff00ff0</k:color><k:fill>maybe</k:fill></k:PolyStyle>
    <k:IconStyle><k:scale>NaN</k:scale></k:IconStyle>
  </k:Style>
  <k:Placemark>
    <k:styleUrl>#bad</k:styleUrl>
    <k:ExtendedData><k:Data name="stroke"><k:value>#ff0000</k:value></k:Data></k:ExtendedData>
    <k:Point><k:coordinates> 4.5 , 45.5 </k:coordinates></k:Point>
  </k:Placemark>
  <k:Placemark>
    <k:LineString><k:coordinates>4,45 x,45 4,y 5 5,46,0,1 ,45 4,46</k:coordinates></k:LineString>
  </k:Placemark>
  <k:Placemark><k:styleUrl>other.kml#red</k:styleUrl><k:Point><k:coordinates><![CDATA[4,45]]></k:coordinates></k:Point></k:Placemark>
  <k:Placemark><k:Point><k:coordinates/></k:Point><k:name>nowhere</k:name></k:Placemark>
  <k:Placemark><k:LineString><k:coordinates>4,45</k:coordinates></k:LineString></k:Placemark>
  <k:Placemark><k:Polygon><k:innerBoundaryIs><k:LinearRing><k:coordinates>4,45 5,45 5,46 4,45</k:coordinates></k:LinearRing></k:innerBoundaryIs></k:Polygon></k:Placemark>
  <k:Placemark><k:Polygon><k:outerBoundaryIs><k:LinearRing><k:coordinates></k:coordinates></k:LinearRing></k:outerBoundaryIs></k:Polygon></k:Placemark>
  <k:Point><k:coordinates>4,45</k:coordinates></k:Point>
  <k:Placemark><k:Model><k:Location><k:longitude>4</k:longitude><k:latitude>45</k:latitude></k:Location></k:Model></k:Placemark>
</k:kml>"##;
        assert_eq!(kinds(doc).unwrap(), ["line of 3", "point [4.0, 45.0]"]);
        // The line: the tuples that are two numbers, and nothing from
        // `ExtendedData` on anything.
        assert_eq!(styles(doc), [Style::default(), Style::default()]);
        // A document cut short keeps what it holds, the placemark it was
        // cut in included, less a tuple the cut split, wherever the cut
        // falls: in the coordinates, between elements, in a tag.
        let whole = b"<kml><Placemark><Point><coordinates>4,45</coordinates></Point></Placemark><Placemark><LineString><coordinates>4,45 5,46 6,47</coordinates></LineString></Placemark></kml>";
        let cut = |at: &str| {
            &whole[..whole
                .windows(at.len())
                .position(|w| w == at.as_bytes())
                .unwrap()]
        };
        for (at, expected) in [
            // Text that ends at the cut may end inside its last tuple.
            ("</coordinates></LineString>", "line of 2"),
            (",47</coordinates></LineString>", "line of 2"),
            ("</LineString>", "line of 3"),
            ("LineString></Placemark></kml>", "line of 3"),
        ] {
            assert_eq!(
                kinds(cut(at)).unwrap(),
                ["point [4.0, 45.0]", expected],
                "cut at {at}"
            );
        }
        for at in [" 6,47", " 5,46"] {
            assert_eq!(
                kinds(cut(at)).unwrap(),
                ["point [4.0, 45.0]"],
                "cut at {at}"
            );
        }
        // A map that points at itself, or at nothing, ends in the default.
        let circular = br##"<kml>
  <StyleMap id="a"><Pair><key>normal</key><styleUrl>#b</styleUrl></Pair></StyleMap>
  <StyleMap id="b"><Pair><key>normal</key><styleUrl>#a</styleUrl></Pair></StyleMap>
  <StyleMap id="empty"><Pair><key>highlight</key><styleUrl>#a</styleUrl></Pair></StyleMap>
  <Placemark><styleUrl>#a</styleUrl><Point><coordinates>4,45</coordinates></Point></Placemark>
  <Placemark><styleUrl>#empty</styleUrl><Point><coordinates>4,45</coordinates></Point></Placemark>
</kml>"##;
        assert_eq!(styles(circular), [Style::default(), Style::default()]);
    }

    #[test]
    fn declines_what_is_not_kml_or_holds_no_geometry() {
        for doc in [
            b"" as &[u8],
            b"<kml/>",
            b"<kml><Document><name>empty</name><Placemark><name>x</name></Placemark></Document></kml>",
            b"<kml><Style id=\"s\"><LineStyle><color>ff0000ff</color></LineStyle></Style></kml>",
            b"<gpx><wpt lat=\"45\" lon=\"4\"/></gpx>",
            b"<Document><Placemark><Point><coordinates>4,45</coordinates></Point></Placemark></Document>",
            b"<?xml version=\"1.0\"?><kml><Placemark><Point><coordinates>4,45</coordinates></Point></Placemarks></kml>",
            b"not xml",
            br#"{"type": "Feature"}"#,
        ] {
            assert_eq!(kinds(doc), None, "{}", String::from_utf8_lossy(doc));
        }
    }

    #[test]
    fn takes_the_first_kml_entry_of_a_kmz() {
        use crate::archive::zipped;
        let doc =
            b"<kml><Placemark><Point><coordinates>4,45</coordinates></Point></Placemark></kml>";
        let kmz = zipped(&[("doc.kml", doc), ("files/icon.png", b"\x89PNG")]);
        assert_eq!(document_of_kmz(&kmz).as_deref(), Some(doc as &[u8]));
        // The document need not come first, or be named `doc.kml`, and a
        // Mac's sidecar is not it.
        let kmz = zipped(&[
            ("__MACOSX/._doc.kml", b"\x00\x05\x16\x07"),
            ("files/icon.png", b"\x89PNG"),
            ("Carte.KML", doc),
            ("other.kml", b"<kml/>"),
        ]);
        assert_eq!(document_of_kmz(&kmz).as_deref(), Some(doc as &[u8]));
        assert_eq!(
            document_of_kmz(&zipped(&[("files/icon.png", b"\x89PNG")])),
            None
        );
        assert_eq!(document_of_kmz(&zipped(&[])), None);
        assert_eq!(document_of_kmz(doc), None);
        assert_eq!(document_of_kmz(b""), None);
        // As many entries as allowed, the document last, is read; one more
        // and the archive is not looked through.
        let names: Vec<String> = (1..archive::MAX_ENTRIES)
            .map(|i| format!("files/{i}.png"))
            .collect();
        let mut entries: Vec<(&str, &[u8])> = names
            .iter()
            .map(|name| (name.as_str(), b"\x89PNG" as &[u8]))
            .collect();
        entries.push(("doc.kml", doc));
        assert_eq!(
            document_of_kmz(&zipped(&entries)).as_deref(),
            Some(doc as &[u8])
        );
        entries.insert(0, ("one-more.png", b"\x89PNG"));
        assert_eq!(document_of_kmz(&zipped(&entries)), None);
        // A document over the cap is not read.
        let kmz = zipped(&[("doc.kml", doc)]);
        assert_eq!(document_of_kmz_within(&kmz, doc.len() as u64 - 1), None);
        assert_eq!(
            document_of_kmz_within(&kmz, doc.len() as u64).as_deref(),
            Some(doc as &[u8])
        );
    }
}
