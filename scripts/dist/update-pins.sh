#!/usr/bin/env bash
# Refreshes scripts/dist/pins.env: the images' current index digests (skopeo), the
# newest rustup and its checksum, and Flathub's current SDK, Platform and Rust extension
# commits (flatpak remote-info, the user's flathub remote). The resvg and Inno Setup versions
# of the macOS and Windows packages are kept as they are; change those by hand. Prints the
# diff; review it, rebuild the packages, and commit. Needs network, skopeo, curl and flatpak.
set -euo pipefail
cd "$(dirname "$0")"
digest() { printf 'sha256:%s' "$(skopeo inspect --raw "docker://$1" | sha256sum | cut -d' ' -f1)"; }
commit() { flatpak remote-info flathub "runtime/$1/x86_64/25.08" | sed -n 's/^ *Commit: //p'; }
rustup=$(curl -fsS https://static.rust-lang.org/rustup/release-stable.toml | sed -n "s/^version = '\(.*\)'/\1/p")
rustup_sum() { curl -fsS "https://static.rust-lang.org/rustup/archive/$rustup/$1-unknown-linux-gnu/rustup-init.sha256" | cut -d' ' -f1; }
new=$(mktemp)
sed -n '1,/^GITWEFT_PIN/{/^GITWEFT_PIN/!p}' pins.env > "$new"
cat >> "$new" <<PINS
GITWEFT_PIN_FEDORA_44=registry.fedoraproject.org/fedora:44@$(digest registry.fedoraproject.org/fedora:44)
GITWEFT_PIN_UBUNTU_24_04=docker.io/library/ubuntu:24.04@$(digest docker.io/library/ubuntu:24.04)
GITWEFT_PIN_UBUNTU_26_04=docker.io/library/ubuntu:26.04@$(digest docker.io/library/ubuntu:26.04)
GITWEFT_PIN_DEBIAN_13=docker.io/library/debian:13@$(digest docker.io/library/debian:13)
GITWEFT_PIN_RUSTUP_VERSION=$rustup
GITWEFT_PIN_RUSTUP_INIT_SHA256=$(rustup_sum x86_64)
GITWEFT_PIN_RUSTUP_INIT_SHA256_AARCH64=$(rustup_sum aarch64)
GITWEFT_PIN_FLATPAK_SDK=$(commit org.freedesktop.Sdk)
GITWEFT_PIN_FLATPAK_PLATFORM=$(commit org.freedesktop.Platform)
GITWEFT_PIN_FLATPAK_RUST=$(commit org.freedesktop.Sdk.Extension.rust-stable)
PINS
grep -E '^GITWEFT_PIN_(RESVG|INNO_SETUP)_VERSION=' pins.env >> "$new"
diff -u pins.env "$new" || true
mv "$new" pins.env
