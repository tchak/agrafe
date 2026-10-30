//! The GML reader: every geometry in a GML namespace, wherever it sits,
//! each in the system its `srsName` names. GML has no styling (SLD is a
//! separate document), so the configured default applies.
//!
//! The reader does not model features: an application schema names its
//! features and the properties holding their geometry, and those cannot
//! be known, so `<cadastre:Parcelle>`, `<gml:featureMember>`,
//! `<wfs:member>` and the property are traversal. A geometry consumes
//! its content, so a polygon's ring is not drawn twice; inside one, an
//! element that is neither a part of it nor a position is skipped with
//! its content. `boundedBy` and its `Envelope` are not drawn, and an
//! `xlink:href` member, which holds nothing, is nothing.
//!
//! Both generations of the vocabulary: `Point`, `LineString`, `Polygon`
//! (`exterior`/`interior`, and GML 2's `outerBoundaryIs`/
//! `innerBoundaryIs`), `MultiPoint`, `MultiLineString`, `MultiPolygon`,
//! `MultiCurve`, `MultiSurface` and `MultiGeometry` (flattened, like a
//! geometry collection), their `*Member` and `*Members` wrappers; a
//! `Curve` of `LineStringSegment`s joined end to start into one line, a
//! `Surface` of `PolygonPatch`es, a polygon each, a `Ring` of curve
//! members joined the same way, a `CompositeCurve` joined like a ring
//! and a `CompositeSurface` flattened like a collection, the orientable
//! wrappers of each as their base. `Arc`, `Circle`, the other segment
//! kinds, `Solid` and the rest are skipped. Positions are `pos`,
//! `posList` (flat, `srsDimension` on the list, else inherited, else 2),
//! GML 2's `coordinates` (tuples split by `ts`, ordinates by `cs`, with
//! its `decimal`) and `coord` with its `X` and `Y`; a third ordinate is
//! dropped.
//!
//! The system is the `srsName` on the geometry, or on its positions,
//! where the schema allows one, else the nearest
//! enclosing element's, where the envelope of a `boundedBy` counts for
//! the element it bounds: the collection's, which is where a WFS puts
//! it, or a feature's own, for the geometries after it. GML has no
//! default, so a geometry with none declines the document, like a
//! shapefile without its `.prj`. Axis
//! order follows the OGC policy: the URN and `opengis.net/def` spellings
//! of EPSG:4326 are latitude first, and swapped here; `EPSG:4326`, GML
//! 2's `epsg.xml#4326` and CRS84 are longitude first, and Lambert-93 and
//! Web Mercator easting first in every spelling.

use std::collections::HashMap;

use agrafe_types::gml::NAMESPACES;
use geojson::{Geometry, GeometryValue, Position};
use quick_xml::NsReader;
use quick_xml::events::{BytesStart, Event};
use quick_xml::name::{Namespace, ResolveResult};

use crate::Crs;

/// How deep geometries may nest in one another: a `MultiSurface` of
/// `Surface`s of `PolygonPatch`es of rings is four. A geometry deeper
/// than this is skipped with its content, so that the stack of what is
/// open stays small whatever the document.
const MAX_NESTING: usize = 16;

/// Two ordinates, as the document writes them.
type Xy = [f64; 2];

/// A geometry once read, before its system is known. Collections are
/// flattened into their members, so nothing here nests.
#[derive(Debug, Clone, PartialEq)]
enum Part {
    Point(Xy),
    Line(Vec<Xy>),
    /// The outer ring first, then the holes.
    Polygon(Vec<Vec<Xy>>),
}

/// The geometries of the GML document `bytes`, each with its system, in
/// document order, a collection's members one by one. `None` when the
/// document is not well-formed, holds no geometry, or has one without a
/// `srsName` or in a system that is not handled: nothing to draw. A
/// document that ends before its elements close, between two of them or
/// inside a tag, is drawn as far as it goes, the geometry it was reading
/// included.
pub(crate) fn geometries(bytes: &[u8]) -> Option<Vec<(Geometry, Crs)>> {
    let read = Parser::default().parse(bytes)?;
    let systems: Vec<Option<(Crs, bool)>> = read.srs.names.iter().map(|n| system(n)).collect();
    let mut out = Vec::with_capacity(read.parts.len());
    for (part, srs) in read.parts {
        let (crs, swap) = systems[srs?].clone()?;
        out.push((geometry_of(part, swap), crs));
    }
    (!out.is_empty()).then_some(out)
}

/// The system `name` spells, and whether its axes are latitude first.
fn system(name: &str) -> Option<(Crs, bool)> {
    let crs = Crs::named(name)?;
    let name = name.trim().to_ascii_lowercase();
    let latitude_first = crs == Crs::Degrees
        && !name.ends_with("crs84")
        && (name.starts_with("urn:")
            || name.starts_with("http://www.opengis.net/def/crs/")
            || name.starts_with("https://www.opengis.net/def/crs/"));
    Some((crs, latitude_first))
}

/// `part` as a geometry, its two ordinates swapped when `swap`.
fn geometry_of(part: Part, swap: bool) -> Geometry {
    let position = |[a, b]: Xy| Position::from(if swap { [b, a] } else { [a, b] });
    let positions = |xys: Vec<Xy>| xys.into_iter().map(position).collect::<Vec<_>>();
    Geometry::new(match part {
        Part::Point(xy) => GeometryValue::Point {
            coordinates: position(xy),
        },
        Part::Line(line) => GeometryValue::LineString {
            coordinates: positions(line),
        },
        Part::Polygon(rings) => GeometryValue::Polygon {
            coordinates: rings.into_iter().map(positions).collect(),
        },
    })
}

/// The `srsName`s of a document, each spelling once, so that a system is
/// looked up once and not per geometry.
#[derive(Debug, Default)]
struct Names {
    names: Vec<String>,
    index: HashMap<String, usize>,
}

