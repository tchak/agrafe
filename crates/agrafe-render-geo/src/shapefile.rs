//! The zipped shapefile reader: every `.shp` layer of the archive, folder
//! or not, each in the coordinate system its sibling `.prj` names (see
//! [`prj`]). The `.shx` and `.dbf` are not read: the `.shp` is
//! self-describing, and the attributes are not drawn. The format has no
//! styling; the configured default applies.
//!
//! A `.shp` is a 100-byte header, its file code big-endian and its
//! version and shape type little-endian, then records, each a big-endian
//! number and content length and a content of little-endian values: a
//! shape type, then for a point its coordinates, for the others a
//! bounding box, the part and point counts, the parts' first indices and
//! the points. The `Z` and `M` variants are the same layout with extra
//! ranges and values after the points, which are not read. A `MultiPatch`
//! and a null shape are skipped. A polygon record is a flat list of
//! rings, clockwise an outer and anticlockwise a hole, without nesting:
//! all of a record's rings go into one polygon, whose even-odd fill keeps
//! the holes, with no pairing of rings.
//!
//! A file cut short is drawn as far as its last whole record, as a
//! truncated XML document is; a record that contradicts itself, a part
//! out of its points or a count past its content, declines the archive.

mod prj;

use std::collections::HashMap;

use geojson::{Geometry, GeometryValue, Position};

use crate::Crs;
use crate::archive;

/// A `.prj` read at most: a WKT string is a few hundred bytes.
const MAX_PRJ: u64 = 64 * 1024;
/// The size of a `.shp` header.
const HEADER: usize = 100;
/// The big-endian number a `.shp` opens on.
const FILE_CODE: u32 = 9994;
/// The version in a `.shp` header, the only one there is.
const VERSION: u32 = 1000;

/// The geometries of each `.shp` layer of the zipped shapefile `bytes`,
/// in archive and record order, with the system its `.prj` names. `None`
/// when the bytes are not a ZIP, hold no `.shp`, a layer has no `.prj`
/// or one in a system that is not handled (nothing is guessed from the
/// ranges of the numbers), a layer is not a shapefile or contradicts
/// itself, or the layers inflate past what an archive is read to.
pub(crate) fn layers(bytes: &[u8]) -> Option<Vec<(Vec<Geometry>, Crs)>> {
    layers_within(bytes, archive::MAX_INFLATED)
}

fn layers_within(bytes: &[u8], max_inflated: u64) -> Option<Vec<(Vec<Geometry>, Crs)>> {
    let mut zip = archive::open(bytes)?;
    let mut shps = Vec::new();
    let mut prjs = HashMap::new();
    for (index, name) in archive::names(&zip) {
        if let Some(stem) = name.strip_suffix(".shp") {
            shps.push((index, stem.to_owned()));
        } else if let Some(stem) = name.strip_suffix(".prj") {
            prjs.entry(stem.to_owned()).or_insert(index);
        }
    }
    let mut budget = max_inflated;
    let mut layers = Vec::new();
    for (index, stem) in shps {
        let prj = archive::read(&mut zip, *prjs.get(&stem)?, MAX_PRJ)?;
        let crs = prj::crs(&prj)?;
        let shp = archive::read(&mut zip, index, budget)?;
        budget -= shp.len() as u64;
        layers.push((geometries(&shp)?, crs));
    }
    (!layers.is_empty()).then_some(layers)
}

/// The geometries of the `.shp` file `shp`, one per record that holds
/// one, in record order. `None` when it is not a `.shp` or a record
/// contradicts itself.
fn geometries(shp: &[u8]) -> Option<Vec<Geometry>> {
    if be_u32(shp, 0)? != FILE_CODE || le_u32(shp, 28)? != VERSION {
        return None;
    }
    let mut rest = shp.get(HEADER..)?;
    let mut out = Vec::new();
    while rest.len() >= 8 {
        // The content length counts 16-bit words.
        let len = usize::try_from(be_u32(rest, 4)?).ok()?.checked_mul(2)?;
        let Some(content) = rest.get(8..8 + len) else {
            break;
        };
        out.extend(record(content)?);
        rest = &rest[8 + len..];
    }
    Some(out)
}

