#!/usr/bin/env bash
# No URL in the app's code (docs/OFFLINE.md, #51): the Rust sources may hold no http(s) URL
# literal, apart from the two LFS pointer spec versions, identifiers that git-lfs writes
# into pointer files and nothing fetches. Tests are left out (a file's `#[cfg(test)] mod … {`
# and all after it: it sits at the end of the file), and so are comment lines. A test-only
# function or `#[cfg(test)] mod x;` higher up is scanned like the rest (#48: stopping at the
# first `#[cfg(test)]` left most of some files unread).
#   url-audit.sh [ROOT…]     scan ROOT… (default: crates/*/src)
#   url-audit.sh --self-test  prove a URL literal is caught, and tests, comments and the
#                             LFS identifiers aren't
set -euo pipefail
ALLOWED='https://git-lfs\.github\.com/spec/v1|https://hawser\.github\.com/spec/v1'
# Prints `file:line: text` for each URL literal outside tests and comments.
scan() {
    find "$@" -name '*.rs' -print0 | while IFS= read -r -d '' file; do
        awk -v f="$file" '
            test && /^[[:space:]]*(pub(\([a-z]+\))? )?mod [A-Za-z_0-9]+[[:space:]]*\{/ { exit }
            { test = 0 }
            /^[[:space:]]*#\[cfg\(test\)\]/ { test = 1; next }
            /^[[:space:]]*\/\// { next }
            /"[^"]*https?:\/\// { print f ":" FNR ": " $0 }' "$file"
    done | { grep -Ev "$ALLOWED" || true; }
}

if [ "${1:-}" = --self-test ]; then
    dir=$(mktemp -d)
    trap 'rm -rf "$dir"' EXIT
    cat > "$dir/code.rs" <<'EOF'
#[cfg(test)]
mod ui_test;
#[cfg(test)]
fn helper() {}
fn ping() -> &'static str { "https://telemetry.example/ping" }
#[cfg(test)]
mod tests { const A: &str = "https://test.example/"; }
EOF
    cat > "$dir/allowed.rs" <<'EOF'
// See https://example.com/docs for the format.
const SPEC: &[u8] = b"https://git-lfs.github.com/spec/v1";
EOF
    hits=$(scan "$dir")
    if [ "$(printf '%s\n' "$hits" | grep -c .)" != 1 ] || [[ $hits != *telemetry.example* ]]; then
        echo "url-audit: the self-test found '$hits', not just the one URL in code" >&2
        exit 1
    fi
    echo "url-audit: the self-test's URL is caught; its test, comment and LFS identifier aren't"
    exit 0
fi

[ $# -gt 0 ] || set -- crates/*/src
hits=$(scan "$@")
if [ -n "$hits" ]; then
    printf '%s\n' "$hits"
    echo "url-audit: URL literals in the app's code (above)" >&2
    exit 1
fi
echo "url-audit: no URL literal in the app's code"
