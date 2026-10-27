#!/usr/bin/env bash
# The release RPM, built as COPR builds it. In a clean Fedora 44 container: the SRPM is made
# from sources.py's tarballs and the spec inside them, its BuildRequires are installed
# (`dnf builddep`: Fedora's own Rust), and a normal user rebuilds it. The RPM is installed
# there, and its binary then runs the smoke test in a container with no network at all.
# Usage: scripts/dist/rpm.sh [DIST_DIR] [OUT_DIR]
# Writes OUT_DIR (default target/rpm): gitweft-VERSION-1.fc44.src.rpm, …ARCH.rpm (x86_64 or
# aarch64, for GITWEFT_ARCH: scripts/dist/arch.sh) and rpmbuild.log. Needs podman (CONTAINER=docker works too) and the Fedora 44 image at the
# digest in pins.env (GITWEFT_RPM_IMAGE picks another); dnf downloads the dependencies inside
# the container.
set -euo pipefail
cd "$(dirname "$0")/../.."
engine=${CONTAINER:-podman}
dist=$(realpath "${1:-target/dist-src}")
out=$(mkdir -p "${2:-target/rpm}" && realpath "${2:-target/rpm}")
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
. scripts/dist/pins.env
. scripts/dist/arch.sh
image=${GITWEFT_RPM_IMAGE:-$GITWEFT_PIN_FEDORA_44}
name=gitweft-rpm-$$
cleanup() {
    "$engine" rm -f "$name" > /dev/null 2>&1 || true
    "$engine" rmi -f "localhost/$name" > /dev/null 2>&1 || true
}
trap cleanup EXIT
rm -f "$out"/gitweft-*.rpm

"$engine" run --name "$name" --platform "$platform" -v "$dist:/dist:ro,z" -v "$out:/out:Z" \
    -e VERSION="$version" "$image" bash -euc '
    dnf -y -q install rpm-build "dnf-command(builddep)" > /dev/null
    useradd -m builder
    top=/home/builder/rpmbuild
    install -d -o builder $top $top/SOURCES $top/SPECS
    for f in gitweft-$VERSION.tar.xz gitweft-$VERSION-vendor.tar.xz gitweft-$VERSION-bundled.inc; do
        install -o builder -m0644 /dist/$f $top/SOURCES/
    done
    # The spec as released, from the source tarball.
    tar -xOf /dist/gitweft-$VERSION.tar.xz gitweft-$VERSION/packaging/rpm/gitweft.spec \
        > $top/SPECS/gitweft.spec
    su builder -c "rpmbuild -bs $top/SPECS/gitweft.spec" > /dev/null
    dnf -y -q builddep $top/SRPMS/gitweft-*.src.rpm > /dev/null
    echo "rustc: $(rustc --version) (from $(rpm -qf "$(command -v rustc)"))"
    su builder -c "rpmbuild --rebuild $top/SRPMS/gitweft-*.src.rpm" > /out/rpmbuild.log 2>&1 \
        || { tail -40 /out/rpmbuild.log; exit 1; }
    cp $top/SRPMS/gitweft-*.src.rpm $top/RPMS/$(uname -m)/gitweft-*.rpm /out/
    dnf -y -q install $top/RPMS/$(uname -m)/gitweft-*.rpm > /dev/null
    git init -q /tmp/repo
    git -C /tmp/repo -c user.name=T -c user.email=t@example.com commit -q --allow-empty -m first
'
"$engine" commit "$name" "localhost/$name" > /dev/null
echo "--- installed, no network:"
"$engine" run --rm --platform "$platform" --network none "localhost/$name" bash -euc '
    rpm -q gitweft
    gitweft --version
    gitweft --headless-smoke /tmp/repo | tail -1
'
ls -l "$out"/gitweft-*.rpm
