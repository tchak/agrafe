# Spreadsheets

A `rows` blob is a spreadsheet agrafe builds for you. Declare the columns,
send the rows as JSONL, and a job streams a CSV, XLSX or ODS file into the
bucket. Nothing is held whole in memory, so an export of a million rows
costs the same as one of a hundred.

## Create and send

```sh
SHEET=$(curl -s localhost:8080/blobs \
  -H 'content-type: application/json' \
  -d '{"filename": "users.xlsx",
       "content_type": "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
       "input_type": "rows",
       "columns": [{"name": "Name"}, {"name": "Signed up", "type": "date"}, {"name": "Active"}]}' \
  | jq -r .id)

printf '%s\n' '["Ada", "2026-01-15", true]' '["Grace", "2026-02-03", false]' \
  | curl -s -X PUT "localhost:8080/blobs/$SHEET" --data-binary @- | jq .state
# "uploaded"
```

The `PUT` validates every line and answers once the source is stored. The
blob is then `uploaded`, and a `rows` job builds the file. It becomes
`ready` when the build is done; the [webhook](webhooks.md) reports it, or
poll `GET /blobs/:id`.

The output format is the blob's `content_type`:

| Content type | Format |
|---|---|
| `text/csv` | CSV |
| `application/vnd.openxmlformats-officedocument.spreadsheetml.sheet` | XLSX |
| `application/vnd.oasis.opendocument.spreadsheet` | ODS |

Any other content type is refused at creation.

## Columns

A column is `{"name": "...", "type": "..."}`. The name is the header text:
any non-empty string without control or bidirectional formatting
characters, duplicates allowed. The type is optional.

Each line of the source is a JSON array with exactly one value per column,
in order. A value is a string, a number, a boolean or `null`, which is an
empty cell. Objects and arrays as values are refused.

| Column type | Accepted values | Written as |
|---|---|---|
| none | any scalar | what the JSON says: a number is numeric, a boolean is boolean, a string is text |
| `date` | `null` or `YYYY-MM-DD` | a date cell |
| `datetime` | `null` or `YYYY-MM-DDTHH:MM:SS`, optional fraction and offset (`Z`, `±HH`, `±HHMM`, `±HH:MM`) | a date and time cell, the offset dropped |

Spreadsheets have no time zones, so a datetime is written as the wall-clock
time given. Format it in the zone your users read.

Dates are never guessed from the shape of a string: a free-text value that
looks like a date stays text. Numbers are written as their JSON lexeme, so
`1.10` stays `1.10`; Excel keeps fifteen significant digits, so send 64-bit
identifiers as strings. A number that does not fit a double, such as
`1e999`, is refused.

There are no other types. Currencies, percentages and durations are display
formats, and a backend that wants `Oui` rather than `VRAI` sends the string.

## Several sheets

An XLSX or ODS workbook can hold several sheets. Declare `sheets` instead of
`columns`:

```json
{
  "filename": "users.xlsx",
  "content_type": "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
  "input_type": "rows",
  "sheets": [
    {"name": "Users", "columns": [{"name": "Name"}, {"name": "Signed up", "type": "date"}]},
    {"name": "Teams", "columns": [{"name": "Team"}, {"name": "Members"}]}
  ]
}
```

Send the rows of the first sheet, then a `{"sheet": "..."}` line before the
rows of each following sheet, in the declared order:

```jsonl
["Ada", "2026-01-15"]
["Grace", "2026-02-03"]
{"sheet": "Teams"}
["Compilers", 2]
```

A switch must name the current sheet or a declared sheet after it. Naming the
current sheet changes nothing, so a loop may announce every sheet, the first
included, before its rows. A sheet no switch points at gets its header alone.

Sheet names follow Excel's rules, since Excel refuses to open a workbook
that breaks them: not blank, at most 31 characters, none of `[ ] : * ? / \`,
no control characters, not starting or ending with an apostrophe, not
`History`, and unique ignoring case. With `columns`, the single sheet is
named after the filename's stem, adjusted to fit the same rules. A CSV holds
one sheet, so `sheets` is refused for `text/csv`.

## Large exports: parts

An export of a million records takes the backend a while to produce. Rather
than holding one connection open for all of it, send the source in parts:
short requests, each holding what the backend produced since the last one.

```sh
curl -s -X PUT "localhost:8080/blobs/$SHEET/parts/1" --data-binary @page-1.jsonl
curl -s -X PUT "localhost:8080/blobs/$SHEET/parts/2" --data-binary @page-2.jsonl
curl -s -X POST "localhost:8080/blobs/$SHEET/finish"
```

- Parts are numbered from 1 and consecutive. A part that is not the next
  one answers `409`, so retrying a part that already landed is harmless.
- `GET /blobs/:id` reports `source_parts`, `source_rows` and `source_size`,
  which is where a backend resumes after a crash or a deploy.
- A part is whole lines, none longer than a megabyte, ending with a
  newline. A source holds at most 10 000 parts.
- `PUT /blobs/:id` is part 1 and the finish in one call.
- `POST /blobs/:id/finish` seals the source and enqueues the build. A finish
  with no parts answers `409`.

Every line is checked as the part arrives. A bad line answers `422` naming
the line, counted within the part, and the reason. The part is discarded,
the parts already stored are kept, and the backend can send the part again.

A source that is never finished is an abandoned export. `DELETE /blobs/:id`
is the way out; as a backstop, the reaper expires a `created` blob whose
last part is older than `upload.abandoned_after`, one day by default.

## Limits

| Limit | XLSX and ODS | CSV |
|---|---|---|
| Rows per sheet, header included | 1 048 576 | none |
| Columns per sheet | 16 384 | none |
| Characters per cell, counted in UTF-16 units | 32 767 | none |
| Sheets per workbook | 1 000 | one |

A sheet beyond the column limit, or a column name beyond the cell limit, is
refused at creation. A source beyond a sheet's row limit, or a text beyond
the cell limit, is refused at the line that crossed it.

## Format details

- **CSV** follows RFC 4180, in UTF-8 with a byte order mark so Excel reads
  the accents. A value starting with `=`, `+`, `-`, `@`, a tab or a carriage
  return is prefixed with a single quote, so user-entered text cannot become
  a formula on an agent's desktop. CSV is the format for machines; an XLSX
  is what people should open.
- **XLSX** cells are typed, with inline strings. Text cells are never
  evaluated. Dates use Excel's built-in date formats, so they show in the
  user's locale.
- **ODS** cells are typed, with the displayed text alongside the value.

A built spreadsheet is not virus scanned, since it contains only what agrafe
wrote. A CSV gets a text thumbnail; an XLSX or ODS gets the category icon.
None has a preview.
