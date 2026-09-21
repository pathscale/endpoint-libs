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
# Two copies in ONE graph means honey_id-types' re-exported WsRequest/WsResponse
# traits are different types with the same name, and the resulting error names
# two different endpoint-libs paths. See docs/chain.md.
#
# "Per graph" is the whole point, and a Cargo.lock cannot answer it. A lock is
# per workspace: it lists every version any member resolved, with no record of
# which member reached which. So counting copies in the lock cannot tell the
# real defect from a workspace whose members resolve different versions in
# graphs that never meet.
#
# EndpointValidator is the standing example of the second kind. Its
# `endpoint-validator` member is on 2.1; `ws-load-test` is deliberately held on
# 1.9, because endpoint-libs 2.0 made WsClient's futures non-Send and that
# breaks its JoinSet-of-workers model -- moving it needs a runtime plus a
# LocalSet per pinned core, which would change the concurrency characteristics
# its benchmarks exist to measure. The reason is written down in
# ws-load-test/Cargo.toml, where the pin is. Nothing links the two members, so
# no binary ever sees two copies and nothing can fail to unify. The lock count
# reported it anyway, every run, forever -- and a check that is permanently red
# is a check people stop reading, which costs more than not having it.
#
# So the lock count is only a prefilter. When it is greater than 1, ask cargo
# for the actual graphs: one `cargo tree` per workspace member. A member that
# reaches two endpoint-libs versions is the real defect; members that each
# reach one are fine no matter how many the lock holds.

# Versions of endpoint-libs reachable from one workspace member, one per line.
# --locked everywhere: this script is read-only, and a bare `cargo tree` will
# rewrite a consumer's Cargo.lock if the manifests have moved past it. --offline
# first so a normal run needs no network at all; the second form is allowed to
# fetch the index for a crate missing from the local cache, and still refuses to
# change the lock. If both fail the repo is reported unresolved, not green.
member_graph_versions() {
    cargo tree --locked --offline -p "$1" --prefix none --format '{p}' 2>/dev/null ||
        cargo tree --locked -p "$1" --prefix none --format '{p}' 2>/dev/null
}

# Workspace members of the repo we are already cd'd into, one name per line.
# A single-crate repo is a one-member workspace, so this needs no special case.
workspace_members() {
    { cargo tree --locked --offline --workspace --depth 0 --prefix none --format '{p}' 2>/dev/null ||
        cargo tree --locked --workspace --depth 0 --prefix none --format '{p}' 2>/dev/null; } |
        sed -nE 's/^([^ ]+) v[0-9].*/\1/p'
}

head_ "One endpoint-libs per graph"
for r in "${TOOLS[@]}" "${BACKENDS[@]}"; do
    have "$r" || { skip "$r (not checked out)"; continue; }
    lock="$(repo "$r")/Cargo.lock"
    [[ -f "$lock" ]] || { skip "$r (no Cargo.lock)"; continue; }
    n=$(grep -c '^name = "endpoint-libs"$' "$lock" 2>/dev/null || echo 0)
    vers=$(grep -A1 '^name = "endpoint-libs"$' "$lock" | grep '^version' | sed 's/[" ]//g;s/version=//' | tr '\n' ' ')
    if [[ "$n" -eq 0 ]]; then
        skip "$r does not depend on endpoint-libs"
        continue
    fi
    if [[ "$n" -eq 1 ]]; then
        pass "$r resolves exactly 1 ($vers)"
        continue
    fi

    # More than one in the lock. Only cargo can say whether any single graph
    # sees more than one, so without cargo this is unresolved, not green.
    if ! command -v cargo >/dev/null 2>&1; then
        skip "$r holds $n copies in Cargo.lock (${vers}) and cargo is not installed, so the graphs cannot be checked"
        continue
    fi

    members=$(cd "$(repo "$r")" && workspace_members)
    if [[ -z "$members" ]]; then
        skip "$r holds $n copies in Cargo.lock (${vers}) and cargo tree could not list its workspace members"
        continue
    fi

    bad=""
    split=""
    for m in $members; do
        mv_=$(cd "$(repo "$r")" && member_graph_versions "$m" |
            sed -nE 's/^endpoint-libs v([0-9][^ ]*).*/\1/p' | sort -u | tr '\n' ' ')
        case "$(echo "$mv_" | wc -w | tr -d ' ')" in
            0) ;;
            1) split="$split $m=${mv_% }" ;;
            *) bad="$bad $m sees ${mv_% };" ;;
        esac
    done

    if [[ -n "$bad" ]]; then
        fail "$r:${bad% } -- traits re-exported through honey_id-types will not unify. See docs/chain.md"
    else
        pass "$r resolves $n copies in one lock but one per graph:${split} (separate members, nothing to unify)"
    fi
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

# ── 5. Every endpoint-libs feature compiles standalone ───────────────────────
# A consumer picks features one at a time, so every feature has to declare the
# dependencies its own code names. `signal` and `log_reader` did not, and it
# stayed invisible because nothing ever built them without `types`. The feature
# list lives in Cargo.toml, not in a script, so this cannot go stale.
head_ "Every endpoint-libs feature standalone"
if $QUICK; then
    skip "--quick: skipped"
elif ! have endpoint-libs; then
    skip "endpoint-libs (not checked out)"
else
    if out=$("$(repo endpoint-libs)/scripts/check-features.sh" 2>&1); then
        pass "every feature compiles alone"
    else
        fail "some features do not compile alone; run ./scripts/check-features.sh in endpoint-libs"
        echo "$out" | grep -a 'FAIL' | sed 's/^/    /'
    fi
fi

# ── 6. Local versions vs crates.io ───────────────────────────────────────────
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