impl Names {
    fn intern(&mut self, name: String) -> usize {
        if let Some(&at) = self.index.get(&name) {
            return at;
        }
        self.names.push(name.clone());
        self.index.insert(name, self.names.len() - 1);
        self.names.len() - 1
    }
}

/// The document once read: its geometries, each with the `srsName` it,
/// an enclosing element or the envelope bounding one carries.
#[derive(Debug, Default)]
struct Read {
    parts: Vec<(Part, Option<usize>)>,
    srs: Names,
}

/// What a geometry element builds.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Kind {
    /// `Point`: its one position.
    Point,
    /// `LineString`, `LinearRing`, `LineStringSegment`: its positions.
    Line,
    /// `Curve`, `Ring`, `CompositeCurve`, `OrientableCurve`: its
    /// segments or curve members, joined.
    Joined,
    /// `Polygon`, `PolygonPatch`: its rings.
    Polygon,
    /// The collections, `Surface`, `CompositeSurface` and
    /// `OrientableSurface`: their members, flattened.
    Many,
}

impl Kind {
    fn of(name: &str) -> Option<Kind> {
        Some(match name {
            "Point" => Kind::Point,
            "LineString" | "LinearRing" | "LineStringSegment" => Kind::Line,
            "Curve" | "Ring" | "CompositeCurve" | "OrientableCurve" => Kind::Joined,
            "Polygon" | "PolygonPatch" => Kind::Polygon,
            "MultiPoint" | "MultiLineString" | "MultiPolygon" | "MultiCurve" | "MultiSurface"
            | "MultiGeometry" | "Surface" | "CompositeSurface" | "OrientableSurface" => Kind::Many,
            _ => return None,
        })
    }
}

/// Which ring of a polygon is being read.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Role {
    Outer,
    Inner,
}

/// An open geometry element.
#[derive(Debug)]
struct Node {
    kind: Kind,
    /// The element depth it closes at.
    depth: usize,
    /// The `srsName` it or its nearest ancestor carries, or one of its
    /// parts', when neither does.
    srs: Option<usize>,
    /// A point's, a line's, a joined curve's positions.
    positions: Vec<Xy>,
    /// A `posList` that does not divide into positions: the geometry is
    /// dropped.
    broken: bool,
    outer: Option<Vec<Xy>>,
    inners: Vec<Vec<Xy>>,
    role: Option<Role>,
    /// A collection's members.
    parts: Vec<Part>,
    /// A GML 2 `coord` open: the depth it closes at, its `X` and `Y`.
    coord: Option<(usize, Option<f64>, Option<f64>)>,
}

impl Node {
    fn new(kind: Kind, depth: usize, srs: Option<usize>) -> Node {
        Node {
            kind,
            depth,
            srs,
            positions: Vec::new(),
            broken: false,
            outer: None,
            inners: Vec::new(),
            role: None,
            parts: Vec::new(),
            coord: None,
        }
    }

    /// What the element built, once it closes: nothing when it holds too
    /// little to draw.
    fn finish(self) -> Vec<Part> {
        match self.kind {
            _ if self.broken => Vec::new(),
            Kind::Point => self
                .positions
                .first()
                .map(|&xy| Part::Point(xy))
                .into_iter()
                .collect(),
            Kind::Line | Kind::Joined if self.positions.len() >= 2 => {
                vec![Part::Line(self.positions)]
            }
            Kind::Line | Kind::Joined => Vec::new(),
            Kind::Polygon => match self.outer {
                Some(outer) => {
                    let mut rings = vec![outer];
                    rings.extend(self.inners);
                    vec![Part::Polygon(rings)]
                }
                None => Vec::new(),
            },
            Kind::Many => self.parts,
        }
    }

    /// `part`, built by a geometry inside this one, taken in: a point
    /// into a line (GML 3.0's `pointProperty`), a line into a joined
    /// curve end to start, a ring of at least four positions into a
    /// polygon by its role, anything into a collection.
    fn take(&mut self, part: Part) {
        match (self.kind, part) {
            (Kind::Line, Part::Point(xy)) => self.positions.push(xy),
            (Kind::Joined, Part::Line(line)) => {
                let skip = usize::from(self.positions.last() == line.first());
                self.positions.extend_from_slice(&line[skip..]);
            }
            (Kind::Polygon, Part::Line(ring)) if ring.len() >= 4 => match self.role {
                Some(Role::Outer) if self.outer.is_none() => self.outer = Some(ring),
                Some(Role::Inner) => self.inners.push(ring),
                _ => {}
            },
            (Kind::Many, part) => self.parts.push(part),
            _ => {}
        }
    }
}

/// A leaf element whose text is read.
#[derive(Debug)]
enum Leaf {
    /// `pos`, one position, whatever its dimension.
    Pos,
    /// `posList`, with the dimension of its positions.
    PosList(usize),
    /// GML 2's `coordinates`, with its tuple and ordinate separators and
    /// its decimal mark.
    Coordinates {
        ts: String,
        cs: String,
        decimal: String,
    },
    X,
    Y,
}

/// The reader's state: the geometries open, the element depth, what is
/// skipped, the `srsName`s and `srsDimension`s in scope.
#[derive(Debug, Default)]
struct Parser {
    read: Read,
    nodes: Vec<Node>,
    depth: usize,
    /// The depth of the element skipped with its content.
    skip: Option<usize>,
    /// The depth of the `boundedBy` open, if one is.
    bounded_by: Option<usize>,
    /// The `srsName`s in scope, by the depth of the element they hold
    /// for: their own, or the one a `boundedBy` envelope bounds.
    srs: Vec<(usize, usize)>,
    /// The `srsDimension`s in scope, by the depth of their element.
    dimensions: Vec<(usize, usize)>,
    /// The leaf being read and its text.
    text: Option<(Leaf, String)>,
}

