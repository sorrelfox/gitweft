#!/usr/bin/env bash
# The Flatpak. Builds packaging/flatpak/io.github.sorrelfox.Gitweft.yml in the Freedesktop
# SDK with the rust-stable extension, from sources.py's tarballs (flatpak-builder gives the
# build no network), into a local repository; bundles it; then runs the built app's smoke
# test in the sandbox with no network.
# Usage: scripts/dist/flatpak.sh [DIST_DIR] [OUT_DIR]
# Writes OUT_DIR (default target/flatpak)/Gitweft-VERSION-x86_64.flatpak. Needs
# flatpak-builder and, installed at the commits pins.env pins (flatpak-pins.sh checks),
# org.freedesktop.Sdk//25.08, org.freedesktop.Platform//25.08 and
# org.freedesktop.Sdk.Extension.rust-stable//25.08; GITWEFT_FLATPAK_DEPS=flathub installs
# them there (for the user) from that remote.
set -euo pipefail
cd "$(dirname "$0")/../.."
dist=$(realpath "${1:-target/dist-src}")
out=$(mkdir -p "${2:-target/flatpak}" && realpath "${2:-target/flatpak}")
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
id=io.github.sorrelfox.Gitweft

# The manifest names the tarballs relative to itself (target/dist-src) and by @VERSION@:
# copy it next to the tarballs actually used, with this version's names.
manifest=$out/$id.yml
sed -e "s#path: ../../target/dist-src/#path: $dist/#" -e "s#@VERSION@#$version#g" \
    packaging/flatpak/$id.yml > "$manifest"
# The SDK, Platform and Rust extension at their pinned commits. Not flatpak-builder's
# --install-deps-from: it would update them past the pins.
bash scripts/dist/flatpak-pins.sh
# --disable-cache: flatpak-builder keys its cache on the manifest, not on a local archive's
# contents, so a rebuilt tarball would otherwise be skipped.
flatpak-builder --user --force-clean --disable-cache --state-dir="$out/state" --repo="$out/repo" \
    "$out/build" "$manifest" > "$out/build.log" 2>&1 || { tail -40 "$out/build.log"; exit 1; }
flatpak build-bundle --runtime-repo=https://dl.flathub.org/repo/flathub.flatpakrepo \
    "$out/repo" "$out/Gitweft-$version-x86_64.flatpak" "$id"

# The smoke test as users run it: the bundle installed (for the user) and run on the
# Platform runtime with no network. (`flatpak build` would run on the SDK, which has its own
# git and so can't tell whether the app brings one.) An install that was there before
# stays; one made here is removed again.
bundle=$out/Gitweft-$version-x86_64.flatpak
repo=$out/smoke-repo
rm -rf "$repo"
git init -q "$repo"
git -C "$repo" -c user.name=T -c user.email=t@example.com commit -q --allow-empty -m first
had=$(flatpak info --user "$id" > /dev/null 2>&1 && echo yes || echo no)
# --no-deps: the bundle names Flathub as its runtime's repository, and installing it updated
# the Platform past its pin, so the next build stopped at flatpak-pins.sh. The pinned
# Platform is in place already (flatpak-pins.sh ran first).
flatpak install --user --noninteractive -y --reinstall --no-deps "$bundle" > /dev/null
echo "--- installed, no network:"
flatpak run --unshare=network --filesystem="$repo" "$id" --version
flatpak run --unshare=network --filesystem="$repo" "$id" --headless-smoke "$repo" | grep -E '^(git|ok)'
[ "$had" = yes ] || flatpak uninstall --user --noninteractive -y "$id" > /dev/null
ls -l "$bundle"
