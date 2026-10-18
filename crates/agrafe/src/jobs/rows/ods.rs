//! ODS as a streamed package: `mimetype` first and stored as the format
//! requires, then the manifest and a streamed `content.xml` holding the
//! sheets as tables in sequence, with typed cells, the typed value in its
//! attribute and the displayed text alongside. See DESIGN.md, "Rows".

use std::io::Write;

use super::package::Package;
use super::xml::{push_attr, push_text};
use super::{SourceRows, header};
use crate::classify;
use crate::jobs::build::{BuildError, Sink};
use crate::source::rows::{Cell, Sheet};

const MANIFEST: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
<manifest:manifest xmlns:manifest="urn:oasis:names:tc:opendocument:xmlns:manifest:1.0" manifest:version="1.2"><manifest:file-entry manifest:full-path="/" manifest:version="1.2" manifest:media-type="application/vnd.oasis.opendocument.spreadsheet"/><manifest:file-entry manifest:full-path="content.xml" manifest:media-type="text/xml"/></manifest:manifest>"#;

pub async fn write(
    sheets: &[Sheet],
    rows: &mut SourceRows<'_>,
    sink: &mut Sink,
) -> Result<(), BuildError> {
    let mut package = Package::new(sink);
    package.stored("mimetype", classify::ODS.as_bytes()).await?;
    package.file("META-INF/manifest.xml", MANIFEST).await?;

    package.begin("content.xml").await?;
    package.buf().extend_from_slice(
        br#"<?xml version="1.0" encoding="UTF-8"?>
<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" office:version="1.2"><office:body><office:spreadsheet>"#,
    );
    for (index, sheet) in sheets.iter().enumerate() {
        rows.begin_sheet(index);
        let buf = package.buf();
        buf.extend_from_slice(b"<table:table table:name=\"");
        push_attr(buf, &sheet.name);
        write!(
            buf,
            "\"><table:table-column table:number-columns-repeated=\"{}\"/>",
            sheet.columns.len()
        )
        .expect("vec");
        push_row(buf, &header(sheet));
        while rows
            .next_row(|cells| push_row(package.buf(), cells))
            .await?
            .is_some()
        {
            package.flush(false).await?;
        }
        package.buf().extend_from_slice(b"</table:table>");
    }
    package
        .buf()
        .extend_from_slice(b"</office:spreadsheet></office:body></office:document-content>");
    package.end().await?;
    package.finish().await
}

/// One `<table:table-row>`, every cell typed.
pub fn push_row(out: &mut Vec<u8>, cells: &[Cell<'_>]) {
    out.extend_from_slice(b"<table:table-row>");
    for cell in cells {
        match cell {
            Cell::Null => out.extend_from_slice(b"<table:table-cell/>"),
            Cell::Bool(b) => {
                let text = if *b { "true" } else { "false" };
                write!(
                    out,
                    "<table:table-cell office:value-type=\"boolean\" office:boolean-value=\"{text}\"><text:p>{text}</text:p></table:table-cell>"
                )
                .expect("vec");
            }
            Cell::Number(lexeme) => {
                write!(
                    out,
                    "<table:table-cell office:value-type=\"float\" office:value=\"{lexeme}\"><text:p>{lexeme}</text:p></table:table-cell>"
                )
                .expect("vec");
            }
            Cell::Text(text) => {
                out.extend_from_slice(b"<table:table-cell office:value-type=\"string\">");
                push_paragraphs(out, text);
                out.extend_from_slice(b"</table:table-cell>");
            }
            Cell::Date(date) => push_date(out, &date.to_string()),
            Cell::DateTime(at) => push_date(out, &at.to_string()),
        }
    }
    out.extend_from_slice(b"</table:table-row>");
}

/// A date cell, with the ISO string shown so a viewer without a data
/// style still shows something sensible.
fn push_date(out: &mut Vec<u8>, iso: &str) {
    write!(
        out,
        "<table:table-cell office:value-type=\"date\" office:date-value=\"{iso}\"><text:p>{iso}</text:p></table:table-cell>"
    )
    .expect("vec");
}

/// `text` as paragraphs: one per line, with tabs and spaces marked up
/// wherever ODF would collapse them otherwise: a single space between two
/// characters is the only one kept as it is. This is what LibreOffice
/// writes.
fn push_paragraphs(out: &mut Vec<u8>, text: &str) {
    for line in text.split('\n') {
        out.extend_from_slice(b"<text:p>");
        let line = line.strip_suffix('\r').unwrap_or(line);
        // Whether the last character was a space, or there was none yet:
        // a leading space collapses too.
        let mut after_space = true;
        let mut run = String::new();
        let flush = |out: &mut Vec<u8>, run: &mut String| {
            if !run.is_empty() {
                push_text(out, run);
                run.clear();
            }
        };
        let mut chars = line.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '\t' => {
                    flush(out, &mut run);
                    out.extend_from_slice(b"<text:tab/>");
                    after_space = true;
                }
                ' ' => {
                    let mut count = 1;
                    while chars.peek() == Some(&' ') {
                        chars.next();
                        count += 1;
                    }
                    // The first space of a run between two characters is a
                    // literal; a leading or trailing run, one after a tab
                    // and the rest of a longer one are elements.
                    if !after_space && chars.peek().is_some() {
                        run.push(' ');
                        count -= 1;
                    }
                    if count > 0 {
                        flush(out, &mut run);
                        if count == 1 {
                            out.extend_from_slice(b"<text:s/>");
                        } else {
                            write!(out, "<text:s text:c=\"{count}\"/>").expect("vec");
                        }
                    }
                    after_space = true;
                }
                c => {
                    run.push(c);
                    after_space = false;
                }
            }
        }
        flush(out, &mut run);
        out.extend_from_slice(b"</text:p>");
    }
}

