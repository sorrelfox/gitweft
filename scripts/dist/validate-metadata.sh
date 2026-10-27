#!/usr/bin/env bash
# The desktop file and the metainfo, validated as software centres and Flathub do:
# desktop-file-validate, and appstreamcli at its pedantic level (its hints are shown, and
# fail nothing). Part of `just release` and the release workflow; the packages validate
# their installed copies again.
# Usage: scripts/dist/validate-metadata.sh. Needs desktop-file-utils and appstream.
set -euo pipefail
cd "$(dirname "$0")/../.."
id=io.github.sorrelfox.Gitweft
desktop-file-validate "packaging/linux/$id.desktop"
appstreamcli validate --pedantic --no-net "packaging/linux/$id.metainfo.xml"
