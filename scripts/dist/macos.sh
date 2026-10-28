#!/usr/bin/env bash
# The macOS app and its disk image. Builds the release binary for Apple silicon and for
# Intel and joins them with lipo into one universal binary, lays out Gitweft.app around it
# (packaging/macos/Info.plist with Cargo.toml's version, the icon made with iconutil, the
# license notices), signs it, and puts it in a compressed disk image next to a link to
# /Applications.
# Usage: scripts/dist/macos.sh [OUT_DIR]
# Writes OUT_DIR (default target/macos)/Gitweft.app and Gitweft-VERSION-macos-universal.dmg.
#
# Signing: ad hoc (`codesign --sign -`), which is what Apple silicon needs to run it at all,
# unless GITWEFT_MACOS_SIGN_IDENTITY names a Developer ID Application certificate in the
# keychain. Then the app and the image are signed with it, the app with the hardened runtime
# and packaging/macos/Gitweft.entitlements, and when notarytool has credentials too
# (GITWEFT_MACOS_NOTARY_PROFILE, a keychain profile made with `xcrun notarytool
# store-credentials`, or GITWEFT_MACOS_NOTARY_APPLE_ID, GITWEFT_MACOS_NOTARY_TEAM_ID and
# GITWEFT_MACOS_NOTARY_PASSWORD, an app-specific password), both are notarised and the
# tickets stapled to them.
#
# Runs on a Mac with Xcode (GPUI compiles its Metal shaders with Xcode's metal compiler,
# which the Command Line Tools alone lack), rustup and Python 3.11 or later. The crates are
# downloaded once, by `cargo fetch --locked`, against Cargo.lock's checksums; the builds
# then run offline. The icon is rendered with resvg, the version pins.env names, built into
# target/tools on the first run.
set -euo pipefail
cd "$(dirname "$0")/../.."
[ "$(uname -s)" = Darwin ] || { echo "macos.sh: run it on a Mac" >&2; exit 1; }
root=$(pwd)
out=$(mkdir -p "${1:-target/macos}" && cd "${1:-target/macos}" && pwd)
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
. scripts/dist/pins.env
id=io.github.sorrelfox.Gitweft
targets=(aarch64-apple-darwin x86_64-apple-darwin)
identity=${GITWEFT_MACOS_SIGN_IDENTITY:-}
# The notary password stays out of the environment of the build and its build scripts.
notary_password=${GITWEFT_MACOS_NOTARY_PASSWORD:-}
unset GITWEFT_MACOS_NOTARY_PASSWORD
step() { printf '\n=== %s\n' "$*"; }

# GPUI's shaders are compiled for 10.15.7 (-mmacosx-version-min in its build script), so
# that is the oldest macOS the app runs on; the rest of the build is told the same.
export MACOSX_DEPLOYMENT_TARGET=10.15.7
# No path of this machine in the binary (panic messages carry source paths).
cargo_home=${CARGO_HOME:-$HOME/.cargo}
# shellcheck disable=SC2206 # RUSTFLAGS is a space-separated list by definition
flags=(${RUSTFLAGS:-} "--remap-path-prefix=$root=/usr/src/gitweft" "--remap-path-prefix=$cargo_home=/cargo")
export CARGO_ENCODED_RUSTFLAGS=$(IFS=$'\x1f'; echo "${flags[*]}")
unset RUSTFLAGS

step "build (${targets[*]})"
rustup target add "${targets[@]}"
cargo fetch --locked
for target in "${targets[@]}"; do
    cargo build --frozen --profile dist -p gitweft --target "$target"
done
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
lipo -create -output "$work/gitweft" "target/${targets[0]}/dist/gitweft" "target/${targets[1]}/dist/gitweft"
echo "gitweft: $(lipo -archs "$work/gitweft")"

step "icon"
resvg=target/tools/resvg-$GITWEFT_PIN_RESVG_VERSION/bin/resvg
if [ ! -x "$resvg" ]; then
    cargo install --locked --quiet --version "$GITWEFT_PIN_RESVG_VERSION" \
        --root "target/tools/resvg-$GITWEFT_PIN_RESVG_VERSION" resvg
