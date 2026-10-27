#!/usr/bin/env bash
# Starts the AppImage where none of the libraries it bundles exist: a Debian 13 container
# with only libxcb1 and git installed. The AppImage excludelist promises libxcb,
# libwayland-client, libEGL, glibc and libgcc_s on every host and nothing else, so a library
# the image forgot fails the start here. Then the offline smoke test, in a container with no
# network at all. Usage: scripts/dist/test-appimage.sh [APPIMAGE]
# APPIMAGE defaults to target/appimage/Gitweft-*-ARCH.AppImage (scripts/dist/appimage.sh; the
# containers run as its architecture, GITWEFT_ARCH: scripts/dist/arch.sh).
# The image is debian:13 at pins.env's digest (GITWEFT_APPIMAGE_TEST_IMAGE picks another);
# podman runs it (CONTAINER=docker works too), and it is removed afterwards. The runtime
# extracts the image instead of mounting it, because a container has no FUSE.
set -euo pipefail
cd "$(dirname "$0")/../.."
engine=${CONTAINER:-podman}
. scripts/dist/pins.env
. scripts/dist/arch.sh
appimage=$(realpath "${1:-$(ls target/appimage/Gitweft-*-"$arch".AppImage | head -1)}")
image=${GITWEFT_APPIMAGE_TEST_IMAGE:-$GITWEFT_PIN_DEBIAN_13}
name=gitweft-appimage-test-$$
cleanup() {
    "$engine" rm -f "$name" > /dev/null 2>&1 || true
    "$engine" rmi -f "localhost/$name" > /dev/null 2>&1 || true
}
trap cleanup EXIT
echo "=== $image, with libxcb1 and git only"
"$engine" run --name "$name" --platform "$platform" "$image" bash -euc '
    export DEBIAN_FRONTEND=noninteractive
    apt-get -qq update
    apt-get -qq install -y --no-install-recommends libxcb1 git > /dev/null
    git init -q /tmp/repo
    git -C /tmp/repo -c user.name=T -c user.email=t@example.com commit -q --allow-empty -m first
'
"$engine" commit "$name" "localhost/$name" > /dev/null
"$engine" rm "$name" > /dev/null
"$engine" run --rm --platform "$platform" --network none -v "$appimage:/Gitweft.AppImage:ro,Z" \
    "localhost/$name" bash -euc '
    cd /tmp
    /Gitweft.AppImage --appimage-extract > /dev/null
    # Where each library comes from: the bundle, or the host (none may be missing).
    ldd squashfs-root/usr/bin/gitweft | sed -n "s/^[[:space:]]*\\([^ ]*\\) => \\([^ ]*\\).*/\\1 from \\2/p" \
        | sed "s|/tmp/squashfs-root/usr/lib/.*|the bundle|"
    if ldd squashfs-root/usr/bin/gitweft | grep -q "not found"; then
        echo "test-appimage.sh: a library is missing" >&2; exit 1
    fi
    squashfs-root/AppRun --version
    squashfs-root/AppRun --headless-smoke /tmp/repo | tail -1
'
