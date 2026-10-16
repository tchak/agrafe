//! A one-page PDF whose single image XObject covers most of the page, drawn
//! upright, with little text: the image is handed out as is when it is a
//! JPEG or JPEG 2000 stream, or as a PPM/PGM when it is raw samples.

use lopdf::content::{Content, Operation};
use lopdf::{Document, LoadOptions, Object, ObjectId, Stream};

use super::{Extracted, Thresholds};

/// Levels of the page tree walked for inherited attributes.
const MAX_DEPTH: usize = 64;

pub fn extract(bytes: &[u8], thresholds: &Thresholds, max_output: usize) -> Option<Extracted> {
    let doc = Document::load_mem_with_options(
        bytes,
        LoadOptions {
            max_decompressed_size: Some(max_output),
            ..Default::default()
        },
    )
    .ok()?;
    let pages = doc.get_pages();
    if pages.len() != 1 {
        return None;
    }
    let page_id = *pages.values().next()?;
    let (page_width, page_height) = page_size(&doc, page_id)?;
    let (name, image) = single_image(&doc, page_id)?;

    let content = doc.get_page_content_with_limit(page_id, max_output).ok()?;
    let ops = Content::decode(&content).ok()?;
    let placed = Placement::find(&ops.operations, &name, thresholds.max_text_chars)?;
    if placed.width * placed.height < thresholds.min_coverage * page_width * page_height {
        return None;
    }
    image_bytes(&doc, image, max_output)
}

/// The page box, from the page or an ancestor. `CropBox` is what is shown;
/// `MediaBox` otherwise.
fn page_size(doc: &Document, page_id: ObjectId) -> Option<(f32, f32)> {
    let mut dict = doc.get_dictionary(page_id).ok()?;
    for _ in 0..MAX_DEPTH {
        for key in [&b"CropBox"[..], b"MediaBox"] {
            if let Ok(array) = dict.get_deref(key, doc).and_then(Object::as_array) {
                let v: Vec<f32> = array.iter().filter_map(|o| o.as_float().ok()).collect();
                if v.len() != 4 {
                    return None;
                }
                let (w, h) = ((v[2] - v[0]).abs(), (v[3] - v[1]).abs());
                return (w > 0.0 && h > 0.0).then_some((w, h));
            }
        }
        dict = dict
            .get_deref(b"Parent", doc)
            .and_then(Object::as_dict)
            .ok()?;
    }
    None
}

/// The page's one image XObject, by resource name. A form XObject could
/// draw anything, including more images, so its presence disqualifies.
fn single_image(doc: &Document, page_id: ObjectId) -> Option<(Vec<u8>, &Stream)> {
    let (own, inherited) = doc.get_page_resources(page_id).ok()?;
    let mut image = None;
    let resources = own.into_iter().chain(
        inherited
            .iter()
            .filter_map(|id| doc.get_dictionary(*id).ok()),
    );
    for resources in resources {
        let Ok(xobjects) = resources
            .get_deref(b"XObject", doc)
            .and_then(Object::as_dict)
        else {
            continue;
        };
        for (name, value) in xobjects.iter() {
            let stream = doc.dereference(value).ok()?.1.as_stream().ok()?;
            match stream.dict.get(b"Subtype").and_then(Object::as_name).ok()? {
                b"Image" if image.is_none() => image = Some((name.clone(), stream)),
                _ => return None,
            }
        }
    }
    image
}

/// A PDF transformation matrix `[a b c d e f]`.
#[derive(Debug, Clone, Copy)]
struct Matrix([f32; 6]);

impl Matrix {
    const IDENTITY: Self = Self([1.0, 0.0, 0.0, 1.0, 0.0, 0.0]);

    /// `self` applied first, then `other`: what `cm` does to the CTM.
    fn then(self, other: Self) -> Self {
        let [a, b, c, d, e, f] = self.0;
        let [a2, b2, c2, d2, e2, f2] = other.0;
        Self([
            a * a2 + b * c2,
            a * b2 + b * d2,
            c * a2 + d * c2,
            c * b2 + d * d2,
            e * a2 + f * c2 + e2,
            e * b2 + f * d2 + f2,
        ])
    }

