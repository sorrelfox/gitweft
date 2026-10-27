#!/usr/bin/env bash
# The AppImage: the DEB's binary (built in Ubuntu 24.04, so older systems run it too), with
# the desktop file, metainfo, the DEB's icons (SVGs and the PNGs scripts/dist/icons.sh
# rendered; .DirIcon is the 256 px PNG), the libraries deb.sh set aside next to the binary
# (usr/lib, where the binary's run path looks; the libraries every host has stay out) and
# the license notices, in a zstd squashfs after the AppImage project's type-2 runtime. git
# comes from the host (Gitweft checks its version).
# Usage: scripts/dist/appimage.sh [BINARY] [DIST_DIR] [OUT_DIR]
# Defaults: target/deb/gitweft (scripts/dist/deb.sh; its icons/ and lib/ must be next to
# it), target/dist-src, target/appimage.
# Needs mksquashfs (squashfs-tools) and curl. The runtime, of the binary's architecture
# (GITWEFT_ARCH: scripts/dist/arch.sh), is pinned by tag and sha256 and downloaded once into
# target/tools: the build's only download (the app never makes one).
# Timestamps come from the sources (SOURCE_DATE_EPOCH), so the image is reproducible.
set -euo pipefail
cd "$(dirname "$0")/../.."
binary=${1:-target/deb/gitweft}
dist=${2:-target/dist-src}
out=${3:-target/appimage}
. scripts/dist/arch.sh
runtime_tag=20251108
case $arch in
    x86_64) runtime_sha256=2fca8b443c92510f1483a883f60061ad09b46b978b2631c807cd873a47ec260d ;;
    aarch64) runtime_sha256=00cbdfcf917cc6c0ff6d3347d59e0ca1f7f45a6df1a428a0d6d8a78664d87444 ;;
esac
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
test -x "$binary" || { echo "appimage.sh: no $binary (run scripts/dist/deb.sh first)" >&2; exit 1; }
icons=$(dirname "$binary")/icons
test -d "$icons/hicolor" || { echo "appimage.sh: no $icons (run scripts/dist/deb.sh first)" >&2; exit 1; }
libs=$(dirname "$binary")/lib
test -d "$libs/copyright" || { echo "appimage.sh: no $libs (run scripts/dist/deb.sh first)" >&2; exit 1; }
SOURCE_DATE_EPOCH=$(tar -xOf "$dist/gitweft-$version-vendor.tar.xz" "gitweft-$version/.source-date-epoch")
export SOURCE_DATE_EPOCH

runtime=target/tools/runtime-$arch-$runtime_tag
if [ ! -f "$runtime" ]; then
    mkdir -p target/tools
    curl -fsSL -o "$runtime.part" \
        "https://github.com/AppImage/type2-runtime/releases/download/$runtime_tag/runtime-$arch"
    mv "$runtime.part" "$runtime"
fi
echo "$runtime_sha256  $runtime" | sha256sum --check --quiet -

rm -rf "$out"
app=$out/AppDir
id=io.github.sorrelfox.Gitweft
install -Dm0755 "$binary" "$app/usr/bin/gitweft"
install -Dm0755 packaging/appimage/AppRun "$app/AppRun"
install -Dm0644 packaging/linux/$id.desktop "$app/usr/share/applications/$id.desktop"
install -Dm0644 packaging/linux/$id.metainfo.xml "$app/usr/share/metainfo/$id.appdata.xml"
cp -r "$icons" "$app/usr/share/icons"
install -Dm0644 -t "$app/usr/lib" "$libs"/*.so.*
for f in "$libs"/copyright/*; do
    install -Dm0644 "$f" "$app/usr/share/licenses/gitweft/$(basename "$f").copyright"
done
install -Dm0644 -t "$app/usr/share/licenses/gitweft" \
    LICENSE assets/fonts/IBMPlex-OFL.txt assets/fonts/Inter-OFL.txt \
    assets/fonts/JetBrainsMono-OFL.txt "$dist/THIRD-PARTY-LICENSES.txt"
ln -s usr/share/applications/$id.desktop "$app/$id.desktop"
ln -s usr/share/icons/hicolor/scalable/apps/$id.svg "$app/$id.svg"
# The AppImage spec wants a PNG here.
ln -s usr/share/icons/hicolor/256x256/apps/$id.png "$app/.DirIcon"
mksquashfs "$app" "$out/image.squashfs" -root-owned -noappend -no-xattrs -comp zstd -quiet
image=$out/Gitweft-$version-$arch.AppImage
cat "$runtime" "$out/image.squashfs" > "$image"
chmod 0755 "$image"
rm -rf "$out/image.squashfs" "$app"
ls -l "$image"
