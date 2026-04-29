#!/usr/bin/env bash
# Test ws-echo TLS server ↔ ws_echo_ws_client round-trip.
# Tests both HTTP/1.1 and HTTP/2 WebSocket upgrades.
set -euo pipefail

cd "$(dirname "$0")/.."
REPO_ROOT="$PWD"
SERVER_PORT=8443
PASS=0
FAIL=0

cleanup() {
    for pid in "${SERVER_PIDS[@]:-}"; do
        kill "$pid" 2>/dev/null || true
    done
    wait 2>/dev/null || true
}
trap cleanup EXIT

build_artifact() {
    local feat="$1"
    local example="$2"
    echo "  Building $example (features: $feat)..."
    cargo build --features "$feat" --example "$example" -p endpoint-libs --quiet 2>&1
}

start_server() {
    local server_example="$1"
    local binary="target/debug/examples/$server_example"

    echo "  Starting $server_example..."
    "$binary" &>/tmp/ws_server_$$.log &
    local pid=$!
    SERVER_PIDS+=("$pid")

    for i in $(seq 1 15); do
        if nc -z localhost "$SERVER_PORT" 2>/dev/null; then
            echo "  Server ready (port $SERVER_PORT, pid $pid)"
            return 0
        fi
        sleep 0.5
    done
    echo "  FAIL: server did not start within timeout"
    kill "$pid" 2>/dev/null || true
    return 1
}

run_client() {
    local server_url="$1"
    echo "  Running client against $server_url..."
    echo "hello" | timeout 10 \
        target/debug/examples/ws_echo_ws_client "$server_url" 2>/dev/null
}

# Run server then client then assert; cleans up between iterations.
test_scenario() {
    local label="$1"
    local server_example="$2"

    echo ""
    echo "═══ $label ═══"

    if ! start_server "$server_example"; then
        FAIL=$((FAIL + 1))
        return
    fi

    sleep 1

    local client_out
    client_out=$(run_client "wss://localhost:$SERVER_PORT" 2>&1 || true)

    if echo "$client_out" | grep -q '"echo: hello"'; then
        echo "  PASS"
        PASS=$((PASS + 1))
    elif echo "$client_out" | grep -qi "error"; then
        echo "  FAIL: client error"
        echo "    $client_out" | grep -i error | head -3
        FAIL=$((FAIL + 1))
    else
        echo "  FAIL: unexpected output"
        echo "    $client_out"
        FAIL=$((FAIL + 1))
    fi

    # Kill server for this scenario
    for pid in "${SERVER_PIDS[@]:-}"; do
        kill "$pid" 2>/dev/null || true
    done
    SERVER_PIDS=()
    sleep 1

    if nc -z localhost "$SERVER_PORT" 2>/dev/null; then
        echo "  WARNING: port $SERVER_PORT still in use, force-killing..."
        fuser -k "$SERVER_PORT/tcp" 2>/dev/null || true
        sleep 1
    fi
}

SERVER_PIDS=()

# Build server once — ws-http1 includes both h2 and http/1.1 ALPN,
# so this one binary handles both H1 and H2 clients.
build_artifact "ws,ws-http1" ws_echo_server

# ── H1 test ──
build_artifact "ws-client" ws_echo_ws_client
test_scenario "Tungstenite HTTP/1.1 (ws+ws-http1)" ws_echo_server

# ── H2 test ──
build_artifact "ws-client" ws_echo_ws_client
test_scenario "Tungstenite HTTP/2 (ws)" ws_echo_server

# ── WTX ──
build_artifact "ws-wtx" ws_echo_server_wtx
build_artifact "ws-client,ws-wtx" ws_echo_ws_client
test_scenario "WTX HTTP/1.1 (ws-wtx)" ws_echo_server_wtx
build_artifact "ws-wtx,ws-wtx-http2" ws_echo_server_wtx
build_artifact "ws-client" ws_echo_ws_client
test_scenario "WTX HTTP/2 (ws-wtx+ws-wtx-http2)" ws_echo_server_wtx

echo ""
echo "═══════════════════════════"
echo "Results: $PASS passed, $FAIL failed"
echo "═══════════════════════════"
[ "$FAIL" -eq 0 ]
