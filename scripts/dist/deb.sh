#!/usr/bin/env bash
# The DEB. Builds the release binary in an Ubuntu 24.04 container (the oldest glibc we ship
# for, so it also runs on Debian 13 and later Ubuntu) from sources.py's tarballs with
# scripts/dist/build.sh, then packs it with dpkg-deb. Depends come from dpkg-shlibdeps, plus
# the libraries GPUI loads with dlopen (Vulkan, Wayland), which no scan sees.
# Usage: scripts/dist/deb.sh [DIST_DIR] [OUT_DIR]
# Writes OUT_DIR (default target/deb)/gitweft_VERSION-1_ARCH.deb (amd64 or arm64, for
# GITWEFT_ARCH: scripts/dist/arch.sh), and what the AppImage
# takes too: OUT_DIR/gitweft (the binary, with a run path for OUT_DIR/lib), OUT_DIR/icons
# (scripts/dist/icons.sh's) and OUT_DIR/lib (the libraries the AppImage bundles, with their
# copyright files).
# Needs podman (CONTAINER=docker works too). The image is ubuntu:24.04 at the digest in
# pins.env (GITWEFT_DEB_IMAGE picks another); apt downloads the build dependencies inside
# the container, and rustup (pinned and checked, rustup-init.sh) the Rust of
# rust-toolchain.toml when the image has no cargo.
set -euo pipefail
cd "$(dirname "$0")/../.."
dist=$(realpath "${1:-target/dist-src}")
out=$(mkdir -p "${2:-target/deb}" && realpath "${2:-target/deb}")
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
engine=${CONTAINER:-podman}
. scripts/dist/pins.env
. scripts/dist/arch.sh
image=${GITWEFT_DEB_IMAGE:-$GITWEFT_PIN_UBUNTU_24_04}
rustup_sha256=$GITWEFT_PIN_RUSTUP_INIT_SHA256
[ "$arch" = aarch64 ] && rustup_sha256=$GITWEFT_PIN_RUSTUP_INIT_SHA256_AARCH64
toolchain=$(sed -n 's/^channel = "\(.*\)"/\1/p' rust-toolchain.toml)
rm -rf "$out"/gitweft_*.deb "$out/gitweft" "$out/icons" "$out/lib"

