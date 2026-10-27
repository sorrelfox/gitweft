#!/usr/bin/env bash
# Flathub's manifest for this version. Flathub builds the app itself, from sources it
# downloads, so packaging/flatpak/io.github.sorrelfox.Gitweft.yml is written with the
# release's tarballs as URLs on the GitHub Release, each with its SHA-256, in place of the
# local paths. Also writes flathub.json, naming the architectures Flathub builds: those
# whose crates the vendor tarball holds (sources.py's TARGETS).
# Usage: scripts/dist/flathub.sh DIST_DIR [OUT_DIR]
# DIST_DIR holds this version's two tarballs, exactly as published (the release workflow
# runs this on the files it attaches). Writes OUT_DIR (default target/flathub)/
# {io.github.sorrelfox.Gitweft.yml,flathub.json}: the files of the app's Flathub repository.
set -euo pipefail
cd "$(dirname "$0")/../.."
dist=$(realpath "$1")
out=$(mkdir -p "${2:-target/flathub}" && realpath "${2:-target/flathub}")
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
repository=$(sed -n 's/^repository = "\(.*\)"/\1/p' Cargo.toml | head -1)
id=io.github.sorrelfox.Gitweft
base=$repository/releases/download/v$version

sums=()
for name in "gitweft-$version.tar.xz" "gitweft-$version-vendor.tar.xz"; do
    [ -f "$dist/$name" ] || { echo "flathub.sh: $dist/$name is missing" >&2; exit 1; }
    sums+=("$name=$(sha256sum "$dist/$name" | cut -d' ' -f1)")
done

# The template's own header (the comment block on top) describes the local build: it gives
# way to one for Flathub. Each `path:` source becomes `url:` and `sha256:`.
{
    echo "# Gitweft $version on Flathub, from the release's source tarballs. Written by"
    echo "# scripts/dist/flathub.sh from packaging/flatpak/$id.yml in"
    echo "# $repository: change that file, not this one."
    sed -e "s#@VERSION@#$version#g" "packaging/flatpak/$id.yml" | awk -v header=1 -v base="$base" -v sums="${sums[*]}" '
        BEGIN { n = split(sums, pairs, " "); for (i = 1; i <= n; i++) { split(pairs[i], kv, "="); sum[kv[1]] = kv[2] } }
        header && /^#/ { next }
        { header = 0 }
        match($0, /^ *path: \.\.\/\.\.\/target\/dist-src\//) {
            indent = substr($0, 1, index($0, "path:") - 1)
            name = substr($0, RLENGTH + 1)
            if (!(name in sum)) { print "flathub.sh: no tarball for " name > "/dev/stderr"; exit 1 }
            print indent "url: " base "/" name
            print indent "sha256: " sum[name]
            next
        }
        { print }
    '
} > "$out/$id.yml"
printf '{\n  "only-arches": ["x86_64", "aarch64"]\n}\n' > "$out/flathub.json"

grep -qE '^ *path: ' "$out/$id.yml" && { echo "flathub.sh: a local path is left in the manifest" >&2; exit 1; }
echo "flathub.sh: $out/$id.yml and flathub.json, sources from $base/"
