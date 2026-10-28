//! A one-page PDF whose single image XObject covers most of the page, drawn
//! upright, with little text and nothing else painted over or around it:
//! the image is handed out as is when it is a JPEG or JPEG 2000 stream, or
//! as a PPM/PGM when it is raw samples.

use lopdf::content::{Content, Operation};
use std::collections::HashSet;

use lopdf::{Dictionary, Document, LoadOptions, Object, ObjectId, Stream};

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
    let page = page_box(&doc, page_id)?;
    if !upright(&doc, page_id)? || !annotations_paint_nothing(&doc, page_id)? {
        return None;
    }
    let (name, image) = single_image(&doc, page_id)?;
    let resources = resources(&doc, page_id)?;
    if !fonts_draw_glyphs(&doc, &resources)? {
        return None;
    }
    let states = plain_states(&doc, &resources)?;

    let content = doc.get_page_content_with_limit(page_id, max_output).ok()?;
    let ops = Content::decode(&content).ok()?;
    let placed = placement(&ops.operations, &name, &states, thresholds.max_text_chars)?;
    // What shows is the part of the image inside the page box.
    if placed.intersect(page).area() < thresholds.min_coverage * page.area() {
        return None;
    }
    image_bytes(&doc, image, max_output)
}

/// The page box, from the page or an ancestor. `CropBox` is what is shown;
/// `MediaBox` otherwise.
fn page_box(doc: &Document, page_id: ObjectId) -> Option<Rect> {
    let mut dict = doc.get_dictionary(page_id).ok()?;
    for _ in 0..MAX_DEPTH {
        for key in [&b"CropBox"[..], b"MediaBox"] {
            if let Ok(array) = dict.get_deref(key, doc).and_then(Object::as_array) {
                let v: Vec<f32> = array.iter().filter_map(|o| o.as_float().ok()).collect();
                if v.len() != 4 {
                    return None;
                }
                let page = Rect {
                    x0: v[0].min(v[2]),
                    y0: v[1].min(v[3]),
                    x1: v[0].max(v[2]),
                    y1: v[1].max(v[3]),
                };
                return (page.area() > 0.0).then_some(page);
            }
        }
        dict = dict
            .get_deref(b"Parent", doc)
            .and_then(Object::as_dict)
            .ok()?;
    }
    None
}

/// Whether the page is shown as it is drawn: its `Rotate`, from the page or
/// an ancestor, a multiple of 360 or absent. A scan app writes a landscape
/// page as an upright image on a page the viewer turns, which extracted as
/// is would be sideways. `None` when the value is not an integer.
fn upright(doc: &Document, page_id: ObjectId) -> Option<bool> {
    let mut dict = doc.get_dictionary(page_id).ok()?;
    for _ in 0..MAX_DEPTH {
        if let Ok(rotate) = dict.get_deref(b"Rotate", doc) {
            return Some(rotate.as_i64().ok()?.rem_euclid(360) == 0);
        }
        let Ok(parent) = dict.get_deref(b"Parent", doc).and_then(Object::as_dict) else {
            return Some(true);
        };
        dict = parent;
    }
    None
}

/// Whether the page's annotations draw nothing over it: none, or links
/// and pop-ups only. A filled square annotation is as good a cover as a
/// rectangle in the content stream. `None` when they cannot be read.
fn annotations_paint_nothing(doc: &Document, page_id: ObjectId) -> Option<bool> {
    let page = doc.get_dictionary(page_id).ok()?;
    let Ok(annots) = page.get_deref(b"Annots", doc) else {
        return Some(true);
    };
    for annot in annots.as_array().ok()? {
        let annot = doc.dereference(annot).ok()?.1.as_dict().ok()?;
        match annot.get(b"Subtype").and_then(Object::as_name).ok()? {
            b"Link" | b"Popup" => {}
            _ => return Some(false),
        }
    }
    Some(true)
}

/// The page's resource dictionaries, its own and the inherited ones.
fn resources(doc: &Document, page_id: ObjectId) -> Option<Vec<&Dictionary>> {
    let (own, inherited) = doc.get_page_resources(page_id).ok()?;
    Some(
        own.into_iter()
            .chain(
                inherited
                    .iter()
                    .filter_map(|id| doc.get_dictionary(*id).ok()),
            )
            .collect(),
    )
}

