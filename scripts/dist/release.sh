#!/usr/bin/env bash
# The whole release on this machine, the same steps as the release workflow on GitHub
# (.github/workflows/release.yml):
# sources and notices, the RPM (Fedora 44 container), the DEB (Ubuntu 24.04 container) and
# its installation on Debian 13 and Ubuntu 24.04/26.04 and lintian, the AppImage and its start
# in a bare Debian 13 container, the Flatpak bundle, then SHA256SUMS.
# Usage: scripts/dist/release.sh [--worktree] [--aarch64]
# Everything lands in target/release/VERSION/. `--worktree` takes the working tree instead
# of HEAD (to try changes). `--aarch64` builds the aarch64 RPM, DEB and AppImage instead and
# adds them to that folder, beside an earlier run's x86_64 files (on an x86_64 machine its
# containers need QEMU user-mode emulation: scripts/dist/arch.sh); Flathub builds the
# aarch64 Flatpak itself. Needs python3, podman, mksquashfs, curl, desktop-file-utils and
# appstream; see docs/PACKAGING.md for the images each step uses and how to point them
# elsewhere.
set -euo pipefail
cd "$(dirname "$0")/../.."
sources_flags=()
for flag in "$@"; do
    case $flag in
        --aarch64) export GITWEFT_ARCH=aarch64 ;;
        *) sources_flags+=("$flag") ;;
    esac
done
. scripts/dist/arch.sh
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
out=target/release/$version
step() { printf '\n=== %s\n' "$*"; }

step "versions"
bash scripts/dist/check-version.sh
step "desktop file and metainfo"
bash scripts/dist/validate-metadata.sh
step "sources"
cargo fetch --locked
python3 scripts/dist/sources.py target/dist-src ${sources_flags[@]+"${sources_flags[@]}"}
step "RPM ($arch)"
bash scripts/dist/rpm.sh target/dist-src target/rpm
step "DEB ($deb_arch)"
bash scripts/dist/deb.sh target/dist-src target/deb
bash scripts/dist/test-deb.sh "target/deb/gitweft_${version}-1_$deb_arch.deb"
bash scripts/dist/lint-deb.sh
step "AppImage ($arch)"
bash scripts/dist/appimage.sh target/deb/gitweft target/dist-src target/appimage
bash scripts/dist/test-appimage.sh "target/appimage/Gitweft-$version-$arch.AppImage"

if [ "$arch" = x86_64 ]; then
    step "Flatpak"
    bash scripts/dist/flatpak.sh target/dist-src target/flatpak
fi

step "release files"
if [ "$arch" = x86_64 ]; then
    rm -rf "$out"
    mkdir -p "$out"
    cp target/dist-src/gitweft-"$version".tar.xz target/dist-src/gitweft-"$version"-vendor.tar.xz \
        target/rpm/gitweft-"$version"-*.src.rpm "target/flatpak/Gitweft-$version-x86_64.flatpak" "$out/"
else
    mkdir -p "$out"
fi
cp target/rpm/gitweft-"$version"-*."$arch".rpm "target/deb/gitweft_${version}-1_$deb_arch.deb" \
    "target/appimage/Gitweft-$version-$arch.AppImage" "$out/"
(cd "$out" && rm -f SHA256SUMS && sha256sum -- * > SHA256SUMS)
ls -l "$out"
echo
echo "The release files are in $out/"
