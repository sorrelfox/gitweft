#!/usr/bin/env bash
# Puts the Flatpak build's SDK, Platform and Rust extension at the commits pins.env pins,
# so every Flatpak build of a version uses the same toolchain and runtime.
# What isn't installed at all is installed for the user from Flathub (GITWEFT_FLATPAK_DEPS
# names another remote). What is installed at another commit is moved to the pinned one
# only with GITWEFT_FLATPAK_DEPS set; without it, that difference is an error that says
# so. scripts/dist/flatpak.sh runs it first.
set -euo pipefail
cd "$(dirname "$0")/../.."
. scripts/dist/pins.env
pin() {
    local ref=$1 want=$2 have remote
    have=$(flatpak info --user --show-commit "$ref" 2> /dev/null || true)
    [ "$have" = "$want" ] && return
    remote=${GITWEFT_FLATPAK_DEPS:-}
    if [ -z "$remote" ] && [ -z "$have" ]; then
        # Not installed: it comes from Flathub, added as a user remote when it isn't one
        # yet, so a first `just flatpak` needs nothing more.
        remote=flathub
        flatpak remote-add --user --if-not-exists flathub https://dl.flathub.org/repo/flathub.flatpakrepo
    fi
    if [ -z "$remote" ]; then
        # Installed at another commit: moving it is a choice (other apps may use it).
        echo "flatpak-pins.sh: $ref is at $have, pins.env pins $want;" \
            "run with GITWEFT_FLATPAK_DEPS=flathub to move it there" >&2
        return 1
    fi
    [ -n "$have" ] || flatpak install --user --noninteractive -y "$remote" "$ref" > /dev/null
    flatpak update --user --noninteractive -y --commit="$want" "$ref" > /dev/null
    have=$(flatpak info --user --show-commit "$ref")
    [ "$have" = "$want" ] || { echo "flatpak-pins.sh: $ref stayed at $have, not $want" >&2; return 1; }
    echo "flatpak-pins.sh: $ref at $want"
}
status=0
pin org.freedesktop.Sdk//25.08 "$GITWEFT_PIN_FLATPAK_SDK" || status=1
pin org.freedesktop.Platform//25.08 "$GITWEFT_PIN_FLATPAK_PLATFORM" || status=1
pin org.freedesktop.Sdk.Extension.rust-stable//25.08 "$GITWEFT_PIN_FLATPAK_RUST" || status=1
exit $status
