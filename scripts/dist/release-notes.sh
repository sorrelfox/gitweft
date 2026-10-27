#!/usr/bin/env bash
# A release's notes: its entry in CHANGELOG.md, without the heading (the GitHub Release has
# its own title). The release workflow gives them to the draft Release.
# Usage: scripts/dist/release-notes.sh VERSION
set -euo pipefail
cd "$(dirname "$0")/../.."
version=${1:?usage: release-notes.sh VERSION}
notes=$(awk -v heading="## $version " '
    index($0, heading) == 1 { found = 1; next }
    found && /^## / { exit }
    found { print }
' CHANGELOG.md | sed -e '/./,$!d')
[ -n "$notes" ] || { echo "release-notes.sh: CHANGELOG.md has no entry for $version" >&2; exit 1; }
printf '%s\n' "$notes"
