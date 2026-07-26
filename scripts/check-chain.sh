#!/usr/bin/env bash
# Verify the endpoint-libs dependency chain is internally consistent.
#
# Read-only: builds and inspects, never writes to a repo. Safe to run any time.
#
#   ./scripts/check-chain.sh            # check everything
#   ./scripts/check-chain.sh --quick    # skip cargo build/test (metadata only)
#
# Exits non-zero if anything is inconsistent. See docs/chain.md for why each of
# these matters, and docs/release-order.md for the release runbook.

set -uo pipefail

CODE_ROOT="${CODE_ROOT:-$(cd "$(dirname "$0")/../.." && pwd)}"
QUICK=false
[[ "${1:-}" == "--quick" ]] && QUICK=true

TOOLS=(endpoint-libs endpointgen honey_id-types EndpointValidator)
BACKENDS=(api.support.cafe web3.trading-backend nofilter.io-backend
          pays.online-backend auth.honey.id-backend api.honey.id-backend)

FAILURES=0
pass() { printf '  \033[32m✓\033[0m %s\n' "$1"; }
fail() { printf '  \033[31m✗\033[0m %s\n' "$1"; FAILURES=$((FAILURES + 1)); }
skip() { printf '  \033[33m-\033[0m %s\n' "$1"; }
head_() { printf '\n\033[1m%s\033[0m\n' "$1"; }

repo() { echo "$CODE_ROOT/$1"; }
have() { [[ -d "$(repo "$1")" ]]; }

# ── 1. Exactly one endpoint-libs per dependency graph ────────────────────────
# Two copies means honey_id-types' re-exported WsRequest/WsResponse traits are
# different types with the same name, and the resulting error names two
# different endpoint-libs paths. See docs/chain.md.
head_ "One endpoint-libs per graph"
for r in "${TOOLS[@]}" "${BACKENDS[@]}"; do
    have "$r" || { skip "$r (not checked out)"; continue; }
    lock="$(repo "$r")/Cargo.lock"
    [[ -f "$lock" ]] || { skip "$r (no Cargo.lock)"; continue; }
    n=$(grep -c '^name = "endpoint-libs"$' "$lock" 2>/dev/null || echo 0)
    vers=$(grep -A1 '^name = "endpoint-libs"$' "$lock" | grep '^version' | sed 's/[" ]//g;s/version=//' | tr '\n' ' ')
    case "$n" in
        0) skip "$r does not depend on endpoint-libs" ;;
        1) pass "$r resolves exactly 1 ($vers)" ;;
        *) fail "$r resolves $n copies: ${vers}— traits re-exported through honey_id-types will not unify. See docs/chain.md" ;;
    esac
done

# ── 2. version.toml agrees with Cargo.lock ───────────────────────────────────
# endpoint-gen compares its own endpoint-libs requirement against [libs] here.
# A stale declaration is the usual cause of a baffling refusal to generate.
head_ "config/version.toml matches Cargo.lock"
for r in "${BACKENDS[@]}"; do
    have "$r" || { skip "$r (not checked out)"; continue; }
    vt="$(repo "$r")/config/version.toml"
    lock="$(repo "$r")/Cargo.lock"
    [[ -f "$vt" && -f "$lock" ]] || { skip "$r (missing version.toml or lock)"; continue; }

    declared=$(awk '/^\[libs\]/{f=1;next} f&&/^version/{gsub(/[" ]/,"");sub(/version=/,"");print;exit}' "$vt")
    resolved=$(grep -A1 '^name = "endpoint-libs"$' "$lock" | grep '^version' | head -1 | sed 's/[" ]//g;s/version=//')

    if [[ "$declared" == "$resolved" ]]; then
        pass "$r declares $declared"
    else
        fail "$r declares [libs] $declared but Cargo.lock resolves $resolved"
    fi
done

# ── 3. Generated artifacts still match the RON ───────────────────────────────
head_ "Generated docs match their RON (endpoint-gen --check)"
if ! command -v endpoint-gen >/dev/null 2>&1; then
    skip "endpoint-gen not installed (cargo install endpoint-gen)"
else
    for r in "${BACKENDS[@]}"; do
        have "$r" || { skip "$r (not checked out)"; continue; }
        [[ -d "$(repo "$r")/config" ]] || { skip "$r (no config/)"; continue; }
        if out=$(cd "$(repo "$r")" && endpoint-gen --config-dir config --check 2>&1); then
            pass "$r: ${out##*: }"
        else
            fail "$r: $(echo "$out" | head -3 | tr '\n' ' ')"
        fi
    done
fi

# ── 4. Each tool repo builds and tests ───────────────────────────────────────
if $QUICK; then
    head_ "Build & test"
    skip "--quick: skipped"
else
    head_ "Build & test"
    for r in "${TOOLS[@]}"; do
        have "$r" || { skip "$r (not checked out)"; continue; }
        if (cd "$(repo "$r")" && cargo test --quiet >/dev/null 2>&1); then
            pass "$r tests pass"
        else
            fail "$r: cargo test failed — run it there for detail"
        fi
    done
fi

# ── 5. Local versions vs crates.io ───────────────────────────────────────────
# Informational: an unpublished local bump is normal mid-release, but you should
# know it is the case rather than discover it from a consumer.
head_ "Local version vs crates.io"
for r in endpoint-libs endpointgen honey_id-types; do
    have "$r" || { skip "$r (not checked out)"; continue; }
    name=$(awk '/^\[package\]/{f=1;next} f&&/^name/{gsub(/[" ]/,"");sub(/name=/,"");print;exit}' "$(repo "$r")/Cargo.toml")
    local_v=$(awk '/^\[package\]/{f=1;next} f&&/^version/{gsub(/[" ]/,"");sub(/version=/,"");print;exit}' "$(repo "$r")/Cargo.toml")
    pub_v=$(cargo search "$name" --limit 1 2>/dev/null | head -1 | sed 's/.*= "//;s/".*//')
    if [[ "$local_v" == "$pub_v" ]]; then
        pass "$name $local_v published"
    else
        skip "$name local $local_v, crates.io $pub_v (unpublished bump)"
    fi
done

# ── Result ───────────────────────────────────────────────────────────────────
echo
if (( FAILURES == 0 )); then
    printf '\033[32mChain is consistent.\033[0m\n'
    exit 0
fi
printf '\033[31m%d inconsistency(ies).\033[0m See docs/chain.md.\n' "$FAILURES"
exit 1
