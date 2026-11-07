# agrafe-extract

[![crates.io](https://img.shields.io/crates/v/agrafe-extract.svg)](https://crates.io/crates/agrafe-extract)

A sandboxed helper of [agrafe](https://github.com/tchak/agrafe), the file
storage service, and the `agrafe-extract` binary.

Pulls the one image out of a document that is really a wrapped picture:
a PDF whose single page is one image, or a Writer package (a DOCX or an
ODT, or one of their macro-enabled, template and master variants) holding
one media file and next to no text. Users answer "send a screenshot" with
such files, and the embedded image at full resolution is a better preview
than a render of the page around it.

It parses untrusted containers (`lopdf`, `zip`, `quick-xml`), so the
agrafe workers run it as a sandboxed subprocess, never in the server,
which does not link this crate. Every decode is bounded, and anything
doubtful writes nothing: the server then renders the page instead.

## Install

It ships with the `agrafe` binary and is installed with it, from the same
release: the command line between the two changes with the releases, and
the server's startup probe, which runs each helper on sample files,
catches a helper that fails them, not every mismatch:

```sh
cargo install --locked agrafe-server agrafe-extract agrafe-render-text \
  agrafe-render-document agrafe-render-geo
```

The server finds it on the `PATH`, or at `media.extract_command` in its
configuration. The Docker images, `ghcr.io/tchak/agrafe`, carry it. See
[Deployment](https://tchak.github.io/agrafe/guide/deployment.html).

## Not a tool of its own

It is not meant to be run by hand: its command line is a contract with
the agrafe release it ships with and changes with it. It reads no
configuration, writes no logs and never touches the network. The library
is the helper's own code, published so that the binary can be installed,
with no API meant for other crates.

## License

Licensed under either of the [Apache License, Version 2.0](https://github.com/tchak/agrafe/blob/main/LICENSE-APACHE)
or the [MIT license](https://github.com/tchak/agrafe/blob/main/LICENSE-MIT),
at your option.
