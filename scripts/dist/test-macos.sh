#!/usr/bin/env bash
# Mounts the disk image, checks the app's signature and layout, and runs the smoke test on
# a fresh repository from the image itself: under Rosetta too when the Mac has it, so both
# halves of the universal binary run, and with no network (sandbox-exec), as the Linux
# packages are tested.
# Usage: scripts/dist/test-macos.sh DMG
set -euo pipefail
dmg=${1:?usage: test-macos.sh DMG}
work=$(mktemp -d)
mounted=$work/mnt
cleanup() {
    if [ -d "$mounted/Gitweft.app" ]; then
        hdiutil detach -quiet "$mounted" || hdiutil detach -quiet -force "$mounted"
    fi
    rm -rf "$work"
}
trap cleanup EXIT
mkdir "$mounted"
hdiutil attach -quiet -nobrowse -readonly -mountpoint "$mounted" "$dmg"
app=$mounted/Gitweft.app
[ -L "$mounted/Applications" ] || { echo "test-macos.sh: no Applications link in the image" >&2; exit 1; }
plutil -lint "$app/Contents/Info.plist"
for file in MacOS/gitweft Resources/Gitweft.icns Resources/LICENSE Resources/THIRD-PARTY-LICENSES.txt; do
    [ -f "$app/Contents/$file" ] || { echo "test-macos.sh: the app has no Contents/$file" >&2; exit 1; }
done
codesign --verify --strict --verbose=2 "$app"
signature=$(codesign --display --verbose=2 "$app" 2>&1)
grep -E '^(Identifier|Format|Authority|Signature)=' <<< "$signature"
# A Developer ID signature must also satisfy Gatekeeper, and a notarised image carries its ticket.
if grep -q '^Authority=Developer ID Application' <<< "$signature"; then
    spctl --assess --type execute --verbose "$app"
    if xcrun stapler validate "$dmg"; then
        spctl --assess --type open --context context:primary-signature --verbose "$dmg"
    fi
fi

repo=$work/repo
git init -q "$repo"
git -C "$repo" -c user.name=T -c user.email=t@example.com commit -q --allow-empty -m first
offline='(version 1) (allow default) (deny network*)'
binary=$app/Contents/MacOS/gitweft
echo "--- $(lipo -archs "$binary"), from the image, no network:"
# This Mac's architecture, and Intel's under Rosetta when an Apple silicon Mac has it.
archs=("$(uname -m)")
[ "${archs[0]}" = arm64 ] && arch -x86_64 /usr/bin/true 2> /dev/null && archs+=(x86_64)
for arch in "${archs[@]}"; do
    echo "$arch: $(arch "-$arch" "$binary" --version)"
    sandbox-exec -p "$offline" arch "-$arch" "$binary" --headless-smoke "$repo" | tail -1
done
