//! Building a `rows` blob: the source parts read back in order, each line
//! a row of cells against the columns of the sheet it belongs to, written
//! row by row as CSV, XLSX or ODS per the blob's content type, encrypted
//! and uploaded as it is produced through the shared `build` sink. The
//! sheet formats are ZIP packages whose XML is written by hand and
//! deflated as it goes; no spreadsheet library, since the ones available
//! build the workbook in memory. A workbook's sheets are written in order,
//! each consuming the source up to the `{"sheet": "..."}` line that
//! switches to a later one. See [DESIGN.md, "Rows"].
//!
//! [DESIGN.md, "Rows"]: https://github.com/tchak/agrafe/blob/main/DESIGN.md#rows

mod csv;
mod ods;
mod package;
mod xlsx;
mod xml;

use super::build::{self, Assemble, BuildError, Counts, Sink};
use super::{Job, JobError};
use crate::app::Agrafe;
use crate::blob::{Blob, InputType, repo};
use crate::classify;
use crate::source::Source;
use crate::source::rows::{Cell, Line, Sheet, parse_source_line, switch_target};

/// Bytes a writer gathers before handing them on: to the compressor of a
/// package, or to the sink for CSV.
const FLUSH_AT: usize = 256 * 1024;

pub async fn run(state: &Agrafe, job: &Job) -> Result<(), JobError> {
    build::run::<Rows>(state, job).await
}

/// The rows build: the sheets, in the blob's format.
pub(crate) struct Rows;

impl Assemble for Rows {
    const INPUT: InputType = InputType::Rows;

    async fn assemble(state: &Agrafe, blob: &Blob, sink: &mut Sink) -> Result<Counts, BuildError> {
        let sheets = repo::sheets(state.pool(), blob.id)
            .await?
            .ok_or(BuildError::Incomplete {
                blob_id: blob.id,
                what: "columns",
            })?;
        let sheets = sheets.as_slice();
        let mut rows = SourceRows::new(Source::new(state, blob), sheets);
        match blob.content_type.as_str() {
            // A CSV declares one sheet; creation refuses more.
            classify::CSV => csv::write(&sheets[0], &mut rows, sink).await?,
            classify::XLSX => xlsx::write(sheets, &mut rows, sink).await?,
            classify::ODS => ods::write(sheets, &mut rows, sink).await?,
            _ => {
                return Err(BuildError::Incomplete {
                    blob_id: blob.id,
                    what: "spreadsheet content type",
                });
            }
        }
        Ok(Counts::default())
    }
}

/// The source, one parsed row at a time, sheet by sheet: the rows of a
/// sheet end at the source's end or at the line switching to a later
/// sheet, which is held until the writer reaches that sheet.
struct SourceRows<'a> {
    source: Source<'a>,
    sheets: &'a [Sheet],
    /// Index of the sheet whose rows are being read.
    current: usize,
    /// A switch read ahead of its sheet: the index it names. Until the
    /// writer begins that sheet, there are no more rows.
    pending: Option<usize>,
    /// The source is exhausted.
    done: bool,
}

impl<'a> SourceRows<'a> {
    fn new(source: Source<'a>, sheets: &'a [Sheet]) -> Self {
        Self {
            source,
            sheets,
            current: 0,
            pending: None,
            done: false,
        }
    }

    /// The writer is about to emit sheet `index`: its rows follow, unless a
    /// switch read ahead points past it, in which case it has none.
    fn begin_sheet(&mut self, index: usize) {
        self.current = index;
        if self.pending == Some(index) {
            self.pending = None;
        }
    }

    /// The next row's cells of the current sheet, or `None` after its last.
    /// The line is handed to `f` rather than returned, since the cells
    /// borrow it. A switch naming the current sheet is skipped like the
    /// accept pass skips it; holding it as pending would end the sheet and
    /// never clear, leaving every later sheet empty.
    async fn next_row<R>(
        &mut self,
        f: impl FnOnce(&[Cell<'_>]) -> R,
    ) -> Result<Option<R>, BuildError> {
        loop {
            if self.done || self.pending.is_some() {
                return Ok(None);
            }
            let Some(line) = self.source.next_line().await? else {
                self.done = true;
                return Ok(None);
            };
            let sheets = self.sheets;
            let parsed = parse_source_line(&line.bytes, &sheets[self.current].columns)
                .map_err(|reason| self.source.malformed(line.line, reason))?;
            match parsed {
                Line::Switch(name) => {
                    let target = switch_target(sheets, self.current, &name)
                        .map_err(|reason| self.source.malformed(line.line, reason))?;
                    if target != self.current {
                        self.pending = Some(target);
                        return Ok(None);
                    }
                }
                Line::Row(cells) => return Ok(Some(f(&cells))),
            }
        }
    }
}

/// The header row: every column's name as a text cell.
fn header(sheet: &Sheet) -> Vec<Cell<'_>> {
    sheet
        .columns
        .iter()
        .map(|c| Cell::Text(c.name.as_str().into()))
        .collect()
}
