#!/usr/bin/env bash
#
# Performance A/B tooling for `benches/`.
#
# Deliberately *not* part of the CI gate — `scripts/ci-local.sh` and
# `.github/workflows/ci.yml` are held in sync by `tests/ci_mirror.rs`, and
# benchmarks are a development instrument, not a pass/fail check. Nothing here
# is invoked by either.
#
# Usage:
#   scripts/perf-compare.sh save <name>      capture a criterion baseline
#   scripts/perf-compare.sh diff <name>      re-run against a saved baseline
#   scripts/perf-compare.sh footprint        allocations/bar, bytes/bar, peak RSS
#   scripts/perf-compare.sh icount [bars]    callgrind instruction counts
#
# The usual A/B loop:
#
#   git switch main
#   scripts/perf-compare.sh save before
#   git switch -                       # your change
#   scripts/perf-compare.sh diff before
#
# criterion prints a per-benchmark change with a significance verdict, so a
# move inside the noise reads as "No change in performance" rather than as a
# number you have to judge by eye.
#
# `icount` uses callgrind rather than `perf stat`: perf needs
# `perf_event_paranoid` privileges and is not installed on every dev box, while
# callgrind is deterministic — the same binary on the same input returns the
# same instruction count every time, so a 1% change is real rather than noise.
# The cost is a ~50× slowdown, hence the small default bar count.

set -euo pipefail

cd "$(dirname "$0")/.."

BENCHES=(tree driver indicators multi_asset wallet metrics)

bench_args=(--warm-up-time 1 --measurement-time 3)

usage() {
    sed -n '3,30p' "$0" | sed 's/^# \{0,1\}//'
    exit "${1:-1}"
}

cmd="${1:-}"
shift || true

case "$cmd" in
save)
    name="${1:?usage: perf-compare.sh save <name>}"
    for b in "${BENCHES[@]}"; do
        cargo bench --bench "$b" -- "${bench_args[@]}" --save-baseline "$name"
    done
    echo
    echo "baseline '$name' saved under target/criterion/"
    ;;

diff)
    name="${1:?usage: perf-compare.sh diff <name>}"
    for b in "${BENCHES[@]}"; do
        cargo bench --bench "$b" -- "${bench_args[@]}" --baseline "$name"
    done
    ;;

footprint)
    cargo bench --bench footprint
    ;;

icount)
    bars="${1:-2000}"
    if ! command -v valgrind >/dev/null 2>&1; then
        echo "valgrind not found — install it, or use 'diff' for wall-clock only" >&2
        exit 127
    fi
    # Build first so the compile does not land inside the callgrind run.
    cargo bench --bench tree --no-run 2>/dev/null
    bin=$(ls -t target/release/deps/tree-* | grep -v '\.d$' | head -1)
    out=$(mktemp -d)/callgrind.out
    echo "callgrind: $bin (bars≈$bars, this takes a few minutes)"
    valgrind --tool=callgrind --callgrind-out-file="$out" \
        "$bin" --profile-time 1 tree/drive >/dev/null 2>&1
    echo "total instructions: $(grep -m1 '^summary:' "$out" | awk '{print $2}')"
    echo "full profile: $out  (open with callgrind_annotate)"
    ;;

*)
    usage 0
    ;;
esac
