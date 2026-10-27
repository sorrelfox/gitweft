#!/usr/bin/env bash
# Builds the release sources twice, in two folders at different paths, and compares the two
# binaries byte for byte. Usage: scripts/dist/repro-check.sh [DIST_DIR] [WORK_DIR]
# DIST_DIR holds sources.py's tarballs (default target/dist-src); WORK_DIR is scratch
# (default target/repro, removed first).
set -euo pipefail
cd "$(dirname "$0")/../.."
dist=$(realpath "${1:-target/dist-src}")
work=${2:-target/repro}
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
rm -rf "$work"
bins=()
for side in a "b/a longer path"; do
    mkdir -p "$work/$side"
    tar -xf "$dist/gitweft-$version.tar.xz" -C "$work/$side"
    tar -xf "$dist/gitweft-$version-vendor.tar.xz" -C "$work/$side"
    (cd "$work/$side/gitweft-$version" && bash scripts/dist/build.sh > /dev/null)
    bins+=("$work/$side/gitweft-$version/target/dist/gitweft")
done
sha256sum "${bins[@]}"
if cmp -s "${bins[0]}" "${bins[1]}"; then
    echo "reproducible: the two builds are identical"
else
    echo "NOT reproducible: the two builds differ" >&2
    exit 1
fi
