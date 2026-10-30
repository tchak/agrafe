//! A shapefile's `.prj`: the coordinate system of its layer, in ESRI's
//! WKT 1, `PROJCS["RGF_1993_Lambert_93",GEOGCS[...],PROJECTION[...],
//! PARAMETER[...],UNIT[...]]`. A system has several names, ESRI's,
//! EPSG's, the one the exporting tool made up, so it is recognised by
//! what defines it, never by its name: a `GEOGCS` on the WGS 84 or GRS 80
//! ellipsoid (WGS 84, RGF93, ETRS89, which a map preview cannot tell
//! apart) in degrees from Greenwich is degrees; a Lambert conformal conic
//! on it with standard parallels 44° and 49°, central meridian 3°, false
//! origin (700 000, 6 600 000), in metres, is Lambert-93; a Mercator on
//! the auxiliary sphere, centred, in metres, is Web Mercator. Anything
//! else declines.

use crate::{Crs, Lcc};

/// How deep the brackets of a `.prj` may nest: a `PROJCS` is four.
const MAX_DEPTH: usize = 8;

/// The system the WKT `prj` defines, when it is one that is handled.
pub(super) fn crs(prj: &[u8]) -> Option<Crs> {
    let text = std::str::from_utf8(prj).ok()?;
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut parser = Parser { rest: text };
    let root = parser.node(0)?;
    if !parser
        .rest
        .trim_matches(|c: char| c.is_whitespace() || c == '\0')
        .is_empty()
    {
        return None;
    }
    match root.keyword.to_ascii_uppercase().as_str() {
        "GEOGCS" => degrees(&root).then_some(Crs::Degrees),
        "PROJCS" => projected(&root),
        _ => None,
    }
}

/// Whether the `GEOGCS` node is longitude and latitude in degrees from
/// Greenwich on the WGS 84 or the GRS 80 ellipsoid, which differ by a
/// tenth of a millimetre.
fn degrees(geogcs: &Node) -> bool {
    let ellipsoid = geogcs
        .child("DATUM")
        .and_then(|datum| datum.child("SPHEROID"))
        .map(|spheroid| (spheroid.number(0), spheroid.number(1)));
    let Some((Some(a), Some(inverse_flattening))) = ellipsoid else {
        return false;
    };
    near(a, 6_378_137.0)
        && (298.257_222..298.257_224).contains(&inverse_flattening)
        && geogcs
            .child("PRIMEM")
            .and_then(|primem| primem.number(0))
            .is_some_and(|longitude| near(longitude, 0.0))
        && geogcs
            .child("UNIT")
            .and_then(|unit| unit.number(0))
            .is_some_and(|radians| near(radians, 1f64.to_radians()))
}

/// The projected system the `PROJCS` node defines, when it is in metres
/// on a `GEOGCS` [`degrees`] accepts and is Lambert-93 or Web Mercator.
fn projected(projcs: &Node) -> Option<Crs> {
    if !projcs.child("GEOGCS").is_some_and(degrees)
        || !projcs
            .child("UNIT")
            .and_then(|unit| unit.number(0))
            .is_some_and(|metres| near(metres, 1.0))
    {
        return None;
    }
    let method = normalized(projcs.child("PROJECTION")?.text(0)?);
    let mut parameters = projcs
        .children("PARAMETER")
        .map(|parameter| Some((normalized(parameter.text(0)?), parameter.number(0)?)))
        .collect::<Option<Vec<_>>>()?;
    match method.as_str() {
        "lambertconformalconic" | "lambertconformalconic2sp" => {
            // The two standard parallels, in either order.
            let mut parallels = [None, None];
            parameters.retain(|(name, value)| {
                let slot = match name.as_str() {
                    "standardparallel1" => 0,
                    "standardparallel2" => 1,
                    _ => return true,
                };
                parallels[slot] = Some(*value);
                false
            });
            let [Some(first), Some(second)] = parallels else {
                return None;
            };
            let (south, north) = (first.min(second), first.max(second));
            (near(south, 44.0)
                && near(north, 49.0)
                && matches(
                    &parameters,
                    &[
                        ("falseeasting", 700_000.0),
                        ("falsenorthing", 6_600_000.0),
                        ("centralmeridian", 3.0),
                    ],
                    &[("latitudeoforigin", 46.5), ("scalefactor", 1.0)],
                ))
            .then(|| Crs::Lambert93(Lcc::lambert93()))
        }
        "mercatorauxiliarysphere" | "popularvisualisationpseudomercator" => matches(
            &parameters,
            &[],
            &[
                ("falseeasting", 0.0),
                ("falsenorthing", 0.0),
                ("centralmeridian", 0.0),
                ("standardparallel1", 0.0),
                ("latitudeoforigin", 0.0),
                ("scalefactor", 1.0),
                // On the sphere of the ellipsoid's semi-major axis.
                ("auxiliaryspheretype", 0.0),
            ],
        )
        .then_some(Crs::WebMercator),
        _ => None,
    }
}

