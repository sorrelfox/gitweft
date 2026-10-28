#!/usr/bin/env bash
# Runs the installer without questions for the current user, into a temporary folder,
# checks its Start menu entry, runs the smoke test on a fresh repository with the installed
# app, and uninstalls it again, checking that nothing stays behind; then the same smoke test
# from the unpacked zip. The installer's part is skipped when Gitweft is installed already,
# because it would replace that installation.
# Usage: scripts/dist/test-windows.sh SETUP_EXE ZIP   (on Windows, in Git Bash)
set -euo pipefail
setup=${1:?usage: test-windows.sh SETUP_EXE ZIP}
zip=${2:?usage: test-windows.sh SETUP_EXE ZIP}
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
native() { MSYS2_ARG_CONV_EXCL='*' "$@"; }
fail() { echo "test-windows.sh: $*" >&2; exit 1; }

repo=$work/repo
git init -q "$repo"
git -C "$repo" -c user.name=T -c user.email=t@example.com commit -q --allow-empty -m first
smoke() {
    local version
    version=$("$1" --version)
    [ -n "$version" ] || fail "$1 --version printed nothing"
    echo "$version"
    "$1" --headless-smoke "$(cygpath -w "$repo")" | tail -1
}

# Inno Setup's uninstall key: the AppId with "_is1".
key='HKCU\Software\Microsoft\Windows\CurrentVersion\Uninstall\io.github.sorrelfox.Gitweft_is1'
if native reg query "$key" > /dev/null 2>&1; then
    echo "--- Gitweft is installed already: the installer is not tested"
else
    echo "--- installed for this user:"
    app=$work/installed
    native "$setup" /VERYSILENT /SUPPRESSMSGBOXES /NORESTART /CURRENTUSER "/DIR=$(cygpath -w "$app")" \
        "/LOG=$(cygpath -w "$work/setup.log")" || { cat "$work/setup.log"; fail "the installer failed"; }
    shortcut="$(cygpath -u "$APPDATA")/Microsoft/Windows/Start Menu/Programs/Gitweft.lnk"
    [ -f "$shortcut" ] || fail "no Start menu entry"
    native reg query "$key" /v DisplayVersion | grep DisplayVersion
    smoke "$app/gitweft.exe"
    native "$app/unins000.exe" /VERYSILENT /SUPPRESSMSGBOXES /NORESTART
    # The uninstaller runs from a copy of itself in the temporary folder and returns at once.
    for _ in $(seq 60); do
        [ -e "$app/gitweft.exe" ] || [ -e "$shortcut" ] || break
        sleep 1
    done
    [ ! -e "$app/gitweft.exe" ] && [ ! -e "$shortcut" ] || fail "the uninstaller left the app or its shortcut"
    ! native reg query "$key" > /dev/null 2>&1 || fail "the uninstaller left its registry key"
    echo "uninstalled"
fi

echo "--- from the zip:"
mkdir "$work/zip"
"$(cygpath -u -S)/tar.exe" -x -f "$(cygpath -w "$zip")" -C "$(cygpath -w "$work/zip")"
smoke "$(echo "$work"/zip/Gitweft-*/gitweft.exe)"
