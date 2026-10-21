# syntax=docker/dockerfile:1
#
# The agrafe image: the server and its four helpers on Debian trixie, whose
# libvips carries the security fixes (see docs/src/guide/deployment.md,
# "libvips"). Two variants, chosen by VARIANT:
#
#   docker build .                              # without LibreOffice
#   docker build --build-arg VARIANT=office .   # with LibreOffice
#
# The build stage compiles with the toolchain and the runtime stages copy
# the binaries alone; both are trixie, so the binaries run against the
# glibc they were linked with.

ARG RUST_VERSION=1.98
# `default` or `office`: the runtime stage the image starts from, and the
# file under docker/ that becomes its configuration.
ARG VARIANT=default

FROM rust:${RUST_VERSION}-slim-trixie AS build
WORKDIR /src
COPY . .
# The caches make a local rebuild incremental; in CI they start empty.
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/src/target,sharing=locked \
    cargo build --release --locked --bins \
 && mkdir /out \
 && cd target/release \
 && cp agrafe agrafe-extract agrafe-render-text agrafe-render-document \
      agrafe-render-geo /out/

FROM debian:trixie-slim AS runtime-default
# vips for every render, with SVG (rsvg) for the maps; the fonts for PDFs
# that do not embed theirs; the CA roots for the fetches, the webhook and
# the map layers. coreutils, which the startup probe runs, is in the base.
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      libvips-tools ca-certificates fontconfig fonts-dejavu-core fonts-liberation \
 && rm -rf /var/lib/apt/lists/* \
 # The sandbox cannot keep a font cache of its own between jobs.
 && fc-cache -f \
 && useradd --system --create-home --home-dir /var/lib/agrafe \
      --shell /usr/sbin/nologin agrafe \
 && install -d -o agrafe -g agrafe /var/lib/agrafe/data /var/tmp/agrafe

FROM runtime-default AS runtime-office
# About 400 MB. Writer, Calc, Impress and Draw: the startup probe converts
# a file of each.
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      libreoffice-writer-nogui libreoffice-calc-nogui \
      libreoffice-impress-nogui libreoffice-draw-nogui \
 && rm -rf /var/lib/apt/lists/* \
 && fc-cache -f
# A profile warmed by a throwaway conversion, which media.office's
# profile_template copies into each run's work directory.
USER agrafe
RUN echo warm > /tmp/warm.txt \
 && soffice -env:UserInstallation=file:///var/lib/agrafe/soffice-profile \
      --headless --norestore --nologo --convert-to pdf --outdir /tmp /tmp/warm.txt \
 && rm /tmp/warm.txt /tmp/warm.pdf
USER root

FROM runtime-${VARIANT}
ARG VARIANT
LABEL org.opencontainers.image.source="https://github.com/tchak/agrafe" \
      org.opencontainers.image.description="agrafe, a file storage service: encrypted, scanned, with previews" \
      org.opencontainers.image.licenses="MIT OR Apache-2.0"
COPY docker/${VARIANT}.toml /etc/agrafe/agrafe.toml
COPY --from=build /out/ /usr/local/bin/
ENV AGRAFE_CONFIG=/etc/agrafe/agrafe.toml
USER agrafe
WORKDIR /var/lib/agrafe
EXPOSE 8080 8081
ENTRYPOINT ["agrafe"]
CMD ["serve"]