impl Parser {
    fn parse(mut self, bytes: &[u8]) -> Option<Read> {
        let mut reader = NsReader::from_reader(bytes);
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
                Event::Start(ref start) | Event::Empty(ref start) => {
                    root_seen = true;
                    let (namespace, _) = reader.resolver().resolve_element(start.name());
                    let gml = matches!(namespace, ResolveResult::Bound(Namespace(ns)) if NAMESPACES.contains(&ns));
                    self.open(start, gml);
                    if matches!(event, Event::Empty(_)) {
                        self.close(false);
                    }
                }
                Event::Text(text) => {
                    if let Some((_, buffer)) = &mut self.text {
                        buffer.push_str(&text);
                    }
                }
                Event::CData(text) => {
                    if let Some((_, buffer)) = &mut self.text {
                        buffer.push_str(&text);
                    }
                }
                Event::GeneralRef(reference) => {
                    if let Some((_, buffer)) = &mut self.text {
                        crate::push_reference(buffer, &reference);
                    }
                }
                Event::End(_) => self.close(false),
                Event::Eof => break,
                _ => {}
            }
            buf.clear();
        }
        // Close what the document left open at its end, innermost first,
        // as its end tags would have: the leaf being read, less a number
        // the cut may have split, then each element.
        while self.depth > 0 {
            self.close(true);
        }
        Some(self.read)
    }

    fn open(&mut self, start: &BytesStart<'_>, gml: bool) {
        self.depth += 1;
        if self.skip.is_some() {
            return;
        }
        let depth = self.depth;
        let attribute = |name: &str| {
            start
                .attributes()
                .flatten()
                .find(|a| a.key.local_name().as_ref() == name)
                .map(|a| a.value.trim().to_string())
        };
        if let Some(name) = attribute("srsName").filter(|name| !name.is_empty()) {
            let at = self.read.srs.intern(name);
            self.srs.push((depth, at));
        }
        if let Some(dimension) = attribute("srsDimension").and_then(|d| d.parse().ok()) {
            self.dimensions.push((depth, dimension));
        }
        let local = start.local_name();
        let name = local.as_ref();
        let in_geometry = !self.nodes.is_empty();
        // GML's own, or WFS 2.0's, which wraps the same envelope.
        if name == "boundedBy" && !in_geometry {
            self.bounded_by = Some(depth);
            return;
        }
        if !gml {
            // An application schema's feature or property: traversal
            // outside a geometry, foreign inside one.
            if in_geometry {
                self.skip = Some(depth);
            }
            return;
        }
        if let Some(kind) = Kind::of(name) {
            if self.nodes.len() >= MAX_NESTING {
                self.skip = Some(depth);
                return;
            }
            let srs = self.srs.last().map(|&(_, at)| at);
            self.nodes.push(Node::new(kind, depth, srs));
            return;
        }
        match name {
            "Envelope" | "Box" => {
                // The envelope of a `boundedBy` names the system of what
                // it bounds, the collection or the feature, for the
                // geometries after it that name none: its `srsName` holds
                // for that element rather than for the envelope.
                if self.bounded_by == Some(depth - 1)
                    && let Some(last) = self.srs.last_mut()
                    && last.0 == depth
                {
                    last.0 = depth - 2;
                }
                self.skip = Some(depth);
            }
            // Volumes are not drawn, nor their faces.
            "Solid" | "MultiSolid" | "CompositeSolid" if !in_geometry => {
                self.skip = Some(depth);
            }
            _ if !in_geometry => {}
            "pos" => self.position(Leaf::Pos),
            "posList" => {
                let dimension = self.dimensions.last().map_or(2, |&(_, d)| d);
                self.position(Leaf::PosList(dimension));
            }
            "coordinates" => {
                let or = |value: Option<String>, default: &str| {
                    value
                        .filter(|v| !v.is_empty())
                        .unwrap_or_else(|| default.to_string())
                };
                // Separators are taken as written, spaces included.
                let raw = |name: &str| {
                    start
                        .attributes()
                        .flatten()
                        .find(|a| a.key.local_name().as_ref() == name)
                        .map(|a| a.value.to_string())
                };
                self.position(Leaf::Coordinates {
                    ts: or(raw("ts"), " "),
                    cs: or(raw("cs"), ","),
                    decimal: or(raw("decimal"), "."),
                });
            }
            "coord" => {
                if let Some(node) = self.nodes.last_mut() {
                    node.coord = Some((depth, None, None));
                }
            }
            "X" => self.text = Some((Leaf::X, String::new())),
            "Y" => self.text = Some((Leaf::Y, String::new())),
            "exterior" | "outerBoundaryIs" => self.role(Role::Outer),
            "interior" | "innerBoundaryIs" => self.role(Role::Inner),
            "Z" | "segments" | "patches" | "pointProperty" | "pointRep" | "baseCurve"
            | "baseSurface" => {}
            _ if name.ends_with("Member") || name.ends_with("Members") => {}
            // `Arc`, `Circle`, `Solid`, and whatever else a geometry
            // holds that is not drawn.
            _ => self.skip = Some(depth),
        }
    }

    /// A position element opens: its text is read, and a `srsName` on
    /// it, which the schema allows, is its geometry's when that has none
    /// of its own or an enclosing element's.
    fn position(&mut self, leaf: Leaf) {
        self.text = Some((leaf, String::new()));
        if let Some(node) = self.nodes.last_mut()
            && node.srs.is_none()
        {
            node.srs = self.srs.last().map(|&(_, at)| at);
        }
    }

    fn role(&mut self, role: Role) {
        if let Some(node) = self.nodes.last_mut()
            && node.kind == Kind::Polygon
        {
            node.role = Some(role);
        }
    }

    /// The element at the current depth closed, by its end tag or by the
    /// document's end when `cut`.
    fn close(&mut self, cut: bool) {
        let depth = self.depth;
        self.depth -= 1;
        if self.skip.is_some_and(|at| at < depth) {
            return;
        }
        // What the element brought into scope, skipped or not, and what
        // an envelope rescoped to it.
        while self.srs.last().is_some_and(|&(at, _)| at >= depth) {
            self.srs.pop();
        }
        while self.dimensions.last().is_some_and(|&(at, _)| at >= depth) {
            self.dimensions.pop();
        }
        if self.skip.take().is_some() {
            return;
        }
        if let Some((leaf, text)) = self.text.take() {
            self.leaf(leaf, &text, cut);
        }
        if self.bounded_by == Some(depth) {
            self.bounded_by = None;
        }
        if let Some(node) = self.nodes.last_mut()
            && let Some((_, x, y)) = node.coord.take_if(|(at, _, _)| *at == depth)
            && let (Some(x), Some(y)) = (x, y)
        {
            node.positions.push([x, y]);
        }
        if let Some(node) = self.nodes.pop_if(|node| node.depth == depth) {
            let srs = node.srs;
            let parts = node.finish();
            match self.nodes.last_mut() {
                Some(parent) => {
                    parent.srs = parent.srs.or(srs);
                    for part in parts {
                        parent.take(part);
                    }
                }
                None => self
                    .read
                    .parts
                    .extend(parts.into_iter().map(|part| (part, srs))),
            }
        }
    }

    /// The text of a leaf element, once it closes; `cut` when the
    /// document ended in it, and its last number may be a part of one.
    fn leaf(&mut self, leaf: Leaf, text: &str, cut: bool) {
        let text = if cut && !text.ends_with(char::is_whitespace) {
            &text[..text.rfind(char::is_whitespace).unwrap_or(0)]
        } else {
            text
        };
        let Some(node) = self.nodes.last_mut() else {
            return;
        };
        match leaf {
            Leaf::Pos => {
                let mut numbers = text.split_whitespace().map(number);
                if let (Some(Some(x)), Some(Some(y))) = (numbers.next(), numbers.next()) {
                    node.positions.push([x, y]);
                }
            }
            Leaf::PosList(dimension) => {
                let numbers: Option<Vec<f64>> = text.split_whitespace().map(number).collect();
                let Some(mut numbers) = numbers.filter(|_| dimension >= 2) else {
                    node.broken = true;
                    return;
                };
                if cut {
                    numbers.truncate(numbers.len() - numbers.len() % dimension);
                }
                if numbers.len() % dimension != 0 {
                    node.broken = true;
                    return;
                }
                node.positions
                    .extend(numbers.chunks_exact(dimension).map(|xy| [xy[0], xy[1]]));
            }
            Leaf::Coordinates { ts, cs, decimal } => {
                let split = |text: &'_ str, by: &str| -> Vec<String> {
                    if by.trim().is_empty() {
                        text.split_whitespace().map(str::to_string).collect()
                    } else {
                        text.split(by)
                            .map(str::trim)
                            .filter(|t| !t.is_empty())
                            .map(str::to_string)
                            .collect()
                    }
                };
                for tuple in split(text, &ts) {
                    let ordinates = split(&tuple, &cs);
                    let ordinate = |at: usize| {
                        let o = ordinates.get(at)?;
                        if decimal == "." {
                            number(o)
                        } else {
                            number(&o.replace(&decimal, "."))
                        }
                    };
                    if let (Some(x), Some(y)) = (ordinate(0), ordinate(1)) {
                        node.positions.push([x, y]);
                    }
                }
            }
            Leaf::X | Leaf::Y => {
                if let Some((_, x, y)) = &mut node.coord {
                    let value = number(text.trim());
                    if matches!(leaf, Leaf::X) {
                        *x = value;
                    } else {
                        *y = value;
                    }
                }
            }
        }
    }
}

