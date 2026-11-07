# agrafe-types

[![crates.io](https://img.shields.io/crates/v/agrafe-types.svg)](https://crates.io/crates/agrafe-types)
[![docs.rs](https://img.shields.io/docsrs/agrafe-types)](https://docs.rs/agrafe-types)

What a file is, for [agrafe](https://github.com/tchak/agrafe), the file
storage service: content type constants, the classification of a type
into a category, magic-byte sniffing, the office format table, the
renderer's loader allowlist, and the contracts between the server and its
sandboxed helpers (the text sample, the map spec and extent). Pure and
small, shared by the server, the renderer and the helpers, so that both
sides of each contract agree by construction.

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
