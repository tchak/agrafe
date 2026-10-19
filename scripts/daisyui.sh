#!/bin/sh
# Assembles the stylesheet the dashboard serves,
# crates/agrafe-dashboard/assets/daisyui.css, from the daisyUI release
# below: its reset, the light and dark themes, and the components the
# templates use, in the order daisyUI's own chunks.css lists them. The
# component files are plain layered CSS, so no Tailwind and no node. The
# result is committed; run this to change the version or the component set.
set -eu
VERSION=5.7.50
CDN="https://cdn.jsdelivr.net/npm/daisyui@$VERSION"
COMPONENTS="table navbar input button link badge alert tab stat select fieldset label"
out="$(dirname "$0")/../crates/agrafe-dashboard/assets/daisyui.css"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

fetch() {
    curl -fsSL "$CDN/$1" -o "$tmp/$(basename "$1")"
}

for f in theme/light.css theme/dark.css base/properties.css base/rootcolor.css base/reset.css base/svg.css; do
    fetch "$f"
done
for c in $COMPONENTS; do
    fetch "components/$c.css"
done
curl -fsSL "$CDN/LICENSE" -o "$out.LICENSE"

{
    echo "/* daisyUI $VERSION, MIT, assembled by scripts/daisyui.sh: base, themes, $COMPONENTS */"
    echo "@layer base, utilities;"
    cat "$tmp/light.css"
    # The dark theme applies by preference, not only by data-theme.
    sed '1s/.*/@media (prefers-color-scheme: dark){:root:not([data-theme]),[data-theme="dark"]{/' "$tmp/dark.css"
    echo "}"
    for f in properties rootcolor reset svg; do
        cat "$tmp/$f.css"
        echo
    done
    for c in $COMPONENTS; do
        cat "$tmp/$c.css"
        echo
    done
} > "$out"
wc -c "$out"
