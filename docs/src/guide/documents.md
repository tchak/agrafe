# PDF documents

A `document` blob is a PDF compiled by [typst](https://typst.app) from a
workspace of files: a template, its includes, its images and its fonts, plus
the data of one document. The compiler runs in a sandboxed helper,
`agrafe-render-document`, never in the server.

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

A document is declared `application/pdf`; anything else is refused at
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
| `document.max_files` | 10 000 | The manifest's lines across its parts |

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
since a template cannot do without a file, and when the compile is killed at
the sandbox's time limit. Fix the template or the data, then create a new
document blob.

Failures of agrafe's own, such as the bucket, the database or the helper
failing to start, are retried with the usual backoff.

## After the build

The PDF is rendered like any other: its first page becomes its thumbnail,
and it has no preview unless it wraps a single image. Builds have their own
worker pool, `jobs.concurrency.document`, two per process by default, and
the blob's priority orders the queue.

Before a worker that handles `document` jobs starts, it compiles a probe
workspace through the exact command line the jobs use, so a missing or
mismatched helper fails the deployment rather than every document.
