#!/usr/bin/env bash
# Attach to the board's serial console (115200 baud, resets the board).
#
# Usage: scripts/monitor.sh [PORT] [espflash monitor args...]
#   scripts/monitor.sh                            # /dev/ttyACM0
#   scripts/monitor.sh /dev/ttyUSB0
#   scripts/monitor.sh /dev/ttyACM0 --after no-reset   # keep the app running
#
# The boot log prints the board address and readiness:
#   boot: RTSP advertising <ip>:554
#   boot: ready recording=... audio=true rtsp=true
#   selftest: PASS
set -euo pipefail

port="${ESP32_PORT:-/dev/ttyACM0}"
if [ $# -gt 0 ] && [[ "$1" != -* ]]; then
  port="$1"
  shift
fi

espflash_bin="$(command -v espflash || true)"
if [ -z "$espflash_bin" ]; then
  espflash_bin="/home/esp/.cargo/bin/espflash"
fi
[ -x "$espflash_bin" ] || { echo "monitor: espflash not found" >&2; exit 1; }

exec "$espflash_bin" monitor --port "$port" "$@"