"$engine" run --rm --platform "$platform" -v "$dist:/dist:ro,z" -v "$out:/out:Z" \
    -e VERSION="$version" -e DEB_ARCH="$deb_arch" -e TOOLCHAIN="$toolchain" \
    -v "$PWD/scripts/dist/rustup-init.sh:/rustup-init.sh:ro,Z" \
    -e RUSTUP_VERSION="$GITWEFT_PIN_RUSTUP_VERSION" -e RUSTUP_SHA256="$rustup_sha256" \
    "$image" bash -euc '
    export DEBIAN_FRONTEND=noninteractive PATH=/opt/cargo/bin:$PATH
    apt-get -qq update
    apt-get -qq install -y --no-install-recommends pkg-config libxcb1-dev libxkbcommon-dev \
        libxkbcommon-x11-dev libfreetype-dev libfontconfig-dev libegl-dev \
        desktop-file-utils appstream librsvg2-bin gcc libc6-dev dpkg-dev ca-certificates curl \
        libxkbcommon0 libxkbcommon-x11-0 libxcb-xkb1 libvulkan1 libwayland-egl1 patchelf \
        > /dev/null
    if ! command -v cargo > /dev/null; then
        sh /rustup-init.sh "$RUSTUP_VERSION" "$RUSTUP_SHA256" -y -q --profile minimal \
            --default-toolchain "$TOOLCHAIN" > /dev/null
        . "$HOME/.cargo/env"
    fi
    echo "rustc: $(rustc --version)"
    mkdir -p /build && cd /build
    tar -xf /dist/gitweft-$VERSION.tar.xz
    tar -xf /dist/gitweft-$VERSION-vendor.tar.xz
    cd gitweft-$VERSION
    bash scripts/dist/build.sh > /dev/null 2> /build/cargo.log || { tail -30 /build/cargo.log; exit 1; }
    export SOURCE_DATE_EPOCH=$(cat .source-date-epoch)

    id=io.github.sorrelfox.Gitweft
    root=/build/pkg
    doc=$root/usr/share/doc/gitweft
    install -Dm0755 target/dist/gitweft $root/usr/bin/gitweft
    # Debian ships binaries stripped (lintian: unstripped-binary-or-object).
    strip --strip-unneeded --remove-section=.comment --remove-section=.note $root/usr/bin/gitweft
    install -Dm0644 packaging/linux/$id.desktop $root/usr/share/applications/$id.desktop
    install -Dm0644 packaging/linux/$id.metainfo.xml $root/usr/share/metainfo/$id.metainfo.xml
    bash scripts/dist/icons.sh $root/usr/share
    bash scripts/dist/manpage.sh target/dist/gitweft $root/usr/share/man/man1/gitweft.1.gz
    install -Dm0644 packaging/deb/copyright $doc/copyright
    install -m0644 -t $doc THIRD-PARTY-LICENSES.txt assets/fonts/IBMPlex-OFL.txt \
        assets/fonts/Inter-OFL.txt assets/fonts/JetBrainsMono-OFL.txt
    # The entry the metainfo and the RPM spec have: the newest <release>.
    { read -r release_date; read -r release_note; } < <(bash scripts/dist/release-entry.sh .)
    # The note wrapped to Debian'\''s 80 columns (lintian: debian-changelog-line-too-long).
    note=$(fold -s -w 74 <<< "$release_note" | sed -e "s/ *\$//" -e "1s/^/  * /" -e "1!s/^/    /")
    printf "gitweft (%s-1) unstable; urgency=medium\n\n%s\n\n -- sorrelfox <sorrelfox@users.noreply.github.com>  %s\n" \
        "$VERSION" "$note" "$(date -uR -d "$release_date 12:00")" \
        | gzip -9n > $doc/changelog.Debian.gz
    desktop-file-validate $root/usr/share/applications/$id.desktop
    appstreamcli validate --no-net $root/usr/share/metainfo/$id.metainfo.xml > /dev/null

    # Depends as Debian computes them for the linked libraries (dpkg-shlibdeps reads
    # debian/control), then what the app loads with dlopen (the Vulkan loader, the Wayland
    # client library, EGL and its Wayland glue for the OpenGL fallback), then git.
    mkdir -p debian
    printf "Source: gitweft\n\nPackage: gitweft\nArchitecture: %s\n" $DEB_ARCH > debian/control
    linked=$(dpkg-shlibdeps -O -e $root/usr/bin/gitweft 2> /dev/null | sed -n "s/^shlibs:Depends=//p")
    depends="$linked, libvulkan1, libwayland-client0, libegl1, libwayland-egl1, git (>= 1:2.39)"
    size=$(du -sk --apparent-size $root | cut -f1)
    mkdir -p $root/DEBIAN
    sed -e "s/@VERSION@/$VERSION/" -e "s/@ARCH@/$DEB_ARCH/" -e "s/@SIZE@/$size/" -e "s/@DEPENDS@/$depends/" \
        packaging/deb/control.in > $root/DEBIAN/control
    find $root -exec touch -h -d @$SOURCE_DATE_EPOCH {} +
    dpkg-deb --root-owner-group -Zxz --build $root /out/gitweft_${VERSION}-1_${DEB_ARCH}.deb > /dev/null
    cp target/dist/gitweft /out/gitweft
    cp -r $root/usr/share/icons /out/icons
    # What the AppImage bundles: the libraries the binary needs at start or loads at run time
    # that the AppImage excludelist does not promise on every host (libxcb, libwayland-client,
    # libEGL, glibc and libgcc_s it does), taken from this image so that they match the
    # binary'\''s glibc, with their copyright files. The AppImage'\''s copy of the binary finds
    # them through an RPATH, $ORIGIN/../lib. An RPATH rather than a RUNPATH, so that the
    # libraries'\'' own dependencies (libxkbcommon-x11 needs libxcb-xkb) and the ones the
    # program dlopens resolve there too; and no LD_LIBRARY_PATH, which would leak into git
    # and the external tools the app starts.
    mkdir -p /out/lib/copyright
    for lib in libxkbcommon.so.0 libxkbcommon-x11.so.0 libxcb-xkb.so.1 libvulkan.so.1 libwayland-egl.so.1; do
        path=/usr/lib/$(uname -m)-linux-gnu/$lib
        cp -L $path /out/lib/$lib
        pkg=$(dpkg -S $path | cut -d: -f1)
        cp /usr/share/doc/$pkg/copyright /out/lib/copyright/$pkg
    done
    find /out/lib -exec touch -h -d @$SOURCE_DATE_EPOCH {} +
    patchelf --force-rpath --set-rpath "\$ORIGIN/../lib" /out/gitweft
    dpkg-deb --field /out/gitweft_${VERSION}-1_${DEB_ARCH}.deb Architecture Version Depends Installed-Size
'
ls -l "$out"
