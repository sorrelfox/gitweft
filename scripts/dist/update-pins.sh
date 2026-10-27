#!/usr/bin/env bash
# Refreshes scripts/dist/pins.env (#58): the images' current index digests (skopeo), the
# newest rustup and its checksum, and Flathub's current SDK, Platform and Rust extension
# commits (flatpak remote-info, the user's flathub remote). Prints the diff; review it,
# rebuild the packages, and commit. Needs network, skopeo, curl and flatpak.
set -euo pipefail
cd "$(dirname "$0")"
digest() { printf 'sha256:%s' "$(skopeo inspect --raw "docker://$1" | sha256sum | cut -d' ' -f1)"; }
commit() { flatpak remote-info flathub "runtime/$1/x86_64/25.08" | sed -n 's/^ *Commit: //p'; }
rustup=$(curl -fsS https://static.rust-lang.org/rustup/release-stable.toml | sed -n "s/^version = '\(.*\)'/\1/p")
rustup_sum=$(curl -fsS "https://static.rust-lang.org/rustup/archive/$rustup/x86_64-unknown-linux-gnu/rustup-init.sha256" | cut -d' ' -f1)
new=$(mktemp)
sed -n '1,/^GITWEFT_PIN/{/^GITWEFT_PIN/!p}' pins.env > "$new"
cat >> "$new" <<PINS
GITWEFT_PIN_FEDORA_44=registry.fedoraproject.org/fedora:44@$(digest registry.fedoraproject.org/fedora:44)
GITWEFT_PIN_UBUNTU_24_04=docker.io/library/ubuntu:24.04@$(digest docker.io/library/ubuntu:24.04)
GITWEFT_PIN_UBUNTU_26_04=docker.io/library/ubuntu:26.04@$(digest docker.io/library/ubuntu:26.04)
GITWEFT_PIN_DEBIAN_13=docker.io/library/debian:13@$(digest docker.io/library/debian:13)
GITWEFT_PIN_RUSTUP_VERSION=$rustup
GITWEFT_PIN_RUSTUP_INIT_SHA256=$rustup_sum
GITWEFT_PIN_FLATPAK_SDK=$(commit org.freedesktop.Sdk)
GITWEFT_PIN_FLATPAK_PLATFORM=$(commit org.freedesktop.Platform)
GITWEFT_PIN_FLATPAK_RUST=$(commit org.freedesktop.Sdk.Extension.rust-stable)
PINS
diff -u pins.env "$new" || true
mv "$new" pins.env
