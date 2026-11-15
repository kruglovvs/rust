#!/usr/bin/env bash
# Live LAN API contract run (plan Todo 16).
#
#   ./harness/api_contract.sh http://192.168.1.33
#   ESP32_BOARD_URL=http://192.168.1.33 ./harness/api_contract.sh
#
# Runs the live driver and the in-process driver of tests/api_contract.rs, then
# Schemathesis when it is installed. Schemathesis is optional: its absence is
# recorded and does not fail the run.
#
# The last line is always `CONTRACT PASS` or `CONTRACT FAIL`, and the exit code
# matches it: 0 for pass, 1 for a failed check, 2 for a missing board URL.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
LOG_DIR="${API_CONTRACT_LOG_DIR:-$ROOT/.omo/evidence/task-16-firmware-slim-and-file-server}"
CONTRACT_LOG="$LOG_DIR/contract.log"
SCHEMATHESIS_LOG="$LOG_DIR/schemathesis.log"
BASE="${1:-${ESP32_BOARD_URL:-}}"

if [ -z "$BASE" ]; then
    echo "ESP32_BOARD_URL is required" >&2
    echo "usage: ESP32_BOARD_URL=http://<board-ip> $0    (or: $0 http://<board-ip>)" >&2
    echo "CONTRACT FAIL"
    exit 2
fi

mkdir -p "$LOG_DIR" || {
    echo "cannot create $LOG_DIR" >&2
    echo "CONTRACT FAIL"
    exit 1
}

export ESP32_BOARD_URL="$BASE"
export API_CONTRACT_REQUIRE_LIVE=1

status=0
echo "== cargo test --test api_contract (ESP32_BOARD_URL=$ESP32_BOARD_URL) =="
(
    cd "$ROOT/harness" && cargo test --test api_contract -- --nocapture
) 2>&1 | tee "$CONTRACT_LOG"
if [ "${PIPESTATUS[0]}" -ne 0 ]; then
    status=1
fi

if command -v schemathesis >/dev/null 2>&1; then
    echo "== schemathesis run ../firmware/api/openapi.json --base-url $BASE/api/v1 =="
    schemathesis run "$ROOT/firmware/api/openapi.json" --base-url "$BASE/api/v1" --checks all 2>&1 \
        | tee "$SCHEMATHESIS_LOG"
    if [ "${PIPESTATUS[0]}" -ne 0 ]; then
        status=1
    fi
else
    echo "schemathesis not on PATH - skipped (absence is not a failure)" | tee "$SCHEMATHESIS_LOG"
fi

if [ "$status" -eq 0 ]; then
    echo "CONTRACT PASS" | tee -a "$CONTRACT_LOG"
else
    echo "CONTRACT FAIL" | tee -a "$CONTRACT_LOG"
fi
exit "$status"
