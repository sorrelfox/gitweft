#!/bin/sh
# Installs rustup (no toolchain) from rustup-init of the pinned version, checked against its
# pinned SHA-256 before it runs, instead of piping sh.rustup.rs into sh.
# Usage: rustup-init.sh VERSION SHA256 [RUSTUP-INIT OPTIONS…]
# (Linux on x86_64 or aarch64: SHA256 is that of this machine's rustup-init; needs curl)
set -eu
version=$1 sum=$2
shift 2
# Named rustup-init: rustup picks what it does by the name it runs under.
dir=$(mktemp -d)
trap 'rm -rf "$dir"' EXIT
file=$dir/rustup-init
curl -fsSL --proto '=https' -o "$file" \
    "https://static.rust-lang.org/rustup/archive/$version/$(uname -m)-unknown-linux-gnu/rustup-init"
echo "$sum  $file" | sha256sum -c --quiet - || { echo "rustup-init $version: checksum mismatch" >&2; exit 1; }
chmod +x "$file"
"$file" "$@"
