# Attachments and lifetime

An attachment records that one of your records uses a blob. It is three
strings, the triple Active Storage uses:

- `record_type`, the kind of record, such as `Dossier`;
- `record_id`, its identifier as your backend writes it, such as `42`;
- `name`, the role the blob plays for it, such as `piece_justificative` or
  `avatar`.

A blob can be attached any number of times, from records that do not know
about each other, and a record can attach several blobs under one name.
Whether a name is one-to-one or one-to-many is your backend's rule.

## Attach

```sh
curl -s "localhost:8080/blobs/$ID/attachments" \
  -H 'content-type: application/json' \
  -d '{"record_type": "Dossier", "record_id": "42", "name": "piece_justificative"}'
```

```json
{
  "id": "019a1c2e-…",
  "blob_id": "019a1c2d-…",
  "record_type": "Dossier",
  "record_id": "42",
  "name": "piece_justificative",
  "created_at": "2026-10-19T09:12:03.418Z"
}
```

Attaching is idempotent. A new attachment answers `201`; a triple that is
already attached answers `200` with the existing attachment.

A blob can be attached before its bytes arrive, in `created`. When the
backend knows the record at creation, it can create the blob attached in
one transaction:

```json
{"filename": "export.zip", "content_type": "application/zip", "input_type": "archive",
 "attachment": {"record_type": "Procedure", "record_id": "7", "name": "export"}}
```

Attaching is refused for a blob that is expired or deleted (`410`) or
infected (`409`).

The three strings must not be blank, are at most 255 bytes, and contain no
control or bidirectional formatting characters. Unlike filenames they are
not trimmed: an identifier is yours to write.

## Lifetime follows use

Every blob is created with an expiry, `blobs.default_expires_in` (one day)
unless the request gives one. Then:

- the **first attachment** clears `expires_at`;
- the **last detach** sets it again to the default, counted from that
  moment.

For a blob that is not in a terminal state, "attached" and "has no expiry"
are the same thing. There is no other way to create a blob that never
expires: permanent is spelled "attached".

```text
POST /blobs ─► expires in 1 day ─► attach ─► no expiry ─► detach last ─► expires in 1 day
```

## Detach

```sh
curl -s -X DELETE "localhost:8080/attachments/$ATTACHMENT_ID"   # 204
```

Detach works on a blob in any state. On an infected blob, the last detach
is what starts the clock for its objects to be purged.

## Delete

```sh
curl -s -X DELETE "localhost:8080/blobs/$ID"   # 204
```

Deleting an attached blob is refused with `409`. A blob is shared by
attachments that do not know of each other, so a backend acting for one
record cannot know the blob is free. Detach is the only operation on behalf
of one record. A blob nobody uses expires on its own; a backend in a hurry
detaches, then deletes.

A delete is soft. The objects are purged `upload.purge_delay` later, one
hour by default, so a download that started before the delete can finish.
The row stays as a [tombstone](concepts.md#tombstones).

## Abandoned blobs

An attachment at creation promises the blob is in use, not that its bytes
will arrive. A browser that never sends the `PUT`, or a fetch that gave up,
would leave an attached `created` blob with no expiry forever. The reaper
therefore expires a `created` blob that is attached and has not been touched
for `upload.abandoned_after`, one day by default. Its attachments are kept,
so the record's side sees `blob_state: "expired"` in the listing, and the
[webhook](webhooks.md) sends an `expired` event.

## List

```sh
curl -s 'localhost:8080/attachments?record_type=Dossier&record_id=42' | jq '.items[]'
```

Each listed attachment carries the state of its blob, `blob_state`, and
`blob_failed_at` when the job that was to finish the blob died, so a
record's files can be checked in one request.

| Filter | Notes |
|---|---|
| `record_type` | |
| `record_id` | Only together with `record_type`; alone it is a `400` |
| `name` | |
| `blob_id` | Every record using one blob |
| `cursor`, `limit` | Pagination: `limit` from 1 to 200, 50 by default |

`GET /blobs/:id` also inlines the blob's attachments, oldest first, and
`GET /blobs?attached=false` lists the blobs nothing uses.

The public port never sees attachments: they are your backend's statements
about its own records.

## Replacing a file

Blobs are write-once. To replace a record's file:

1. create and upload the new blob;
2. attach it to the record;
3. detach the old one, which expires a day later and is purged.
