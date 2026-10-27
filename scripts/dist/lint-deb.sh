#!/usr/bin/env bash
# Checks the DEB with lintian, Debian's package checker, in a Debian 13 container.
# Errors fail; warnings are printed. The tags this package has on purpose are named below
# with the reason, and nothing else is hidden.
# Usage: scripts/dist/lint-deb.sh [DEB]   (default: target/deb/gitweft_*_amd64.deb)
set -euo pipefail
cd "$(dirname "$0")/../.."
deb=$(realpath "${1:-$(ls target/deb/gitweft_*_amd64.deb | head -1)}")
engine=${CONTAINER:-podman}
. scripts/dist/pins.env
# Debian's first upload of a package closes its ITP bug; this package isn't uploaded to
# Debian (it's built from our own tree), so there is none to close.
suppress=(
    initial-upload-closes-no-bugs
)
"$engine" run --rm -v "$deb:/pkg.deb:ro,Z" "$GITWEFT_PIN_DEBIAN_13" bash -euc '
    export DEBIAN_FRONTEND=noninteractive
    apt-get -qq update
    apt-get -qq install -y --no-install-recommends lintian > /dev/null
    # lintian runs as a user of its own (it warns about root).
    useradd -m lint
    su lint -c "lintian --fail-on error --display-level \">=warning\" --suppress-tags \"$1\" /pkg.deb"
    echo "lintian: no errors"
' lint "$(IFS=,; echo "${suppress[*]}")"
