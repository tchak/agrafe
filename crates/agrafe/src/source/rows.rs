//! The source of a `rows` blob: the sheets and their columns declared at
//! creation, and one JSON array of cell values per line, positional
//! against the current sheet's columns, checked as the part arrives. An
//! object line `{"sheet": "..."}` switches to a later sheet. See
//! DESIGN.md, "Rows".

use std::borrow::Cow;

use bytes::Bytes;
use jiff::civil;
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

use super::{LineError, Lines};
use crate::app::Agrafe;
use crate::blob::Blob;
use crate::blob::repo::{PartAccepted, SheetPosition};
use crate::classify;
use crate::text::is_bidi_control;
use crate::upload::{Tap, UploadError};

/// Rows an XLSX or ODS sheet holds at most, the header included.
pub const MAX_SHEET_ROWS: u64 = 1_048_576;

/// Columns an XLSX or ODS sheet holds at most.
pub const MAX_SHEET_COLUMNS: usize = 16_384;

/// Characters a sheet cell holds at most, counted as Excel counts them, in
/// UTF-16 units. Excel repairs a workbook with a longer text, truncating
/// the cell; a header is a cell too.
pub const MAX_SHEET_CELL_LEN: usize = 32_767;

/// Longest name an XLSX sheet may have.
pub const MAX_SHEET_NAME_LEN: usize = 31;

/// The name a sheet gets when nothing of the filename is usable.
pub const DEFAULT_SHEET_NAME: &str = "Sheet1";

/// Whether `content_type` is a sheet format, with its row and column
/// limits, rather than CSV.
pub fn is_sheet(content_type: &str) -> bool {
    matches!(content_type, classify::XLSX | classify::ODS)
}

/// What a typed column's values must be. A column without a type takes
/// what the JSON scalar says.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ColumnType {
    Date,
    Datetime,
}

agrafe_types::str_enum!(ColumnType {
    Date => "date",
    Datetime => "datetime",
});

/// One column, as declared at creation and stored on the row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Column {
    /// The header text.
    pub name: String,
    #[serde(default, rename = "type", skip_serializing_if = "Option::is_none")]
    pub kind: Option<ColumnType>,
}

/// One sheet of the workbook, as declared at creation and stored on the
/// row in `source_columns`, a list of these in workbook order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sheet {
    pub name: String,
    pub columns: Vec<Column>,
}

/// Why a declared sheet name is not one Excel accepts.
pub fn check_sheet_name(name: &str) -> Result<(), &'static str> {
    if name.trim().is_empty() {
        return Err("must not be empty");
    }
    if name.chars().count() > MAX_SHEET_NAME_LEN {
        return Err("is longer than the 31 characters a sheet name holds");
    }
    if name
        .chars()
        .any(|c| matches!(c, '[' | ']' | ':' | '*' | '?' | '/' | '\\'))
    {
        return Err("must not contain any of [ ] : * ? / \\");
    }
    if name.chars().any(|c| c.is_control() || is_bidi_control(c)) {
        return Err("must not contain control characters");
    }
    if name.starts_with('\'') || name.ends_with('\'') {
        return Err("must not start or end with an apostrophe");
    }
    if name.eq_ignore_ascii_case("history") {
        return Err("is reserved by Excel for its change tracking");
    }
    Ok(())
}

/// The index of the sheet a `{"sheet": "..."}` line names, which must come
/// after `current` in workbook order: the source is grouped by sheet, so
/// a sheet is written whole before the next. The message says what is
/// wrong with the switch.
pub fn switch_target(sheets: &[Sheet], current: usize, name: &str) -> Result<usize, String> {
    let Some(index) = sheets.iter().position(|s| s.name == name) else {
        return Err(format!("no sheet named {name:?} is declared"));
    };
    if index == current {
        return Err(format!(
            "sheet {name:?} is already the current sheet; rows before any switch belong to the first sheet"
        ));
    }
    if index < current {
        return Err(format!(
            "sheet {name:?} comes before the current sheet {:?}; a source is grouped by sheet in workbook order",
            sheets[current].name
        ));
    }
    Ok(index)
}