/// The geometry of a record's `content`: `Some(None)` for a null shape,
/// a `MultiPatch`, or one whose lines and rings are too short to draw;
/// `None` when the record contradicts itself or has a type the format
/// does not define.
fn record(content: &[u8]) -> Option<Option<Geometry>> {
    let geometry = |value| Some(Some(Geometry::new(value)));
    match le_u32(content, 0)? {
        // Null, MultiPatch.
        0 | 31 => Some(None),
        // Point, PointZ, PointM.
        1 | 11 | 21 => geometry(GeometryValue::Point {
            coordinates: positions(content, 4, 1)?.pop()?,
        }),
        // MultiPoint, MultiPointZ, MultiPointM, past the bounding box.
        8 | 18 | 28 => {
            let n = count(content, 36)?;
            let points = positions(content, 40, n)?;
            if points.is_empty() {
                return Some(None);
            }
            geometry(GeometryValue::MultiPoint {
                coordinates: points,
            })
        }
        // PolyLine, Polygon and their Z and M variants, past the bounding
        // box.
        kind @ (3 | 13 | 23 | 5 | 15 | 25) => {
            let parts = count(content, 36)?;
            let n = count(content, 40)?;
            let at = parts.checked_mul(4)?.checked_add(44)?;
            let mut points = positions(content, at, n)?;
            let starts = (0..parts)
                .map(|part| count(content, 44 + 4 * part))
                .collect::<Option<Vec<_>>>()?;
            // Each part runs from its first index to the next one's, the
            // first from the record's first point.
            if starts.first().is_some_and(|&first| first != 0)
                || starts.windows(2).any(|pair| pair[0] > pair[1])
                || starts.last().is_some_and(|&last| last > n)
            {
                return None;
            }
            let mut runs = Vec::with_capacity(parts);
            for &start in starts.iter().rev() {
                runs.push(points.split_off(start));
            }
            runs.reverse();
            if kind % 10 == 3 {
                runs.retain(|run| run.len() >= 2);
                if runs.is_empty() {
                    return Some(None);
                }
                geometry(GeometryValue::MultiLineString { coordinates: runs })
            } else {
                // A closed ring has its first point again at its end.
                runs.retain(|run| run.len() >= 4);
                if runs.is_empty() {
                    return Some(None);
                }
                geometry(GeometryValue::Polygon { coordinates: runs })
            }
        }
        _ => None,
    }
}

/// The `n` points of `content` from byte `at`, each two little-endian
/// doubles, when they are all in it.
fn positions(content: &[u8], at: usize, n: usize) -> Option<Vec<Position>> {
    let bytes = content.get(at..at.checked_add(n.checked_mul(16)?)?)?;
    let (points, _) = bytes.as_chunks::<16>();
    Some(
        points
            .iter()
            .map(|point| {
                let (xy, _) = point.as_chunks::<8>();
                Position::from([f64::from_le_bytes(xy[0]), f64::from_le_bytes(xy[1])])
            })
            .collect(),
    )
}

/// The little-endian count at byte `at` of `content`.
fn count(content: &[u8], at: usize) -> Option<usize> {
    usize::try_from(le_u32(content, at)?).ok()
}