    fn from_operands(operands: &[Object]) -> Option<Self> {
        let v: Vec<f32> = operands.iter().filter_map(|o| o.as_float().ok()).collect();
        v.try_into().ok().map(Self)
    }
}

/// Where the image lands on the page, in page units.
#[derive(Debug, Clone, Copy)]
struct Placement {
    width: f32,
    height: f32,
}

impl Placement {
    /// Walk the content stream: track the CTM through `q`/`Q`/`cm`, count
    /// text, and record the box the named image is drawn into. `None` when
    /// the image is not drawn upright, or there is too much text.
    fn find(ops: &[Operation], image: &[u8], max_text_chars: u32) -> Option<Self> {
        let mut ctm = Matrix::IDENTITY;
        let mut stack = Vec::new();
        let mut placed: Option<Placement> = None;
        let mut text_chars = 0u64;
        for op in ops {
            match op.operator.as_str() {
                "q" => stack.push(ctm),
                "Q" => ctm = stack.pop().unwrap_or(Matrix::IDENTITY),
                "cm" => ctm = Matrix::from_operands(&op.operands)?.then(ctm),
                "Do" => {
                    if op.operands.first().and_then(|o| o.as_name().ok()) != Some(image) {
                        continue;
                    }
                    // The image maps the unit square through the CTM. Only
                    // an unrotated, unflipped placement looks like the
                    // extracted image.
                    let [a, b, c, d, _, _] = ctm.0;
                    if b.abs() > 0.01 || c.abs() > 0.01 || a <= 0.0 || d <= 0.0 {
                        return None;
                    }
                    let this = Placement {
                        width: a,
                        height: d,
                    };
                    placed = Some(match placed {
                        Some(p) if p.width * p.height >= a * d => p,
                        _ => this,
                    });
                }
                "Tj" | "'" => text_chars += op.operands.last().map_or(0, string_len),
                "\"" => text_chars += op.operands.get(2).map_or(0, string_len),
                "TJ" => {
                    if let Some(Object::Array(parts)) = op.operands.first() {
                        text_chars += parts.iter().map(string_len).sum::<u64>();
                    }
                }
                _ => {}
            }
            if text_chars > u64::from(max_text_chars) {
                return None;
            }
        }
        placed
    }
}

/// Bytes of a string operand: one per character in simple fonts, two in
/// composite ones, so a count that errs on the side of "too much text".
fn string_len(o: &Object) -> u64 {
    match o {
        Object::String(s, _) => s.len() as u64,
        _ => 0,
    }
}

/// The image stream's bytes in a format libvips loads. Encoded streams
/// (JPEG, JPEG 2000) are passed through; raw 8 or 16 bit grey or RGB
/// samples get a PNM header. Anything else is left to the page renderer.
fn image_bytes(doc: &Document, image: &Stream, max_output: usize) -> Option<Extracted> {
    let dict = &image.dict;
    if dict
        .get(b"ImageMask")
        .is_ok_and(|o| matches!(o, Object::Boolean(true)))
    {
        return None;
    }
    let filters = image.filters().unwrap_or_default();
    match filters.as_slice() {
        [b"DCTDecode"] => {
            return Some(Extracted {
                bytes: image.content.clone(),
                format: "jpeg",
            });
        }
        [b"JPXDecode"] => {
            return Some(Extracted {
                bytes: image.content.clone(),
                format: "jp2",
            });
        }
        _ => {}
    }
    // Raw samples, possibly behind a lossless filter. Any image filter left
    // in the chain makes this fail, which is the answer we want.
    let data = image.decompressed_content_with_limit(max_output).ok()?;
    let dimension = |key: &[u8]| {
        dict.get_deref(key, doc)
            .ok()
            .and_then(|o| o.as_i64().ok())
            .and_then(|n| usize::try_from(n).ok())
            .filter(|n| *n > 0)
    };
    let (width, height) = (dimension(b"Width")?, dimension(b"Height")?);
    let bits = dict
        .get_deref(b"BitsPerComponent", doc)
        .ok()?
        .as_i64()
        .ok()?;
    let maxval = match bits {
        8 => 255,
        16 => 65535,
        _ => return None,
    };
    if dict.get(b"Decode").is_ok() {
        return None;
    }
    let channels = channels(doc, dict.get_deref(b"ColorSpace", doc).ok()?)?;
    let expected = width
        .checked_mul(height)?
        .checked_mul(channels)?
        .checked_mul(bits as usize / 8)?;
    if data.len() < expected {
        return None;
    }
    let (magic, format) = if channels == 3 {
        ("P6", "ppm")
    } else {
        ("P5", "pgm")
    };
    let mut bytes = format!("{magic}\n{width} {height}\n{maxval}\n").into_bytes();
    bytes.extend_from_slice(&data[..expected]);
    Some(Extracted { bytes, format })
}