/// The name the sheet of a `rows` blob declared with `columns` gets: the
/// filename's stem, made acceptable to Excel, which caps names at 31
/// characters, forbids `[ ] : * ? / \` and a leading or trailing
/// apostrophe, reserves `History` for its change tracking, and wants
/// something there.
pub fn sheet_name(filename: &str) -> String {
    let stem = match filename.rsplit_once('.') {
        Some((stem, _)) if !stem.is_empty() => stem,
        _ => filename,
    };
    let cleaned: String = stem
        .chars()
        .map(|c| match c {
            '[' | ']' | ':' | '*' | '?' | '/' | '\\' => '_',
            c => c,
        })
        .collect();
    let trimmed = cleaned.trim().trim_matches('\'').trim();
    let name: String = trimmed.chars().take(MAX_SHEET_NAME_LEN).collect();
    let name = name.trim_end().trim_end_matches('\'').trim_end();
    if name.is_empty() || name.eq_ignore_ascii_case("history") {
        DEFAULT_SHEET_NAME.to_string()
    } else {
        name.to_string()
    }
}

/// One cell as the source gave it: a scalar, or an empty cell. It borrows
/// from the line wherever it can, so that accept checks a row without
/// copying it and the build escapes a text straight from the source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cell<'a> {
    Null,
    Bool(bool),
    /// The JSON lexeme, written as given rather than parsed to a float
    /// and back, so `1.10` stays `1.10`.
    Number(&'a str),
    /// The line's own bytes when the JSON string has no escapes, which is
    /// nearly always; decoded into a `String` otherwise.
    Text(Cow<'a, str>),
    Date(civil::Date),
    DateTime(civil::DateTime),
}

/// One line of the source: a row of the current sheet, or a switch to the
/// sheet named.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Line<'a> {
    Row(Vec<Cell<'a>>),
    Switch(String),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SwitchLine {
    sheet: String,
}

/// Parse and check one line: a JSON array of cell values against
/// `columns`, or `{"sheet": "..."}`. The message names what is wrong with
/// it. Which sheet a switch may name is the caller's to check, with
/// [`switch_target`].
pub fn parse_source_line<'a>(line: &'a [u8], columns: &[Column]) -> Result<Line<'a>, String> {
    let first = line.iter().find(|b| !b.is_ascii_whitespace()).copied();
    if first == Some(b'{') {
        let switch: SwitchLine = serde_json::from_slice(line)
            .map_err(|e| format!("expected {{\"sheet\": \"...\"}} to switch sheets: {e}"))?;
        return Ok(Line::Switch(switch.sheet));
    }
    parse_line(line, columns).map(Line::Row)
}

/// Parse and check one row against `columns`. The message names what is
/// wrong with it.
pub fn parse_line<'a>(line: &'a [u8], columns: &[Column]) -> Result<Vec<Cell<'a>>, String> {
    let values: Vec<&'a RawValue> = serde_json::from_slice(line)
        .map_err(|e| format!("expected a JSON array of cell values: {e}"))?;
    if values.len() != columns.len() {
        return Err(format!(
            "expected {} cells, one per declared column, got {}",
            columns.len(),
            values.len()
        ));
    }
    values
        .iter()
        .zip(columns)
        .enumerate()
        .map(|(i, (raw, column))| {
            parse_cell(raw.get(), column.kind)
                .map_err(|reason| format!("cell {} ({}): {reason}", i + 1, column.name))
        })
        .collect()
}

/// Whether `text` fits a sheet cell.
pub fn fits_sheet_cell(text: &str) -> bool {
    // Every UTF-16 unit is at least one byte of UTF-8, so a short text
    // needs no counting.
    text.len() <= MAX_SHEET_CELL_LEN || text.encode_utf16().count() <= MAX_SHEET_CELL_LEN
}

