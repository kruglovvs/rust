#!/usr/bin/env bash
# Serial on-device test runner (plan Todo 15).
#
# Runs every firmware test binary serially on the board through
# `espflash flash --monitor`, reads results from serial, and reports PASS only
# when each binary emits a clean `test result: ok.` line.
#
# Usage: scripts/run-device-tests.sh [--timeout-seconds N] [--port DEV]
#                                    [--evidence DIR] [--self-test]
#
# Exit codes:
#   0  every binary clean
#   1  test failure, panic, or FAILED result
#   2  wall-clock timeout
#   3  idle watchdog: no test heartbeat for 60 s
#   4  hang/reset: boot banner or watchdog reset mid-run
#   5  result-line count does not match binaries run (truncated suite)
#   6  build or usage error
set -u

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
FW="$ROOT/firmware"
TIMEOUT_S=300
IDLE_S=60
PORT=/dev/ttyACM0
EVIDENCE="$ROOT/.omo/evidence/task-15"
SELF_TEST=0

while [ $# -gt 0 ]; do
  case "$1" in
    --timeout-seconds) TIMEOUT_S="${2:-}"; shift 2 ;;
    --port) PORT="${2:-}"; shift 2 ;;
    --evidence) EVIDENCE="${2:-}"; shift 2 ;;
    --self-test) SELF_TEST=1; shift ;;
    *) echo "usage: $0 [--timeout-seconds N] [--port DEV] [--evidence DIR] [--self-test]" >&2; exit 6 ;;
  esac
done

case "$TIMEOUT_S" in
  ''|*[!0-9]*) echo "run-device-tests: --timeout-seconds must be a positive integer" >&2; exit 6 ;;
esac

