//! The GPX reader: waypoints, routes and track segments as geometries,
//! always in WGS 84 degrees by the specification. A `<wpt>` is a point,
//! a `<rte>` is a line of its `<rtept>`, each `<trkseg>` of a `<trk>` is
//! a line of its `<trkpt>`. The schema has no styling: Garmin's and
//! OsmAnd's colour extensions, like `<sym>`, `<name>`, `<ele>` and
//! `<time>`, are ignored, as is every element the reader does not know,
//! so extensions and future elements cost nothing.

use geojson::{Geometry, GeometryValue, Position};
use quick_xml::Reader;
use quick_xml::events::{BytesStart, Event};

/// The geometries of the GPX document `bytes`, in document order. `None`
/// when the document is not GPX, is not well-formed, or holds no point
/// with coordinates: nothing to draw. A document that ends before its
/// elements close is drawn as far as it goes.
pub(crate) fn geometries(bytes: &[u8]) -> Option<Vec<Geometry>> {
    let mut reader = Reader::from_reader(bytes);
    let mut buf = Vec::new();
    let mut geometries = Vec::new();
    let mut root_seen = false;
    // The route or segment being read, when inside one.
    let mut line: Option<Vec<Position>> = None;
    loop {
        match reader.read_event_into(&mut buf).ok()? {
            Event::Start(start) | Event::Empty(start) => {
                if !root_seen {
                    if start.local_name().as_ref() != "gpx" {
                        return None;
                    }
                    root_seen = true;
                }
                match start.local_name().as_ref() {
                    "wpt" => {
                        if let Some(position) = position_of(&start) {
                            geometries.push(Geometry::new(GeometryValue::Point {
                                coordinates: position,
                            }));
                        }
                    }
                    "rte" | "trkseg" => line = Some(Vec::new()),
                    "rtept" | "trkpt" => {
                        if let (Some(line), Some(position)) = (&mut line, position_of(&start)) {
                            line.push(position);
                        }
                    }
                    _ => {}
                }
            }
            Event::End(end) => {
                if matches!(end.local_name().as_ref(), "rte" | "trkseg") {
                    // A segment with fewer than two points is not a line.
                    if let Some(positions) = line.take().filter(|line| line.len() >= 2) {
                        geometries.push(Geometry::new(GeometryValue::LineString {
                            coordinates: positions,
                        }));
                    }
                }
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    (!geometries.is_empty()).then_some(geometries)
}

/// The `lon` and `lat` attributes of a point element, as a position.
/// `None` when either is missing or not a number: a point with no place.
fn position_of(element: &BytesStart<'_>) -> Option<Position> {
    let (mut lon, mut lat) = (None, None);
    for attribute in element.attributes().flatten() {
        let value = attribute.value.trim().parse::<f64>().ok();
        match attribute.key.local_name().as_ref() {
            "lon" => lon = value,
            "lat" => lat = value,
            _ => {}
        }
    }
    Some(Position::from([lon?, lat?]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(bytes: &[u8]) -> Option<Vec<String>> {
        geometries(bytes).map(|geometries| {
            geometries
                .iter()
                .map(|g| match &g.value {
                    GeometryValue::Point { coordinates } => {
                        format!("point {:?}", coordinates.as_slice())
                    }
                    GeometryValue::LineString { coordinates } => {
                        format!("line of {}", coordinates.len())
                    }
                    other => panic!("{other:?}"),
                })
                .collect()
        })
    }

    #[test]
    fn reads_waypoints_routes_and_segments_in_order() {
        let doc = br#"<?xml version="1.0" encoding="UTF-8"?>
<gpx version="1.1" creator="hand" xmlns="http://www.topografix.com/GPX/1/1">
  <metadata><name>Test</name></metadata>
  <wpt lat="45.764" lon="4.8357"><ele>170</ele><name>Lyon</name><sym>Flag</sym></wpt>
  <rte>
    <name>R</name>
    <rtept lat="45.75" lon="4.80"><name>a</name></rtept>
    <rtept lat="45.76" lon="4.81"/>
    <rtept lat="45.77" lon="4.82"/>
  </rte>
  <trk>
    <name>T</name>
    <trkseg>
      <trkpt lat="45.70" lon="4.90"><ele>200</ele><time>2026-01-01T00:00:00Z</time></trkpt>
      <trkpt lat="45.71" lon="4.91"/>
    </trkseg>
    <trkseg>
      <trkpt lat="45.72" lon="4.92"/>
      <trkpt lat="45.73" lon="4.93"/>
      <trkpt lat="45.74" lon="4.94"/>
    </trkseg>
  </trk>
  <wpt lon="4.0" lat="46.0"/>
</gpx>"#;
        assert_eq!(
            kinds(doc).unwrap(),
            [
                "point [4.8357, 45.764]",
                "line of 3",
                "line of 2",
                "line of 3",
                "point [4.0, 46.0]",
            ]
        );
    }

    #[test]
    fn ignores_what_it_does_not_know() {
        // Extensions, a namespace prefix, a colour, and a point without
        // coordinates or with ones that are not numbers.
        let doc = br##"<g:gpx xmlns:g="http://www.topografix.com/GPX/1/1" xmlns:gpxx="http://www.garmin.com/xmlschemas/GpxExtensions/v3">
  <g:trk>
    <g:extensions><gpxx:TrackExtension><gpxx:DisplayColor>Red</gpxx:DisplayColor></gpxx:TrackExtension></g:extensions>
    <g:trkseg>
      <g:trkpt lat="45.70" lon="4.90"><g:extensions><gpxx:TrackPointExtension><gpxx:Depth>1</gpxx:Depth></gpxx:TrackPointExtension></g:extensions></g:trkpt>
      <g:trkpt lat="x" lon="4.91"/>
      <g:trkpt lat="45.71"/>
      <g:trkpt lat=" 45.72 " lon=" 4.92 "/>
    </g:trkseg>
  </g:trk>
  <g:wpt lat="45" lon="4" color="#ff0000"><g:name>x</g:name></g:wpt>
  <g:wpt lat="45"/>
  <unknown><g:wpt lat="46" lon="5"/></unknown>
</g:gpx>"##;
        assert_eq!(
            kinds(doc).unwrap(),
            ["line of 2", "point [4.0, 45.0]", "point [5.0, 46.0]"]
        );
    }

    #[test]
    fn drops_a_segment_of_one_point_and_a_point_outside_a_line() {
        let doc = br#"<gpx>
  <rtept lat="45" lon="4"/>
  <trk><trkseg><trkpt lat="45.70" lon="4.90"/></trkseg></trk>
  <rte><rtept lat="45.70" lon="4.90"/><rtept lat="45.70" lon="4.90"/></rte>
  <trkpt lat="45" lon="4"/>
</gpx>"#;
        assert_eq!(kinds(doc).unwrap(), ["line of 2"]);
        // A document cut short keeps what it holds.
        assert_eq!(
            kinds(b"<gpx><wpt lat=\"45\" lon=\"4\"/><trk><trkseg><trkpt lat=\"45\" lon=\"4\"/>")
                .unwrap(),
            ["point [4.0, 45.0]"]
        );
    }

    #[test]
    fn declines_what_is_not_gpx_or_holds_no_point() {
        for doc in [
            b"" as &[u8],
            b"<gpx/>",
            b"<gpx><metadata><name>empty</name></metadata><trk><trkseg/></trk></gpx>",
            b"<kml><Placemark><Point><coordinates>4,45</coordinates></Point></Placemark></kml>",
            b"<svg><wpt lat=\"45\" lon=\"4\"/></svg>",
            b"<?xml version=\"1.0\"?><gpx><wpt lat=\"45\" lon=\"4\"></gpx>",
            b"not xml",
            br#"{"type": "Feature"}"#,
        ] {
            assert_eq!(kinds(doc), None, "{}", String::from_utf8_lossy(doc));
        }
    }
}
