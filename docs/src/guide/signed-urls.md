# Minting signed URLs from another project

agrafe mints signed URLs itself, in every blob's metadata and through
`POST /blobs/:id/urls`. A backend that renders many pages of files does not
want a round trip per link, though. Holding the signing key, it can build
the same URLs locally, the way Rails builds `rails_blob_path` without asking
anything.

This chapter specifies the scheme completely, gives verified
implementations in Ruby, JavaScript and Python, and explains how to keep a
backend's minter in step with the server.

## What the backend needs

| Value | Where it comes from |
|---|---|
| A key id and its secret | An entry of `[signing.keys]` in agrafe's configuration |
| The public base URL | `server.public_base_url`, such as `https://files.example` |
| The window | `signing.window`, one hour by default |
| The maximum TTL | `signing.ttl.max`, seven days by default |
| The blob's `id` and `filename` | Stored by the backend when it created the blob |
| Optionally, the blob's `envelope` | `GET /blobs/:id` once the blob is `ready`, for [sealed URLs](#sealed-urls) |

Give each minting project a key of its own. Every key in `[signing.keys]`
verifies, while `signing.current_key` is only what agrafe itself mints
with. A dedicated key id can be rotated or revoked without touching anyone
else.

```toml
[signing]
current_key = "k1"

[signing.keys]
k1 = "…"        # agrafe's own minting
backend1 = "…"  # the backend's minter, from AGRAFE_SIGNING__KEYS__BACKEND1
```

Keys are at least 32 random bytes, standard base64. Key ids use only
letters, digits, `-` and `_`.

## The scheme

A public URL is the base URL, `/blobs/`, a token, then the **tail**:

```text
https://files.example/blobs/<token><tail>
```

The tail is the path after the token, starting at `/<id>`, with the query
string if there is one. The token is one path segment:

```text
token   = "v1." key_id "." expires "." base64url_nopad(hmac_sha256(key, message))
message = METHOD "\n" expires "\n" tail
```

- `METHOD` is `GET` or `PUT`. A `HEAD` request is verified as `GET`, so
  sign reads as `GET`.
- `expires` is a Unix timestamp in seconds, in decimal, with no leading
  zeros.
- The signature is URL-safe base64 without padding.
- The server verifies the tail exactly as received: nothing is decoded,
  reordered or re-encoded. Your encoding must therefore produce the same
  bytes as the server's, which the rules below pin down.

### Tails

| Purpose | Method | Tail |
|---|---|---|
| Upload | `PUT` | `/<id>` |
| Metadata | `GET` | `/<id>` |
| Original | `GET` | `/<id>/original/<filename>[?query]` |
| Preview | `GET` | `/<id>/preview/<filename>[?query]` |
| Thumbnail | `GET` | `/<id>/thumbnail/<filename>[?query]` |

The id is the lowercase hyphenated UUID, as agrafe returns it. The filename
is the blob's own filename: a different one answers `404`. Use the
`filename` query parameter to change the download name.

### Encoding the filename

The filename segment is percent-encoded as UTF-8, leaving only the RFC 3986
unreserved characters as they are: `A-Z`, `a-z`, `0-9`, `-`, `.`, `_` and
`~`. Everything else, spaces and `/` included, is encoded, in uppercase hex.

| Filename | Segment |
|---|---|
| `photo.jpg` | `photo.jpg` |
| `a b/c?d#e.png` | `a%20b%2Fc%3Fd%23e.png` |
| `été.pdf` | `%C3%A9t%C3%A9.pdf` |
| `100%.txt` | `100%25.txt` |

### Encoding the query

The query is `application/x-www-form-urlencoded`, with the parameters
**sorted by name**: alphanumerics and `*-._` stay, a space is `+`, and
everything else is percent-encoded as UTF-8. Omit the `?` when there are no
parameters.

| Parameters | Query |
|---|---|
| `wait=5000` | `wait=5000` |
| `filename=été x.pdf`, `disposition=attachment` | `disposition=attachment&filename=%C3%A9t%C3%A9+x.pdf` |

The parameters a read URL may carry:

