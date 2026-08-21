#!/usr/bin/env bash
# Run the exact commands CI runs, locally, in the same order.
#
# CI is the only gate that matters, and three of its checks fire *nowhere else*:
# the rustdoc lints (only under `RUSTDOCFLAGS=-D warnings`), clippy over
# `python/src` (~11k lines every other clippy invocation scopes past), and the
# feature matrix (`live` is compiled nowhere else at all). Running `cargo test`
# and calling it done is how a green local tree pushes a red CI.
#
# Every command below is copied verbatim from `.github/workflows/ci.yml`, and
# `tests/ci_mirror.rs` fails if the two ever drift.
#
#   scripts/ci-local.sh            # everything (what CI does)
#   scripts/ci-local.sh rust       # one job: fmt | rust | version-sync | features | python
#   FAST=1 scripts/ci-local.sh     # skip the feature matrix and the wheel build
#
# The Python job builds and installs a release wheel, which is slow. `FAST=1`
# runs clippy over the bindings and the tests against whatever is already
# installed — enough for an inner loop, not enough before a push. It creates
# `python/.venv` (via `uv`) if the checkout hasn't got one, so a fresh clone
# runs the same gate as a working tree rather than failing on a missing venv.

set -uo pipefail

cd "$(dirname "$0")/.."

failures=()
run() { # run <label> <cmd...>
    local label="$1"
    shift
    printf '\n\033[1m== %s ==\033[0m\n%s\n' "$label" "$*"
    if "$@"; then
        printf '\033[32mok\033[0m — %s\n' "$label"
    else
        printf '\033[31mFAILED\033[0m — %s\n' "$label"
        failures+=("$label")
    fi
}

job="${1:-all}"

