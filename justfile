# Gitweft developer tasks. `just` lists them; `just check` runs everything CI runs.

# A `dev.just` next to this file, when there is one, adds recipes for a developer's own
# machine; it is read only if it exists.
import? 'dev.just'

default:
    @just --list

# Everything CI runs, in order of speed
check: fmt-check versions url-audit style-audit lint lint-release deny test smoke-offline socket-audit

fmt:
    cargo fmt --all

fmt-check:
    cargo fmt --all --check

# The packaging files carry Cargo.toml's version (scripts/dist/check-version.sh)
versions:
    bash scripts/dist/check-version.sh

# No URL literal in the app's code, tests and comments aside (scripts/url-audit.sh,
# docs/OFFLINE.md). The self-test first shows the audit does catch one.
url-audit:
    bash scripts/url-audit.sh --self-test
    bash scripts/url-audit.sh

# Views take colors from the palette and sizes from the metrics (CONTRIBUTING.md): no
# `cx.theme()`, kit text sizes or `px()` sizes that ignore the zoom; cut text goes through
# `gitweft_ui::truncated`, which shows it whole on hover. scripts/style-audit.py.
style-audit:
    python3 scripts/style-audit.py --self-test
    python3 scripts/style-audit.py

lint:
    cargo clippy --workspace --all-targets -- -D warnings

# Code behind `cfg(debug_assertions)` (the GITWEFT_DEBUG_* hooks) can leave imports or
# `mut`s unused only in release builds, which `lint` never sees.
lint-release:
    cargo clippy --release -p gitweft-workspace -- -D warnings

# Dependency bans (no HTTP/TLS/telemetry crates) and license policy
deny:
    cargo deny check

test:
    cargo test --workspace

# The tests against another git, built from source into target/git: `just test-git 2.39.5`
# for the oldest supported, 2.43.0 for Ubuntu 24.04's (scripts/test-with-git.sh).
test-git version *args:
    bash scripts/test-with-git.sh {{version}} {{args}}

# Criterion benchmarks: the graph layout, the diff algorithms, highlighting. `just bench
# -- --save-baseline main` records a baseline to compare with (`just bench-gate`).
bench *args:
    cargo bench -p gitweft-graph -p gitweft-diff -p gitweft-syntax --bench '*' {{args}}

# Fails when a benchmark got more than 10% slower than a saved baseline (scripts/bench-gate.sh).
# A manual step on a quiet machine, not part of `check`: benchmark numbers need one.
bench-gate baseline="main":
    bash scripts/bench-gate.sh {{baseline}}

# Proves the app loads a repository with no network at all (docs/OFFLINE.md)
smoke-offline repo=".":
    cargo build -q -p gitweft
    unshare -rn target/debug/gitweft --headless-smoke {{repo}}

# Proves the app opens no internet socket while it loads a repository: `--headless-smoke`
# under strace (scripts/socket-audit.sh, docs/OFFLINE.md). The self-test first shows the
# audit does catch one.
socket-audit repo=".":
    bash scripts/socket-audit.sh --self-test
    cargo build -q -p gitweft
    bash scripts/socket-audit.sh target/debug/gitweft {{repo}}

# Build the app with a cargo profile: `just build` (dev), `just build release`, or
# `just build dist` for the binary as the packages ship it (full optimisation, stripped)
build profile="dev":
    cargo build --profile {{profile}} -p gitweft
    @echo; echo "The binary is at target/{{ if profile == "dev" { "debug" } else { profile } }}/gitweft"

run *args:
    cargo run -p gitweft -- {{args}}

# Release sources in target/dist-src: the source and vendored-crates tarballs, the license
# notices, the RPM's bundled-crates macros (scripts/dist/sources.py). `--worktree` takes the
# working tree instead of HEAD, to try uncommitted packaging changes. The crates go into
# cargo's cache first (the one step that may need the network); every build then uses the
# tarballs, offline.
dist-src *flags:
    cargo fetch --locked
    python3 scripts/dist/sources.py target/dist-src {{flags}}

# Builds the release sources twice at different paths and compares the binaries
repro-check: dist-src
    bash scripts/dist/repro-check.sh

# Release RPM and SRPM in target/rpm, built in a Fedora 44 container with Fedora's own Rust,
# as COPR does, then installed there and smoke-tested with no network (scripts/dist/rpm.sh)
rpm *flags: (dist-src flags)
    bash scripts/dist/rpm.sh target/dist-src target/rpm
    @echo; echo "The RPM is ready:"; ls -1 target/rpm/gitweft-*.rpm

# Release DEB in target/deb, built in an Ubuntu 24.04 container, then installed on Debian 13
# and Ubuntu 24.04/26.04 and smoke-tested with no network, then checked with lintian
# (scripts/dist/{deb,test-deb,lint-deb}.sh)
deb *flags: (deb-build flags)
    bash scripts/dist/test-deb.sh
    bash scripts/dist/lint-deb.sh
    @echo; echo "The DEB is ready:"; ls -1 target/deb/gitweft_*.deb

# The DEB and its binary, without the installation tests (the AppImage repacks that binary)
[private]
deb-build *flags: (dist-src flags)
    bash scripts/dist/deb.sh target/dist-src target/deb

# Release AppImage in target/appimage from the DEB's binary and the libraries deb.sh sets
# aside, built first when they aren't there (`just deb` rebuilds them), then started in a
# Debian 13 container that has only libxcb and git, and smoke-tested there with no network
# (scripts/dist/{appimage,test-appimage}.sh)
appimage: appimage-inputs
    bash scripts/dist/appimage.sh target/deb/gitweft target/dist-src target/appimage
    bash scripts/dist/test-appimage.sh
    @echo; echo "The AppImage is ready:"; ls -1 target/appimage/*.AppImage

# What appimage.sh needs: the DEB's binary, icons and libraries, and the release sources
[private]
appimage-inputs:
    #!/usr/bin/env bash
    set -euo pipefail
    if [ ! -x target/deb/gitweft ] || [ ! -d target/deb/icons/hicolor ] || [ ! -d target/deb/lib/copyright ] \
        || [ ! -d target/dist-src ]; then
        just deb-build
    fi

# Flatpak bundle in target/flatpak, built in the Freedesktop SDK (installed for the user
# from Flathub on the first run, with its rust-stable extension), then smoke-tested in the
# sandbox with no network (scripts/dist/flatpak.sh)
flatpak *flags: (dist-src flags)
    bash scripts/dist/flatpak.sh target/dist-src target/flatpak
    @echo; echo "The Flatpak bundle is ready:"; ls -1 target/flatpak/*.flatpak

# Flathub's manifest and flathub.json in target/flathub, with the tarballs in DIR (the
# published ones; the release workflow does this for each tag) as the Release's URLs and
# their SHA-256 (scripts/dist/flathub.sh)
flathub dir="target/dist-src":
    bash scripts/dist/flathub.sh {{dir}} target/flathub

# Every package, tested, with SHA256SUMS, in target/release/VERSION (scripts/dist/release.sh:
# sources, RPM, DEB, AppImage, Flatpak; the release workflow on GitHub runs the same steps on
# a `v*` tag and drafts a Release). `--worktree` takes uncommitted files; `--aarch64` adds
# the aarch64 RPM, DEB and AppImage (under QEMU emulation here). The single packages'
# recipes build for GITWEFT_ARCH (x86_64 or aarch64) when it is set.
release *flags:
    bash scripts/dist/release.sh {{flags}}
