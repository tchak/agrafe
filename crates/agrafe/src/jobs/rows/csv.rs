//! CSV per RFC 4180: comma separated, CRLF, a field quoted when it needs
//! to be, UTF-8 with a byte order mark so Excel reads the accents. Text
//! that a spreadsheet would evaluate as a formula is prefixed with a
//! single quote. See [DESIGN.md, "Rows"].
//!
//! [DESIGN.md, "Rows"]: https://github.com/tchak/agrafe/blob/main/DESIGN.md#rows

use super::{FLUSH_AT, SourceRows, header};
use crate::jobs::build::{BuildError, Sink};
use crate::source::rows::{Cell, Sheet};

const BOM: &[u8] = b"\xEF\xBB\xBF";

pub async fn write(
    sheet: &Sheet,
    rows: &mut SourceRows<'_>,
    sink: &mut Sink,
) -> Result<(), BuildError> {
    let mut out = Vec::with_capacity(2 * FLUSH_AT);
    out.extend_from_slice(BOM);
    push_row(&mut out, &header(sheet));
    while rows
        .next_row(|cells| push_row(&mut out, cells))
        .await?
        .is_some()
    {
        if out.len() >= FLUSH_AT {
            sink.write(&out).await?;
            out.clear();
        }
    }
    sink.write(&out).await
}

/// One record: the fields, comma separated, and CRLF.
pub fn push_row(out: &mut Vec<u8>, cells: &[Cell<'_>]) {
    for (i, cell) in cells.iter().enumerate() {
        if i > 0 {
            out.push(b',');
        }
        match cell {
            Cell::Null => {}
            Cell::Bool(true) => out.extend_from_slice(b"true"),
            Cell::Bool(false) => out.extend_from_slice(b"false"),
            Cell::Number(lexeme) => out.extend_from_slice(lexeme.as_bytes()),
            Cell::Text(text) => push_text(out, text),
            Cell::Date(date) => out.extend_from_slice(date.to_string().as_bytes()),
            Cell::DateTime(at) => out.extend_from_slice(at.to_string().as_bytes()),
        }
    }
    out.extend_from_slice(b"\r\n");
}

/// A text field: quoted when it holds a quote, a comma or a line break,
/// with quotes doubled; prefixed with `'` when a spreadsheet opening the
/// file would take it for a formula, so a backend exporting user-entered
/// text cannot ship `=HYPERLINK(...)` to an agent's desktop.
fn push_text(out: &mut Vec<u8>, text: &str) {
    let formula = text.starts_with(['=', '+', '-', '@', '\t', '\r']);
    let quote = text.contains(['"', ',', '\r', '\n']);
    if quote {
        out.push(b'"');
    }
    if formula {
        out.push(b'\'');
    }
    if text.contains('"') {
        out.extend_from_slice(text.replace('"', "\"\"").as_bytes());
    } else {
        out.extend_from_slice(text.as_bytes());
    }
    if quote {
        out.push(b'"');
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
    fn records() {
        assert_eq!(
            row(&[
                Cell::Text("plain".into()),
                Cell::Null,
                Cell::Number("1.10"),
                Cell::Bool(true),
                Cell::Date(civil::date(2026, 10, 17)),
                Cell::DateTime(civil::date(2026, 10, 17).at(8, 44, 26, 500_000_000)),
            ]),
            "plain,,1.10,true,2026-10-17,2026-10-17T08:44:26.5\r\n"
        );
        assert_eq!(
            row(&[
                Cell::Text("a,b".into()),
                Cell::Text("say \"hi\"".into()),
                Cell::Text("two\nlines".into()),
                Cell::Text(" spaced ".into()),
                Cell::Text("été".into()),
            ]),
            "\"a,b\",\"say \"\"hi\"\"\",\"two\nlines\", spaced ,été\r\n"
        );
    }

    #[test]
    fn formulas_are_neutralised() {
        assert_eq!(
            row(&[Cell::Text("=HYPERLINK(\"x\")".into())]),
            "\"'=HYPERLINK(\"\"x\"\")\"\r\n"
        );
        assert_eq!(row(&[Cell::Text("+1".into())]), "'+1\r\n");
        assert_eq!(row(&[Cell::Text("-1".into())]), "'-1\r\n");
        assert_eq!(row(&[Cell::Text("@cmd".into())]), "'@cmd\r\n");
        assert_eq!(row(&[Cell::Text("\tx".into())]), "'\tx\r\n");
        assert_eq!(row(&[Cell::Text("\rx".into())]), "\"'\rx\"\r\n");
        assert_eq!(row(&[Cell::Number("-1")]), "-1\r\n", "numbers are bare");
    }

    #[test]
    fn empty_row() {
        assert_eq!(row(&[]), "\r\n");
        assert_eq!(row(&[Cell::Null, Cell::Null]), ",\r\n");
    }
}