/// The check a sheet format adds to a parsed row: no text longer than a
/// cell holds. The message names the cell, as `parse_line`'s do.
pub fn check_sheet_cells(cells: &[Cell<'_>], columns: &[Column]) -> Result<(), String> {
    for (i, (cell, column)) in cells.iter().zip(columns).enumerate() {
        if let Cell::Text(text) = cell
            && !fits_sheet_cell(text)
        {
            return Err(format!(
                "cell {} ({}): text is longer than the {MAX_SHEET_CELL_LEN} characters a sheet cell holds",
                i + 1,
                column.name
            ));
        }
    }
    Ok(())
}

fn parse_cell(raw: &str, kind: Option<ColumnType>) -> Result<Cell<'_>, String> {
    let expects = |kind: ColumnType| {
        match kind {
            ColumnType::Date => "must be a date as YYYY-MM-DD, or null",
            ColumnType::Datetime => {
                "must be a datetime as YYYY-MM-DDTHH:MM:SS, with an optional fraction and offset, or null"
            }
        }
        .to_string()
    };
    match raw.as_bytes().first() {
        Some(b'n') => Ok(Cell::Null),
        Some(b'"') => {
            let text = decode_string(raw)?;
            match kind {
                None => Ok(Cell::Text(text)),
                Some(ColumnType::Date) => parse_date(&text)
                    .map(Cell::Date)
                    .ok_or_else(|| expects(ColumnType::Date)),
                Some(ColumnType::Datetime) => parse_datetime(&text)
                    .map(Cell::DateTime)
                    .ok_or_else(|| expects(ColumnType::Datetime)),
            }
        }
        Some(b'[') | Some(b'{') => Err(
            "must be a scalar: a string, a number, a boolean or null, not an array or an object"
                .to_string(),
        ),
        Some(b't') | Some(b'f') => match kind {
            None => Ok(Cell::Bool(raw == "true")),
            Some(kind) => Err(expects(kind)),
        },
        _ => match kind {
            Some(kind) => Err(expects(kind)),
            None => {
                let value: f64 = raw.parse().map_err(|_| "is not a number".to_string())?;
                if !value.is_finite() {
                    return Err("is a number too large for a spreadsheet cell".to_string());
                }
                Ok(Cell::Number(raw))
            }
        },
    }
}

/// The content of a JSON string lexeme the array parse has already
/// validated: the slice between its quotes when it holds no escape, which
/// serde guarantees is then the string itself, decoded otherwise.
fn decode_string(raw: &str) -> Result<Cow<'_, str>, String> {
    let inner = &raw[1..raw.len() - 1];
    if inner.contains('\\') {
        serde_json::from_str(raw)
            .map(Cow::Owned)
            .map_err(|e| e.to_string())
    } else {
        Ok(Cow::Borrowed(inner))
    }
}

/// `YYYY-MM-DD`, a valid calendar date.
pub fn parse_date(s: &str) -> Option<civil::Date> {
    let b = s.as_bytes();
    if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    let year = digits(&b[0..4])?;
    let month = digits(&b[5..7])?;
    let day = digits(&b[8..10])?;
    civil::Date::new(year as i16, month as i8, day as i8).ok()
}

/// `YYYY-MM-DDTHH:MM:SS`, an optional fraction of up to nine digits, an
/// optional offset in any ISO 8601 form, `Z`, `±HH`, `±HHMM` or `±HH:MM`,
/// which is dropped: a spreadsheet has no timezone, and what the backend
/// sends is what shows.
pub fn parse_datetime(s: &str) -> Option<civil::DateTime> {
    let (date, rest) = s.split_at_checked(10)?;
    let date = parse_date(date)?;
    let rest = rest.strip_prefix('T')?;
    let b = rest.as_bytes();
    if b.len() < 8 || b[2] != b':' || b[5] != b':' {
        return None;
    }
    let hour = digits(&b[0..2])?;
    let minute = digits(&b[3..5])?;
    let second = digits(&b[6..8])?;
    let mut rest = &rest[8..];
    let mut nanos: i32 = 0;
    if let Some(fraction) = rest.strip_prefix('.') {
        let n = fraction.bytes().take_while(u8::is_ascii_digit).count();
        if n == 0 || n > 9 {
            return None;
        }
        nanos = digits(&fraction.as_bytes()[..n])? as i32 * 10i32.pow(9 - n as u32);
        rest = &fraction[n..];
    }
    match rest {
        "" | "Z" => {}
        offset => {
            let b = offset.as_bytes();
            if !(b[0] == b'+' || b[0] == b'-') {
                return None;
            }
            let (hours, minutes) = match b.len() {
                3 => (&b[1..3], None),
                5 => (&b[1..3], Some(&b[3..5])),
                6 if b[3] == b':' => (&b[1..3], Some(&b[4..6])),
                _ => return None,
            };
            if digits(hours)? > 23 || minutes.map(digits).unwrap_or(Some(0))? > 59 {
                return None;
            }
        }
    }
    civil::DateTime::new(
        date.year(),
        date.month(),
        date.day(),
        hour as i8,
        minute as i8,
        second as i8,
        nanos,
    )
    .ok()
}

