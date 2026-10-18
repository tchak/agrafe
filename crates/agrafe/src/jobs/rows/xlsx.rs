//! XLSX as a streamed package: one sheet, inline strings rather than a
//! shared string table, numbers and booleans as typed cells, dates and
//! datetimes as serial numbers with a cell style pointing at the built-in
//! number formats 14 and 22, so Excel shows them in the user's locale. See
//! DESIGN.md, "Rows".

use std::borrow::Cow;
use std::io::Write;

use jiff::civil;

use super::package::Package;
use super::xml::{push_attr, push_text};
use super::{SourceRows, header};
use crate::jobs::build::{BuildError, Sink};
use crate::source::rows::{Cell, Sheet};

const CONTENT_TYPES: &[u8] = br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/><Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/><Override PartName="/xl/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.styles+xml"/></Types>"#;

const ROOT_RELS: &[u8] = br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>"#;

const WORKBOOK_RELS: &[u8] = br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/></Relationships>"#;

/// Three cell formats: the default, the built-in date format 14 and the
/// built-in datetime format 22, both rendered by Excel in the locale.
const STYLES: &[u8] = br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<styleSheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><fonts count="1"><font><sz val="11"/><name val="Calibri"/></font></fonts><fills count="2"><fill><patternFill patternType="none"/></fill><fill><patternFill patternType="gray125"/></fill></fills><borders count="1"><border><left/><right/><top/><bottom/><diagonal/></border></borders><cellStyleXfs count="1"><xf numFmtId="0" fontId="0" fillId="0" borderId="0"/></cellStyleXfs><cellXfs count="3"><xf numFmtId="0" fontId="0" fillId="0" borderId="0" xfId="0"/><xf numFmtId="14" fontId="0" fillId="0" borderId="0" xfId="0" applyNumberFormat="1"/><xf numFmtId="22" fontId="0" fillId="0" borderId="0" xfId="0" applyNumberFormat="1"/></cellXfs><cellStyles count="1"><cellStyle name="Normal" xfId="0" builtinId="0"/></cellStyles></styleSheet>"#;

/// The `s` attribute of a date and of a datetime cell, by `STYLES`.
const DATE_STYLE: &str = "1";
const DATETIME_STYLE: &str = "2";

/// Day zero of Excel's serial dates.
const EPOCH: civil::Date = civil::Date::constant(1899, 12, 30);

/// Excel takes 1900 for a leap year, so serials before this day are off
/// by one and negative ones do not display at all; such a date is written
/// as text instead, which is what Excel itself does when one is typed.
const FIRST_SERIAL: civil::Date = civil::Date::constant(1900, 3, 1);

pub async fn write(
    sheet: &Sheet,
    rows: &mut SourceRows<'_>,
    sink: &mut Sink,
) -> Result<(), BuildError> {
    let mut package = Package::new(sink);
    package.file("[Content_Types].xml", CONTENT_TYPES).await?;
    package.file("_rels/.rels", ROOT_RELS).await?;
    package.file("xl/workbook.xml", &workbook(sheet)).await?;
    package
        .file("xl/_rels/workbook.xml.rels", WORKBOOK_RELS)
        .await?;
    package.file("xl/styles.xml", STYLES).await?;

    let letters: Vec<String> = (0..sheet.columns.len()).map(column_letters).collect();
    package.begin("xl/worksheets/sheet1.xml").await?;
    package.buf().extend_from_slice(
        br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData>"#,
    );
    push_row(package.buf(), &letters, 1, &header(sheet));
    // The number of the last row written, the header's to begin with.
    let mut r = 1u64;
    loop {
        let buf = package.buf();
        let wrote = rows
            .next_row(|cells| {
                r += 1;
                push_row(buf, &letters, r, cells)
            })
            .await?;
        if wrote.is_none() {
            break;
        }
        package.flush(false).await?;
    }
    package.buf().extend_from_slice(b"</sheetData></worksheet>");
    package.end().await?;
    package.finish().await
}

fn workbook(sheet: &Sheet) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(
        br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name=""#,
    );
    push_attr(&mut out, &sheet.name);
    out.extend_from_slice(br#"" sheetId="1" r:id="rId1"/></sheets></workbook>"#);
    out
}

/// The letters of column `index`, from zero: `A`, `B`, ..., `Z`, `AA`.
pub fn column_letters(index: usize) -> String {
    let mut n = index + 1;
    let mut letters = Vec::new();
    while n > 0 {
        n -= 1;
        letters.push(b'A' + (n % 26) as u8);
        n /= 26;
    }
    letters.reverse();
    String::from_utf8(letters).expect("ascii")
}

