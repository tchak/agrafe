//! Building a `rows` blob: the source parts read back in order, each line
//! a row of cells against the columns declared at creation, written row
//! by row as CSV, XLSX or ODS per the blob's content type, encrypted and
//! uploaded as it is produced through the shared `build` sink. The sheet
//! formats are ZIP packages whose XML is written by hand and deflated as
//! it goes; no spreadsheet library, since the ones available build the
//! workbook in memory. See DESIGN.md, "Rows".

mod csv;
mod ods;
mod package;
mod xlsx;
mod xml;

use super::build::{self, Assemble, BuildError, Counts, Sink};
use super::{Job, JobError};
use crate::app::Agrafe;
use crate::blob::{Blob, InputType};
use crate::classify;
use crate::source::Source;
use crate::source::rows::{Cell, Sheet, parse_line};

/// Bytes a writer gathers before handing them on: to the compressor of a
/// package, or to the sink for CSV.
const FLUSH_AT: usize = 256 * 1024;

pub async fn run(state: &Agrafe, job: &Job) -> Result<(), JobError> {
    build::run::<Rows>(state, job).await
}

/// The rows build: the one sheet, in the blob's format.
pub(crate) struct Rows;

impl Assemble for Rows {
    const INPUT: InputType = InputType::Rows;

    async fn assemble(state: &Agrafe, blob: &Blob, sink: &mut Sink) -> Result<Counts, BuildError> {
        let sheet = blob.sheet().ok_or(BuildError::Incomplete {
            blob_id: blob.id,
            what: "columns",
        })?;
        let mut rows = SourceRows {
            source: Source::new(state, blob),
            sheet,
        };
        match blob.content_type.as_str() {
            classify::CSV => csv::write(sheet, &mut rows, sink).await?,
            classify::XLSX => xlsx::write(sheet, &mut rows, sink).await?,
            classify::ODS => ods::write(sheet, &mut rows, sink).await?,
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

/// The source, one parsed row at a time.
struct SourceRows<'a> {
    source: Source<'a>,
    sheet: &'a Sheet,
}

impl SourceRows<'_> {
    /// The next row's cells, or `None` after the last. The line is handed
    /// to `f` rather than returned, since the cells borrow it.
    async fn next_row<R>(
        &mut self,
        f: impl FnOnce(&[Cell<'_>]) -> R,
    ) -> Result<Option<R>, BuildError> {
        let Some(line) = self.source.next_line().await? else {
            return Ok(None);
        };
        let cells = parse_line(&line.bytes, &self.sheet.columns)
            .map_err(|reason| self.source.malformed(line.line, reason))?;
        Ok(Some(f(&cells)))
    }
}

/// The header row: every column's name as a text cell.
fn header(sheet: &Sheet) -> Vec<Cell<'static>> {
    sheet
        .columns
        .iter()
        .map(|c| Cell::Text(c.name.clone()))
        .collect()
}