/// A run of ASCII digits as a number, up to nine of them.
fn digits(b: &[u8]) -> Option<u32> {
    if b.is_empty() || b.len() > 9 || !b.iter().all(u8::is_ascii_digit) {
        return None;
    }
    Some(b.iter().fold(0u32, |n, d| n * 10 + u32::from(d - b'0')))
}

/// The accept pass over one part of rows: lines parsed as they arrive,
/// against the current sheet's columns, within the sheet's limits. Where
/// the part leaves the source, the sheet and the rows in it, goes on the
/// row for the next part.
pub struct Accept {
    sheets: Vec<Sheet>,
    /// `upload.max_size`, the bound on one part.
    max_part: u64,
    /// Whether the output is a sheet, with its cell length limit.
    sheet: bool,
    /// Data rows a sheet holds at most, when the format has a limit.
    row_limit: Option<u64>,
    /// Index of the sheet the lines go to.
    current: usize,
    /// Lines in the current sheet, the parts before this one included.
    sheet_rows: u64,
    lines: Lines,
    rows: u64,
    bytes: u64,
}

impl Accept {
    pub fn new(state: &Agrafe, blob: &Blob) -> Result<Self, UploadError> {
        let sheets = blob.sheets().ok_or(UploadError::NoColumns)?.to_vec();
        let sheet = is_sheet(&blob.content_type);
        let current = usize::try_from(blob.source_sheet).unwrap_or(0);
        if current >= sheets.len() {
            return Err(UploadError::NoColumns);
        }
        Ok(Self {
            sheets,
            max_part: state.config().upload.max_size.as_u64(),
            sheet,
            // The header takes one row of the sheet.
            row_limit: sheet.then_some(MAX_SHEET_ROWS - 1),
            current,
            sheet_rows: blob.source_sheet_rows.max(0) as u64,
            lines: Lines::new(),
            rows: 0,
            bytes: 0,
        })
    }
}

impl Tap for Accept {
    type Output = PartAccepted;

    async fn chunk(&mut self, chunk: &Bytes) -> Result<(), UploadError> {
        self.bytes += chunk.len() as u64;
        if self.bytes > self.max_part {
            return Err(UploadError::PartTooLarge(self.max_part));
        }
        self.lines.push(chunk);
        while let Some((line, bytes)) = self.lines.next_line()? {
            let columns = &self.sheets[self.current].columns;
            let cells = match parse_source_line(&bytes, columns)
                .map_err(|reason| LineError { line, reason })?
            {
                Line::Switch(name) => {
                    self.current = switch_target(&self.sheets, self.current, &name)
                        .map_err(|reason| LineError { line, reason })?;
                    self.sheet_rows = 0;
                    continue;
                }
                Line::Row(cells) => cells,
            };
            if self.sheet {
                check_sheet_cells(&cells, columns).map_err(|reason| LineError { line, reason })?;
            }
            self.rows += 1;
            self.sheet_rows += 1;
            if let Some(limit) = self.row_limit
                && self.sheet_rows > limit
            {
                return Err(LineError {
                    line,
                    reason: format!(
                        "sheet {:?} would exceed {MAX_SHEET_ROWS} rows, the header included",
                        self.sheets[self.current].name
                    ),
                }
                .into());
            }
        }
        Ok(())
    }