/// Samples per pixel for the colour spaces PNM can carry.
fn channels(doc: &Document, space: &Object) -> Option<usize> {
    match space {
        Object::Name(name) => match name.as_slice() {
            b"DeviceRGB" => Some(3),
            b"DeviceGray" => Some(1),
            _ => None,
        },
        Object::Array(parts) => {
            let family = parts.first()?.as_name().ok()?;
            match family {
                b"CalRGB" => Some(3),
                b"CalGray" => Some(1),
                b"ICCBased" => {
                    let profile = doc.dereference(parts.get(1)?).ok()?.1.as_stream().ok()?;
                    match profile.dict.get_deref(b"N", doc).ok()?.as_i64().ok()? {
                        3 => Some(3),
                        1 => Some(1),
                        _ => None,
                    }
                }
                _ => None,
            }
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIMIT: usize = 16 * 1024 * 1024;

    fn thresholds() -> Thresholds {
        Thresholds {
            min_coverage: 0.7,
            max_text_chars: 50,
        }
    }

    #[test]
    fn jpeg_covering_the_page_is_extracted() {
        let pdf = include_bytes!("../../../tests/fixtures/image.pdf");
        let out = extract(pdf, &thresholds(), LIMIT).unwrap();
        assert_eq!(out.format, "jpeg");
        assert_eq!(&out.bytes[..3], &[0xff, 0xd8, 0xff]);
        let jpeg = include_bytes!("../../../tests/fixtures/photo.jpg");
        assert_eq!(out.bytes, jpeg);
    }

    #[test]
    fn raw_samples_become_pnm() {
        let pdf = include_bytes!("../../../tests/fixtures/flate.pdf");
        let out = extract(pdf, &thresholds(), LIMIT).unwrap();
        assert_eq!(out.format, "ppm");
        assert!(out.bytes.starts_with(b"P6\n4 2\n255\n"));
        assert_eq!(out.bytes.len(), "P6\n4 2\n255\n".len() + 4 * 2 * 3);
    }

    #[test]
    fn text_pages_are_not_images() {
        let pdf = include_bytes!("../../../tests/fixtures/text.pdf");
        assert_eq!(extract(pdf, &thresholds(), LIMIT), None);
        let pdf = include_bytes!("../../../tests/fixtures/mixed.pdf");
        assert_eq!(extract(pdf, &thresholds(), LIMIT), None);
        // The same image with a generous text allowance passes.
        let lenient = Thresholds {
            max_text_chars: 10_000,
            ..thresholds()
        };
        assert!(extract(pdf, &lenient, LIMIT).is_some());
    }

    #[test]
    fn small_or_rotated_images_are_not_extracted() {
        let pdf = include_bytes!("../../../tests/fixtures/small.pdf");
        assert_eq!(extract(pdf, &thresholds(), LIMIT), None);
        let pdf = include_bytes!("../../../tests/fixtures/rotated.pdf");
        assert_eq!(extract(pdf, &thresholds(), LIMIT), None);
    }

    #[test]
    fn garbage_is_none() {
        assert_eq!(extract(b"%PDF-1.4 garbage", &thresholds(), LIMIT), None);
        assert_eq!(extract(b"", &thresholds(), LIMIT), None);
    }

    #[test]
    fn matrices_compose() {
        let scale = Matrix([2.0, 0.0, 0.0, 3.0, 0.0, 0.0]);
        let shift = Matrix([1.0, 0.0, 0.0, 1.0, 10.0, 20.0]);
        let m = scale.then(shift);
        assert_eq!(m.0, [2.0, 0.0, 0.0, 3.0, 10.0, 20.0]);
        let m = shift.then(scale);
        assert_eq!(m.0, [2.0, 0.0, 0.0, 3.0, 20.0, 60.0]);
    }
}