# EVIDENCE is consumed from inside `(cd "$FW" && ...)`, so a relative value
# would resolve against firmware/ instead of the caller's cwd. Normalise it to
# an absolute path against the ROOT, which is stable across that subshell.
case "$EVIDENCE" in
  /*) ;;
  *) EVIDENCE="$ROOT/$EVIDENCE" ;;
esac
mkdir -p "$EVIDENCE"

ESP_ENV=(env -i HOME=/home/esp
  PATH=/home/esp/.cargo/bin:/usr/local/bin:/usr/bin:/bin
  RUSTUP_HOME=/home/esp/.rustup CARGO_HOME=/home/esp/.cargo
  IDF_PATH=/opt/esp/idf IDF_TOOLS_PATH=/opt/esp/idf-tools
  RUSTUP_TOOLCHAIN=esp
  CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$FW/target}"
  CARGO_WORKSPACE_DIR="$FW")

# evaluate LOG EXPECTED REASON
#   REASON is the monitor-loop termination cause: ok | timeout | idle | boot.
# Prints one VERDICT line and returns its exit code. Pure text analysis, so the
# self-test drives it with synthetic logs.
evaluate() {
  local log="$1" expected="$2" reason="$3"
  local results panics failed boots
  results=$(grep -c '^test result: ' "$log" 2>/dev/null)
  panics=$(grep -c 'panicked at' "$log" 2>/dev/null)
  failed=$(grep -cE '^test .* \.\.\. FAILED$|^test result: FAILED' "$log" 2>/dev/null)
  boots=$(grep -c 'Calling app_main()' "$log" 2>/dev/null)
  results=${results:-0}; panics=${panics:-0}; failed=${failed:-0}; boots=${boots:-0}

  case "$reason" in
    timeout) echo "VERDICT 2 wall-clock timeout after ${TIMEOUT_S}s (results=$results/$expected)"; return 2 ;;
    idle)    echo "VERDICT 3 idle watchdog: no test heartbeat for ${IDLE_S}s (results=$results/$expected)"; return 3 ;;
    boot)
      # A boot banner after every expected result line is the normal post-run
      # reset, not a hang. Only a boot before the results complete is a failure.
      if [ "$results" -ge "$expected" ] && [ "$panics" -eq 0 ] && [ "$failed" -eq 0 ]; then
        echo "VERDICT 0 all $expected binaries clean (post-run reset ignored)"
        return 0
      fi
      echo "VERDICT 4 hang/reset: boot banner mid-run (boots=$boots results=$results/$expected)"; return 4 ;;
  esac

  if [ "$results" -lt "$expected" ] && grep -qE 'Task watchdog got triggered|Guru Meditation|rst:0x[0-9a-fA-F]+ \(' "$log" 2>/dev/null; then
    echo "VERDICT 4 hang/reset: watchdog or reset banner in serial output (results=$results/$expected)"; return 4
  fi
  if [ "$panics" -gt 0 ]; then
    echo "VERDICT 1 panic in serial output ($panics)"; return 1
  fi
  if [ "$failed" -gt 0 ]; then
    echo "VERDICT 1 FAILED test or result line present ($failed)"; return 1
  fi
  if [ "$results" -ne "$expected" ]; then
    echo "VERDICT 5 result-line count mismatch: got $results, expected $expected binaries"; return 5
  fi
  echo "VERDICT 0 all $expected binaries clean"; return 0
}

if [ "$SELF_TEST" -eq 1 ]; then
  mkdir -p "$EVIDENCE"
  T="$(mktemp -d)"
  printf 'running 3 tests\ntest a::b ... ok\ntest result: ok. 3 passed; 0 failed; 0 ignored\n' > "$T/clean.log"
  printf "running 1 tests\nthread 'x' panicked at src/a.rs:1:1:\nboom\ntest result: ok. 1 passed; 0 failed\n" > "$T/panic.log"
  printf 'running 2 tests\ntest a ... FAILED\ntest result: FAILED. 1 passed; 1 failed\n' > "$T/failed.log"
  printf 'I (1) main_task: Calling app_main()\nrunning 2 tests\nrst:0xc (RTC_SW_CPU_RST)\nI (2) main_task: Calling app_main()\ntest result: ok. 2 passed; 0 failed\n' > "$T/boot.log"
  printf 'running 3 tests\ntest a ... ok\n' > "$T/truncated.log"
  printf 'running 1 tests\nE (6510) task_wdt: Task watchdog got triggered\n' > "$T/wdreset.log"
  printf 'test result: ok. 1 passed; 0 failed\n' > "$T/count-mismatch.log"
  {
    echo "== runner self-test (synthetic logs) =="
    fail=0
    check() { # name file expected reason want_exit
      local out got verdict
      out=$(evaluate "$2" "$3" "$4"); got=$?
      verdict=PASS; [ "$got" -eq "$5" ] || { verdict=FAIL; fail=1; }
      echo "[$verdict] $1 :: exit=$got want=$5 :: $out"
    }
    check "clean-pass"                 "$T/clean.log"           1 ok      0
    check "panic"                      "$T/panic.log"           1 ok      1
    check "FAILED"                     "$T/failed.log"          1 ok      1
    check "mid-run-boot-banner"        "$T/boot.log"            2 ok      4
    check "watchdog-reset-banner"      "$T/wdreset.log"         1 ok      4
    check "truncated-suite-count"      "$T/truncated.log"       2 ok      5
    check "result-count-mismatch"      "$T/count-mismatch.log"  2 ok      5
    check "idle-silent-past-window"    "$T/clean.log"           1 idle    3
    check "wall-clock-timeout"         "$T/clean.log"           1 timeout 2
    if [ "$fail" -eq 0 ]; then echo "SELF-TEST: ALL PASS"; else echo "SELF-TEST: FAILURES"; fi
  } | tee "$EVIDENCE/runner-selftest.txt"
  rm -rf "$T"
  grep -q 'SELF-TEST: ALL PASS' "$EVIDENCE/runner-selftest.txt"
  exit $?
fi

mkdir -p "$EVIDENCE"
LOG="$EVIDENCE/serial.log"

BUILD_JSON="$(mktemp)"
if ! (cd "$FW" && "${ESP_ENV[@]}" cargo test --no-run --workspace --message-format=json >"$BUILD_JSON" 2>"$EVIDENCE/build.err"); then
  echo "run-device-tests: cargo test --no-run failed (see $EVIDENCE/build.err)" >&2
  rm -f "$BUILD_JSON"; exit 6
fi
# Only TEST-profile artifacts may be flashed. `cargo test --no-run` also emits
# the ordinary binary, and that binary runs the stub `main()` instead of
# libtest: it prints "camera bindings present", `app_main` returns, and the run
# ends with an IDLE0 stack-overflow reset and no `test result:` line.
#
# The filter must anchor on the *profile* object. Cargo emits TWO `"test"`
# booleans per record: `profile.test` (was this built by `cargo test`) and
# `target.test` (does this target support a harness — true for the ordinary
# bin too). Matching a bare `"test":true` therefore selects the stub binary as
# well. `profile` contains no nested braces, so `[^}]*` cannot run past it.
mapfile -t BINS < <(grep '"profile":{[^}]*"test":true' "$BUILD_JSON" | grep -o '"executable":"[^"]*"' | sed 's/^"executable":"//;s/"$//')
rm -f "$BUILD_JSON"
if [ "${#BINS[@]}" -eq 0 ]; then
  echo "run-device-tests: no test binaries produced" >&2; exit 6
fi
EXPECTED=${#BINS[@]}

: > "$LOG"
START=$(date +%s)
FINAL=ok

for BIN in "${BINS[@]}"; do
  echo "== running $(basename "$BIN") ==" >> "$LOG"
  FIFO="$(mktemp -u)"
  mkfifo "$FIFO"
  "${ESP_ENV[@]}" espflash flash --monitor --non-interactive --port "$PORT" "$BIN" >"$FIFO" 2>&1 &
  MON=$!
  exec 3<"$FIFO"
  LAST=$(date +%s)
  REASON=""
  while [ -z "$REASON" ]; do
    NOW=$(date +%s)
    if [ $((NOW - START)) -ge "$TIMEOUT_S" ]; then REASON=timeout; break; fi
    if [ $((NOW - LAST)) -ge "$IDLE_S" ]; then REASON=idle; break; fi
    if IFS= read -r -t 1 LINE <&3; then
      printf '%s\n' "$LINE" >> "$LOG"
      if [[ "$LINE" == "test "* || "$LINE" == *"HEARTBEAT"* ]]; then
        LAST=$(date +%s)
      fi
      if [[ "$LINE" == "test result: "* ]]; then
        LAST=$(date +%s)
        REASON=done
        break
      fi
    else
      if ! kill -0 "$MON" 2>/dev/null; then REASON=idle; break; fi
    fi
  done
  exec 3<&-
  kill "$MON" 2>/dev/null
  pkill -P "$MON" 2>/dev/null
  wait "$MON" 2>/dev/null
  rm -f "$FIFO"
  if [ "$REASON" != "done" ]; then FINAL="$REASON"; break; fi
done

evaluate "$LOG" "$EXPECTED" "$FINAL"
RC=$?
echo "log: $LOG  binaries: $EXPECTED  elapsed: $(( $(date +%s) - START ))s" | tee -a "$LOG"
exit "$RC"
