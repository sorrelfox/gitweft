#!/usr/bin/env bash
# The whole release on this machine, the same steps as the release workflow on GitHub
# (.github/workflows/release.yml):
# sources and notices, the RPM (Fedora 44 container), the DEB (Ubuntu 24.04 container) and
# its installation on Debian 13 and Ubuntu 24.04/26.04 and lintian, the AppImage, the Flatpak bundle,
# then SHA256SUMS.
# Usage: scripts/dist/release.sh [--worktree]
# Everything lands in target/release/VERSION/. `--worktree` takes the working tree instead
# of HEAD (to try changes). Needs python3, podman, mksquashfs, curl, desktop-file-utils and
# appstream; see docs/PACKAGING.md for the images each step uses and how to point them
# elsewhere.
set -euo pipefail
cd "$(dirname "$0")/../.."
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
out=target/release/$version
step() { printf '\n=== %s\n' "$*"; }

step "versions"
bash scripts/dist/check-version.sh
step "desktop file and metainfo"
bash scripts/dist/validate-metadata.sh
step "sources"
cargo fetch --locked
python3 scripts/dist/sources.py target/dist-src "$@"
step "RPM"
bash scripts/dist/rpm.sh target/dist-src target/rpm
step "DEB"
bash scripts/dist/deb.sh target/dist-src target/deb
bash scripts/dist/test-deb.sh "target/deb/gitweft_${version}-1_amd64.deb"
bash scripts/dist/lint-deb.sh
step "AppImage"
bash scripts/dist/appimage.sh target/deb/gitweft target/dist-src target/appimage

step "Flatpak"
bash scripts/dist/flatpak.sh target/dist-src target/flatpak

step "release files"
rm -rf "$out"
mkdir -p "$out"
cp target/dist-src/gitweft-"$version".tar.xz target/dist-src/gitweft-"$version"-vendor.tar.xz \
    target/rpm/gitweft-"$version"-*.rpm "target/deb/gitweft_${version}-1_amd64.deb" \
    "target/appimage/Gitweft-$version-x86_64.AppImage" \
    "target/flatpak/Gitweft-$version-x86_64.flatpak" "$out/"
(cd "$out" && sha256sum -- * > SHA256SUMS)
ls -l "$out"
echo
echo "The release files are in $out/"
