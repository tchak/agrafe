# Security policy

## Supported versions

agrafe has one line of releases: a fix ships as the next version from
`main`, and only the newest release receives fixes, security ones
included. The Docker images follow it: `latest` and `latest-office` are
the newest release, and a `0.x` tag stops moving once `0.(x+1).0` is out.

| Version | Supported |
|---|---|
| The newest release | Yes |
| Anything older | No: upgrade, see [Upgrading](https://tchak.github.io/agrafe/guide/deployment.html#upgrading) |

## Reporting a vulnerability

Report it privately, through GitHub's
[private vulnerability reporting](https://github.com/tchak/agrafe/security/advisories/new),
not in a public issue, a pull request or a discussion. Say what you found,
the version or commit, and how to reproduce it; a proof of concept against
a local deployment helps.

The report is acknowledged, the fix and a date to disclose it are agreed
with you, and the fix ships in a release whose changelog lists it under
`Security`, with a GitHub security advisory that credits you unless you
ask otherwise.

## Scope

The [security model](https://tchak.github.io/agrafe/guide/security.html)
says what agrafe defends and against whom. In scope, among others:

- reading, writing or deleting a blob on the public port without a valid
  signature for it, or past its expiry;
- a user file escaping the renderer sandbox, or being decoded in the
  server process;
- reading an object's plaintext from the store without the KEKs, or a
  key, signature or secret leaking into logs, metrics, traces or errors;
- a `url` blob's fetch reaching an address its network policy refuses,
  or a presigned URL leaking out of the private metadata;
- an uploaded file served on the public port running scripts as its
  origin.

Out of scope: the private port, which has no authentication by design and
belongs on a trusted network; the dashboard reached without the proxy and
`server.private_hosts` the security model asks for; denial of service by a
client the private port trusts; and vulnerabilities in libvips,
LibreOffice, pandoc or ClamAV themselves, which go to their projects
(agrafe's part is how it confines and configures them, which is in scope).
