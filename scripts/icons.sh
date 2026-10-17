#!/bin/sh
# Renders the thumbnail placeholder icons embedded in agrafe-render from
# crates/agrafe-render/assets/icons/icon.svg. Needs `vips` with SVG support. The
# results are committed; run this only to change the design.
set -eu
cd "$(dirname "$0")/../crates/agrafe-render/assets/icons"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

render() {
    name=$1 color=$2 label=$3
    sed -e "s/{COLOR}/$color/" -e "s/{LABEL}/$label/" icon.svg > "$tmp/$name.svg"
    vips copy "$tmp/$name.svg" "$name.webp[Q=90,keep=none]"
}

# Categories.
render image        '#0d9488' IMAGE
render document     '#2563eb' DOC
render spreadsheet  '#16a34a' SHEET
render presentation '#ea580c' SLIDES
render archive      '#7c3aed' ARCHIVE
render audio        '#db2777' AUDIO
render video        '#dc2626' VIDEO
render text         '#475569' TEXT
render other        '#6b7280' FILE

# Type badges.
render pdf  '#b91c1c' PDF
render docx '#2563eb' DOCX
render odt  '#2563eb' ODT
render xlsx '#16a34a' XLSX
render ods  '#16a34a' ODS
render pptx '#ea580c' PPTX
render odp  '#ea580c' ODP
render csv  '#16a34a' CSV
render zip  '#7c3aed' ZIP
render json '#475569' JSON
render xml  '#475569' XML
render svg  '#0d9488' SVG
render mp3  '#db2777' MP3
render mp4  '#dc2626' MP4
