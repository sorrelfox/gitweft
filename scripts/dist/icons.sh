#!/usr/bin/env bash
# The app's icons for a package, into DATADIR/icons/hicolor: the scalable and symbolic SVGs as
# they are, and PNGs rendered from the scalable one at the sizes desktops look for (the
# AppImage's .DirIcon, older launchers and software centres want a PNG).
# Usage: scripts/dist/icons.sh DATADIR   (e.g. $root/usr/share, or /app/share in the Flatpak)
# Needs rsvg-convert: librsvg2-tools on Fedora, librsvg2-bin on Debian and Ubuntu; the
# Freedesktop SDK has it.
set -euo pipefail
data=$(realpath -m "${1:?usage: icons.sh DATADIR}")
cd "$(dirname "$0")/../.."
id=io.github.sorrelfox.Gitweft
src=packaging/linux/icons/hicolor
out=$data/icons/hicolor
install -Dm0644 "$src/scalable/apps/$id.svg" "$out/scalable/apps/$id.svg"
install -Dm0644 "$src/symbolic/apps/$id-symbolic.svg" "$out/symbolic/apps/$id-symbolic.svg"
for size in 16 24 32 48 64 128 256 512; do
    mkdir -p "$out/${size}x$size/apps"
    rsvg-convert -w "$size" -h "$size" -o "$out/${size}x$size/apps/$id.png" \
        "$src/scalable/apps/$id.svg"
    chmod 0644 "$out/${size}x$size/apps/$id.png"
done
