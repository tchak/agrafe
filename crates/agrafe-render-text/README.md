# agrafe-render-text

[![crates.io](https://img.shields.io/crates/v/agrafe-render-text.svg)](https://crates.io/crates/agrafe-render-text)

A sandboxed helper of [agrafe](https://github.com/tchak/agrafe), the file
storage service, and the `agrafe-render-text` binary.

Rasterises the first lines of a text file for its thumbnail, set in an
embedded DejaVu Sans Mono, so that the result does not depend on the fonts,
fontconfig or shaping libraries of the host. It writes a PGM, which `vips`
turns into the thumbnail like any other raster.

The bytes are untrusted, so the agrafe workers run it as a sandboxed
subprocess (`ab_glyph`), never in the server, which does not link this
crate.

## Install

It ships with the `agrafe` binary and is installed with it, from the same
release: the command line between the two changes with the releases, and
the server's startup probe, which runs each helper on sample files,
catches a helper that fails them, not every mismatch:

```sh
cargo install --locked agrafe-server agrafe-extract agrafe-render-text \
  agrafe-render-document agrafe-render-geo
```

The server finds it on the `PATH`, or at `media.render_text_command` in its
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

The embedded DejaVu Sans Mono font is distributed under its own license, in
[assets/fonts/LICENSE](https://github.com/tchak/agrafe/blob/main/crates/agrafe-render-text/assets/fonts/LICENSE).