/// One `<row>`: every cell but the empty ones, each with its reference.
pub fn push_row(out: &mut Vec<u8>, letters: &[String], r: u64, cells: &[Cell<'_>]) {
    write!(out, "<row r=\"{r}\">").expect("vec");
    for (letters, cell) in letters.iter().zip(cells) {
        let reference = |out: &mut Vec<u8>| {
            out.extend_from_slice(b"<c r=\"");
            out.extend_from_slice(letters.as_bytes());
            write!(out, "{r}\"").expect("vec");
        };
        match cell {
            Cell::Null => {}
            Cell::Bool(b) => {
                reference(out);
                out.extend_from_slice(if *b {
                    b" t=\"b\"><v>1</v></c>"
                } else {
                    b" t=\"b\"><v>0</v></c>"
                });
            }
            Cell::Number(lexeme) => {
                reference(out);
                out.extend_from_slice(b"><v>");
                out.extend_from_slice(lexeme.as_bytes());
                out.extend_from_slice(b"</v></c>");
            }
            Cell::Text(text) => {
                reference(out);
                push_inline_string(out, text);
            }
            Cell::Date(date) => {
                reference(out);
                match date_serial(*date) {
                    Some(serial) => {
                        write!(out, " s=\"{DATE_STYLE}\"><v>{serial}</v></c>").expect("vec");
                    }
                    None => push_inline_string(out, &date.to_string()),
                }
            }
            Cell::DateTime(at) => {
                reference(out);
                match datetime_serial(*at) {
                    Some(serial) => {
                        write!(out, " s=\"{DATETIME_STYLE}\"><v>{serial}</v></c>").expect("vec");
                    }
                    None => push_inline_string(out, &at.to_string()),
                }
            }
        }
    }
    out.extend_from_slice(b"</row>");
}

/// The rest of a text cell after its reference: an inline string, which
/// Excel never evaluates, with its whitespace kept.
fn push_inline_string(out: &mut Vec<u8>, text: &str) {
    out.extend_from_slice(b" t=\"inlineStr\"><is><t xml:space=\"preserve\">");
    push_text(out, &escape_underscores(text));
    out.extend_from_slice(b"</t></is></c>");
}

/// Excel reads `_x0041_` in a string as the character U+0041; a text that
/// happens to contain such a sequence gets its underscore escaped the same
/// way, as `_x005F_`, so it shows as sent.
fn escape_underscores(text: &str) -> Cow<'_, str> {
    let bytes = text.as_bytes();
    let is_escape = |at: usize| {
        bytes.len() >= at + 7
            && bytes[at] == b'_'
            && bytes[at + 1] == b'x'
            && bytes[at + 2..at + 6].iter().all(u8::is_ascii_hexdigit)
            && bytes[at + 6] == b'_'
    };
    if !(0..bytes.len()).any(&is_escape) {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len() + 16);
    for (at, c) in text.char_indices() {
        if c == '_' && is_escape(at) {
            out.push_str("_x005F_");
        } else {
            out.push(c);
        }
    }
    Cow::Owned(out)
}

/// Days since `EPOCH`, for a date Excel's serials can show.
fn date_serial(date: civil::Date) -> Option<i64> {
    if date < FIRST_SERIAL {
        return None;
    }
    let days = EPOCH.until(date).ok()?.get_days();
    Some(i64::from(days))
}

/// The day's serial plus the time as a fraction of it.
fn datetime_serial(at: civil::DateTime) -> Option<f64> {
    let days = date_serial(at.date())? as f64;
    let seconds = f64::from(at.hour()) * 3600.0
        + f64::from(at.minute()) * 60.0
        + f64::from(at.second())
        + f64::from(at.subsec_nanosecond()) / 1e9;
    Some(days + seconds / 86_400.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letters() {
        assert_eq!(column_letters(0), "A");
        assert_eq!(column_letters(25), "Z");
        assert_eq!(column_letters(26), "AA");
        assert_eq!(column_letters(27), "AB");
        assert_eq!(column_letters(701), "ZZ");
        assert_eq!(column_letters(702), "AAA");
        assert_eq!(column_letters(16_383), "XFD", "the last column of a sheet");
    }

    #[test]
    fn serials() {
        assert_eq!(date_serial(civil::date(1900, 3, 1)), Some(61));
        assert_eq!(date_serial(civil::date(1970, 1, 1)), Some(25_569));
        assert_eq!(date_serial(civil::date(2026, 10, 17)), Some(46_312));
        assert_eq!(date_serial(civil::date(1900, 2, 28)), None);
        assert_eq!(date_serial(civil::date(1850, 1, 1)), None);
        assert_eq!(
            datetime_serial(civil::date(2026, 10, 17).at(12, 0, 0, 0)),
            Some(46_312.5)
        );
        assert_eq!(
            datetime_serial(civil::date(2026, 10, 17).at(0, 0, 0, 0)),
            Some(46_312.0)
        );
        let close = datetime_serial(civil::date(2026, 10, 17).at(8, 44, 26, 500_000_000)).unwrap();
        assert!((close - 46_312.364_195_6).abs() < 1e-6, "{close}");
    }

    #[test]
    fn rows() {
        let letters: Vec<String> = (0..7).map(column_letters).collect();
        let mut out = Vec::new();
        push_row(
            &mut out,
            &letters,
            2,
            &[
                Cell::Text("a<b".into()),
                Cell::Null,
                Cell::Number("1.10"),
                Cell::Bool(false),
                Cell::Date(civil::date(2026, 10, 17)),
                Cell::DateTime(civil::date(2026, 10, 17).at(12, 0, 0, 0)),
                Cell::Date(civil::date(1850, 1, 1)),
            ],
        );
        assert_eq!(
            String::from_utf8(out).unwrap(),
            concat!(
                r#"<row r="2">"#,
                r#"<c r="A2" t="inlineStr"><is><t xml:space="preserve">a&lt;b</t></is></c>"#,
                r#"<c r="C2"><v>1.10</v></c>"#,
                r#"<c r="D2" t="b"><v>0</v></c>"#,
                r#"<c r="E2" s="1"><v>46312</v></c>"#,
                r#"<c r="F2" s="2"><v>46312.5</v></c>"#,
                r#"<c r="G2" t="inlineStr"><is><t xml:space="preserve">1850-01-01</t></is></c>"#,
                "</row>"
            )
        );
    }

    #[test]
    fn underscores() {
        assert_eq!(escape_underscores("plain_x"), "plain_x");
        assert_eq!(escape_underscores("_x0041_"), "_x005F_x0041_");
        assert_eq!(escape_underscores("a_xBEEF_b_x12_"), "a_x005F_xBEEF_b_x12_");
        assert_eq!(escape_underscores("é_x00e9_"), "é_x005F_x00e9_");
    }
}