# --- fmt: rustfmt + ruff -----------------------------------------------------
# Cheapest job in the file and the one most likely to be what's red, so it runs
# first. Both are `--check`: this script reports, it never rewrites your tree.
# `cargo fmt --all` / `ruff format` (no flag) are the fixing forms, and
# `scripts/hooks/pre-commit` runs them for you on staged files.
#
# Ruff is resolved rather than assumed. CI pins it to a minor series because the
# formatter's output is stable within one and not across, and a local run that
# used a different series would report a diff CI won't — the exact failure this
# script exists to prevent. So: use the ruff on PATH only if it is in range,
# otherwise let `uv` fetch one that is. `uv` is already this script's dependency
# (the python job builds the venv with it).
if [[ $job == all || $job == fmt ]]; then
    run "fmt / rustfmt" \
        cargo fmt --all -- --check

    ruff_pin='ruff>=0.15,<0.16'   # keep in sync with ci.yml's `Install ruff`
    ruff_cmd=()
    if command -v ruff >/dev/null && [[ $(ruff --version) == "ruff 0.15."* ]]; then
        ruff_cmd=(ruff)
    elif command -v uv >/dev/null; then
        echo "no in-range ruff on PATH — using uv to fetch $ruff_pin"
        ruff_cmd=(uv tool run --quiet --from "$ruff_pin" ruff)
    fi
    if ((${#ruff_cmd[@]})); then
        # `.` and not a file list: `ruff.toml` at the root is what makes one
        # invocation cover `python/` and `tools/` under one configuration.
        run "fmt / ruff format" \
            "${ruff_cmd[@]}" format --check .
    else
        printf '\033[31mFAILED\033[0m — %s\n' "fmt / ruff format (no ruff, no uv)"
        failures+=("fmt / ruff format (install ruff, or uv — https://docs.astral.sh/uv/)")
    fi
fi

# --- rust: test + clippy -----------------------------------------------------
# Scoped to `-p fugazi`: the `fugazi-python` member links libpython and cannot
# be `cargo test`ed standalone (the python job covers it).
if [[ $job == all || $job == rust ]]; then
    FUGAZI_REQUIRE_FIXTURES=1 run "rust / Test" \
        cargo test -p fugazi
    # `live` is not in the default feature set, so the line above compiles
    # `tests/live_*.rs` to nothing and skips every `#[cfg(test)]` in
    # `src/live/`. `--lib` too: those unit tests are half the live coverage.
    # Offline — `wiremock` on localhost.
    run "rust / Test (live wallets)" \
        cargo test -p fugazi --features live --lib --test live_okx --test live_coinbase --test live_portfolio
    run "rust / Clippy" \
        cargo clippy -p fugazi --all-targets -- -D warnings
    run "rust / Clippy (derive crate)" \
        cargo clippy -p fugazi-derive --all-targets -- -D warnings
    RUSTDOCFLAGS="-D warnings" run "rust / Docs" \
        cargo doc --no-deps -p fugazi
    # What `cargo publish` would upload. See the workflow for why this is
    # `--workspace --exclude fugazi-python` and not `-p fugazi`.
    #
    # The workflow hardcodes `target/`; this cannot. A caller may run the script
    # under `CARGO_TARGET_DIR` — verifying a change in a throwaway worktree is
    # the usual reason — and the crate then lands somewhere `target/package`
    # isn't. That used to leave `bytes` empty, and an empty `bytes` fails the
    # comparison below, so the check reported an over-size crate when the real
    # problem was that it never found one. `cargo metadata` is the only answer
    # that also covers `build.target-dir` set in a config file; the `||` chain
    # falls back if it can't run. Finding no crate at all is now its own error.
    run "rust / Package size" bash -c '
        cargo package --workspace --exclude fugazi-python --no-verify --allow-dirty || exit 1
        target=$(cargo metadata --format-version 1 --no-deps 2>/dev/null |
                 sed -n "s/.*\"target_directory\":\"\([^\"]*\)\".*/\1/p")
        target=${target:-${CARGO_TARGET_DIR:-target}}
        crate=$(find "$target/package" -maxdepth 1 -name "fugazi-*.crate" \
                  ! -name "fugazi-derive-*" -printf "%s %p\n" |
                sort -rn | head -1)
        [ -n "$crate" ] ||
            { echo "no fugazi-*.crate under $target/package"; exit 1; }
        bytes=${crate%% *}
        budget=$((5 * 1024 * 1024))
        printf "packaged at %s bytes (budget %s)\n" "$bytes" "$budget"
        [ "$bytes" -le "$budget" ] ||
            { echo "over budget — check Cargo.toml exclude"; exit 1; }
    '
fi

# --- version-sync ------------------------------------------------------------
# The seven places a bump has to touch. Cheap, and the one job whose failure is
# invisible locally until a release goes out wrong.
if [[ $job == all || $job == version-sync ]]; then
    run "version-sync" bash -c '
        set -uo pipefail
        fail=0
        root=$(grep -m1 "^version = " Cargo.toml | cut -d\" -f2)
        echo "root Cargo.toml: $root"
        check() {
            if [ "$2" != "$3" ]; then echo "  MISMATCH: $1 is '"'"'$2'"'"', expected '"'"'$3'"'"'"; fail=1
            else echo "  ok: $1 = $2"; fi
        }
        check "fugazi-derive/Cargo.toml" \
          "$(grep -m1 "^version = " fugazi-derive/Cargo.toml | cut -d\" -f2)" "$root"
        check "root fugazi-derive pin" \
          "$(grep -m1 "fugazi-derive = " Cargo.toml | sed "s/.*version = \"\([^\"]*\)\".*/\1/")" "$root"
        check "python/Cargo.toml" \
          "$(grep -m1 "^version = " python/Cargo.toml | cut -d\" -f2)" "$root"
        check "python/pyproject.toml" \
          "$(grep -m1 "^version = " python/pyproject.toml | cut -d\" -f2)" "$root"
        check "python/uv.lock" \
          "$(grep -A2 "^name = \"fugazi\"$" python/uv.lock | grep -m1 "^version = " | cut -d\" -f2)" "$root"
        check "README.md install snippet" \
          "$(grep -m1 "fugazi = \"" README.md | cut -d\" -f2)" "$(echo "$root" | cut -d. -f1,2)"
        exit $fail
    '
fi

# --- features ----------------------------------------------------------------
# Restricted rows check `--lib`: the integration tests reach for `fugazi::spec`
# unconditionally. `--all-features` is the row that covers every target.
# Spelled out rather than looped over a variable: a loop hides the actual
# command behind `$features`, which is where drift goes unnoticed. These lines
# are the workflow's matrix expanded, and `tests/ci_mirror.rs` compares them
# literally.
if [[ $job == all || $job == features ]] && [[ -z ${FAST:-} ]]; then
    run "features (default off) / Check" \
        cargo check -p fugazi --no-default-features --lib
    run "features (default off) / Clippy" \
        cargo clippy -p fugazi --no-default-features --lib -- -D warnings
    run "features (runtime) / Check" \
        cargo check -p fugazi --no-default-features --features runtime --lib
    run "features (runtime) / Clippy" \
        cargo clippy -p fugazi --no-default-features --features runtime --lib -- -D warnings
    run "features (sources) / Check" \
        cargo check -p fugazi --no-default-features --features sources --lib
    run "features (sources) / Clippy" \
        cargo clippy -p fugazi --no-default-features --features sources --lib -- -D warnings
    run "features (parallel) / Check" \
        cargo check -p fugazi --no-default-features --features parallel --lib
    run "features (parallel) / Clippy" \
        cargo clippy -p fugazi --no-default-features --features parallel --lib -- -D warnings
    run "features (spec) / Check" \
        cargo check -p fugazi --no-default-features --features spec --lib
    run "features (spec) / Clippy" \
        cargo clippy -p fugazi --no-default-features --features spec --lib -- -D warnings
    run "features (live) / Check" \
        cargo check -p fugazi --no-default-features --features live --lib
    run "features (live) / Clippy" \
        cargo clippy -p fugazi --no-default-features --features live --lib -- -D warnings
    run "features (all) / Check" \
        cargo check -p fugazi --all-features --all-targets
    run "features (all) / Clippy" \
        cargo clippy -p fugazi --all-features --all-targets -- -D warnings
fi

# --- python ------------------------------------------------------------------
if [[ $job == all || $job == python ]]; then
    run "python / Clippy (bindings)" \
        cargo clippy -p fugazi-python --all-targets -- -D warnings
    # `python/.venv` is gitignored, so a fresh clone or a throwaway worktree has
    # none — and "this checkout has no venv" is a fact about the checkout, not a
    # verdict on the code. Build one instead of failing the gate; CI gets the
    # equivalent from `setup-python` + `pip install`. Runs before the FAST guard
    # because pytest needs the venv in both modes.
    #
    # The package list mirrors ci.yml's explicit install. `jsonschema`,
    # `pyyaml` and `mypy` are load-bearing rather than incidental: the schema
    # test files and `test_stubs.py` `importorskip` them, so a venv built
    # without them makes those tests skip silently — green by not running, which
    # is the one outcome a gate must never produce.
    run "python / Venv" bash -c '
        cd python
        [ -x .venv/bin/python ] && exit 0
        command -v uv >/dev/null || { echo "uv is not installed — see https://docs.astral.sh/uv/"; exit 1; }
        echo "no python/.venv — creating one"
        uv venv .venv --python 3.13 &&
        uv pip install --python .venv/bin/python \
            maturin pytest numpy pandas polars jsonschema pyyaml mypy
    '
    if [[ -z ${FAST:-} ]]; then
        # CI builds a release wheel and pip-installs it. Locally the dev venv is
        # the equivalent, and `maturin develop` is the same link path without the
        # release build.
        run "python / Build + install" bash -c '
            cd python && uv run --no-project --python .venv/bin/python maturin develop
        '
    fi
    run "python / pytest" bash -c 'cd python && .venv/bin/python -m pytest -q'
fi

# --- verdict -----------------------------------------------------------------
printf '\n'
if ((${#failures[@]})); then
    printf '\033[31m%d check(s) failed:\033[0m\n' "${#failures[@]}"
    printf '  - %s\n' "${failures[@]}"
    exit 1
fi
printf '\033[32mall checks passed\033[0m — this is what CI runs\n'