    async fn end(mut self) -> Result<PartAccepted, UploadError> {
        self.lines.end()?;
        Ok(PartAccepted {
            rows: self.rows,
            bytes: self.bytes,
            size: None,
            sheet: Some(SheetPosition {
                index: self.current as u32,
                rows: self.sheet_rows,
            }),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn columns(spec: &[(&str, Option<ColumnType>)]) -> Vec<Column> {
        spec.iter()
            .map(|(name, kind)| Column {
                name: name.to_string(),
                kind: *kind,
            })
            .collect()
    }

    #[test]
    fn lines_are_positional_arrays_of_scalars() {
        let cols = columns(&[
            ("name", None),
            ("n", None),
            ("ok", None),
            ("day", Some(ColumnType::Date)),
            ("at", Some(ColumnType::Datetime)),
        ]);
        let cells = parse_line(
            br#"["a\"b", 1.10, true, "2026-10-17", "2026-10-17T08:44:26.5+02:00"]"#,
            &cols,
        )
        .unwrap();
        assert_eq!(
            cells,
            vec![
                Cell::Text("a\"b".into()),
                Cell::Number("1.10"),
                Cell::Bool(true),
                Cell::Date(civil::date(2026, 10, 17)),
                Cell::DateTime(civil::date(2026, 10, 17).at(8, 44, 26, 500_000_000)),
            ]
        );
        // A string without escapes is the line's own bytes.
        assert!(matches!(&cells[0], Cell::Text(Cow::Owned(_))));
        let cells = parse_line(r#"["plain é", 1, true, null, null]"#.as_bytes(), &cols).unwrap();
        assert!(matches!(&cells[0], Cell::Text(Cow::Borrowed("plain é"))));
        let cells = parse_line(br#"[null, -0.5e3, false, null, null]"#, &cols).unwrap();
        assert_eq!(cells[0], Cell::Null);
        assert_eq!(cells[1], Cell::Number("-0.5e3"));
        assert_eq!(cells[2], Cell::Bool(false));
        assert_eq!(cells[3], Cell::Null);

        for (bad, says) in [
            (&br#"{"name": "a"}"#[..], "array"),
            (b"nope", "array"),
            (br#"["a", 1, true, null]"#, "expected 5 cells"),
            (br#"["a", 1, true, null, null, 1]"#, "got 6"),
            (
                br#"[["a"], 1, true, null, null]"#,
                "cell 1 (name): must be a scalar",
            ),
            (br#"[{}, 1, true, null, null]"#, "not an array or an object"),
            (
                br#"["a", 1e999, true, null, null]"#,
                "cell 2 (n): is a number too large",
            ),
            (
                br#"["a", 1, true, "17/10/2026", null]"#,
                "cell 4 (day): must be a date",
            ),
            (
                br#"["a", 1, true, 20261017, null]"#,
                "cell 4 (day): must be a date",
            ),
            (br#"["a", 1, true, true, null]"#, "cell 4 (day)"),
            (
                br#"["a", 1, true, null, "2026-10-17"]"#,
                "cell 5 (at): must be a datetime",
            ),
            (
                br#"["a", 1, true, null, "2026-10-17 08:44:26"]"#,
                "cell 5 (at)",
            ),
        ] {
            let err = parse_line(bad, &cols).unwrap_err();
            assert!(err.contains(says), "{bad:?}: {err}");
        }
    }

    #[test]
    fn sheet_cells_hold_so_much_text() {
        let cols = columns(&[("a", None), ("b", None)]);
        let fits = "x".repeat(MAX_SHEET_CELL_LEN);
        let cells = vec![Cell::Text(fits.as_str().into()), Cell::Number("1")];
        assert_eq!(check_sheet_cells(&cells, &cols), Ok(()));
        let cells = vec![Cell::Null, Cell::Text(format!("{fits}x").into())];
        let err = check_sheet_cells(&cells, &cols).unwrap_err();
        assert!(err.starts_with("cell 2 (b): text is longer"), "{err}");
        // Counted as Excel counts, in UTF-16 units: an emoji is two.
        assert!(fits_sheet_cell(&"é".repeat(MAX_SHEET_CELL_LEN)));
        assert!(!fits_sheet_cell(&"😀".repeat(MAX_SHEET_CELL_LEN / 2 + 1)));
    }

    #[test]
    fn dates() {
        assert_eq!(parse_date("2026-10-17"), Some(civil::date(2026, 10, 17)));
        assert_eq!(parse_date("2024-02-29"), Some(civil::date(2024, 2, 29)));
        for bad in [
            "2023-02-29",
            "2026-13-01",
            "2026-1-01",
            "20261017",
            "2026-10-17T00:00:00",
            "",
            "abcd-ef-gh",
        ] {
            assert_eq!(parse_date(bad), None, "{bad}");
        }
    }

    #[test]
    fn datetimes() {
        let noon = civil::date(2026, 10, 17).at(12, 0, 0, 0);
        assert_eq!(parse_datetime("2026-10-17T12:00:00"), Some(noon));
        assert_eq!(parse_datetime("2026-10-17T12:00:00Z"), Some(noon));
        assert_eq!(parse_datetime("2026-10-17T12:00:00+02:00"), Some(noon));
        assert_eq!(parse_datetime("2026-10-17T12:00:00-05:30"), Some(noon));
        assert_eq!(parse_datetime("2026-10-17T12:00:00+0200"), Some(noon));
        assert_eq!(parse_datetime("2026-10-17T12:00:00+02"), Some(noon));
        assert_eq!(
            parse_datetime("2026-10-17T12:00:00.5-05"),
            Some(civil::date(2026, 10, 17).at(12, 0, 0, 500_000_000))
        );
        assert_eq!(
            parse_datetime("2026-10-17T12:00:00.123456789"),
            Some(civil::date(2026, 10, 17).at(12, 0, 0, 123_456_789))
        );
        assert_eq!(
            parse_datetime("2026-10-17T12:00:00.5Z"),
            Some(civil::date(2026, 10, 17).at(12, 0, 0, 500_000_000))
        );
        for bad in [
            "2026-10-17",
            "2026-10-17 12:00:00",
            "2026-10-17T12:00",
            "2026-10-17T24:00:00",
            "2026-10-17T12:60:00",
            "2026-10-17T12:00:00.",
            "2026-10-17T12:00:00.1234567890",
            "2026-10-17T12:00:00+2:00",
            "2026-10-17T12:00:00+2",
            "2026-10-17T12:00:00+020",
            "2026-10-17T12:00:00+02:0",
            "2026-10-17T12:00:00+24:00",
            "2026-10-17T12:00:00+2400",
            "2026-10-17T12:00:00+24",
            "2026-10-17T12:00:00+0260",
            "2026-10-17T12:00:00+",
            "2026-10-17T12:00:00z",
            "2026-10-17T12:00:00Zx",
        ] {
            assert_eq!(parse_datetime(bad), None, "{bad}");
        }
    }

    #[test]
    fn sheet_names() {
        assert_eq!(sheet_name("export.xlsx"), "export");
        assert_eq!(sheet_name("dossiers 2026.ods"), "dossiers 2026");
        assert_eq!(sheet_name("export"), "export");
        assert_eq!(sheet_name(".xlsx"), ".xlsx");
        assert_eq!(sheet_name("a:b[c]*d?e.xlsx"), "a_b_c__d_e");
        assert_eq!(sheet_name("'quoted'.xlsx"), "quoted");
        assert_eq!(sheet_name("'.xlsx"), DEFAULT_SHEET_NAME);
        assert_eq!(sheet_name("???.xlsx"), "___");
        assert_eq!(sheet_name("History.xlsx"), DEFAULT_SHEET_NAME);
        assert_eq!(sheet_name("history.ods"), DEFAULT_SHEET_NAME);
        assert_eq!(sheet_name("Historique.xlsx"), "Historique");
        let long = format!("{}.xlsx", "é".repeat(40));
        assert_eq!(sheet_name(&long).chars().count(), MAX_SHEET_NAME_LEN);
    }

    #[test]
    fn declared_sheet_names() {
        assert_eq!(check_sheet_name("Dossiers 2026"), Ok(()));
        assert_eq!(check_sheet_name("été"), Ok(()));
        assert_eq!(check_sheet_name("Historique"), Ok(()));
        assert_eq!(check_sheet_name(&"x".repeat(MAX_SHEET_NAME_LEN)), Ok(()));
        for (bad, says) in [
            ("", "empty"),
            ("   ", "empty"),
            ("x".repeat(MAX_SHEET_NAME_LEN + 1).as_str(), "31 characters"),
            ("a:b", "[ ] : * ? / \\"),
            ("a/b", "[ ] : * ? / \\"),
            ("a\\b", "[ ] : * ? / \\"),
            ("[a]", "[ ] : * ? / \\"),
            ("a*", "[ ] : * ? / \\"),
            ("a?", "[ ] : * ? / \\"),
            ("a\nb", "control"),
            ("a\u{202E}b", "control"),
            ("'quoted'", "apostrophe"),
            ("'lead", "apostrophe"),
            ("trail'", "apostrophe"),
            ("History", "reserved"),
            ("history", "reserved"),
        ] {
            let err = check_sheet_name(bad).unwrap_err();
            assert!(err.contains(says), "{bad:?}: {err}");
        }
    }

    fn sheets(names: &[&str]) -> Vec<Sheet> {
        names
            .iter()
            .map(|name| Sheet {
                name: name.to_string(),
                columns: columns(&[("a", None)]),
            })
            .collect()
    }

    #[test]
    fn switches_go_forward() {
        let sheets = sheets(&["one", "two", "three"]);
        assert_eq!(switch_target(&sheets, 0, "two"), Ok(1));
        assert_eq!(switch_target(&sheets, 0, "three"), Ok(2));
        assert_eq!(switch_target(&sheets, 1, "three"), Ok(2));
        let err = switch_target(&sheets, 0, "one").unwrap_err();
        assert!(err.contains("already the current sheet"), "{err}");
        let err = switch_target(&sheets, 2, "one").unwrap_err();
        assert!(
            err.contains("comes before the current sheet \"three\""),
            "{err}"
        );
        let err = switch_target(&sheets, 0, "Two").unwrap_err();
        assert!(err.contains("no sheet named \"Two\""), "{err}");
    }

    #[test]
    fn source_lines_are_rows_or_switches() {
        let cols = columns(&[("a", None)]);
        assert_eq!(
            parse_source_line(br#"["x"]"#, &cols),
            Ok(Line::Row(vec![Cell::Text("x".into())]))
        );
        assert_eq!(
            parse_source_line(br#"{"sheet": "two"}"#, &cols),
            Ok(Line::Switch("two".into()))
        );
        assert_eq!(
            parse_source_line(r#"  {"sheet":"t\u00e9"}"#.as_bytes(), &cols),
            Ok(Line::Switch("té".into()))
        );
        for (bad, says) in [
            (&br#"{"sheet": 1}"#[..], "to switch sheets"),
            (br#"{"sheet": "two", "rows": 1}"#, "to switch sheets"),
            (br#"{"name": "two"}"#, "to switch sheets"),
            (br#"{}"#, "to switch sheets"),
            (br#"nope"#, "expected a JSON array"),
        ] {
            let err = parse_source_line(bad, &cols).unwrap_err();
            assert!(err.contains(says), "{bad:?}: {err}");
        }
    }

    #[test]
    fn columns_serialize_without_an_absent_type() {
        let sheet = Sheet {
            name: "s".into(),
            columns: columns(&[("a", None), ("b", Some(ColumnType::Date))]),
        };
        assert_eq!(
            serde_json::to_string(&sheet).unwrap(),
            r#"{"name":"s","columns":[{"name":"a"},{"name":"b","type":"date"}]}"#
        );
        let parsed: Column = serde_json::from_str(r#"{"name":"x","type":"datetime"}"#).unwrap();
        assert_eq!(parsed.kind, Some(ColumnType::Datetime));
        assert!(serde_json::from_str::<Column>(r#"{"name":"x","type":"time"}"#).is_err());
        assert!(serde_json::from_str::<Column>(r#"{"name":"x","width":3}"#).is_err());
        assert!(
            serde_json::from_str::<Sheet>(r#"{"name":"s","columns":[{"name":"a"}],"x":1}"#)
                .is_err()
        );
    }
}
