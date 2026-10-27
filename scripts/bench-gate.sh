#!/usr/bin/env bash
# The benchmarks' regression gate (#26): runs `just bench` against a saved criterion
# baseline and fails when a benchmark got slower by more than 10%.
#
#   git switch main && just bench -- --save-baseline main   # once, on the commit to beat
#   git switch my-change && just bench-gate main            # exit 1 on a slowdown
#
# A manual step until CI runs benchmarks (#52): the numbers need a quiet machine, and a
# shared runner's noise would trip a 10% gate.
set -uo pipefail
baseline=${1:-main}
cd "$(dirname "$0")/.." || exit 1
log=$(mktemp)
trap 'rm -f "$log"' EXIT
# `--bench '*'`: only the criterion targets (the libraries' own test harness in bench mode
# refuses criterion's options).
cargo bench -p gitweft-graph -p gitweft-diff -p gitweft-syntax --bench '*' -- \
    --baseline "$baseline" --noise-threshold 0.10 2>&1 | tee "$log"
status=${PIPESTATUS[0]}
if [ "$status" != 0 ]; then
    echo "bench-gate: cargo bench failed (exit $status); is there a '$baseline' baseline?"
    exit "$status"
fi
if grep -q "Performance has regressed" "$log"; then
    echo "bench-gate: slower than the '$baseline' baseline by more than 10%:"
    grep -B 4 "Performance has regressed" "$log" | grep -v '^--$'
    exit 1
fi
echo "bench-gate: nothing more than 10% slower than the '$baseline' baseline"
