#!/usr/bin/env bash
# Test the ws-echo server through each client binary.
# Requires: websocat, jq, cargo
#
# Usage:
#   ./examples/test_ws_echo.sh [server_url]
#
# Defaults to ws://localhost:8080 if no URL is given.
# For wss:// targets use the full URL, e.g.:
#   ./examples/test_ws_echo.sh wss://ws-echo.liftmap.pro

set -euo pipefail

URL="${1:-ws://localhost:8080}"
TOTAL_PASS=0
TOTAL_FAIL=0

# ---------------------------------------------------------------------------
# Full protocol test cases (raw JSON in, raw JSON out)
# Used by: websocat, ws_echo_rustls, ws_echo_native_tls
# ---------------------------------------------------------------------------

declare -a FULL_DESC=(
    "Echo: basic echo"
    "Echo: seq is echoed back"
    "Echo: method is echoed back"
    "Echo: response type is Immediate"
    "Echo: empty message"
    "Echo: message with special characters"
    "ReceiveUserInfo: minimal"
    "ReceiveUserInfo: with appPubId"
    "ReceiveUserInfo: with token"
    "ReceiveUserInfo: with appPubId and token"
    "ReceiveUserInfo: error code is 100400 (BAD_REQUEST)"
    "ReceiveUserInfo: username appears in error message"
    "Error: unknown method returns error"
    "Error: missing params field (no response expected)"
    "Error: wrong params type"
)

declare -a FULL_PAYLOAD=(
    '{"method":1,"seq":1,"params":{"message":"hello"}}'
    '{"method":1,"seq":42,"params":{"message":"seq test"}}'
    '{"method":1,"seq":2,"params":{"message":"method test"}}'
    '{"method":1,"seq":3,"params":{"message":"type test"}}'
    '{"method":1,"seq":4,"params":{"message":""}}'
    '{"method":1,"seq":5,"params":{"message":"hello world! @#$%"}}'
    '{"method":211,"seq":10,"params":{"userPubId":"a1b2c3d4-e5f6-7890-abcd-ef1234567890","username":"alice"}}'
    '{"method":211,"seq":11,"params":{"userPubId":"a1b2c3d4-e5f6-7890-abcd-ef1234567890","username":"alice","appPubId":"deadbeef-0000-0000-0000-000000000001"}}'
    '{"method":211,"seq":12,"params":{"userPubId":"a1b2c3d4-e5f6-7890-abcd-ef1234567890","username":"alice","token":"my-secret-token"}}'
    '{"method":211,"seq":13,"params":{"userPubId":"a1b2c3d4-e5f6-7890-abcd-ef1234567890","username":"alice","appPubId":"deadbeef-0000-0000-0000-000000000001","token":"my-token"}}'
    '{"method":211,"seq":14,"params":{"userPubId":"a1b2c3d4-e5f6-7890-abcd-ef1234567890","username":"alice"}}'
    '{"method":211,"seq":15,"params":{"userPubId":"a1b2c3d4-e5f6-7890-abcd-ef1234567890","username":"bob"}}'
    '{"method":9999,"seq":20,"params":{}}'
    '{"method":1,"seq":21}'
    '{"method":1,"seq":22,"params":"not an object"}'
)

declare -a FULL_ASSERT=(
    '.params.message == "echo: hello"'
    '.seq == 42'
    '.method == 1'
    '.type == "Immediate"'
    '.params.message == "echo: "'
    '.params.message == "echo: hello world! @#$%"'
    '.type == "Error" and (.params | test("Test passed"))'
    '.type == "Error" and (.params | test("Test passed"))'
    '.type == "Error" and (.params | test("Test passed"))'
    '.type == "Error" and (.params | test("Test passed"))'
    '.code == 100400'
    '.params | test("bob")'
    '.type == "Error"'
    '.type == "Error"'
    '.type == "Error"'
)

# ---------------------------------------------------------------------------
# Echo-only test cases (plain message string in, JSON out)
# Used by: ws_echo_ws_client (wraps stdin lines in EchoRequest automatically)
# ---------------------------------------------------------------------------

declare -a ECHO_DESC=(
    "Echo: basic echo"
    "Echo: empty message"
    "Echo: message with special characters"
)

declare -a ECHO_INPUT=(
    "hello"
    ""
    "hello world! @#\$%"
)

declare -a ECHO_ASSERT=(
    '.params.message == "echo: hello"'
    '.params.message == "echo: "'
    '.params.message == "echo: hello world! @#$%"'
)

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

PASS_COUNT=0
FAIL_COUNT=0

check_response() {
    local desc="$1"
    local response="$2"
    local assertion="$3"

    printf "  %-55s " "$desc"
    local result
    result=$(echo "$response" | jq -r "$assertion" 2>/dev/null || echo "false")
    if [ "$result" = "true" ]; then
        echo "PASS"
        PASS_COUNT=$((PASS_COUNT + 1))
    else
        echo "FAIL"
        echo "    response: $response"
        FAIL_COUNT=$((FAIL_COUNT + 1))
    fi
}

