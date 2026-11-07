# agrafe-render-geo

[![crates.io](https://img.shields.io/crates/v/agrafe-render-geo.svg)](https://crates.io/crates/agrafe-render-geo)

A sandboxed helper of [agrafe](https://github.com/tchak/agrafe), the file
storage service, and the `agrafe-render-geo` binary.

Draws a map of a GeoJSON, GPX, KML, KMZ or GML file or a zipped
shapefile: the geometry projected to Web Mercator, fitted into the
thumbnail's box, simplified to the pixel and written as SVG paths in the
file's simplestyle-spec colours, with the extent it chose. The server
fetches the background for that extent and `vips` composites the two.

The file is untrusted, so the agrafe workers run it as a sandboxed
subprocess (`geojson`, `quick-xml`, `zip`), never in the server, which
does not link this crate. What it exchanges with the server, the map spec
in and the extent out, is typed in `agrafe-types`.

## Install

It ships with the `agrafe` binary and is installed with it, from the same
release: the command line between the two changes with the releases, and
the server's startup probe, which runs each helper on sample files,
catches a helper that fails them, not every mismatch:

```sh
cargo install --locked agrafe-server agrafe-extract agrafe-render-text \
  agrafe-render-document agrafe-render-geo
```

The server finds it on the `PATH`, or at `media.render_geo_command` in its
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
