# agrafe-render

[![crates.io](https://img.shields.io/crates/v/agrafe-render.svg)](https://crates.io/crates/agrafe-render)
[![docs.rs](https://img.shields.io/docsrs/agrafe-render?label=docs.rs)](https://docs.rs/agrafe-render)

The previews and thumbnails of [agrafe](https://github.com/tchak/agrafe),
the file storage service: which files render and from what (`plan`), the
drivers of `vips`, of the sandboxed helpers and of pandoc, the category
icons for the rest, the startup probe that checks every tool against
sample files, and the `[media]` configuration.

Nothing is decoded in the calling process: `vips`, LibreOffice, pandoc and
the [agrafe-extract](https://crates.io/crates/agrafe-extract),
[agrafe-render-text](https://crates.io/crates/agrafe-render-text),
[agrafe-render-document](https://crates.io/crates/agrafe-render-document)
and [agrafe-render-geo](https://crates.io/crates/agrafe-render-geo) helpers
run through [agrafe-sandbox](https://crates.io/crates/agrafe-sandbox), and
this crate links none of their parsers. The one thing it does itself is
fetch the background layers of a map from a configured provider. See
[Previews and thumbnails](https://tchak.github.io/agrafe/guide/representations.html).

## Part of agrafe

A building block of [agrafe](https://github.com/tchak/agrafe), the file
storage service. It can be used on its own, but its API serves agrafe's
and moves with its releases: every agrafe crate is released together, at
one version, and before 1.0 a minor version may break it, as the
[changelog](https://github.com/tchak/agrafe/blob/main/CHANGELOG.md) says.
To run agrafe, see [agrafe-server](https://crates.io/crates/agrafe-server)
and the [documentation](https://tchak.github.io/agrafe/).

## License

Licensed under either of the [Apache License, Version 2.0](https://github.com/tchak/agrafe/blob/main/LICENSE-APACHE)
or the [MIT license](https://github.com/tchak/agrafe/blob/main/LICENSE-MIT),
at your option.