section_summary() {
    echo "  → $PASS_COUNT passed, $FAIL_COUNT failed"
    TOTAL_PASS=$((TOTAL_PASS + PASS_COUNT))
    TOTAL_FAIL=$((TOTAL_FAIL + FAIL_COUNT))
    PASS_COUNT=0
    FAIL_COUNT=0
}

# Run full-protocol tests by piping raw JSON payloads to a command.
# The command must: accept a single JSON payload on stdin, print the JSON response to stdout.
run_full_tests() {
    local send_cmd="$1"   # command that reads one line stdin → one line stdout (JSON only)
    for i in "${!FULL_DESC[@]}"; do
        local raw response
        # timeout prevents hanging when the server closes the connection without responding
        if ! raw=$(echo "${FULL_PAYLOAD[$i]}" | timeout 5 $send_cmd 2>/dev/null); then
            printf "  %-55s FAIL (connection error or timeout)\n" "${FULL_DESC[$i]}"
            FAIL_COUNT=$((FAIL_COUNT + 1))
            continue
        fi
        # take only the first JSON line (guards against extra output)
        response=$(echo "$raw" | grep -m1 '^{' || true)
        if [ -z "$response" ]; then
            printf "  %-55s FAIL (no JSON in response)\n" "${FULL_DESC[$i]}"
            FAIL_COUNT=$((FAIL_COUNT + 1))
            continue
        fi
        check_response "${FULL_DESC[$i]}" "$response" "${FULL_ASSERT[$i]}"
    done
    section_summary
}

# Run echo-only tests by piping plain message strings.
run_echo_tests() {
    local send_cmd="$1"
    for i in "${!ECHO_DESC[@]}"; do
        local input="${ECHO_INPUT[$i]}"
        local response
        # Skip empty-string input for binaries that skip empty lines
        if [ -z "$input" ]; then
            printf "  %-55s SKIP (empty input not forwarded by binary)\n" "${ECHO_DESC[$i]}"
            continue
        fi
        if ! response=$(echo "$input" | $send_cmd 2>/dev/null); then
            printf "  %-55s FAIL (connection error)\n" "${ECHO_DESC[$i]}"
            FAIL_COUNT=$((FAIL_COUNT + 1))
            continue
        fi
        check_response "${ECHO_DESC[$i]}" "$response" "${ECHO_ASSERT[$i]}"
    done
    section_summary
}

# ---------------------------------------------------------------------------
# Build binaries
# ---------------------------------------------------------------------------

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
BIN_DIR="$REPO_ROOT/target/debug/examples"

echo "Building binaries..."
cargo build \
    --manifest-path "$REPO_ROOT/Cargo.toml" \
    --example ws_echo_rustls \
    --example ws_echo_native_tls \
    --example ws_echo_ws_client \
    --features native-tls \
    --quiet
echo "Done."
echo ""

# Wrappers so run_full_tests / run_echo_tests get a single-arg command
websocat_send()    { websocat --one-message "$URL" | tr -d '\r'; }
rustls_send()      { "$BIN_DIR/ws_echo_rustls"    "$URL"; }
native_tls_send()  { "$BIN_DIR/ws_echo_native_tls" "$URL"; }
ws_client_send()            { "$BIN_DIR/ws_echo_ws_client" "$URL"; }
ws_client_native_tls_send() { "$BIN_DIR/ws_echo_ws_client" "$URL" --native-tls; }

# ---------------------------------------------------------------------------
# Run
# ---------------------------------------------------------------------------

echo "Testing ws-echo server at $URL"
echo "=========================================================="

echo ""
echo "[ websocat — baseline, full protocol ]"
echo "----------------------------------------------------------"
run_full_tests websocat_send

echo ""
echo "[ ws_echo_rustls — raw tungstenite + rustls, full protocol ]"
echo "----------------------------------------------------------"
run_full_tests rustls_send

echo ""
echo "[ ws_echo_native_tls — raw tungstenite + native-tls, full protocol ]"
echo "----------------------------------------------------------"
run_full_tests native_tls_send

echo ""
echo "[ ws_echo_ws_client (rustls) — WsClient, Echo only ]"
echo "----------------------------------------------------------"
run_echo_tests ws_client_send

echo ""
echo "[ ws_echo_ws_client (native-tls) — WsClient, Echo only ]"
echo "----------------------------------------------------------"
run_echo_tests ws_client_native_tls_send

echo ""
echo "=========================================================="
printf "Total: %d passed, %d failed\n" "$TOTAL_PASS" "$TOTAL_FAIL"
echo ""

if [ "$TOTAL_FAIL" -gt 0 ]; then
    exit 1
fi
