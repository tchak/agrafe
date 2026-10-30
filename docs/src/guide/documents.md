# Documents: PDF, DOCX and ODT

A `document` blob is a document compiled by [typst](https://typst.app) from
a workspace of files: a template, its includes, its images and its fonts,
plus the data of one document. It is a PDF, or, from the same template, an
editable DOCX or ODT: typst compiles the workspace to HTML and
[pandoc](https://pandoc.org) writes the office file. The compiler and
pandoc run sandboxed, never in the server.

| `content_type` | Built as |
|---|---|
| `application/pdf` | typst's PDF |
| `application/vnd.openxmlformats-officedocument.wordprocessingml.document` (DOCX) | typst's HTML, written as a DOCX by pandoc |
| `application/vnd.oasis.opendocument.text` (ODT) | typst's HTML, written as an ODT by pandoc |

## Templates are blobs

There is no template registry. A template is an ordinary blob, attached to an
owner of yours so it does not expire:

```sh
TEMPLATE=$(curl -s localhost:8080/blobs \
  -H 'content-type: application/json' \
  -d "{\"filename\": \"invoice.typ\", \"content_type\": \"text/plain\",
       \"input_type\": \"raw\",
       \"checksum\": \"$(openssl dgst -sha256 -binary invoice.typ | base64)\",
       \"size\": $(wc -c < invoice.typ | tr -d ' '), \"skip_virus_scan\": true,
       \"attachment\": {\"owner_kind\": \"template\", \"owner_id\": \"invoice\", \"role\": \"source\"}}" \
  | jq -r .id)
curl -s -X PUT "localhost:8080/blobs/$TEMPLATE" --data-binary @invoice.typ > /dev/null
```

Keep its id. A new version of the template is a new blob, attached in place
of the old one, which then expires on its own. Logos, includes and fonts are
blobs the same way.

## Compose a document

```sh
DOC=$(curl -s localhost:8080/blobs \
  -H 'content-type: application/json' \
  -d '{"filename": "invoice-12.pdf", "content_type": "application/pdf",
       "input_type": "document"}' | jq -r .id)

printf '%s\n' \
  "{\"path\": \"main.typ\", \"blob_id\": \"$TEMPLATE\"}" \
  "{\"path\": \"logo.png\", \"blob_id\": \"$LOGO\"}" \
  '{"path": "data.json", "json": {"client": "Ada", "items": [{"label": "Consulting", "amount": "1,200.00"}]}}' \
  | curl -s -X PUT "localhost:8080/blobs/$DOC" --data-binary @- | jq .state
# "uploaded"
```

A document is declared PDF, DOCX or ODT; anything else is refused at
creation. The source is a workspace manifest in JSONL, one file per line:

| Line | File |
|---|---|
| `{"path": "...", "blob_id": "..."}` | Taken from a blob, which must be `ready` |
| `{"path": "...", "json": ...}` | Inline JSON, written exactly as given, key order and formatting included |
| `{"path": "...", "text": "..."}` | Inline text |

A line takes exactly one of `blob_id`, `json` and `text`. Inline files are
bounded by the one-megabyte line limit; larger data becomes a blob first.
The manifest can be sent in [parts](spreadsheets.md#large-exports-parts).

The entry file is `main.typ`, or the path given as `main` at creation:

```json
{"filename": "invoice-12.pdf", "content_type": "application/pdf",
 "input_type": "document", "main": "templates/invoice.typ"}
```

## Writing templates

A template is plain typst, with every path relative to the workspace root:

```typst
#let d = json("data.json")
#include "lib/header.typ"
#image("logo.png", width: 4cm)

= Invoice for #d.client

#table(
  columns: 2,
  ..d.items.map(i => (i.label, i.amount)).flatten()
)
```

- **Fonts.** Every `.ttf` and `.otf` file in the workspace is registered,
  plus the compiler's embedded fonts: Libertinus Serif, New Computer Modern
  and DejaVu Sans Mono. A template works with no font declared.
- **Packages** (`@preview/...`) are refused: the sandbox has no network,
  and the workspace holds every file a document uses.
- **Dates.** `datetime.today()` is the UTC date.
- **Compiler version.** typst 0.15, pinned in the helper.

There is no schema for the data. The backend owns both the template and the
data, and typst fails loudly on a missing key.

## DOCX and ODT

Declare the blob DOCX or ODT instead of PDF, and send the same workspace:

```json
{"filename": "invoice-12.docx",
 "content_type": "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
 "input_type": "document"}
```

The file is built with named styles (Heading 1, List Paragraph, Table,
Footnote Text), so it edits well in Word or LibreOffice and restyles as a
whole.

### The house style: `reference.docx`

A workspace may carry `reference.docx` (for a DOCX) or `reference.odt`
(for an ODT) at its root, a member like any other. When it is there, the
built file takes its styles: fonts, sizes, colours, spacing, margins.
Without one, pandoc's default styles apply. Start from pandoc's own and
restyle it in Word or LibreOffice:

```sh
pandoc -o reference.docx --print-default-data-file reference.docx
```

Upload it as a blob, attach it next to the template, and add it to each
manifest:

```json
{"path": "reference.docx", "blob_id": "..."}
```

### One template, both outputs

`target()` is `"html"` when a DOCX or an ODT is being built and `"paged"`
for a PDF, so a template branches where the two differ:

```typst
#show: body => context if target() == "html" { body } else {
  set page(paper: "a4", margin: 2cm, header: [ACME])
  body
}
```

What crosses into the office file:

- **As structure:** headings, paragraphs, lists, emphasis, links, tables
  with merged cells, figures with captions, footnotes and equations
  (editable equations in a DOCX). `= Heading` is Heading 1, and the
  document's title (`#set document(title: ...)` with `#title()`) is the
  file's title. Heading numbering crosses as text.
- **As media:** images, at their stored resolution.
- **Not at all:** fonts, colours, sizes and spacing. The reference
  document's styles decide.
- **As a picture, or dropped:** page furniture, which has no meaning in
  a flowing document: page breaks, headers and footers, page numbers,
  `place`, `columns`, and content laid out with `box` and `block`
  geometry. Keep it under the PDF branch for clean text.
- **Removed:** what a template writes itself with `html.elem` that would
  load a file or a URL (an `iframe`, an `img` whose `src` is not a
  `data:` URI, …). Images placed with `image()` are inlined and cross.

typst's HTML export is experimental upstream: a template that compiles to
a PDF may warn or fail under the HTML target, and the error says where.

## Paths

Paths are NFC, `/` separated and relative, with no empty, `.` or `..`
component, no backslash, control or bidirectional formatting character, at
most 1 024 bytes and no component longer than 255. A path given twice in one
part is refused at its line. A path given twice across parts, or used both as
a file and as a directory (`lib` and `lib/a.typ`), fails the build.

## Limits

| Setting | Default | Bounds |
|---|---|---|
| `document.max_workspace_size` | 256 MiB | The members' sizes plus the inline bytes |
| `document.max_files` | 10 000 | The manifest's lines across its parts, and the directories their paths make, a path at most 32 components deep |

Both are checked as the manifest arrives and again by the build, which
writes the workspace to the worker's disk under `media.tmp_dir`. A
document's `size` is `null` until it is `ready`.

## When a template does not compile

A document that does not compile kills its job at once, without retries,
since the same template would fail the same way. The blob stays `uploaded`
with `failed_at` set, and `last_error` holds the compiler's diagnostics, one
per line:

```text
main.typ:12:5: error: unknown variable: montnat
```

The same happens when a referenced blob is gone by the time the build runs,
since a template cannot do without a file, when the compile is killed at
the sandbox's time limit, and when the file would be larger than
`media.sandbox.max_output_size`. For a DOCX or an ODT, it also happens when
the template does not compile to HTML, and when pandoc refuses the
conversion, `last_error` then holding what pandoc said:

```text
the document does not convert: Could not parse reference-doc "…/reference.docx": Did not find end of central directory signature
```

Fix the template, the data or the reference document, then create a new
document blob.

Failures of agrafe's own, such as the bucket, the database, the helper or
pandoc failing to start, or pandoc running out of memory, are retried with
the usual backoff.

## After the build

The file is rendered like any other of its type. A PDF's first page becomes
its thumbnail, and it has no preview unless it wraps a single image; a DOCX
or an ODT gets its first page through LibreOffice where one is configured,
and its icon otherwise. Builds have their own
worker pool, `jobs.concurrency.document`, two per process by default, and
the blob's priority orders the queue.

Before a worker that handles `document` jobs starts, it compiles a probe
workspace through the exact command lines the jobs use, to a PDF, and to a
DOCX and an ODT through pandoc, so a missing or mismatched helper, or a
missing or too old pandoc, fails the deployment rather than every
document.
