# ZIP archives

An `archive` blob is a ZIP agrafe assembles from blobs it already holds.
Send a manifest of paths and blob ids, and a job streams the archive into
the bucket. Archives of hundreds of gigabytes are built without touching
local disk.

## Create and send

```sh
ZIP=$(curl -s localhost:8080/blobs \
  -H 'content-type: application/json' \
  -d '{"filename": "expense-42.zip", "content_type": "application/zip",
       "input_type": "archive", "priority": "high"}' | jq -r .id)

printf '%s\n' \
  "{\"path\": \"receipts/receipt.pdf\", \"blob_id\": \"$RECEIPT\"}" \
  "{\"path\": \"receipts/photo.jpg\", \"blob_id\": \"$PHOTO\"}" \
  | curl -s -X PUT "localhost:8080/blobs/$ZIP" --data-binary @- | jq .state
# "uploaded"
```

An archive is declared `application/zip`; anything else is refused at
creation. The manifest is JSONL, one `{"path": "...", "blob_id": "..."}` per
line, and can be sent in [parts](spreadsheets.md#large-exports-parts) like
any generated source. The blob becomes `ready` when the build is done.

## Paths

Each path is normalised to Unicode NFC with `/` as the separator, then
refused if it:

- is empty or absolute;
- has an empty, `.` or `..` component, which includes a trailing slash;
- contains a backslash, a control character or a bidirectional formatting
  character;
- is longer than 65 514 bytes;
- is `ERRORS.txt` at the root, in any case, which is reserved.

## Members

Every member must exist and be `ready` when the manifest is accepted;
anything else is refused at its line. The same blob may appear under
several paths and is written each time.

Duplicate paths are not refused. They are renamed in manifest order: a path
already taken becomes `stem-1.ext`, then `stem-2.ext`, with the counter
before the extension; a compressed file keeps its double extension whole,
so `a.tar.gz` becomes `a-1.tar.gz` and `export.csv.gz` becomes
`export-1.csv.gz`. Paths are compared ignoring case, since
`Report.pdf` and `report.pdf` land on the same file on macOS and Windows.
A file and a folder of the same name collide too: `Report` the file, then
`Report/notes.pdf`, puts the folder at `Report-1/`, with every member under
it. The first occurrence keeps its name.

## Members that disappear

A member that was `ready` when the manifest was accepted can be deleted,
expire or be purged before the build reaches it. The build does not fail for
it. The entry is skipped, and the archive ends with an `ERRORS.txt` at its
root listing every skipped entry, one per line: the path, the blob id and a
reason (`deleted`, `expired` or `infected`), separated by tabs. Nothing is
written when nothing was skipped.

`GET /blobs/:id` reports `archive_entries` and `archive_errors`, so the
backend knows an archive is incomplete without downloading it.

A bucket or database error while reading a member is agrafe's own, and
fails the attempt, which is retried: an archive never comes out incomplete
because of a transient error.

## Size

Entries are stored, not compressed: members are PDFs, images and office
files, already compressed, and deflating them would cost CPU for nothing.
The archive's size is therefore known when the manifest is finished. While
the blob is `created` or `uploaded`, `size` is the running estimate; once
`ready`, it is exact.

The estimate is checked against `archive.max_size`, 100 GiB by default and
at most 309 GiB. `upload.max_size` does not apply: it is sized for one
request. The entries are bounded by `archive.max_entries`, a million by
default, across the parts: a part whose lines go past it is refused with a
`422`, the parts before it kept.

## Workers

Archives have their own worker pool, `jobs.concurrency.archive`, one per
process by default, since a build saturates a network interface. To build
more at once, run processes dedicated to archives on machines of their own:

```sh
agrafe --config agrafe.toml serve --role worker --kinds archive
```

Within the queue, the blob's priority orders builds: an export a user is
waiting on, created `high`, goes before a nightly batch created `low`.

## Compatibility

The archive is a standard ZIP with data descriptors and zip64 where sizes
or counts need it. Entries carry mode `0644` and the member's upload time.
Every desktop tool reads it. Strictly sequential readers of stored entries
with data descriptors may not, which is accepted.

A built archive is not virus scanned: its members were scanned, or uploaded
`skip_virus_scan` by the backend, and an archive is no more and no less
trusted than its members. It gets the category icon as its thumbnail.
