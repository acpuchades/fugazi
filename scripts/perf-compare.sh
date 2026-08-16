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
    # Deterministic instruction counts via callgrind — immune to CPU contention
    # *and* to code layout, so this is what separates "this change does more
    # work" from "this binary got an unluckier layout". A ~10% wall-clock swing
    # on a benchmark whose instruction count went *down* is the latter, and no
    # amount of re-running wall-clock will tell you that.
    #
    # Not a replacement for a quiet-machine criterion run: instruction count
    # ignores cache misses, branch prediction and ILP, so a real win can raise
    # it. Read the two together.
    #
    #   scripts/perf-compare.sh icount <other-worktree> [workload ...]
    #
    # <other-worktree> is a second checkout to compare against, e.g.
    #   git worktree add ../fugazi-base v0.58.0
    # (copy `benches/` in and add the `[[bench]]` entries; the probe only uses
    # public API). Build both with identical codegen settings or you are
    # measuring the profile, not the change:
    #   CARGO_PROFILE_BENCH_LTO=false CARGO_PROFILE_BENCH_CODEGEN_UNITS=16
    other="${1:?usage: perf-compare.sh icount <other-worktree> [workload ...]}"
    shift || true
    set -- "${@:-sma_rust macd_rust sma_yaml macd_yaml tree8}"
    if ! command -v valgrind >/dev/null 2>&1; then
        echo "valgrind not found" >&2; exit 127
    fi
    cargo bench --bench icount --no-run 2>/dev/null
    here=$(ls target/release/deps/icount-* | grep -v '\.d$' | head -1)
    there=$(ls "$other"/target/release/deps/icount-* | grep -v '\.d$' | head -1)
    export LC_ALL=C
    printf "%-12s %15s %15s %9s\n" workload base now change
    for w in $*; do
        o1=$(mktemp); o2=$(mktemp)
        valgrind --tool=callgrind --callgrind-out-file="$o1" "$there" "$w" >/dev/null 2>&1
        valgrind --tool=callgrind --callgrind-out-file="$o2" "$here"  "$w" >/dev/null 2>&1
        a=$(grep -m1 '^summary:' "$o1" | awk '{print $2}')
        b=$(grep -m1 '^summary:' "$o2" | awk '{print $2}')
        printf "%-12s %15s %15s %8.2f%%\n" "$w" "$a" "$b" \
            "$(python3 -c "print(($b-$a)/$a*100)")"
        rm -f "$o1" "$o2"
    done
    ;;

*)
    usage 0
    ;;
esac