| Name | Values | Effect |
|---|---|---|
| `disposition` | `inline` or `attachment` | Show or download |
| `filename` | A valid filename | The download name in `Content-Disposition` |
| `wait` | `0` to `30000` | Milliseconds a preview or thumbnail request waits for a pending render |
| `e` | An envelope | Makes the URL [sealed](#sealed-urls) |

When agrafe mints, it validates these parameters and refuses anything else.
When you mint, nothing validates them until a browser follows the URL, and
a bad value then answers `400` on a correctly signed URL. Validate them in
your minter: `disposition` is exactly `inline` or `attachment`, `wait` is
an integer of at most `30000`, and `filename` follows the [filename
rules](uploads.md#create-the-blob). Unknown parameters are ignored by the
public routes but serve no purpose.

### Choosing the expiry

agrafe rounds expiries up to a window boundary before adding the TTL:

```text
expires = ceil(now / window) * window + ttl
```

Do the same. All URLs minted for one blob inside a window are then
identical, so browsers and CDNs cache them, and your URLs match the ones
agrafe mints.

The public port refuses a token whose expiry lies further in the future
than `window + signing.ttl.max` from now, with `403 signature not valid
yet`. Keep `ttl` at or below `signing.ttl.max`. There are no permanent
URLs: a link that must stay stable is a route of your backend that mints a
fresh URL and redirects to it, which the rounding makes cheap.

## Implementations

The three implementations below reproduce every token, segment, query and
expiry in the repository's test vectors. Each needs only its language's
standard library.

### Ruby

```ruby
require "openssl"
require "base64"
require "erb"
require "uri"

class AgrafeSigner
  def initialize(base_url:, key_id:, key:, window: 3600)
    @base_url = base_url
    @key_id = key_id
    @key = Base64.strict_decode64(key)
    @window = window
  end

  # Rounded up to the window, so URLs minted in one window are identical.
  def expiry(ttl, now: Time.now.to_i)
    (now + @window - 1) / @window * @window + ttl
  end

  def sign(method, expires, tail)
    method = "GET" if method == "HEAD"
    mac = OpenSSL::HMAC.digest("SHA256", @key, "#{method}\n#{expires}\n#{tail}")
    "v1.#{@key_id}.#{expires}.#{Base64.urlsafe_encode64(mac, padding: false)}"
  end

  def url(method, expires, tail)
    "#{@base_url}/blobs/#{sign(method, expires, tail)}#{tail}"
  end

  # kind: "original", "preview" or "thumbnail".
  def read_url(id, filename, kind: "original", ttl: 3600, query: {})
    url("GET", expiry(ttl), representation_tail(id, kind, filename, query))
  end

  def representation_tail(id, kind, filename, query = {})
    tail = "/#{id}/#{kind}/#{ERB::Util.url_encode(filename)}"
    query.empty? ? tail : "#{tail}?#{URI.encode_www_form(query.transform_keys(&:to_s).sort)}"
  end
end
```

```ruby
signer = AgrafeSigner.new(
  base_url: "https://files.example",
  key_id: "backend1",
  key: ENV.fetch("AGRAFE_SIGNING_KEY"),
)

signer.read_url(blob.agrafe_id, blob.filename, kind: "thumbnail")
signer.read_url(blob.agrafe_id, blob.filename,
                query: { disposition: "attachment", filename: "facture-12.pdf" })
```

`ERB::Util.url_encode` leaves exactly the unreserved characters, and
`URI.encode_www_form` follows the form encoding above.

### JavaScript

For Node.js, Deno and Bun:

```js
import { createHmac } from "node:crypto";

// RFC 3986 unreserved characters stay; encodeURIComponent also leaves !'()*.
const encodeSegment = (s) =>
  encodeURIComponent(s).replace(/[!'()*]/g, (c) => "%" + c.charCodeAt(0).toString(16).toUpperCase());

// application/x-www-form-urlencoded, sorted by name.
const encodeQuery = (query) =>
  new URLSearchParams(Object.entries(query).sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0))).toString();

export class AgrafeSigner {
  constructor({ baseUrl, keyId, key, window = 3600 }) {
    Object.assign(this, { baseUrl, keyId, key: Buffer.from(key, "base64"), window });
  }

  expiry(ttl, now = Math.floor(Date.now() / 1000)) {
    return Math.ceil(now / this.window) * this.window + ttl;
  }

  sign(method, expires, tail) {
    if (method === "HEAD") method = "GET";
    const mac = createHmac("sha256", this.key).update(`${method}\n${expires}\n${tail}`).digest("base64url");
    return `v1.${this.keyId}.${expires}.${mac}`;
  }

  url(method, expires, tail) {
    return `${this.baseUrl}/blobs/${this.sign(method, expires, tail)}${tail}`;
  }

  readUrl(id, filename, { kind = "original", ttl = 3600, query = {} } = {}) {
    return this.url("GET", this.expiry(ttl), this.representationTail(id, kind, filename, query));
  }

  representationTail(id, kind, filename, query = {}) {
    const tail = `/${id}/${kind}/${encodeSegment(filename)}`;
    return Object.keys(query).length ? `${tail}?${encodeQuery(query)}` : tail;
  }
}
```

`URLSearchParams` serialises with the form encoding above. Sort with a plain
code-unit comparison, not `localeCompare`, which orders differently.

### Python

```python
import base64, hashlib, hmac, time
from urllib.parse import quote, quote_plus

def encode_segment(s: str) -> str:
    return quote(s, safe="-._~")

def _form(s: str) -> str:
    # quote_plus leaves "~" and encodes "*"; the form encoding does the opposite.
    return quote_plus(s, safe="*").replace("~", "%7E")

def encode_query(query: dict[str, str]) -> str:
    return "&".join(f"{_form(k)}={_form(v)}" for k, v in sorted(query.items()))

class AgrafeSigner:
    def __init__(self, base_url: str, key_id: str, key: str, window: int = 3600):
        self.base_url, self.key_id, self.window = base_url, key_id, window
        self.key = base64.b64decode(key)

    def expiry(self, ttl: int, now: int | None = None) -> int:
        now = int(time.time()) if now is None else now
        return -(-now // self.window) * self.window + ttl

    def sign(self, method: str, expires: int, tail: str) -> str:
        method = "GET" if method == "HEAD" else method
        mac = hmac.new(self.key, f"{method}\n{expires}\n{tail}".encode(), hashlib.sha256).digest()
        return f"v1.{self.key_id}.{expires}." + base64.urlsafe_b64encode(mac).rstrip(b"=").decode()

    def url(self, method: str, expires: int, tail: str) -> str:
        return f"{self.base_url}/blobs/{self.sign(method, expires, tail)}{tail}"

    def read_url(self, id: str, filename: str, kind="original", ttl=3600, query=None) -> str:
        tail = f"/{id}/{kind}/{encode_segment(filename)}"
        if query:
            tail += "?" + encode_query(query)
        return self.url("GET", self.expiry(ttl), tail)
```

`urllib.parse.urlencode` is not a drop-in replacement: it leaves `~` as is
and encodes `*`, which changes the signed bytes for a filename containing
either.

## Sealed URLs

A sealed URL carries the blob's **envelope** in its `e` parameter, so the
public port serves it from the bucket without a database query. The
envelope is an opaque string: store it, never parse it.

1. When the blob becomes `ready`, read `envelope` from `GET /blobs/:id` or
   from the [webhook](webhooks.md)'s `ready` delivery, and store it next to
   the blob's id and filename.
2. Add it as the `e` query parameter of read URLs, sorted with the others:

   ```ruby
   signer.read_url(blob.agrafe_id, blob.filename, query: { e: blob.agrafe_envelope })
   ```

3. Refresh your copy after the renders finish. An envelope is a snapshot: a
   representation still `pending` in it makes the request fall back to the
   database, which works but costs the query. Blobs are write-once, so the
   rest of the envelope never changes.

Mind the lifetime. Nothing on the sealed path reads the row, so a blob
deleted or expired after you minted a sealed URL stays readable through it
until its objects are purged or the URL expires. agrafe caps the URLs it
mints at the blob's own `expires_at`. Do the same for a blob that is not
attached, and choose a TTL you are willing to stand behind.

The envelope also contains the blob's wrapped data key. It is useless
without the key-encryption key and bound to the blob id, but treat the
envelope like the rest of the blob's metadata, not like public data.

## Upload URLs

An upload URL is `PUT` over the tail `/<id>`, with no query:

```ruby
signer.url("PUT", signer.expiry(3600), "/#{blob_id}")
```

Only `raw` and `multipart` blobs created without `skip_virus_scan` accept
public uploads, which the metadata reports as `public_upload_allowed`. A
signed `PUT` on any other blob answers `403`.

## Rotating a key

1. Add the new key to `[signing.keys]` and deploy agrafe. Both keys now
   verify.
2. Switch the backend's minter to the new key id.
3. Wait for the longest TTL you mint with to pass, so that no URL signed
   with the old key is still in use.
4. Remove the old key from `[signing.keys]` and deploy.

Lowering `signing.ttl.max` or `signing.window` takes effect at once:
verification checks expiries against the running configuration, so URLs in
flight beyond the new bound stop working.

## Test vectors

`fixtures/signing-vectors.json` in the repository pins the scheme with
tokens, URLs, filename segments, query strings and expiries computed by the
server from fixed inputs:

```json
{
  "key": "AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=",
  "key_id": "k1",
  "sign": [
    {
      "method": "GET",
      "expires": 2000000000,
      "tail": "/0199f6a0-5b2c-7c3d-8e4f-0a1b2c3d4e5f/original/photo.jpg",
      "token": "v1.k1.2000000000.JRkV0zEVaL5zYHrE9vss5pwX6NuU4vXcR088c1cF0KA"
    }
  ],
  "segments": [{"filename": "été.pdf", "segment": "%C3%A9t%C3%A9.pdf"}],
  "queries": [{"query": {"wait": "5000"}, "string": "wait=5000"}],
  "expiry": [{"now": 7201, "window": 3600, "ttl": 86400, "expires": 97200}]
}
```

Copy the file into your project's test suite and check your minter against
every entry. A change to the scheme bumps the token version, `v1`, so a
passing test stays meaningful. `fixtures/envelope-vectors.json` does the
same for envelopes, which backends store: a later envelope format must keep
decoding them.

A minimal check in Ruby:

```ruby
vectors = JSON.parse(File.read("spec/fixtures/agrafe-signing-vectors.json"))
signer = AgrafeSigner.new(base_url: "https://files.example",
                          key_id: vectors["key_id"], key: vectors["key"])
vectors["sign"].each do |v|
  expect(signer.sign(v["method"], v["expires"], v["tail"])).to eq(v["token"])
  expect(signer.url(v["method"], v["expires"], v["tail"])).to eq(v["url"])
end
vectors["segments"].each { |v| expect(ERB::Util.url_encode(v["filename"])).to eq(v["segment"]) }
vectors["queries"].each { |v| expect(URI.encode_www_form(v["query"].sort)).to eq(v["string"]) }
```