/// Whether the page's fonts are all ones whose glyphs are glyphs: a Type 3
/// font's are drawings of any size and colour, so one character could
/// cover the page whatever mode it is shown in. `None` when they cannot be
/// read.
fn fonts_draw_glyphs(doc: &Document, resources: &[&Dictionary]) -> Option<bool> {
    for resources in resources {
        let Ok(fonts) = resources.get_deref(b"Font", doc).and_then(Object::as_dict) else {
            continue;
        };
        for (_, font) in fonts.iter() {
            let font = doc.dereference(font).ok()?.1.as_dict().ok()?;
            if font.get(b"Subtype").and_then(Object::as_name).ok() == Some(b"Type3") {
                return Some(false);
            }
        }
    }
    Some(true)
}

/// The names of the page's graphics states (`gs`) that change nothing of
/// how the image shows: line, dash and flatness settings, full opacity and
/// the normal blend mode. A soft mask, transparency, another blend mode, a
/// transfer function or a halftone could hide or invert it, and a font set
/// there could show text; a page that selects any other state declines.
/// `None` when they cannot be read.
fn plain_states(doc: &Document, resources: &[&Dictionary]) -> Option<HashSet<Vec<u8>>> {
    const HARMLESS: &[&[u8]] = &[
        b"Type", b"LW", b"LC", b"LJ", b"ML", b"D", b"RI", b"FL", b"SM", b"SA", b"OP", b"op",
        b"OPM", b"AIS", b"TK",
    ];
    let mut plain = HashSet::new();
    for resources in resources {
        let Ok(states) = resources
            .get_deref(b"ExtGState", doc)
            .and_then(Object::as_dict)
        else {
            continue;
        };
        for (name, state) in states.iter() {
            let state = doc.dereference(state).ok()?.1.as_dict().ok()?;
            let harmless = state.iter().all(|(key, value)| match key.as_slice() {
                b"CA" | b"ca" => value.as_float().is_ok_and(|alpha| alpha >= 1.0),
                b"BM" => value
                    .as_name()
                    .is_ok_and(|mode| matches!(mode, b"Normal" | b"Compatible")),
                b"SMask" => value.as_name().is_ok_and(|mask| mask == b"None"),
                key => HARMLESS.contains(&key),
            });
            if harmless {
                plain.insert(name.clone());
            }
        }
    }
    Some(plain)
}