/// A finite number, or nothing.
fn number(text: &str) -> Option<f64> {
    text.trim().parse::<f64>().ok().filter(|n| n.is_finite())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Lcc;

    fn fixture(name: &str) -> Vec<u8> {
        let path = format!("{}/../../fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
        std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
    }

    /// Each geometry read, as its kind and its first position, and the
    /// system it is in.
    fn kinds(doc: &[u8]) -> Option<Vec<(String, Crs)>> {
        let xy = |p: &Position| format!("{} {}", p[0], p[1]);
        geometries(doc).map(|geometries| {
            geometries
                .into_iter()
                .map(|(g, crs)| {
                    let kind = match &g.value {
                        GeometryValue::Point { coordinates } => {
                            format!("point {}", xy(coordinates))
                        }
                        GeometryValue::LineString { coordinates } => {
                            format!("line of {} from {}", coordinates.len(), xy(&coordinates[0]))
                        }
                        GeometryValue::Polygon { coordinates } => format!(
                            "polygon of {:?} from {}",
                            coordinates.iter().map(Vec::len).collect::<Vec<_>>(),
                            xy(&coordinates[0][0])
                        ),
                        other => panic!("{other:?}"),
                    };
                    (kind, crs)
                })
                .collect()
        })
    }

    /// The kinds only, for documents in one system.
    fn shapes(doc: &[u8]) -> Option<Vec<String>> {
        kinds(doc).map(|kinds| kinds.into_iter().map(|(kind, _)| kind).collect())
    }

    /// A GML 3.2 document of `body`, which names no system.
    fn gml32(body: &str) -> Vec<u8> {
        format!(
            r#"<wfs:FeatureCollection xmlns:wfs="http://www.opengis.net/wfs/2.0" xmlns:gml="http://www.opengis.net/gml/3.2" xmlns:x="https://example.org/x">{body}</wfs:FeatureCollection>"#
        )
        .into_bytes()
    }

    fn lambert() -> Crs {
        Crs::Lambert93(Lcc::lambert93())
    }

    #[test]
    fn reads_a_wfs_response_in_gml_3_2() {
        let read = kinds(&fixture("parcels.gml")).unwrap();
        // The parcel with its hole; the path's two segments joined, their
        // shared vertex once; the point; the pipe's third ordinates
        // dropped. The feature without geometry is nothing.
        assert_eq!(
            read,
            [
                (
                    "polygon of [5, 5] from 652400 6861950".to_string(),
                    lambert()
                ),
                ("line of 5 from 652320 6861920".to_string(), lambert()),
                ("point 652469 6862035".to_string(), lambert()),
                ("line of 3 from 652550 6862100".to_string(), lambert()),
            ]
        );
        let pipe = geometries(&fixture("parcels.gml")).unwrap().remove(3).0;
        assert_eq!(
            pipe.value,
            GeometryValue::LineString {
                coordinates: vec![
                    Position::from([652_550.0, 6_862_100.0]),
                    Position::from([652_650.0, 6_862_150.0]),
                    Position::from([652_680.0, 6_862_180.0]),
                ]
            }
        );
    }

    #[test]
    fn reads_gml_2() {
        // `outerBoundaryIs` and `innerBoundaryIs`; `coordinates` with its
        // own separators and decimal mark; `coord` with `X` and `Y`. The
        // box is not drawn.
        assert_eq!(
            kinds(&fixture("places-gml2.gml")).unwrap(),
            [
                (
                    "polygon of [5, 5] from 4.818 45.76".to_string(),
                    Crs::Degrees
                ),
                ("line of 3 from 4.805 45.752".to_string(), Crs::Degrees),
                ("point 4.8357 45.764".to_string(), Crs::Degrees),
            ]
        );
    }

    #[test]
    fn swaps_the_axes_of_latitude_first_systems() {
        let point = |srs: &str| {
            shapes(&gml32(&format!(
                r#"<gml:Point srsName="{srs}"><gml:pos>48.8566 2.3522</gml:pos></gml:Point>"#
            )))
            .unwrap()
            .remove(0)
        };
        for latitude_first in [
            "urn:ogc:def:crs:EPSG::4326",
            "urn:ogc:def:crs:EPSG:6.6:4326",
            "urn:x-ogc:def:crs:EPSG:4326",
            "URN:OGC:DEF:CRS:EPSG::4326",
            "http://www.opengis.net/def/crs/EPSG/0/4326",
            "https://www.opengis.net/def/crs/EPSG/0/4326",
        ] {
            assert_eq!(
                point(latitude_first),
                "point 2.3522 48.8566",
                "{latitude_first}"
            );
        }
        for longitude_first in [
            "EPSG:4326",
            "epsg:4326",
            "http://www.opengis.net/gml/srs/epsg.xml#4326",
            "urn:ogc:def:crs:OGC:1.3:CRS84",
            "http://www.opengis.net/def/crs/OGC/1.3/CRS84",
        ] {
            assert_eq!(
                point(longitude_first),
                "point 48.8566 2.3522",
                "{longitude_first}"
            );
        }
        // Projected systems are easting first in every spelling.
        for name in [
            "urn:ogc:def:crs:EPSG::2154",
            "http://www.opengis.net/def/crs/EPSG/0/2154",
            "http://www.opengis.net/gml/srs/epsg.xml#2154",
            "EPSG:2154",
        ] {
            assert_eq!(system(name), Some((lambert(), false)), "{name}");
        }
        assert_eq!(
            system("urn:ogc:def:crs:EPSG::3857"),
            Some((Crs::WebMercator, false))
        );
        assert_eq!(system("urn:ogc:def:crs:EPSG::27572"), None);
    }

    #[test]
    fn inherits_the_srs_name() {
        // From the collection's envelope to its members, from a feature
        // that carries it, from a member up to a collection that does
        // not, from a feature's own envelope to that feature and no
        // further, and the nearest wins.
        let doc = gml32(
            r#"<wfs:boundedBy><gml:Envelope srsName="EPSG:2154"><gml:lowerCorner>1 2</gml:lowerCorner></gml:Envelope></wfs:boundedBy>
<wfs:member><x:F><x:g><gml:MultiPoint srsName="EPSG:4326"><gml:pointMember><gml:Point><gml:pos>4 45</gml:pos></gml:Point></gml:pointMember><gml:pointMember><gml:Point srsName="EPSG:3857"><gml:pos>5 46</gml:pos></gml:Point></gml:pointMember></gml:MultiPoint></x:g></x:F></wfs:member>
<wfs:member><x:F srsName="EPSG:3857"><x:g><gml:Point><gml:pos>6 47</gml:pos></gml:Point></x:g></x:F></wfs:member>
<wfs:member><x:F><x:g><gml:MultiPoint><gml:pointMember><gml:Point srsName="EPSG:4326"><gml:pos>7 48</gml:pos></gml:Point></gml:pointMember></gml:MultiPoint></x:g></x:F></wfs:member>
<wfs:member><x:F><gml:boundedBy><gml:Envelope srsName="EPSG:4326"/></gml:boundedBy><x:g><gml:Point><gml:pos>8 49</gml:pos></gml:Point></x:g></x:F></wfs:member>
<wfs:member><x:F><x:g><gml:Point><gml:pos>700000 6600000</gml:pos></gml:Point></x:g></x:F></wfs:member>"#,
        );
        assert_eq!(
            kinds(&doc).unwrap(),
            [
                ("point 4 45".to_string(), Crs::Degrees),
                // A member's own system is the collection's: what is
                // drawn is the collection.
                ("point 5 46".to_string(), Crs::Degrees),
                ("point 6 47".to_string(), Crs::WebMercator),
                // Under the collection's envelope, a collection without
                // a system of its own has that one, not a member's.
                ("point 7 48".to_string(), lambert()),
                ("point 8 49".to_string(), Crs::Degrees),
                ("point 700000 6600000".to_string(), lambert()),
            ]
        );
        // On the positions, where the schema allows one, when the
        // geometry has none; the geometry's own wins.
        let on_positions = gml32(
            r#"<gml:Point><gml:pos srsName="EPSG:4326">4 45</gml:pos></gml:Point>
<gml:LineString><gml:posList srsName="EPSG:4326">4 45 5 46</gml:posList></gml:LineString>
<gml:Point srsName="EPSG:3857"><gml:pos srsName="EPSG:4326">6 47</gml:pos></gml:Point>
<gml:Point xmlns:g="http://www.opengis.net/gml"><g:coordinates srsName="EPSG:4326">7,48</g:coordinates></gml:Point>"#,
        );
        assert_eq!(
            kinds(&on_positions).unwrap(),
            [
                ("point 4 45".to_string(), Crs::Degrees),
                ("line of 2 from 4 45".to_string(), Crs::Degrees),
                ("point 6 47".to_string(), Crs::WebMercator),
                ("point 7 48".to_string(), Crs::Degrees),
            ]
        );
        // With nothing enclosing it, a collection takes a member's.
        let bare = gml32(
            r#"<wfs:member><x:F><x:g><gml:MultiPoint><gml:pointMember><gml:Point srsName="EPSG:4326"><gml:pos>7 48</gml:pos></gml:Point></gml:pointMember><gml:pointMember><gml:Point><gml:pos>8 49</gml:pos></gml:Point></gml:pointMember></gml:MultiPoint></x:g></x:F></wfs:member>"#,
        );
        assert_eq!(
            kinds(&bare).unwrap(),
            [
                ("point 7 48".to_string(), Crs::Degrees),
                ("point 8 49".to_string(), Crs::Degrees),
            ]
        );
        // An envelope names the system of what comes after it in the
        // element it bounds, not before: the schema puts `boundedBy`
        // before the properties, and nothing is read twice.
        let late = gml32(
            r#"<wfs:member><x:F><x:g><gml:Point><gml:pos>4 45</gml:pos></gml:Point></x:g></x:F></wfs:member><wfs:boundedBy><gml:Envelope srsName="EPSG:4326"/></wfs:boundedBy>"#,
        );
        assert_eq!(kinds(&late), None);
        // A `boundedBy` with a system of its own, which nothing writes,
        // holds for the `boundedBy` alone: were it to leak past its
        // feature, the second point would be in it and the document drawn.
        let odd = gml32(
            r#"<wfs:member><x:F><gml:boundedBy srsName="EPSG:3857"><gml:Envelope srsName="EPSG:4326"/></gml:boundedBy><x:g><gml:Point><gml:pos>8 49</gml:pos></gml:Point></x:g></x:F></wfs:member><wfs:member><x:F><x:g><gml:Point><gml:pos>9 50</gml:pos></gml:Point></x:g></x:F></wfs:member>"#,
        );
        assert_eq!(kinds(&odd), None);
    }

    #[test]
    fn declines_without_a_system() {
        let point = r#"<gml:Point><gml:pos>4 45</gml:pos></gml:Point>"#;
        // None anywhere: GML has no default.
        assert_eq!(shapes(&gml32(point)), None);
        // An envelope that is not the `boundedBy`'s, or with an empty name.
        assert_eq!(
            shapes(&gml32(&format!(
                r#"<x:extent><gml:Envelope srsName="EPSG:4326"/></x:extent>{point}"#
            ))),
            None
        );
        assert_eq!(
            shapes(&gml32(&format!(
                r#"<gml:MultiPoint srsName=" "><gml:pointMember>{point}</gml:pointMember></gml:MultiPoint>"#
            ))),
            None
        );
        // One geometry without a system declines the document, as one in
        // a system that is not handled does.
        assert_eq!(
            shapes(&gml32(&format!(
                r#"<gml:Point srsName="EPSG:4326"><gml:pos>4 45</gml:pos></gml:Point>{point}"#
            ))),
            None
        );
        assert_eq!(
            shapes(&gml32(
                r#"<gml:Point srsName="EPSG:27572"><gml:pos>4 45</gml:pos></gml:Point>"#
            )),
            None
        );
    }

    #[test]
    fn reads_both_polygon_spellings() {
        let ring = |at: f64| format!("{at} 45 {} 45 {} 46 {at} 46 {at} 45", at + 1.0, at + 1.0);
        let gml3 = gml32(&format!(
            r#"<gml:Polygon srsName="EPSG:4326"><gml:exterior><gml:LinearRing><gml:posList>{}</gml:posList></gml:LinearRing></gml:exterior><gml:interior><gml:LinearRing><gml:posList>{}</gml:posList></gml:LinearRing></gml:interior><gml:interior><gml:LinearRing><gml:posList>{}</gml:posList></gml:LinearRing></gml:interior></gml:Polygon>"#,
            ring(0.0),
            ring(10.0),
            ring(20.0)
        ));
        assert_eq!(shapes(&gml3).unwrap(), ["polygon of [5, 5, 5] from 0 45"]);
        let gml2 = br#"<gml:Polygon xmlns:gml="http://www.opengis.net/gml" srsName="EPSG:4326"><gml:outerBoundaryIs><gml:LinearRing><gml:coordinates>0,45 1,45 1,46 0,46 0,45</gml:coordinates></gml:LinearRing></gml:outerBoundaryIs><gml:innerBoundaryIs><gml:LinearRing><gml:coord><gml:X>0.2</gml:X><gml:Y>45.2</gml:Y></gml:coord><gml:coord><gml:X>0.4</gml:X><gml:Y>45.2</gml:Y></gml:coord><gml:coord><gml:X>0.4</gml:X><gml:Y>45.4</gml:Y></gml:coord><gml:coord><gml:X>0.2</gml:X><gml:Y>45.2</gml:Y></gml:coord></gml:LinearRing></gml:innerBoundaryIs></gml:Polygon>"#;
        assert_eq!(shapes(gml2).unwrap(), ["polygon of [5, 4] from 0 45"]);
        // A ring of fewer than four positions is dropped; a polygon whose
        // outer ring is dropped is nothing.
        let short = gml32(
            r#"<gml:Polygon srsName="EPSG:4326"><gml:exterior><gml:LinearRing><gml:posList>0 45 1 45 1 46 0 45</gml:posList></gml:LinearRing></gml:exterior><gml:interior><gml:LinearRing><gml:posList>0 45 1 45 0 45</gml:posList></gml:LinearRing></gml:interior></gml:Polygon>
<gml:Polygon srsName="EPSG:4326"><gml:exterior><gml:LinearRing><gml:posList>0 45 1 45 0 45</gml:posList></gml:LinearRing></gml:exterior></gml:Polygon>"#,
        );
        assert_eq!(shapes(&short).unwrap(), ["polygon of [4] from 0 45"]);
    }

    #[test]
    fn reads_curves_surfaces_and_rings_of_curves() {
        let doc = gml32(
            r##"<gml:MultiSurface srsName="EPSG:4326"><gml:surfaceMembers>
  <gml:Surface><gml:patches>
    <gml:PolygonPatch><gml:exterior><gml:Ring>
      <gml:curveMember><gml:Curve><gml:segments><gml:LineStringSegment><gml:posList>0 45 1 45 1 46</gml:posList></gml:LineStringSegment></gml:segments></gml:Curve></gml:curveMember>
      <gml:curveMember><gml:LineString><gml:posList>1 46 0 46 0 45</gml:posList></gml:LineString></gml:curveMember>
    </gml:Ring></gml:exterior></gml:PolygonPatch>
    <gml:PolygonPatch><gml:exterior><gml:LinearRing><gml:posList>5 45 6 45 6 46 5 45</gml:posList></gml:LinearRing></gml:exterior></gml:PolygonPatch>
  </gml:patches></gml:Surface>
</gml:surfaceMembers></gml:MultiSurface>
<gml:MultiCurve srsName="EPSG:4326"><gml:curveMember><gml:Curve><gml:segments>
  <gml:LineStringSegment><gml:posList>0 40 1 40</gml:posList></gml:LineStringSegment>
  <gml:Arc><gml:posList>1 40 2 41 3 40</gml:posList></gml:Arc>
  <gml:LineStringSegment><gml:posList>3 40 4 40</gml:posList></gml:LineStringSegment>
</gml:segments></gml:Curve></gml:curveMember><gml:curveMember xlink:href="#elsewhere" xmlns:xlink="http://www.w3.org/1999/xlink"/></gml:MultiCurve>
<gml:LineString srsName="EPSG:4326"><gml:pointProperty><gml:Point><gml:pos>0 30</gml:pos></gml:Point></gml:pointProperty><gml:pos>1 30</gml:pos></gml:LineString>
<gml:MultiSurface srsName="EPSG:4326"><gml:surfaceMember><gml:CompositeSurface>
  <gml:surfaceMember><gml:Polygon><gml:exterior><gml:LinearRing><gml:posList>0 20 1 20 1 21 0 20</gml:posList></gml:LinearRing></gml:exterior></gml:Polygon></gml:surfaceMember>
  <gml:surfaceMember><gml:OrientableSurface orientation="-"><gml:baseSurface><gml:Polygon><gml:exterior><gml:LinearRing><gml:posList>2 20 3 20 3 21 2 20</gml:posList></gml:LinearRing></gml:exterior></gml:Polygon></gml:baseSurface></gml:OrientableSurface></gml:surfaceMember>
</gml:CompositeSurface></gml:surfaceMember></gml:MultiSurface>
<gml:Polygon srsName="EPSG:4326"><gml:exterior><gml:Ring><gml:curveMember><gml:CompositeCurve>
  <gml:curveMember><gml:LineString><gml:posList>0 10 1 10</gml:posList></gml:LineString></gml:curveMember>
  <gml:curveMember><gml:OrientableCurve orientation="-"><gml:baseCurve><gml:LineString><gml:posList>1 10 1 11 0 10</gml:posList></gml:LineString></gml:baseCurve></gml:OrientableCurve></gml:curveMember>
</gml:CompositeCurve></gml:curveMember></gml:Ring></gml:exterior></gml:Polygon>"##,
        );
        assert_eq!(
            shapes(&doc).unwrap(),
            [
                // The two curve members joined into one ring, the shared
                // vertex once.
                "polygon of [5] from 0 45",
                "polygon of [4] from 5 45",
                // The arc skipped: its neighbours joined across the gap.
                "line of 4 from 0 40",
                "line of 2 from 0 30",
                // The composite surface's two polygons, one through its
                // orientable wrapper; the composite curve's two members
                // joined into the ring.
                "polygon of [4] from 0 20",
                "polygon of [4] from 2 20",
                "polygon of [4] from 0 10",
            ]
        );
    }

    #[test]
    fn reads_positions_in_their_dimension() {
        let line = |attributes: &str, list: &str| {
            shapes(&gml32(&format!(
                r#"<gml:LineString srsName="EPSG:4326"{attributes}><gml:posList>{list}</gml:posList></gml:LineString><gml:Point srsName="EPSG:4326"><gml:pos>9 9</gml:pos></gml:Point>"#
            )))
            .unwrap()
        };
        assert_eq!(line("", "0 45 1 46"), ["line of 2 from 0 45", "point 9 9"]);
        assert_eq!(
            line(r#" srsDimension="3""#, "0 45 10 1 46 10"),
            ["line of 2 from 0 45", "point 9 9"]
        );
        // On the list itself, over the geometry's.
        assert_eq!(
            shapes(&gml32(
                r#"<gml:LineString srsName="EPSG:4326" srsDimension="2"><gml:posList srsDimension="4">0 45 1 2 1 46 1 2</gml:posList></gml:LineString>"#
            ))
            .unwrap(),
            ["line of 2 from 0 45"]
        );
        // A count that does not divide, a dimension under two, a number
        // that is not one: the line is dropped, the point stays.
        assert_eq!(line(r#" srsDimension="3""#, "0 45 1 46"), ["point 9 9"]);
        assert_eq!(line("", "0 45 1"), ["point 9 9"]);
        assert_eq!(line(r#" srsDimension="1""#, "0 45 1 46"), ["point 9 9"]);
        assert_eq!(line("", "0 45 x 46"), ["point 9 9"]);
        assert_eq!(line("", "0 45 1 NaN"), ["point 9 9"]);
        // A third ordinate on a `pos` is dropped; a single one is no point.
        assert_eq!(
            shapes(&gml32(
                r#"<gml:Point srsName="EPSG:4326"><gml:pos>4 45 100</gml:pos></gml:Point><gml:Point srsName="EPSG:4326"><gml:pos>4</gml:pos></gml:Point>"#
            ))
            .unwrap(),
            ["point 4 45"]
        );
    }

    #[test]
    fn resolves_character_references_in_positions() {
        let point = |pos: &str| {
            shapes(&gml32(&format!(
                r#"<gml:Point srsName="EPSG:4326"><gml:pos>{pos}</gml:pos></gml:Point>"#
            )))
        };
        assert_eq!(point("&#50;.35 48.85").unwrap(), ["point 2.35 48.85"]);
        assert_eq!(point("2.35&#x20;48.85").unwrap(), ["point 2.35 48.85"]);
        // An entity the document defines itself is not followed, and the
        // number it sat in is not one; nor is one around a predefined one.
        assert_eq!(point("&two;.35 48.85"), None);
        assert_eq!(point("2.35&amp;48.85"), None);
    }

    #[test]
    fn skips_what_is_not_drawn() {
        // The envelope, a solid, a foreign element inside a geometry and
        // the positions under it, a geometry outside a GML namespace,
        // text around the positions.
        let doc = gml32(
            r#"<gml:Envelope srsName="EPSG:4326"><gml:lowerCorner>0 0</gml:lowerCorner><gml:pos>1 1</gml:pos></gml:Envelope>
<gml:Solid srsName="EPSG:4326"><gml:exterior><gml:Shell><gml:surfaceMember><gml:Polygon><gml:exterior><gml:LinearRing><gml:posList>0 45 1 45 1 46 0 45</gml:posList></gml:LinearRing></gml:exterior></gml:Polygon></gml:surfaceMember></gml:Shell></gml:exterior></gml:Solid>
<gml:LineString srsName="EPSG:4326"><x:note><gml:posList>5 5 6 6</gml:posList></x:note><gml:posList> 0 45
  1 46 </gml:posList></gml:LineString>
<x:Point srsName="EPSG:4326"><x:pos>7 7</x:pos></x:Point>
<gml:Point srsName="EPSG:4326"><gml:pos><![CDATA[4 45]]></gml:pos><!-- a comment --></gml:Point>"#,
        );
        assert_eq!(shapes(&doc).unwrap(), ["line of 2 from 0 45", "point 4 45"]);
    }

    #[test]
    fn a_document_cut_short_is_drawn_as_far_as_it_goes() {
        let whole = gml32(
            r#"<gml:Point srsName="EPSG:4326"><gml:pos>4 45</gml:pos></gml:Point><gml:LineString srsName="EPSG:4326"><gml:posList>0 45 1 46 2 47</gml:posList></gml:LineString>"#,
        );
        let cut = |at: &str| {
            whole[..whole
                .windows(at.len())
                .position(|w| w == at.as_bytes())
                .unwrap()]
                .to_vec()
        };
        for (at, expected) in [
            ("</gml:LineString>", "line of 3 from 0 45"),
            // The last number may be a part of one, whole or not: it
            // goes, and its position with it.
            ("</gml:posList>", "line of 2 from 0 45"),
            (" 47<", "line of 2 from 0 45"),
            ("7</gml", "line of 2 from 0 45"),
        ] {
            assert_eq!(
                shapes(&cut(at)).unwrap(),
                ["point 4 45", expected],
                "cut at {at}"
            );
        }
        // A line cut before its second position is not a line yet.
        assert_eq!(shapes(&cut(" 1 46")).unwrap(), ["point 4 45"]);
        // A mismatched end tag is not a cut.
        let mismatched =
            gml32(r#"<gml:Point srsName="EPSG:4326"><gml:pos>4 45</gml:pos></gml:LineString>"#);
        assert_eq!(shapes(&mismatched), None);
    }

    #[test]
    fn nesting_is_bounded() {
        let depth = 10_000;
        let mut doc = String::from(r#"<gml:MultiGeometry srsName="EPSG:4326">"#);
        doc.push_str(&"<gml:geometryMember><gml:MultiGeometry>".repeat(depth));
        doc.push_str(r#"<gml:geometryMember><gml:Point><gml:pos>4 45</gml:pos></gml:Point></gml:geometryMember>"#);
        doc.push_str(&"</gml:MultiGeometry></gml:geometryMember>".repeat(depth));
        doc.push_str(r#"</gml:MultiGeometry><gml:Point srsName="EPSG:4326"><gml:pos>5 46</gml:pos></gml:Point>"#);
        // The point too deep is skipped; the one after is read.
        assert_eq!(shapes(&gml32(&doc)).unwrap(), ["point 5 46"]);
    }

    #[test]
    fn declines_what_holds_no_geometry() {
        assert_eq!(shapes(&fixture("empty.gml")), None);
        for doc in [
            b"" as &[u8],
            b"not xml",
            br#"{"type": "Feature"}"#,
            b"<kml><Placemark><Point><coordinates>4,45</coordinates></Point></Placemark></kml>",
            // GML elements without the namespace are not GML.
            br#"<gml:Point srsName="EPSG:4326"><gml:pos>4 45</gml:pos></gml:Point>"#,
            br#"<Point xmlns="http://www.opengis.net/gml/3.3" srsName="EPSG:4326"><pos>4 45</pos></Point>"#,
            br#"<g:Point xmlns:g="http://www.opengis.net/gml" srsName="EPSG:4326"/>"#,
        ] {
            assert_eq!(shapes(doc), None, "{}", String::from_utf8_lossy(doc));
        }
        // The default namespace is GML too.
        assert_eq!(
            shapes(br#"<Point xmlns="http://www.opengis.net/gml" srsName="EPSG:4326"><pos>4 45</pos></Point>"#).unwrap(),
            ["point 4 45"]
        );
    }
}
