#!/usr/bin/env bash
# Cargo runner for the xtensa target.
#
# `cargo test --target xtensa-esp32s3-espidf` hands the test binary to this
# script. It flashes the binary, streams the serial console, prints libtest's
# own output (so rust-analyzer's Test Explorer can parse pass/fail) and exits
# with the device's result. Test-binary filters cannot be forwarded: the
# ESP-IDF app_main runs with argc=0, so the whole binary runs every time.
#
# `cargo run` also uses this runner; that path simply flashes and attaches the
# monitor as before.
set -euo pipefail

bin="${1:?device-test-runner: missing binary path}"
shift || true

root="$(cd "$(dirname "$0")/.." && pwd)"
target="${CARGO_TARGET_DIR:-$root/firmware/target}"
table="$target/xtensa-esp32s3-espidf/debug/partition-table.bin"
port="${ESP32_PORT:-/dev/ttyACM0}"
timeout_s="${ESP32_TEST_SECS:-180}"

if [[ "$bin" != */deps/* ]]; then
  exec espflash flash --monitor --non-interactive --port "$port" \
    --partition-table "$table" "$bin" "$@"
fi

# Two Test Explorer clicks must not flash concurrently.
lock="${TMPDIR:-/tmp}/esp32-device-test.lock"
if command -v flock >/dev/null 2>&1; then
  exec 9>"$lock"
  flock 9
fi

if ! espflash flash --non-interactive --port "$port" --partition-table "$table" \
     "$bin" >/dev/null 2>&1; then
  echo "device-test-runner: flash failed for $bin" >&2
  exit 1
fi

fifo="$(mktemp -u)"
mkdir -p "$(dirname "$fifo")"
mkfifo "$fifo"
espflash monitor --non-interactive --port "$port" >"$fifo" 2>&1 &
mon=$!
exec 3<"$fifo"

started=$(date +%s)
status=1
while :; do
  if IFS= read -r -t 1 line <&3; then
    printf '%s\n' "$line"
    case "$line" in
      "test result: "*)
        [[ "$line" == *"FAILED"* ]] || status=0
        break
        ;;
    esac
  else
    kill -0 "$mon" 2>/dev/null || break
  fi
  [ $(( $(date +%s) - started )) -ge "$timeout_s" ] && break
done

exec 3<&-
kill "$mon" 2>/dev/null || true
pkill -P "$mon" 2>/dev/null || true
wait "$mon" 2>/dev/null || true
rm -f "$fifo"
exit "$status"
