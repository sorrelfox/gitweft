#!/usr/bin/env bash
# Installs the DEB in Debian 13, Ubuntu 24.04 and Ubuntu 26.04 containers (apt downloads its
# dependencies), then runs the installed binary's offline smoke test in a container with
# no network at all. Usage: scripts/dist/test-deb.sh [DEB]
# DEB defaults to target/deb/gitweft_*_amd64.deb (scripts/dist/deb.sh). The images are
# debian:13, ubuntu:24.04 and ubuntu:26.04 at pins.env's digests (GITWEFT_DEB_TEST_IMAGES,
# space-separated, picks
# others); podman runs them (CONTAINER=docker works too), and they're removed afterwards.
set -euo pipefail
cd "$(dirname "$0")/../.."
deb=$(realpath "${1:-$(ls target/deb/gitweft_*_amd64.deb | head -1)}")
engine=${CONTAINER:-podman}
. scripts/dist/pins.env
images=${GITWEFT_DEB_TEST_IMAGES:-$GITWEFT_PIN_DEBIAN_13 $GITWEFT_PIN_UBUNTU_24_04 $GITWEFT_PIN_UBUNTU_26_04}
name=gitweft-deb-test-$$
cleanup() {
    "$engine" rm -f "$name" > /dev/null 2>&1 || true
    "$engine" rmi -f "localhost/$name" > /dev/null 2>&1 || true
}
trap cleanup EXIT
for image in $images; do
    echo "=== $image"
    "$engine" run --name "$name" -v "$deb:/pkg.deb:ro,Z" "$image" bash -euc '
        export DEBIAN_FRONTEND=noninteractive
        apt-get -qq update
        apt-get -qq install -y /pkg.deb > /dev/null
        dpkg -s gitweft | sed -n "s/^Version: /installed /p"
        git init -q /tmp/repo
        git -C /tmp/repo -c user.name=T -c user.email=t@example.com commit -q --allow-empty -m first
    '
    "$engine" commit "$name" "localhost/$name" > /dev/null
    "$engine" rm "$name" > /dev/null
    "$engine" run --rm --network none "localhost/$name" bash -euc '
        gitweft --version
        gitweft --headless-smoke /tmp/repo | tail -1
    '
    "$engine" rmi -f "localhost/$name" > /dev/null
done