fn be_u32(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

fn le_u32(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Lcc;
    use crate::archive::zipped;

    const WGS84: &[u8] = br#"GEOGCS["GCS_WGS_1984",DATUM["D_WGS_1984",SPHEROID["WGS_1984",6378137.0,298.257223563]],PRIMEM["Greenwich",0.0],UNIT["Degree",0.0174532925199433]]"#;

    fn fixture(name: &str) -> Vec<u8> {
        let path = format!("{}/../../fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
        std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
    }

    /// A `.shp` of `records`' contents, numbered, behind a header of
    /// `kind` whose bounding box is left empty.
    fn shp(kind: u32, records: &[Vec<u8>]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&FILE_CODE.to_be_bytes());
        out.extend_from_slice(&[0; 20]);
        out.extend_from_slice(&0u32.to_be_bytes());
        out.extend_from_slice(&VERSION.to_le_bytes());
        out.extend_from_slice(&kind.to_le_bytes());
        out.extend_from_slice(&[0; 64]);
        for (number, content) in records.iter().enumerate() {
            out.extend_from_slice(&(number as u32 + 1).to_be_bytes());
            out.extend_from_slice(&(content.len() as u32 / 2).to_be_bytes());
            out.extend_from_slice(content);
        }
        let words = out.len() as u32 / 2;
        out[24..28].copy_from_slice(&words.to_be_bytes());
        out
    }

    fn le(values: &[f64]) -> Vec<u8> {
        values.iter().flat_map(|v| v.to_le_bytes()).collect()
    }

    fn point(kind: u32, extra: &[f64]) -> Vec<u8> {
        let mut content = kind.to_le_bytes().to_vec();
        content.extend(le(&[2.35, 48.85]));
        content.extend(le(extra));
        content
    }

    /// A PolyLine or Polygon record of `kind`: the parts' first indices,
    /// then the points, then `extra` values, the Z and M ranges.
    fn multipart(kind: u32, starts: &[u32], points: &[[f64; 2]], extra: &[f64]) -> Vec<u8> {
        let mut content = kind.to_le_bytes().to_vec();
        content.extend(le(&[0.0; 4]));
        content.extend_from_slice(&(starts.len() as u32).to_le_bytes());
        content.extend_from_slice(&(points.len() as u32).to_le_bytes());
        for start in starts {
            content.extend_from_slice(&start.to_le_bytes());
        }
        for p in points {
            content.extend(le(p));
        }
        content.extend(le(extra));
        content
    }

    fn square(at: f64) -> Vec<[f64; 2]> {
        vec![
            [at, at],
            [at, at + 1.0],
            [at + 1.0, at + 1.0],
            [at + 1.0, at],
            [at, at],
        ]
    }

    /// What each geometry is, with how many positions in each part.
    fn kinds(geometries: &[Geometry]) -> Vec<(&'static str, Vec<usize>)> {
        geometries
            .iter()
            .map(|geometry| match &geometry.value {
                GeometryValue::Point { .. } => ("point", vec![1]),
                GeometryValue::MultiPoint { coordinates } => {
                    ("multipoint", vec![coordinates.len()])
                }
                GeometryValue::MultiLineString { coordinates } => {
                    ("lines", coordinates.iter().map(Vec::len).collect())
                }
                GeometryValue::Polygon { coordinates } => {
                    ("polygon", coordinates.iter().map(Vec::len).collect())
                }
                other => panic!("{other:?}"),
            })
            .collect()
    }

    #[test]
    fn reads_every_layer_of_the_fixture_in_lambert_93() {
        let layers = layers(&fixture("parcels.zip")).expect("read");
        assert_eq!(layers.len(), 3);
        for (_, crs) in &layers {
            assert_eq!(*crs, Crs::Lambert93(Lcc::lambert93()));
        }
        assert_eq!(kinds(&layers[0].0), [("point", vec![1])]);
        assert_eq!(kinds(&layers[1].0), [("lines", vec![3, 2])]);
        assert_eq!(kinds(&layers[2].0), [("polygon", vec![5, 5])]);
        let GeometryValue::Point { coordinates } = &layers[0].0[0].value else {
            unreachable!()
        };
        assert_eq!(coordinates.as_slice(), [652_469.023, 6_862_035.259]);
        // Without its `.prj`, a layer is in no known system.
        assert_eq!(super::layers(&fixture("parcels-no-prj.zip")), None);
    }

    #[test]
    fn reads_each_shape_type_and_its_z_and_m_variants() {
        let records = [
            point(1, &[]),
            point(11, &[35.0, 0.0]),
            point(21, &[0.0]),
            // MultiPointZ: its points, then the Z range and values.
            {
                let mut content = 18u32.to_le_bytes().to_vec();
                content.extend(le(&[0.0; 4]));
                content.extend_from_slice(&2u32.to_le_bytes());
                content.extend(le(&[2.0, 48.0, 3.0, 49.0]));
                content.extend(le(&[0.0, 1.0, 0.0, 1.0]));
                content
            },
            multipart(
                3,
                &[0, 2],
                &[[0.0, 0.0], [1.0, 1.0], [2.0, 2.0], [3.0, 3.0]],
                &[],
            ),
            multipart(23, &[0], &[[0.0, 0.0], [1.0, 1.0]], &[0.0, 1.0, 0.0, 1.0]),
            multipart(5, &[0, 5], &[square(0.0), square(0.25)].concat(), &[]),
            multipart(15, &[0], &square(1.0), &[0.0; 2 + 5 + 2 + 5]),
            // A null shape and a MultiPatch are skipped.
            0u32.to_le_bytes().to_vec(),
            {
                let mut content = 31u32.to_le_bytes().to_vec();
                content.extend(le(&[0.0; 4]));
                content
            },
            // A line of one point and a ring of three are not drawn; a
            // record left with nothing is no geometry.
            multipart(3, &[0, 1], &[[0.0, 0.0], [1.0, 1.0], [2.0, 2.0]], &[]),
            multipart(3, &[0], &[[0.0, 0.0]], &[]),
            multipart(
                5,
                &[0, 3],
                &[&[[0.0, 0.0], [1.0, 0.0], [0.0, 0.0]], &square(2.0)[..]].concat(),
                &[],
            ),
            multipart(5, &[], &[], &[]),
        ];
        assert_eq!(
            kinds(&geometries(&shp(1, &records)).expect("read")),
            [
                ("point", vec![1]),
                ("point", vec![1]),
                ("point", vec![1]),
                ("multipoint", vec![2]),
                ("lines", vec![2, 2]),
                ("lines", vec![2]),
                ("polygon", vec![5, 5]),
                ("polygon", vec![5]),
                ("lines", vec![2]),
                ("polygon", vec![5]),
            ]
        );
        assert_eq!(geometries(&shp(1, &[])), Some(Vec::new()));
    }

    #[test]
    fn a_file_cut_short_is_read_to_its_last_whole_record() {
        let file = shp(1, &[point(1, &[]), point(1, &[])]);
        for cut in [file.len() - 1, file.len() - 20, file.len() - 28 + 3] {
            assert_eq!(
                kinds(&geometries(&file[..cut]).expect("read")).len(),
                1,
                "{cut}"
            );
        }
        assert_eq!(geometries(&file[..HEADER]), Some(Vec::new()));
        assert_eq!(geometries(&file[..HEADER - 1]), None);
    }

    #[test]
    fn a_record_that_contradicts_itself_declines() {
        let points = [[0.0, 0.0], [1.0, 1.0], [2.0, 2.0]];
        for record in [
            // Parts out of order, past the points, not from the first.
            multipart(3, &[0, 2, 1], &points, &[]),
            multipart(3, &[0, 4], &points, &[]),
            multipart(3, &[1], &points, &[]),
            // More points or parts than the content holds.
            {
                let mut content = multipart(3, &[0], &points, &[]);
                content[40..44].copy_from_slice(&4u32.to_le_bytes());
                content
            },
            {
                let mut content = multipart(3, &[0], &points, &[]);
                content[36..40].copy_from_slice(&u32::MAX.to_le_bytes());
                content
            },
            {
                let mut content = 8u32.to_le_bytes().to_vec();
                content.extend(le(&[0.0; 4]));
                content.extend_from_slice(&u32::MAX.to_le_bytes());
                content
            },
            // A point without its coordinates, a type the format lacks.
            1u32.to_le_bytes().to_vec(),
            point(2, &[]),
            point(u32::MAX, &[]),
            Vec::new(),
        ] {
            assert_eq!(
                geometries(&shp(3, &[point(1, &[]), record.clone()])),
                None,
                "{record:?}"
            );
        }
        // Not a `.shp`: another file code, another version.
        let mut file = shp(1, &[point(1, &[])]);
        file[3] = 0;
        assert_eq!(geometries(&file), None);
        let mut file = shp(1, &[point(1, &[])]);
        file[28] = 0;
        assert_eq!(geometries(&file), None);
        assert_eq!(geometries(b""), None);
    }

    #[test]
    fn pairs_each_layer_with_its_prj_by_stem() {
        let one = shp(1, &[point(1, &[])]);
        let two = shp(1, &[point(1, &[]), point(1, &[])]);
        let layers = super::layers(&zipped(&[
            ("__MACOSX/export/._a.shp", b"\x00\x05\x16\x07"),
            ("export/A.SHP", &one),
            ("export/a.prj", WGS84),
            ("b.prj", WGS84),
            ("b.shp", &two),
            ("export/a.dbf", b"\x03"),
        ]))
        .expect("read");
        assert_eq!(
            layers
                .iter()
                .map(|(g, crs)| (g.len(), crs.clone()))
                .collect::<Vec<_>>(),
            [(1, Crs::Degrees), (2, Crs::Degrees)]
        );
        // A `.prj` of another stem, or of the same name in another
        // folder, is not the layer's.
        for entries in [
            [("a.shp", &one[..]), ("b.prj", WGS84)],
            [("a.shp", &one), ("export/a.prj", WGS84)],
        ] {
            assert_eq!(super::layers(&zipped(&entries)), None);
        }
        // One layer without a system declines the archive.
        assert_eq!(
            super::layers(&zipped(&[
                ("a.shp", &one),
                ("a.prj", WGS84),
                ("b.shp", &one)
            ])),
            None
        );
        // A layer that is not a `.shp` declines it too.
        assert_eq!(
            super::layers(&zipped(&[("a.shp", b"PK"), ("a.prj", WGS84)])),
            None
        );
        // No layer, or not a ZIP.
        assert_eq!(super::layers(&zipped(&[("a.prj", WGS84)])), None);
        assert_eq!(super::layers(&zipped(&[])), None);
        assert_eq!(super::layers(&one), None);
        assert_eq!(super::layers(b""), None);
    }

    #[test]
    fn the_layers_are_read_within_one_budget() {
        let one = shp(1, &[point(1, &[])]);
        let zip = zipped(&[
            ("a.shp", &one),
            ("a.prj", WGS84),
            ("b.shp", &one),
            ("b.prj", WGS84),
        ]);
        let both = 2 * one.len() as u64;
        assert_eq!(
            layers_within(&zip, both).map(|layers| layers.len()),
            Some(2)
        );
        assert_eq!(layers_within(&zip, both - 1), None);
    }
}