/// The page's one image XObject, by resource name. A form XObject could
/// draw anything, including more images, so its presence disqualifies.
fn single_image(doc: &Document, page_id: ObjectId) -> Option<(Vec<u8>, &Stream)> {
    let mut image = None;
    for resources in resources(doc, page_id)? {
        let Ok(xobjects) = resources
            .get_deref(b"XObject", doc)
            .and_then(Object::as_dict)
        else {
            continue;
        };
        for (name, value) in xobjects.iter() {
            let stream = doc.dereference(value).ok()?.1.as_stream().ok()?;
            match stream.dict.get(b"Subtype").and_then(Object::as_name).ok()? {
                // An image in an optional content group may be hidden.
                b"Image" if image.is_none() && !stream.dict.has(b"OC") => {
                    image = Some((name.clone(), stream));
                }
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

/// An upright rectangle in page units, `x0 <= x1` and `y0 <= y1`.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Rect {
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
}

impl Rect {
    fn area(self) -> f32 {
        (self.x1 - self.x0).max(0.0) * (self.y1 - self.y0).max(0.0)
    }

    /// The part of `self` inside `other`, empty when they do not meet.
    fn intersect(self, other: Self) -> Self {
        Self {
            x0: self.x0.max(other.x0),
            y0: self.y0.max(other.y0),
            x1: self.x1.min(other.x1),
            y1: self.y1.min(other.y1),
        }
    }
}

/// Operators that paint something other than the image or text: path
/// painting, shadings and inline images (which lopdf hands over as one
/// `BI`), and clipping, which may hide most of the image. Any of them
/// makes the page something else than its image, a black rectangle drawn
/// over a scan to begin with.
const PAINTING: &[&str] = &[
    "f", "F", "f*", "B", "B*", "b", "b*", "S", "s", "sh", "BI", "W", "W*",
];

/// The text rendering mode that paints nothing and clips nothing: how a
/// scan's OCR layer is written, searchable and unseen.
const INVISIBLE: i64 = 3;

/// Where the image lands on the page. Walk the content stream: track the
/// CTM and the text rendering mode through `q`/`Q`/`cm`/`Tr`, count text,
/// and record the box the named image is drawn into. `None` when the image
/// is not drawn upright, there is too much text, or anything else is
/// painted, clipped or marked optional, visible text included: a glyph is
/// a shape like any other, and a run of full blocks is how many redact by
/// typing. Only invisible text is allowed, up to `max_text_chars`; `plain`
/// are the graphics states the page may select (see `plain_states`).
fn placement(
    ops: &[Operation],
    image: &[u8],
    plain: &HashSet<Vec<u8>>,
    max_text_chars: u32,
) -> Option<Rect> {
    let mut ctm = Matrix::IDENTITY;
    let mut mode = 0;
    let mut stack = Vec::new();
    let mut placed: Option<Rect> = None;
    let mut text_chars = 0u64;
    for op in ops {
        let shown = match op.operator.as_str() {
            "Tj" | "'" => op.operands.last().map_or(0, string_len),
            "\"" => op.operands.get(2).map_or(0, string_len),
            "TJ" => match op.operands.first() {
                Some(Object::Array(parts)) => parts.iter().map(string_len).sum::<u64>(),
                _ => 0,
            },
            _ => 0,
        };
        if shown > 0 && mode != INVISIBLE {
            return None;
        }
        text_chars += shown;
        match op.operator.as_str() {
            "q" => stack.push((ctm, mode)),
            "Q" => (ctm, mode) = stack.pop().unwrap_or((Matrix::IDENTITY, 0)),
            "cm" => ctm = Matrix::from_operands(&op.operands)?.then(ctm),
            "Tr" => mode = op.operands.first()?.as_i64().ok()?,
            "gs" => {
                let name = op.operands.first()?.as_name().ok()?;
                if !plain.contains(name) {
                    return None;
                }
            }
            "Do" => {
                if op.operands.first().and_then(|o| o.as_name().ok()) != Some(image) {
                    continue;
                }
                // The image maps the unit square through the CTM. Only
                // an unrotated, unflipped placement looks like the
                // extracted image.
                let [a, b, c, d, e, f] = ctm.0;
                if b.abs() > 0.01 || c.abs() > 0.01 || a <= 0.0 || d <= 0.0 {
                    return None;
                }
                let this = Rect {
                    x0: e,
                    y0: f,
                    x1: e + a,
                    y1: f + d,
                };
                placed = Some(match placed {
                    Some(p) if p.area() >= this.area() => p,
                    _ => this,
                });
            }
            // Content marked optional may be shown or hidden.
            "BDC" if op.operands.first().and_then(|o| o.as_name().ok()) == Some(b"OC") => {
                return None;
            }
            painting if PAINTING.contains(&painting) => return None,
            _ => {}
        }
        if text_chars > u64::from(max_text_chars) {
            return None;
        }
    }
    placed
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
    // A mask, soft or stencil or colour key, or the alpha a JPEG 2000
    // stream is told to use as one, shows less of the image than its
    // samples hold; a decode array remaps them, as an inverted CMYK JPEG
    // is flagged. Either way the samples are not the picture.
    let soft_mask_in_data = dict
        .get(b"SMaskInData")
        .ok()
        .and_then(|o| o.as_i64().ok())
        .is_some_and(|n| n != 0);
    if dict.has(b"SMask") || dict.has(b"Mask") || soft_mask_in_data || dict.has(b"Decode") {
        return None;
    }
    // The filter says what the stream claims to be; the bytes are what vips
    // will load, so they have to agree.
    let filters = image.filters().unwrap_or_default();
    let encoded = match filters.as_slice() {
        [b"DCTDecode"] => Some("jpeg"),
        [b"JPXDecode"] => Some("jp2k"),
        _ => None,
    };
    if let Some(format) = encoded {
        // The colour space says how the page paints the samples, which
        // vips decodes as the stream's own: grey, RGB or CMYK agree, an
        // indexed, separation, DeviceN or Lab space does not (a separation
        // JPEG would come out as its negative). A JPEG 2000 stream may go
        // without one and carry its own.
        let agrees = match dict.get_deref(b"ColorSpace", doc) {
            Ok(space) => components(doc, space).is_some(),
            Err(_) => format == "jp2k",
        };
        if !agrees {
            return None;
        }
        return super::raster_format(&image.content)
            .filter(|sniffed| *sniffed == format)
            .map(|format| Extracted {
                bytes: image.content.clone(),
                format,
            });
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
    let channels = components(doc, dict.get_deref(b"ColorSpace", doc).ok()?)?;
    if channels == 4 {
        // CMYK: PNM has no such thing.
        return None;
    }
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

/// Samples per pixel of a grey, RGB or CMYK colour space, the ones whose
/// samples are the colours; `None` for any other.
fn components(doc: &Document, space: &Object) -> Option<usize> {
    match space {
        Object::Name(name) => match name.as_slice() {
            b"DeviceRGB" => Some(3),
            b"DeviceGray" => Some(1),
            b"DeviceCMYK" => Some(4),
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
                        n @ (1 | 3 | 4) => Some(n as usize),
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
    use lopdf::dictionary;

    const LIMIT: usize = 16 * 1024 * 1024;

    fn thresholds() -> Thresholds {
        Thresholds {
            min_coverage: 0.7,
            max_text_chars: 50,
        }
    }

    #[test]
    fn jpeg_covering_the_page_is_extracted() {
        let pdf = include_bytes!("../../../fixtures/image.pdf");
        let out = extract(pdf, &thresholds(), LIMIT).unwrap();
        assert_eq!(out.format, "jpeg");
        assert_eq!(&out.bytes[..3], &[0xff, 0xd8, 0xff]);
        let jpeg = include_bytes!("../../../fixtures/photo.jpg");
        assert_eq!(out.bytes, jpeg);
    }

    #[test]
    fn a_jpeg_stream_must_hold_a_jpeg() {
        let mut doc = Document::load_mem(include_bytes!("../../../fixtures/image.pdf")).unwrap();
        let id = doc
            .objects
            .iter()
            .find_map(|(id, object)| {
                let stream = object.as_stream().ok()?;
                let subtype = stream.dict.get(b"Subtype").and_then(Object::as_name).ok()?;
                (subtype == b"Image").then_some(*id)
            })
            .unwrap();
        if let Some(Object::Stream(stream)) = doc.objects.get_mut(&id) {
            stream.set_content(b"/* XPM */\nstatic char *x[] = {".to_vec());
        }
        let mut bytes = Vec::new();
        doc.save_to(&mut bytes).unwrap();
        assert_eq!(extract(&bytes, &thresholds(), LIMIT), None);
    }

    #[test]
    fn raw_samples_become_pnm() {
        let pdf = include_bytes!("../../../fixtures/flate.pdf");
        let out = extract(pdf, &thresholds(), LIMIT).unwrap();
        assert_eq!(out.format, "ppm");
        assert!(out.bytes.starts_with(b"P6\n4 2\n255\n"));
        assert_eq!(out.bytes.len(), "P6\n4 2\n255\n".len() + 4 * 2 * 3);
    }

    #[test]
    fn text_pages_are_not_images() {
        let pdf = include_bytes!("../../../fixtures/text.pdf");
        assert_eq!(extract(pdf, &thresholds(), LIMIT), None);
        // Visible text is painting, however little of it there is.
        let pdf = include_bytes!("../../../fixtures/mixed.pdf");
        let lenient = Thresholds {
            max_text_chars: 10_000,
            ..thresholds()
        };
        assert_eq!(extract(pdf, &lenient, LIMIT), None);
    }

    /// Text over the image passes only drawn invisible, as an OCR layer is,
    /// and up to `max_text_chars`; shown, or used as a clip, it declines.
    #[test]
    fn only_invisible_text_passes() {
        let image = |content: &str| image_with(content, |_, _| {});
        let draw = "q 60 0 0 30 0 0 cm /Im0 Do Q";
        assert!(image(&format!("{draw} BT 3 Tr /F1 6 Tf (scan) Tj ET")).is_some());
        let long = "x".repeat(51);
        assert_eq!(
            image(&format!("{draw} BT 3 Tr /F1 6 Tf ({long}) Tj ET")),
            None
        );
        // A run of full blocks in any font covers what it is drawn over.
        assert_eq!(image(&format!("{draw} BT /F1 600 Tf (\\333) Tj ET")), None);
        for mode in [0, 1, 2, 4, 5, 6, 7] {
            assert_eq!(
                image(&format!("{draw} BT {mode} Tr /F1 6 Tf (a) Tj ET")),
                None,
                "{mode}"
            );
        }
        assert_eq!(
            image(&format!(
                "{draw} BT 3 Tr /F1 6 Tf [(a) 10 (b)] TJ 0 Tr (c) ' ET"
            )),
            None
        );
        // The mode is graphics state: restored by `Q`.
        assert_eq!(
            image(&format!("{draw} q 3 Tr Q BT /F1 6 Tf (a) Tj ET")),
            None
        );
        assert!(image(&format!("{draw} 3 Tr q 0 Tr Q BT /F1 6 Tf (a) Tj ET")).is_some());
    }

    /// A Type 3 font's glyphs are drawings, which no mode keeps from
    /// covering the page.
    #[test]
    fn a_type3_font_is_not_extracted() {
        let with_font = |subtype: &str| {
            image_with(
                "q 60 0 0 30 0 0 cm /Im0 Do Q BT 3 Tr /F1 6 Tf (a) Tj ET",
                |doc, page| {
                    let font =
                        doc.add_object(dictionary! { "Type" => "Font", "Subtype" => subtype });
                    let resources = doc
                        .get_dictionary_mut(page)
                        .unwrap()
                        .get_mut(b"Resources")
                        .unwrap()
                        .as_dict_mut()
                        .unwrap();
                    resources.set("Font", dictionary! { "F1" => font });
                },
            )
        };
        assert!(with_font("Type1").is_some());
        assert_eq!(with_font("Type3"), None);
    }

    /// A graphics state may set line widths and the like; one that masks,
    /// fades, blends or remaps declines, as does one the page does not have.
    #[test]
    fn only_plain_graphics_states_pass() {
        let with_state = |state: Dictionary| {
            image_with("/GS0 gs q 60 0 0 30 0 0 cm /Im0 Do Q", |doc, page| {
                let resources = doc
                    .get_dictionary_mut(page)
                    .unwrap()
                    .get_mut(b"Resources")
                    .unwrap()
                    .as_dict_mut()
                    .unwrap();
                resources.set("ExtGState", dictionary! { "GS0" => state });
            })
        };
        assert!(with_state(dictionary! { "Type" => "ExtGState", "LW" => 2 }).is_some());
        assert!(
            with_state(dictionary! { "ca" => 1, "CA" => 1.0, "BM" => "Normal", "SMask" => "None" })
                .is_some()
        );
        for state in [
            dictionary! { "ca" => 0 },
            dictionary! { "CA" => 0.5 },
            dictionary! { "BM" => "Difference" },
            dictionary! { "SMask" => dictionary! { "S" => "Luminosity" } },
            dictionary! { "TR" => "Identity" },
            dictionary! { "Font" => vec![0.into(), 600.into()] },
        ] {
            assert_eq!(with_state(state.clone()), None, "{state:?}");
        }
        assert_eq!(
            image_with("/GS9 gs q 60 0 0 30 0 0 cm /Im0 Do Q", |_, _| {}),
            None
        );
    }

    /// `flate.pdf` with its image dictionary changed by `edit`, extracted.
    fn flate_with(edit: impl FnOnce(&mut lopdf::Dictionary)) -> Option<Extracted> {
        image_dict_with(include_bytes!("../../../fixtures/flate.pdf"), edit)
    }

    /// `pdf` with its image dictionary changed by `edit`, extracted.
    fn image_dict_with(pdf: &[u8], edit: impl FnOnce(&mut lopdf::Dictionary)) -> Option<Extracted> {
        let mut doc = Document::load_mem(pdf).unwrap();
        let image = doc
            .objects
            .values_mut()
            .find_map(|object| match object {
                Object::Stream(stream)
                    if stream.dict.get(b"Subtype").and_then(Object::as_name).ok()
                        == Some(b"Image") =>
                {
                    Some(stream)
                }
                _ => None,
            })
            .unwrap();
        edit(&mut image.dict);
        let mut bytes = Vec::new();
        doc.save_to(&mut bytes).unwrap();
        extract(&bytes, &thresholds(), LIMIT)
    }

    #[test]
    fn raw_samples_pnm_cannot_carry_are_left_to_the_renderer() {
        // Grey samples read off the same 24 bytes: a PGM of the first 8.
        let grey = flate_with(|d| d.set("ColorSpace", Object::Name(b"DeviceGray".to_vec())));
        let grey = grey.unwrap();
        assert_eq!(grey.format, "pgm");
        assert!(grey.bytes.starts_with(b"P5\n4 2\n255\n"));
        assert_eq!(grey.bytes.len(), "P5\n4 2\n255\n".len() + 4 * 2);

        // 16 bits a sample needs twice the bytes the stream has.
        assert_eq!(flate_with(|d| d.set("BitsPerComponent", 16)), None);
        assert_eq!(flate_with(|d| d.set("BitsPerComponent", 1)), None);
        assert_eq!(
            flate_with(|d| d.set("ColorSpace", Object::Name(b"DeviceCMYK".to_vec()))),
            None
        );
        assert_eq!(flate_with(|d| d.set("ImageMask", true)), None);
        // A decode array remaps the samples, which PNM cannot say.
        assert_eq!(
            flate_with(|d| d.set(
                "Decode",
                vec![1.into(), 0.into(), 1.into(), 0.into(), 1.into(), 0.into()]
            )),
            None
        );
        assert_eq!(flate_with(|d| d.set("Width", 0)), None);
    }

    /// A JPEG is handed out in a grey, RGB or CMYK space only: in an indexed
    /// or separation one, the page paints other colours than its samples.
    #[test]
    fn a_jpeg_in_another_colour_space_is_left_to_the_renderer() {
        let jpeg = include_bytes!("../../../fixtures/image.pdf");
        let space = |space: Object| image_dict_with(jpeg, |d| d.set("ColorSpace", space));
        let name = |n: &str| Object::Name(n.as_bytes().to_vec());
        assert!(space(name("DeviceRGB")).is_some());
        assert!(space(name("DeviceCMYK")).is_some());
        let separation = Object::Array(vec![
            name("Separation"),
            name("Black"),
            name("DeviceGray"),
            Object::Dictionary(lopdf::Dictionary::new()),
        ]);
        assert_eq!(space(separation), None);
        let indexed = Object::Array(vec![
            name("Indexed"),
            name("DeviceRGB"),
            1.into(),
            Object::string_literal(vec![0u8; 6]),
        ]);
        assert_eq!(space(indexed), None);
        assert_eq!(
            space(Object::Array(vec![
                name("Lab"),
                Object::Dictionary(lopdf::Dictionary::new())
            ])),
            None
        );
        // A JPEG must say its space; it is optional for JPEG 2000 alone.
        assert_eq!(
            image_dict_with(jpeg, |d| {
                d.remove(b"ColorSpace");
            }),
            None
        );
    }

    /// A mask or a decode array, on raw samples or an encoded stream alike,
    /// makes the page something else than the samples.
    #[test]
    fn masked_or_remapped_images_are_left_to_the_renderer() {
        let jpeg = include_bytes!("../../../fixtures/image.pdf");
        let flate = include_bytes!("../../../fixtures/flate.pdf");
        assert!(image_dict_with(jpeg, |_| {}).is_some());
        for pdf in [&jpeg[..], &flate[..]] {
            let mask = || {
                let grey = Stream::new(
                    lopdf::dictionary! {
                        "Type" => "XObject", "Subtype" => "Image", "Width" => 1, "Height" => 1,
                        "ColorSpace" => "DeviceGray", "BitsPerComponent" => 8,
                    },
                    vec![0],
                );
                Object::Stream(grey)
            };
            assert_eq!(image_dict_with(pdf, |d| d.set("SMask", mask())), None);
            assert_eq!(image_dict_with(pdf, |d| d.set("Mask", mask())), None);
            // A colour key: the samples in these ranges are not painted.
            assert_eq!(
                image_dict_with(pdf, |d| d.set(
                    "Mask",
                    vec![
                        0.into(),
                        10.into(),
                        0.into(),
                        10.into(),
                        0.into(),
                        10.into()
                    ]
                )),
                None
            );
            assert_eq!(
                image_dict_with(pdf, |d| d.set(
                    "Decode",
                    vec![1.into(), 0.into(), 1.into(), 0.into(), 1.into(), 0.into()]
                )),
                None
            );
            assert_eq!(image_dict_with(pdf, |d| d.set("SMaskInData", 1)), None);
        }
        assert!(image_dict_with(jpeg, |d| d.set("SMaskInData", 0)).is_some());
    }

    #[test]
    fn colour_spaces_whose_samples_are_the_colours() {
        let mut doc = Document::with_version("1.7");
        let mut icc = |n: i64| {
            let id = doc.add_object(Stream::new(lopdf::dictionary! { "N" => n }, Vec::new()));
            Object::Array(vec![Object::Name(b"ICCBased".to_vec()), id.into()])
        };
        let (icc_rgb, icc_grey, icc_cmyk) = (icc(3), icc(1), icc(4));
        let family = |name: &[u8]| {
            Object::Array(vec![
                Object::Name(name.to_vec()),
                Object::Dictionary(lopdf::Dictionary::new()),
            ])
        };
        for (space, want) in [
            (Object::Name(b"DeviceRGB".to_vec()), Some(3)),
            (Object::Name(b"DeviceGray".to_vec()), Some(1)),
            (Object::Name(b"DeviceCMYK".to_vec()), Some(4)),
            (family(b"CalRGB"), Some(3)),
            (family(b"CalGray"), Some(1)),
            (family(b"Lab"), None),
            (family(b"Indexed"), None),
            (icc_rgb, Some(3)),
            (icc_grey, Some(1)),
            (icc_cmyk, Some(4)),
            (Object::Array(vec![]), None),
            (Object::Integer(3), None),
        ] {
            assert_eq!(components(&doc, &space), want, "{space:?}");
        }
    }

    #[test]
    fn the_page_box_is_inherited() {
        let mut doc = Document::load_mem(include_bytes!("../../../fixtures/image.pdf")).unwrap();
        let page = *doc.get_pages().values().next().unwrap();
        let page_dict = doc.get_dictionary_mut(page).unwrap();
        let media = page_dict.remove(b"MediaBox").unwrap();
        let parent = page_dict.get(b"Parent").unwrap().as_reference().unwrap();
        doc.get_dictionary_mut(parent)
            .unwrap()
            .set("MediaBox", media);
        let mut bytes = Vec::new();
        doc.save_to(&mut bytes).unwrap();
        assert_eq!(
            extract(&bytes, &thresholds(), LIMIT).unwrap().format,
            "jpeg"
        );

        // A box that is not four numbers, or has no area, is no page.
        let page_dict = doc.get_dictionary_mut(page).unwrap();
        page_dict.set("MediaBox", vec![0.into(), 0.into(), 100.into()]);
        let mut bytes = Vec::new();
        doc.save_to(&mut bytes).unwrap();
        assert_eq!(extract(&bytes, &thresholds(), LIMIT), None);
        let page_dict = doc.get_dictionary_mut(page).unwrap();
        page_dict.set("MediaBox", vec![0.into(), 0.into(), 0.into(), 100.into()]);
        let mut bytes = Vec::new();
        doc.save_to(&mut bytes).unwrap();
        assert_eq!(extract(&bytes, &thresholds(), LIMIT), None);
    }

    #[test]
    fn small_or_rotated_images_are_not_extracted() {
        let pdf = include_bytes!("../../../fixtures/small.pdf");
        assert_eq!(extract(pdf, &thresholds(), LIMIT), None);
        let pdf = include_bytes!("../../../fixtures/rotated.pdf");
        assert_eq!(extract(pdf, &thresholds(), LIMIT), None);
    }

    /// `image.pdf`, a 60x30 page with the image drawn over all of it,
    /// with its page changed by `edit` and its content stream replaced by
    /// `content`, extracted.
    fn image_with(content: &str, edit: impl FnOnce(&mut Document, ObjectId)) -> Option<Extracted> {
        let mut doc = Document::load_mem(include_bytes!("../../../fixtures/image.pdf")).unwrap();
        let page = *doc.get_pages().values().next().unwrap();
        doc.change_page_content(page, content.as_bytes().to_vec())
            .unwrap();
        edit(&mut doc, page);
        let mut bytes = Vec::new();
        doc.save_to(&mut bytes).unwrap();
        extract(&bytes, &thresholds(), LIMIT)
    }

    #[test]
    fn the_page_must_be_its_image_and_nothing_else() {
        let image = |content: &str| image_with(content, |_, _| {});
        assert!(image("q 60 0 0 30 0 0 cm /Im0 Do Q").is_some());
        // A link draws nothing.
        let link = image_with("q 60 0 0 30 0 0 cm /Im0 Do Q", |doc, page| {
            let link = doc.add_object(dictionary! {
                "Type" => "Annot",
                "Subtype" => "Link",
                "Rect" => vec![0.into(), 0.into(), 10.into(), 10.into()],
            });
            doc.get_dictionary_mut(page)
                .unwrap()
                .set("Annots", vec![link.into()]);
        });
        assert!(link.is_some());

        // A black rectangle over the scan, the redaction the preview would
        // undo; a frame stroked around it; a shading.
        assert_eq!(
            image("q 60 0 0 30 0 0 cm /Im0 Do Q 0 g 0 0 60 30 re f"),
            None
        );
        assert_eq!(image("0 0 60 30 re S q 60 0 0 30 0 0 cm /Im0 Do Q"), None);
        assert_eq!(image("q 60 0 0 30 0 0 cm /Im0 Do Q /Sh0 sh"), None);
        // An inline image over the whole page.
        assert_eq!(
            image(
                "q 60 0 0 30 0 0 cm /Im0 Do Q \
                 q 60 0 0 30 0 0 cm BI /W 1 /H 1 /CS /G /BPC 8 ID \x00 EI Q"
            ),
            None
        );
        // A clip that shows one unit of the image.
        assert_eq!(image("q 0 0 1 1 re W n 60 0 0 30 0 0 cm /Im0 Do Q"), None);
        // Content in an optional content group.
        assert_eq!(image("/OC /L0 BDC q 60 0 0 30 0 0 cm /Im0 Do Q EMC"), None);
        // A filled square annotation over the page.
        let square = image_with("q 60 0 0 30 0 0 cm /Im0 Do Q", |doc, page| {
            let square = doc.add_object(dictionary! {
                "Type" => "Annot",
                "Subtype" => "Square",
                "Rect" => vec![0.into(), 0.into(), 60.into(), 30.into()],
                "IC" => vec![0.into(), 0.into(), 0.into()],
            });
            doc.get_dictionary_mut(page)
                .unwrap()
                .set("Annots", vec![square.into()]);
        });
        assert_eq!(square, None);
    }

    #[test]
    fn coverage_counts_the_image_inside_the_page() {
        let image = |content: &str| image_with(content, |_, _| {});
        // Off the page altogether, then three quarters off it.
        assert_eq!(image("q 60 0 0 30 1000 1000 cm /Im0 Do Q"), None);
        assert_eq!(image("q 60 0 0 30 45 0 cm /Im0 Do Q"), None);
        // A tenth off, within the 0.7 coverage.
        assert!(image("q 60 0 0 30 6 0 cm /Im0 Do Q").is_some());
        // A page box away from the origin, the image drawn over it.
        let shifted = image_with("q 60 0 0 30 100 200 cm /Im0 Do Q", |doc, page| {
            doc.get_dictionary_mut(page).unwrap().set(
                "MediaBox",
                vec![160.into(), 230.into(), 100.into(), 200.into()],
            );
        });
        assert!(shifted.is_some());
    }

    #[test]
    fn an_optional_image_is_not_extracted() {
        let mut doc = Document::load_mem(include_bytes!("../../../fixtures/image.pdf")).unwrap();
        let group = doc.add_object(lopdf::dictionary! { "Type" => "OCG", "Name" => "Layer" });
        let image = doc
            .objects
            .values_mut()
            .find_map(|object| match object {
                Object::Stream(stream)
                    if stream.dict.get(b"Subtype").and_then(Object::as_name).ok()
                        == Some(b"Image") =>
                {
                    Some(stream)
                }
                _ => None,
            })
            .unwrap();
        image.dict.set("OC", group);
        let mut bytes = Vec::new();
        doc.save_to(&mut bytes).unwrap();
        assert_eq!(extract(&bytes, &thresholds(), LIMIT), None);
    }

    #[test]
    fn a_rotated_page_is_not_extracted() {
        let rotated = |rotate: i64, on_parent: bool| {
            image_with("q 60 0 0 30 0 0 cm /Im0 Do Q", |doc, page| {
                let dict = doc.get_dictionary_mut(page).unwrap();
                if on_parent {
                    let parent = dict.get(b"Parent").unwrap().as_reference().unwrap();
                    doc.get_dictionary_mut(parent)
                        .unwrap()
                        .set("Rotate", rotate);
                } else {
                    dict.set("Rotate", rotate);
                }
            })
        };
        for rotate in [90, 180, 270, -90] {
            assert_eq!(rotated(rotate, false), None, "{rotate}");
        }
        // Inherited from the page tree.
        assert_eq!(rotated(90, true), None);
        // A full turn is no turn.
        assert!(rotated(0, false).is_some());
        assert!(rotated(360, true).is_some());
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