/// Whether `parameters` has each of `required` with its value, and
/// nothing but those and `optional` ones with theirs.
fn matches(
    parameters: &[(String, f64)],
    required: &[(&str, f64)],
    optional: &[(&str, f64)],
) -> bool {
    let has = |(name, value): &(&str, f64)| {
        parameters
            .iter()
            .any(|(n, v)| n == name && near(*v, *value))
    };
    required.iter().all(has)
        && parameters.iter().all(|(name, value)| {
            required
                .iter()
                .chain(optional)
                .any(|(n, v)| n == name && near(*value, *v))
        })
}

/// `name` lowercased, without the underscores and spaces the spellings
/// differ by: `Standard_Parallel_1` and `standard parallel 1` are one.
fn normalized(name: &str) -> String {
    name.chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// Whether two numbers of a `.prj` are the same, to the digits a writer
/// rounds to.
fn near(a: f64, b: f64) -> bool {
    (a - b).abs() <= 1e-9 * b.abs().max(1.0)
}

/// A bracketed element of the WKT: its keyword and its values.
#[derive(Debug)]
struct Node<'a> {
    keyword: &'a str,
    values: Vec<Value<'a>>,
}

#[derive(Debug)]
enum Value<'a> {
    Node(Node<'a>),
    Text(&'a str),
    Number(f64),
    /// A bare word, `EAST` in `AXIS["X",EAST]`.
    Word,
}

impl<'a> Node<'a> {
    fn children(&self, keyword: &'a str) -> impl Iterator<Item = &Node<'a>> {
        self.values.iter().filter_map(move |value| match value {
            Value::Node(node) if node.keyword.eq_ignore_ascii_case(keyword) => Some(node),
            _ => None,
        })
    }

    fn child(&self, keyword: &'a str) -> Option<&Node<'a>> {
        self.children(keyword).next()
    }

    /// The `nth` number among the values.
    fn number(&self, nth: usize) -> Option<f64> {
        self.values
            .iter()
            .filter_map(|value| match value {
                Value::Number(number) => Some(*number),
                _ => None,
            })
            .nth(nth)
    }

    /// The `nth` quoted string among the values.
    fn text(&self, nth: usize) -> Option<&'a str> {
        self.values
            .iter()
            .filter_map(|value| match value {
                Value::Text(text) => Some(*text),
                _ => None,
            })
            .nth(nth)
    }
}

/// A recursive descent over WKT, `KEYWORD[value, ...]` with square or
/// round brackets, a value a quoted string, a number, a bare word or an
/// element, nested at most `MAX_DEPTH` deep.
struct Parser<'a> {
    rest: &'a str,
}

impl<'a> Parser<'a> {
    fn node(&mut self, depth: usize) -> Option<Node<'a>> {
        if depth >= MAX_DEPTH {
            return None;
        }
        let keyword = self.word()?;
        self.rest = self.rest.trim_start();
        self.rest = self.rest.strip_prefix(['[', '('])?;
        let mut values = Vec::new();
        loop {
            values.push(self.value(depth)?);
            self.rest = self.rest.trim_start();
            if let Some(rest) = self.rest.strip_prefix(',') {
                self.rest = rest;
            } else {
                self.rest = self.rest.strip_prefix([']', ')'])?;
                return Some(Node { keyword, values });
            }
        }
    }

    fn value(&mut self, depth: usize) -> Option<Value<'a>> {
        self.rest = self.rest.trim_start();
        if let Some(rest) = self.rest.strip_prefix('"') {
            // A quote inside a string is doubled, which no name of a
            // system handled here has.
            let end = rest.find('"')?;
            self.rest = &rest[end + 1..];
            return Some(Value::Text(&rest[..end]));
        }
        if self
            .rest
            .starts_with(|c: char| c.is_ascii_digit() || matches!(c, '-' | '+' | '.'))
        {
            let end = self
                .rest
                .find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '-' | '+' | '.')))
                .unwrap_or(self.rest.len());
            let number = self.rest[..end].parse().ok()?;
            self.rest = &self.rest[end..];
            return Some(Value::Number(number));
        }
        let before = self.rest;
        self.word()?;
        if self.rest.trim_start().starts_with(['[', '(']) {
            self.rest = before;
            return self.node(depth + 1).map(Value::Node);
        }
        Some(Value::Word)
    }

    fn word(&mut self) -> Option<&'a str> {
        let end = self
            .rest
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .unwrap_or(self.rest.len());
        if end == 0 {
            return None;
        }
        let word = &self.rest[..end];
        self.rest = &self.rest[end..];
        Some(word)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WGS84: &str = r#"GEOGCS["GCS_WGS_1984",DATUM["D_WGS_1984",SPHEROID["WGS_1984",6378137.0,298.257223563]],PRIMEM["Greenwich",0.0],UNIT["Degree",0.0174532925199433]]"#;
    const RGF93: &str = r#"GEOGCS["GCS_RGF_1993",DATUM["D_RGF_1993",SPHEROID["GRS_1980",6378137.0,298.257222101]],PRIMEM["Greenwich",0.0],UNIT["Degree",0.0174532925199433]]"#;
    /// Lambert-93 as ArcGIS and QGIS write it.
    const LAMBERT93: &str = r#"PROJCS["RGF_1993_Lambert_93",GEOGCS["GCS_RGF_1993",DATUM["D_RGF_1993",SPHEROID["GRS_1980",6378137.0,298.257222101]],PRIMEM["Greenwich",0.0],UNIT["Degree",0.0174532925199433]],PROJECTION["Lambert_Conformal_Conic"],PARAMETER["False_Easting",700000.0],PARAMETER["False_Northing",6600000.0],PARAMETER["Central_Meridian",3.0],PARAMETER["Standard_Parallel_1",49.0],PARAMETER["Standard_Parallel_2",44.0],PARAMETER["Latitude_Of_Origin",46.5],UNIT["Meter",1.0]]"#;
    const WEB_MERCATOR: &str = r#"PROJCS["WGS_1984_Web_Mercator_Auxiliary_Sphere",GEOGCS["GCS_WGS_1984",DATUM["D_WGS_1984",SPHEROID["WGS_1984",6378137.0,298.257223563]],PRIMEM["Greenwich",0.0],UNIT["Degree",0.0174532925199433]],PROJECTION["Mercator_Auxiliary_Sphere"],PARAMETER["False_Easting",0.0],PARAMETER["False_Northing",0.0],PARAMETER["Central_Meridian",0.0],PARAMETER["Standard_Parallel_1",0.0],PARAMETER["Auxiliary_Sphere_Type",0.0],UNIT["Meter",1.0]]"#;

    fn crs_of(prj: &str) -> Option<Crs> {
        crs(prj.as_bytes())
    }

    #[test]
    fn recognises_the_systems_by_what_defines_them() {
        assert_eq!(crs_of(WGS84), Some(Crs::Degrees));
        assert_eq!(crs_of(RGF93), Some(Crs::Degrees));
        assert_eq!(crs_of(LAMBERT93), Some(Crs::Lambert93(Lcc::lambert93())));
        assert_eq!(crs_of(WEB_MERCATOR), Some(Crs::WebMercator));
        // GDAL's spelling of Lambert-93: OGC names, integers, authorities
        // and axes, the parallels the other way round, and a newline.
        let gdal = r#"PROJCS["RGF93 v1 / Lambert-93",
            GEOGCS["RGF93 v1",DATUM["Reseau_Geodesique_Francais_1993_v1",SPHEROID["GRS 1980",6378137,298.257222101,AUTHORITY["EPSG","7019"]],AUTHORITY["EPSG","6171"]],PRIMEM["Greenwich",0,AUTHORITY["EPSG","8901"]],UNIT["degree",0.0174532925199433,AUTHORITY["EPSG","9122"]],AUTHORITY["EPSG","4171"]],
            PROJECTION["Lambert_Conformal_Conic_2SP"],PARAMETER["latitude_of_origin",46.5],PARAMETER["central_meridian",3],PARAMETER["standard_parallel_1",44],PARAMETER["standard_parallel_2",49],PARAMETER["false_easting",700000],PARAMETER["false_northing",6600000],
            UNIT["metre",1,AUTHORITY["EPSG","9001"]],AXIS["Easting",EAST],AXIS["Northing",NORTH],AUTHORITY["EPSG","2154"]]
