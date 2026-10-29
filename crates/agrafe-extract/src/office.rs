//! A DOCX or ODT that is a wrapper around one picture: exactly one media
//! file in the archive and next to no text in the body.

use std::io::{BufReader, Cursor, Read};

use quick_xml::Reader;
use quick_xml::events::Event;
use zip::ZipArchive;

use super::{Extracted, Thresholds};

/// Body XML read at most. A body this long is not "almost empty" anyway,
/// and one cut there is not told to be.
const XML_LIMIT: u64 = 8 * 1024 * 1024;
/// Archives with more entries than this are not simple documents.
const MAX_ENTRIES: usize = 10_000;

pub fn extract_docx(bytes: &[u8], thresholds: &Thresholds, max_output: usize) -> Option<Extracted> {
    extract(
        bytes,
        "word/document.xml",
        "word/media/",
        thresholds,
        max_output,
    )
}

pub fn extract_odt(bytes: &[u8], thresholds: &Thresholds, max_output: usize) -> Option<Extracted> {
    extract(bytes, "content.xml", "Pictures/", thresholds, max_output)
}

fn extract(
    bytes: &[u8],
    body: &str,
    media_prefix: &str,
    thresholds: &Thresholds,
    max_output: usize,
) -> Option<Extracted> {
    let mut zip = ZipArchive::new(Cursor::new(bytes)).ok()?;
    if zip.len() > MAX_ENTRIES {
        return None;
    }
    let mut media = None;
    for index in 0..zip.len() {
        let entry = zip.by_index(index).ok()?;
        if entry.is_dir() {
            continue;
        }
        if entry.name().ok()?.starts_with(media_prefix) {
            if media.is_some() {
                return None;
            }
            media = Some(index);
        }
    }
    let media = media?;
    if !text_is_sparse(&mut zip, body, thresholds.max_text_chars)? {
        return None;
    }
    let entry = zip.by_index(media).ok()?;
    let mut data = Vec::new();
    // The size in the entry header is a claim; the read is what is bounded.
    entry
        .take(max_output as u64 + 1)
        .read_to_end(&mut data)
        .ok()?;
    if data.len() > max_output {
        return None;
    }
    let format = super::raster_format(&data)?;
    Some(Extracted {
        header: Vec::new(),
        bytes: data,
        format,
    })
}

/// Whether the body has at most `max_chars` non-whitespace characters of
/// text, CDATA sections included, since a word processor shows them like
/// any text. Markup is not counted; an entity or character reference counts
/// as the one character it stands for. A body that does not end within
/// `XML_LIMIT` is not sparse: what follows the cut is unread.
fn text_is_sparse(zip: &mut ZipArchive<Cursor<&[u8]>>, body: &str, max_chars: u32) -> Option<bool> {
    let entry = zip.by_name(body).ok()?;
    let mut reader = Reader::from_reader(BufReader::new(entry.take(XML_LIMIT)));
    let mut buf = Vec::new();
    let mut chars = 0u64;
    let visible = |text: &str| text.chars().filter(|c| !c.is_whitespace()).count() as u64;
    loop {
        chars += match reader.read_event_into(&mut buf) {
            Ok(Event::Eof) => return Some(reader.get_ref().get_ref().limit() > 0),
            Ok(Event::Text(text)) => visible(&text),
            Ok(Event::CData(text)) => visible(&text),
            Ok(Event::GeneralRef(_)) => 1,
            Ok(_) => 0,
            Err(_) => return None,
        };
        if chars > u64::from(max_chars) {
            return Some(false);
        }
        buf.clear();
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
    fn docx_wrapping_one_picture() {
        let docx = include_bytes!("../../../fixtures/image.docx");
        let out = extract_docx(docx, &thresholds(), LIMIT).unwrap();
        assert_eq!(out.format, "png");
        assert_eq!(out.bytes, include_bytes!("../../../fixtures/photo.png"));
    }

    #[test]
    fn odt_wrapping_one_picture() {
        let odt = include_bytes!("../../../fixtures/image.odt");
        let out = extract_odt(odt, &thresholds(), LIMIT).unwrap();
        assert_eq!(out.format, "png");
    }

    #[test]
    fn documents_with_text_are_documents() {
        let docx = include_bytes!("../../../fixtures/text.docx");
        assert_eq!(extract_docx(docx, &thresholds(), LIMIT), None);
        let lenient = Thresholds {
            max_text_chars: 10_000,
            ..thresholds()
        };
        assert!(extract_docx(docx, &lenient, LIMIT).is_some());
    }

    #[test]
    fn the_picture_must_fit_the_limit_and_be_raster() {
        let docx = include_bytes!("../../../fixtures/image.docx");
        assert_eq!(extract_docx(docx, &thresholds(), 10), None);
        let docx = include_bytes!("../../../fixtures/vector.docx");
        assert_eq!(extract_docx(docx, &thresholds(), LIMIT), None);
    }

    /// A package holding one picture and `body` as its document part.
    fn docx(body: &[u8]) -> Vec<u8> {
        use std::io::Write as _;
        let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default();
        zip.start_file("word/document.xml", options).unwrap();
        zip.write_all(body).unwrap();
        zip.start_file("word/media/image1.png", options).unwrap();
        zip.write_all(include_bytes!("../../../fixtures/photo.png"))
            .unwrap();
        zip.finish().unwrap().into_inner()
    }

    const OPEN: &str = r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body>"#;
    const CLOSE: &str = "</w:body></w:document>";

    #[test]
    fn text_in_cdata_and_references_counts() {
        let words = "Bonjour, ceci est une lettre bien assez longue pour compter comme du texte.";
        let plain = format!("{OPEN}<w:p><w:r><w:t>{words}</w:t></w:r></w:p>{CLOSE}");
        assert_eq!(
            extract_docx(&docx(plain.as_bytes()), &thresholds(), LIMIT),
            None
        );
        let cdata = format!("{OPEN}<w:p><w:r><w:t><![CDATA[{words}]]></w:t></w:r></w:p>{CLOSE}");
        assert_eq!(
            extract_docx(&docx(cdata.as_bytes()), &thresholds(), LIMIT),
            None
        );
        let refs = format!(
            "{OPEN}<w:p><w:r><w:t>{}</w:t></w:r></w:p>{CLOSE}",
            "&amp;".repeat(60)
        );
        assert_eq!(
            extract_docx(&docx(refs.as_bytes()), &thresholds(), LIMIT),
            None
        );
        let empty = format!("{OPEN}<w:p/>{CLOSE}");
        assert!(extract_docx(&docx(empty.as_bytes()), &thresholds(), LIMIT).is_some());
    }

    /// Text past the read limit is unread, so a body cut there between two
    /// elements is not taken for an empty one.
    #[test]
    fn a_body_past_the_limit_is_not_sparse() {
        let padding = "<w:p/>".repeat(XML_LIMIT as usize / 6 + 1);
        let long = format!("{OPEN}{padding}<w:p><w:r><w:t>text</w:t></w:r></w:p>{CLOSE}");
        assert_eq!(
            extract_docx(&docx(long.as_bytes()), &thresholds(), LIMIT),
            None
        );
    }

    #[test]
    fn not_a_zip() {
        assert_eq!(extract_docx(b"PK garbage", &thresholds(), LIMIT), None);
        assert_eq!(extract_odt(b"", &thresholds(), LIMIT), None);
    }
}
