#!/usr/bin/env bash
# The test suite against another git (#79): Gitweft supports git 2.39 and later
# (`GitVersion::MINIMUM`), but a developer machine has one, usually the newest.
#   test-with-git.sh VERSION [CARGO TEST ARGS…]    e.g. 2.39.5, or 2.43.0 (Ubuntu 24.04)
# Downloads git's release tarball from kernel.org once, builds it without its optional parts
# (no curl, OpenSSL, Perl, Python, Tcl or gettext: the tests need none) into
# target/git/VERSION, and runs `cargo test --workspace` with it first on PATH.
set -euo pipefail
[ $# -ge 1 ] || { sed -n '2,7p' "$0" | sed 's/^# \{0,1\}//'; exit 2; }
version=$1; shift
root=$(cd "$(dirname "$0")/.." && pwd)
dir=$root/target/git
prefix=$dir/$version
if [ ! -x "$prefix/bin/git" ]; then
    mkdir -p "$dir"
    tarball=$dir/git-$version.tar.xz
    [ -f "$tarball" ] || curl -fL --proto '=https' -o "$tarball" \
        "https://mirrors.edge.kernel.org/pub/software/scm/git/git-$version.tar.xz"
    rm -rf "$dir/git-$version"
    tar -C "$dir" -xf "$tarball"
    # Old git doesn't build as C23, newer compilers' default.
    make -C "$dir/git-$version" -j"$(nproc)" CFLAGS="-O2 -std=gnu17" prefix="$prefix" \
        NO_OPENSSL=1 NO_CURL=1 NO_EXPAT=1 NO_TCLTK=1 NO_GETTEXT=1 NO_PERL=1 NO_PYTHON=1 \
        install > "$dir/build-$version.log" 2>&1 \
        || { tail -20 "$dir/build-$version.log"; exit 1; }
    rm -rf "$dir/git-$version"
fi
PATH=$prefix/bin:$PATH
echo "test-with-git: $(git --version)"
cd "$root"
exec cargo test --workspace "$@"