"#;
        assert_eq!(crs_of(gdal), Some(Crs::Lambert93(Lcc::lambert93())));
        // A byte order mark, round brackets.
        assert_eq!(
            crs(format!("\u{feff}{}", WGS84.replace('[', "(").replace(']', ")")).as_bytes()),
            Some(Crs::Degrees)
        );
        // The name is not what is looked at.
        assert_eq!(
            crs_of(&LAMBERT93.replace("RGF_1993_Lambert_93", "Mon export")),
            Some(Crs::Lambert93(Lcc::lambert93()))
        );
    }

    #[test]
    fn declines_every_other_system() {
        for prj in [
            // Named Lambert-93, but CC46, a conic zone with other parallels.
            LAMBERT93.replace(
                r#"PARAMETER["Standard_Parallel_1",49.0],PARAMETER["Standard_Parallel_2",44.0],PARAMETER["Latitude_Of_Origin",46.5]"#,
                r#"PARAMETER["Standard_Parallel_1",45.25],PARAMETER["Standard_Parallel_2",46.75],PARAMETER["Latitude_Of_Origin",46.0]"#,
            ),
            // Another false origin.
            LAMBERT93.replace("6600000.0", "2200000.0"),
            // A parallel missing, a parameter Lambert-93 has not.
            LAMBERT93.replace(r#"PARAMETER["Standard_Parallel_2",44.0],"#, ""),
            LAMBERT93.replace(
                r#"UNIT["Meter",1.0]]"#,
                r#"PARAMETER["Azimuth",12.0],UNIT["Meter",1.0]]"#,
            ),
            // In feet.
            LAMBERT93.replace(r#"UNIT["Meter",1.0]]"#, r#"UNIT["Foot_US",0.3048006096012192]]"#),
            // NTF, on Clarke 1880, from Paris.
            RGF93.replace("6378137.0,298.257222101", "6378249.2,293.4660212936269"),
            RGF93.replace(r#"PRIMEM["Greenwich",0.0]"#, r#"PRIMEM["Paris",2.33722917]"#),
            // In grads.
            RGF93.replace("0.0174532925199433", "0.015707963267949"),
            // UTM 31N.
            WEB_MERCATOR
                .replace("Mercator_Auxiliary_Sphere", "Transverse_Mercator")
                .replace(r#"PARAMETER["Auxiliary_Sphere_Type",0.0],"#, ""),
            // On the authalic sphere.
            WEB_MERCATOR.replace(
                r#"PARAMETER["Auxiliary_Sphere_Type",0.0]"#,
                r#"PARAMETER["Auxiliary_Sphere_Type",3.0]"#,
            ),
            // A compound system, a vertical one.
            format!(r#"COMPD_CS["L93 + NGF",{LAMBERT93},VERT_CS["NGF-IGN69"]]"#),
            r#"VERTCS["NGF_IGN69",VDATUM["NGF_IGN69"]]"#.to_owned(),
            // Not WKT.
            format!("{WGS84}]"),
            format!("{WGS84} junk"),
            WGS84[..WGS84.len() - 1].to_owned(),
            "EPSG:2154".to_owned(),
            String::new(),
            "[".repeat(10_000),
            "A[".repeat(10_000),
        ] {
            assert_eq!(crs_of(&prj), None, "{prj}");
        }
        assert_eq!(crs(b"\xff\xfe"), None);
    }
}
