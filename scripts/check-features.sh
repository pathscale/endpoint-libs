#!/usr/bin/env bash
# Verify every feature of this crate compiles on its own.
#
# A feature that is only ever built alongside `types` can forget to declare the
# dependencies its own code uses, because `types` already pulled them in. That
# is invisible until a consumer asks for just that feature, which is exactly
# what an optional feature is for. `signal` was missing `dep:tracing` and
# `log_reader` was missing `dep:tracing` and `dep:chrono` for the whole life of
# the feature table for this reason.
#
# The feature list is read out of Cargo.toml rather than written here, so a
# feature added tomorrow is checked tomorrow without anyone remembering to add
# it to a list.
#
#   ./scripts/check-features.sh              # cargo check every feature alone
#   CHECK_CMD="cargo clippy -- -D warnings" ./scripts/check-features.sh
#
# Exits non-zero if any feature fails to compile standalone.

set -uo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO" || exit 1

# `cargo check` by default; overridable so CI can reuse this for clippy.
CHECK_CMD="${CHECK_CMD:-cargo check --all-targets}"

# Features whose whole surface lives behind another feature's modules, so
# "compiles alone" is true but vacuous. They are still checked; this list only
# exists to explain why a pass here is not the same as coverage.
#   error_aggregation, log_throttling -> src/libs/log.rs, gated on `types`

# Read the feature names straight out of the [features] table. A feature line
# is `name = [` at column zero; the table ends at the next `[section]` header.
features() {
    sed -n '/^\[features\]/,/^\[[a-z]/p' Cargo.toml |
        sed -nE 's/^([a-zA-Z0-9_-]+) = \[.*/\1/p'
}

FAILURES=0
pass() { printf '  \033[32mok  \033[0m %s\n' "$1"; }
fail() { printf '  \033[31mFAIL\033[0m %s\n' "$1"; FAILURES=$((FAILURES + 1)); }

run_one() {
    local label="$1"
    shift
    local log
    # A full template, not `mktemp -t prefix`: that spelling is a BSD-ism and
    # GNU coreutils rejects it with "too few X's", so the macOS-green version
    # would have died on the Linux runner.
    log="$(mktemp "${TMPDIR:-/tmp}/endpoint-libs-feature.XXXXXX")"
    if $CHECK_CMD "$@" >"$log" 2>&1; then
        pass "$label"
    else
        fail "$label"
        sed -nE '/^(error|warning: unused)/,+6p' "$log" | sed 's/^/        /'
    fi
    rm -f "$log"
}

printf '\033[1mEvery feature standalone (%s)\033[0m\n' "$CHECK_CMD"
run_one "--no-default-features" --no-default-features
run_one "(default features)"
for f in $(features); do
    [ "$f" = "default" ] && continue
    run_one "--no-default-features --features $f" --no-default-features --features "$f"
done

echo
if [ "$FAILURES" -eq 0 ]; then
    printf '\033[32mEvery feature compiles standalone.\033[0m\n'
    exit 0
fi
printf '\033[31m%d feature(s) do not compile standalone.\033[0m\n' "$FAILURES"
printf 'A feature must declare every dependency its own code names, with dep:.\n'
exit 1
