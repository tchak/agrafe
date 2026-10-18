-- Multi-sheet workbooks: a `rows` blob may declare several sheets, and its
-- source switches between them with `{"sheet": "..."}` lines. The row
-- limit is per sheet, so accept carries where it is across parts. See
-- DESIGN.md, "Rows".
ALTER TABLE blobs
    -- Index, from zero, of the sheet the source is currently filling.
    ADD COLUMN source_sheet      integer NOT NULL DEFAULT 0 CHECK (source_sheet >= 0),
    -- Lines accepted into that sheet so far; `source_rows` stays the total.
    ADD COLUMN source_sheet_rows bigint  NOT NULL DEFAULT 0 CHECK (source_sheet_rows >= 0);

-- A `rows` blob mid-upload holds one sheet, whose rows so far are all of
-- `source_rows`; without this its limit would start over.
UPDATE blobs SET source_sheet_rows = source_rows WHERE input_type = 'rows';
