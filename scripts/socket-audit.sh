#!/usr/bin/env bash
# The offline guarantee checked at runtime (docs/OFFLINE.md): the app loads a
# repository (`--headless-smoke`) under strace, and none of its processes may open an
# internet (AF_INET/AF_INET6) socket or connect to one. Unix-domain sockets are fine.
#   socket-audit.sh BINARY [REPO]   audit the app on REPO (default: .)
#   socket-audit.sh --self-test     prove the audit notices an internet socket
set -euo pipefail
command -v strace > /dev/null || { echo "socket-audit: strace isn't installed" >&2; exit 2; }
log=$(mktemp)
trap 'rm -f "$log"' EXIT
# Runs the command under strace; its own failure fails the audit (set -e).
traced() { strace -f -qq -e trace=socket,connect -o "$log" "$@" > /dev/null; }
# The traced calls that used an internet address family: printed, and 0 when there are any.
internet() { grep -E 'AF_INET6?[,}) ]' "$log"; }

if [ "${1:-}" = --self-test ]; then
    traced python3 -c 'import socket; socket.socket(socket.AF_INET).close()'
    internet > /dev/null || {
        echo "socket-audit: the self-test's internet socket went unnoticed" >&2
        exit 1
    }
    echo "socket-audit: the self-test's internet socket is caught"
    exit 0
fi

bin=${1:?usage: socket-audit.sh BINARY [REPO] | --self-test}
repo=${2:-.}
traced "$bin" --headless-smoke "$repo"
if internet; then
    echo "socket-audit: $bin opened an internet socket (above)" >&2
    exit 1
fi
echo "socket-audit: no internet socket from $bin"
