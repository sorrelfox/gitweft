#!/usr/bin/env bash
# The Windows installer and portable zip. Builds the release binary for x86_64 (its icon and
# version details come from crates/gitweft/build.rs), writes the license notices of the
# crates in it, packs it with them into a zip, and compiles packaging/windows/gitweft.iss
# with Inno Setup into an installer.
# Usage: scripts/dist/windows.sh [OUT_DIR]
# Writes OUT_DIR (default target/windows)/Gitweft-VERSION-windows-x86_64-setup.exe and
# Gitweft-VERSION-windows-x86_64.zip.
#
# Signing: none, unless GITWEFT_WINDOWS_CERTIFICATE names a code-signing certificate, a .pfx
# file with its password in GITWEFT_WINDOWS_CERTIFICATE_PASSWORD. Then signtool, from the
# Windows SDK (SIGNTOOL names another), signs gitweft.exe and the installer, with a timestamp
# from GITWEFT_WINDOWS_TIMESTAMP_URL (default DigiCert's).
#
# Runs on Windows, in Git Bash, with rustup, the Visual Studio build tools (the MSVC linker
# and the Windows SDK), Python 3.11 or later and Inno Setup 6.3 or later (ISCC names its
# compiler when it is not in one of the usual places). The crates are downloaded once, by
# `cargo fetch --locked`, against Cargo.lock's checksums; the build then runs offline.
set -euo pipefail
cd "$(dirname "$0")/../.."
case $(uname -s) in
    MINGW* | MSYS*) ;;
    *) echo "windows.sh: run it on Windows, in Git Bash" >&2; exit 1 ;;
esac
root=$(pwd)
out=$(mkdir -p "${1:-target/windows}" && cd "${1:-target/windows}" && pwd)
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
target=x86_64-pc-windows-msvc
name=Gitweft-$version-windows-x86_64
certificate=${GITWEFT_WINDOWS_CERTIFICATE:-}
# The certificate's password stays out of the environment of the build and its build scripts.
certificate_password=${GITWEFT_WINDOWS_CERTIFICATE_PASSWORD:-}
unset GITWEFT_WINDOWS_CERTIFICATE_PASSWORD
step() { printf '\n=== %s\n' "$*"; }
# Windows programs take Windows paths, and Git Bash would rewrite arguments that start with
# a slash (Inno Setup's and signtool's options) into paths: MSYS2_ARG_CONV_EXCL stops that.
winpath() { cygpath -w "$1"; }
native() { MSYS2_ARG_CONV_EXCL='*' "$@"; }

python=
for candidate in python3 python; do
    if "$candidate" -c 'import sys; sys.exit(sys.version_info < (3, 11))' 2> /dev/null; then
        python=$candidate
        break
    fi
done
[ -n "$python" ] || { echo "windows.sh: needs Python 3.11 or later" >&2; exit 1; }
# Inno Setup installed for all users, or for one (its own installer's two choices).
iscc=${ISCC:-$(command -v ISCC.exe || true)}
for candidate in "/c/Program Files (x86)/Inno Setup 6/ISCC.exe" \
    "$(cygpath -u "${LOCALAPPDATA:-$HOME/AppData/Local}")/Programs/Inno Setup 6/ISCC.exe"; do
    [ -z "$iscc" ] && [ -f "$candidate" ] && iscc=$candidate
done
[ -n "$iscc" ] || { echo "windows.sh: Inno Setup 6 not found (ISCC names its ISCC.exe)" >&2; exit 1; }
if [ -n "$certificate" ]; then
    # The newest Windows SDK's.
    signtool=${SIGNTOOL:-$({ ls -d "/c/Program Files (x86)/Windows Kits/10/bin/"10.*/x64/signtool.exe 2> /dev/null || true; } \
        | sort -V | tail -1)}
    [ -n "$signtool" ] || { echo "windows.sh: signtool not found (SIGNTOOL names it)" >&2; exit 1; }
fi
sign() {
    [ -n "$certificate" ] || return 0
    native "$signtool" sign /fd sha256 /f "$(winpath "$certificate")" /p "$certificate_password" \
        /tr "${GITWEFT_WINDOWS_TIMESTAMP_URL:-http://timestamp.digicert.com}" /td sha256 \
        /d Gitweft "$(winpath "$1")"
}

# No path of this machine in the binary (panic messages carry source paths).
# shellcheck disable=SC2206 # RUSTFLAGS is a space-separated list by definition
flags=(${RUSTFLAGS:-} "--remap-path-prefix=$(winpath "$root")=/usr/src/gitweft"
    "--remap-path-prefix=$(winpath "${CARGO_HOME:-$HOME/.cargo}")=/cargo")
export CARGO_ENCODED_RUSTFLAGS=$(IFS=$'\x1f'; echo "${flags[*]}")
unset RUSTFLAGS

step "build ($target)"
rustup target add "$target"
cargo fetch --locked
cargo build --frozen --profile dist -p gitweft --target "$target"

step "files"
rm -f "$out"/Gitweft-*-windows-*
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
stage=$work/Gitweft-$version
mkdir "$stage"
cp "target/$target/dist/gitweft.exe" "$stage/"
sign "$stage/gitweft.exe"
cp LICENSE "$stage/LICENSE.txt"
cp assets/fonts/IBMPlex-OFL.txt assets/fonts/Inter-OFL.txt assets/fonts/JetBrainsMono-OFL.txt "$stage/"
"$python" -X utf8 scripts/dist/notices.py "$stage/THIRD-PARTY-LICENSES.txt" "$target"
ls -l "$stage"

step "zip"
# Windows' own tar (bsdtar) writes zip files; Git Bash's GNU tar does not.
(cd "$work" && "$(cygpath -u -S)/tar.exe" -a -c -f "$(winpath "$out/$name.zip")" \
    "Gitweft-$version")

step "installer"
native "$iscc" /Q "/DAppVersion=$version" "/DStage=$(winpath "$stage")" \
    "/DSetupIcon=$(winpath crates/gitweft/resources/windows/gitweft.ico)" \
    "/DOutputDir=$(winpath "$out")" "/DOutputName=$name-setup" "$(winpath packaging/windows/gitweft.iss)"
sign "$out/$name-setup.exe"
ls -l "$out/$name.zip" "$out/$name-setup.exe"
