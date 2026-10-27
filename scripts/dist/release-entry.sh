#!/usr/bin/env bash
# The one source of the release entries: the metainfo's newest <release>. Prints its date
# (YYYY-MM-DD) and its note (the text of its description's first <p>), one per line.
# check-version.sh holds the RPM spec's newest %changelog entry to them, and deb.sh writes
# the DEB's changelog from them.
# Usage: scripts/dist/release-entry.sh [ROOT]   (default: this repository)
set -euo pipefail
root=${1:-$(cd "$(dirname "$0")/../.." && pwd)}
metainfo=$root/packaging/linux/io.github.sorrelfox.Gitweft.metainfo.xml
# From the first <release> to its end.
entry=$(sed -n '/<release /,/<\/release>/p' "$metainfo" | sed '/<\/release>/q')
date=$(sed -n 's/.*<release [^>]*date="\([^"]*\)".*/\1/p' <<< "$entry" | head -1)
note=$(tr '\n' ' ' <<< "$entry" | sed -e 's/<\/p>.*//' -e 's/.*<p>//' \
    -e 's/^[[:space:]]*//' -e 's/[[:space:]]*$//' -e 's/[[:space:]]\{1,\}/ /g')
case $note in *'<'* | '') note= ;; esac
if [ -z "$date" ] || [ -z "$note" ]; then
    echo "release-entry.sh: $metainfo: the newest <release> needs a date and a <p> note" >&2
    exit 1
fi
printf '%s\n%s\n' "$date" "$note"