#[cfg(test)]
mod tests {
    use jiff::civil;

    use super::*;

    fn row(cells: &[Cell<'_>]) -> String {
        let mut out = Vec::new();
        push_row(&mut out, cells);
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn typed_cells() {
        assert_eq!(
            row(&[
                Cell::Text("a&b".into()),
                Cell::Null,
                Cell::Number("1.10"),
                Cell::Bool(true),
                Cell::Date(civil::date(2026, 10, 17)),
                Cell::DateTime(civil::date(2026, 10, 17).at(8, 44, 26, 0)),
            ]),
            concat!(
                "<table:table-row>",
                r#"<table:table-cell office:value-type="string"><text:p>a&amp;b</text:p></table:table-cell>"#,
                "<table:table-cell/>",
                r#"<table:table-cell office:value-type="float" office:value="1.10"><text:p>1.10</text:p></table:table-cell>"#,
                r#"<table:table-cell office:value-type="boolean" office:boolean-value="true"><text:p>true</text:p></table:table-cell>"#,
                r#"<table:table-cell office:value-type="date" office:date-value="2026-10-17"><text:p>2026-10-17</text:p></table:table-cell>"#,
                r#"<table:table-cell office:value-type="date" office:date-value="2026-10-17T08:44:26"><text:p>2026-10-17T08:44:26</text:p></table:table-cell>"#,
                "</table:table-row>"
            )
        );
    }

    #[test]
    fn paragraphs_keep_whitespace() {
        let text = |s: &str| {
            let mut out = Vec::new();
            push_paragraphs(&mut out, s);
            String::from_utf8(out).unwrap()
        };
        assert_eq!(text("one two"), "<text:p>one two</text:p>");
        assert_eq!(
            text("line 1\nline 2\r\n"),
            "<text:p>line 1</text:p><text:p>line 2</text:p><text:p></text:p>"
        );
        assert_eq!(
            text("a  b   c"),
            r#"<text:p>a <text:s/>b <text:s text:c="2"/>c</text:p>"#
        );
        assert_eq!(
            text("  lead"),
            r#"<text:p><text:s text:c="2"/>lead</text:p>"#
        );
        assert_eq!(text(" x"), "<text:p><text:s/>x</text:p>");
        assert_eq!(text("a\tb"), "<text:p>a<text:tab/>b</text:p>");
        assert_eq!(text("a \tb"), "<text:p>a <text:tab/>b</text:p>");
        assert_eq!(text("a\t b"), "<text:p>a<text:tab/><text:s/>b</text:p>");
        assert_eq!(text("trail "), "<text:p>trail<text:s/></text:p>");
        assert_eq!(
            text("trail   "),
            r#"<text:p>trail<text:s text:c="3"/></text:p>"#
        );
        assert_eq!(text(" "), "<text:p><text:s/></text:p>");
        assert_eq!(text(""), "<text:p></text:p>");
    }
}
