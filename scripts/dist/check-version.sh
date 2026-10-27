#!/usr/bin/env bash
# Checks that the packaging files carry Cargo.toml's version, so a release never ships
# mismatched ones: the RPM spec's Version and its newest %changelog entry, and the
# metainfo's newest <release>. That <release> is the one source of the release entries
# (scripts/dist/release-entry.sh): the spec's newest %changelog entry must have its day and
# say its note (deb.sh writes the DEB's changelog from it). The Flatpak manifest and the
# DEB's control.in take the version from Cargo.toml themselves (@VERSION@, filled in by
# flatpak.sh and deb.sh), so they must keep the placeholder.
# Usage: scripts/dist/check-version.sh [ROOT]   (default: this repository)
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
root=${1:-$(cd "$here/../.." && pwd)}
cd "$root"
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
spec=packaging/rpm/gitweft.spec
metainfo=packaging/linux/io.github.sorrelfox.Gitweft.metainfo.xml
manifest=packaging/flatpak/io.github.sorrelfox.Gitweft.yml
control=packaging/deb/control.in
problems=()

spec_version=$(sed -n 's/^Version:[[:space:]]*//p' "$spec" | head -1)
[ "$spec_version" = "$version" ] || problems+=("$spec: Version is ${spec_version:-missing}")
# "* Tue Oct 27 2026 sorrelfox <…> - 1.0.0-1": the version before the release.
changelog=$(sed -n '/^%changelog/,$p' "$spec" | sed -n 's/^\* .* - \([^ -]*\)-[^ ]*$/\1/p' | head -1)
[ "$changelog" = "$version" ] || problems+=("$spec: the newest %changelog entry is for ${changelog:-nothing}")
release=$(sed -n 's/.*<release version="\([^"]*\)".*/\1/p' "$metainfo" | head -1)
[ "$release" = "$version" ] || problems+=("$metainfo: the newest <release> is ${release:-missing}")
if { read -r release_date && read -r release_note; } < <(bash "$here/release-entry.sh" "$root"); then
    entry=$(sed -n '/^%changelog/,$p' "$spec" | awk '/^\* /{n++} n==1')
    entry_day=$(head -1 <<< "$entry" | sed -n 's/^\* \([A-Z][a-z]* [A-Z][a-z]* [0-9]* [0-9]*\) .*/\1/p')
    entry_date=$([ -n "$entry_day" ] && LC_ALL=C date -u -d "$entry_day" +%F 2> /dev/null || true)
    entry_note=$(tail -n +2 <<< "$entry" | tr '\n' ' ' \
        | sed -e 's/^[[:space:]]*-[[:space:]]*//' -e 's/[[:space:]]*$//' -e 's/[[:space:]]\{1,\}/ /g')
    [ "$entry_date" = "$release_date" ] \
        || problems+=("$spec: the newest %changelog entry is from ${entry_day:-no day}, the metainfo's newest <release> from $release_date")
    [ "$entry_note" = "$release_note" ] \
        || problems+=("$spec: the newest %changelog entry says \"$entry_note\", the metainfo's newest <release> \"$release_note\"")
    # CHANGELOG.md: its newest "## <version> (<date>)" and the paragraph under it (#56).
    news=CHANGELOG.md
    news_head=$(sed -n 's/^## \([^ ]*\) (\([0-9-]*\))$/\1 \2/p' "$news" 2> /dev/null | head -1)
    [ "$news_head" = "$version $release_date" ] \
        || problems+=("$news: the newest entry is \"${news_head:-missing}\", not \"$version $release_date\"")
    news_note=$(awk '/^## /{n++; next} n==1 {if (NF) {print; seen=1} else if (seen) exit}' "$news" 2> /dev/null \
        | tr '\n' ' ' | sed -e 's/[[:space:]]*$//' -e 's/[[:space:]]\{1,\}/ /g')
    [ "$news_note" = "$release_note" ] \
        || problems+=("$news: the newest entry says \"$news_note\", the metainfo's newest <release> \"$release_note\"")
else
    problems+=("$metainfo: the newest <release> has no date or no <p> note")
fi
for template in "$manifest" "$control"; do
    grep -q '@VERSION@' "$template" || problems+=("$template: lost its @VERSION@ placeholder")
done

# A CI image file, where there is one, names the Ubuntu 24.04 image pins.env pins.
. scripts/dist/pins.env
for file in ci/Dockerfile; do
    [ -f "$file" ] || continue
    grep -qF "$GITWEFT_PIN_UBUNTU_24_04" "$file" \
        || problems+=("$file: not at pins.env's Ubuntu 24.04 digest ($GITWEFT_PIN_UBUNTU_24_04)")
done

if [ ${#problems[@]} -gt 0 ]; then
    echo "check-version.sh: Cargo.toml says $version, but:" >&2
    printf '  %s\n' "${problems[@]}" >&2
    exit 1
fi
echo "packaging versions: $version"