fi
iconset=$work/Gitweft.iconset
mkdir "$iconset"
svg=packaging/linux/icons/hicolor/scalable/apps/$id.svg
for size in 16 32 128 256 512; do
    "$resvg" -w "$size" -h "$size" "$svg" "$iconset/icon_${size}x$size.png"
    "$resvg" -w $((size * 2)) -h $((size * 2)) "$svg" "$iconset/icon_${size}x$size@2x.png"
done
iconutil --convert icns --output "$work/Gitweft.icns" "$iconset"

step "Gitweft.app"
app=$out/Gitweft.app
rm -rf "$app" "$out"/Gitweft-*.dmg
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
install -m0755 "$work/gitweft" "$app/Contents/MacOS/gitweft"
sed "s/@VERSION@/$version/g" packaging/macos/Info.plist > "$app/Contents/Info.plist"
plutil -lint "$app/Contents/Info.plist"
install -m0644 "$work/Gitweft.icns" "$app/Contents/Resources/Gitweft.icns"
install -m0644 LICENSE assets/fonts/IBMPlex-OFL.txt assets/fonts/Inter-OFL.txt \
    assets/fonts/JetBrainsMono-OFL.txt "$app/Contents/Resources/"
python3 -X utf8 scripts/dist/notices.py "$app/Contents/Resources/THIRD-PARTY-LICENSES.txt" "${targets[@]}"
# codesign refuses files with extended attributes (Finder info, quarantine).
xattr -cr "$app"

# notarytool's credentials, when there are any: a keychain profile, or an Apple ID. (A
# flag beside the array: macOS's bash 3.2 calls an empty array unbound under `set -u`.)
notarising=no
if [ -n "${GITWEFT_MACOS_NOTARY_PROFILE:-}" ]; then
    notarising=yes notary=(--keychain-profile "$GITWEFT_MACOS_NOTARY_PROFILE")
elif [ -n "${GITWEFT_MACOS_NOTARY_APPLE_ID:-}" ] && [ -n "$notary_password" ]; then
    notarising=yes notary=(--apple-id "$GITWEFT_MACOS_NOTARY_APPLE_ID"
        --team-id "${GITWEFT_MACOS_NOTARY_TEAM_ID:?the team ID of the Apple ID}" --password "$notary_password")
fi
# Sends FILE (a zip or a disk image) to Apple's notary service and waits for its verdict;
# on a rejection, prints the service's log, which says what was wrong.
notarise() {
    local result status
    result=$(xcrun notarytool submit "$1" "${notary[@]}" --wait --output-format json)
    status=$(json_field "$result" status)
    echo "notarisation of $(basename "$1"): $status"
    if [ "$status" != Accepted ]; then
        xcrun notarytool log "$(json_field "$result" id)" "${notary[@]}" || true
        exit 1
    fi
}
json_field() { python3 -c 'import json, sys; print(json.loads(sys.argv[1]).get(sys.argv[2], ""))' "$1" "$2"; }

if [ -z "$identity" ]; then
    step "ad-hoc signature"
    codesign --force --sign - "$app"
else
    step "signature: $identity"
    codesign --force --options runtime --timestamp --entitlements packaging/macos/Gitweft.entitlements \
        --sign "$identity" "$app"
    if [ "$notarising" = yes ]; then
        # The app gets a ticket of its own, so that it opens offline once copied out of
        # the image too.
        ditto -c -k --keepParent "$app" "$work/Gitweft.zip"
        notarise "$work/Gitweft.zip"
        xcrun stapler staple "$app"
    fi
fi
codesign --verify --strict --verbose=2 "$app"

step "disk image"
dmg=$out/Gitweft-$version-macos-universal.dmg
mkdir "$work/image"
cp -R "$app" "$work/image/"
ln -s /Applications "$work/image/Applications"
# hdiutil sometimes fails with "Resource busy" on a busy machine; a second try works.
for attempt in 1 2 3; do
    hdiutil create -volname Gitweft -srcfolder "$work/image" -fs HFS+ -format UDZO \
        -imagekey zlib-level=9 -ov "$dmg" && break
    [ "$attempt" = 3 ] && exit 1
    sleep 10
done
if [ -n "$identity" ]; then
    codesign --force --timestamp --sign "$identity" "$dmg"
    if [ "$notarising" = yes ]; then
        notarise "$dmg"
        xcrun stapler staple "$dmg"
    fi
fi
ls -l "$dmg"
