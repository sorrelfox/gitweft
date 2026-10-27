#!/usr/bin/env bash
# The release build, offline and reproducible. Run it from a source tree with the vendor
# tarball extracted over it (scripts/dist/sources.py): the RPM, DEB, AppImage and Flatpak
# builds all use it. Writes target/dist/gitweft (or $CARGO_TARGET_DIR/dist/gitweft).
#
# Reproducible: paths under the tree are remapped to /usr/src/gitweft, the timestamp comes
# from .source-date-epoch, and a private CARGO_HOME keeps the user's cargo config out.
set -euo pipefail
root=$(pwd)
if [ ! -f .cargo/config.toml ] || [ ! -d vendor ]; then
    echo "build.sh: run it from a source tree with the vendor tarball extracted" >&2
    exit 1
fi
export SOURCE_DATE_EPOCH=${SOURCE_DATE_EPOCH:-$(cat .source-date-epoch)}
# const-random (ahash's hash keys, through GPUI's XIM crate) bakes random numbers into the
# binary unless it has a seed.
export CONST_RANDOM_SEED=${CONST_RANDOM_SEED:-gitweft-$SOURCE_DATE_EPOCH}
export CARGO_HOME=${CARGO_HOME:-$root/.cargo-home}
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$root/target}
# Encoded (\x1f between flags), so a tree whose path has spaces still works.
# shellcheck disable=SC2206 # RUSTFLAGS is a space-separated list by definition
flags=(${RUSTFLAGS:-} "--remap-path-prefix=$root=/usr/src/gitweft")
export CARGO_ENCODED_RUSTFLAGS=$(IFS=$'\x1f'; echo "${flags[*]}")
unset RUSTFLAGS
# The only C (wayland-backend's log shim) keeps no path once the dist profile strips debug
# info, and CFLAGS can't hold a path with spaces: repro-check builds in one to prove it.
cargo build --frozen --profile dist -p gitweft
ls -l "$CARGO_TARGET_DIR/dist/gitweft"
