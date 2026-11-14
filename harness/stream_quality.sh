#!/usr/bin/env bash
# End-to-end stream quality harness.
#
# 1. optionally flash the firmware (ELF + partition table)
# 2. wait up to --wait seconds for the RTSP port to accept connections
# 3. record --seconds of the stream with stream copy
# 4. analyze bitrate, scale, A/V parameters and artefacts (see analyze_stream.py)
#
# Usage: harness/stream_quality.sh [--url URL] [--port DEV] [--seconds N]
#                                  [--wait N] [--no-flash] [--out DIR]
#                                  [--width N] [--height N] [--min-frames N]
# Exit: 0 PASS, 1 quality FAIL, 2 stream never came up, 3 record/build error.
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
URL="${ESP32_RTSP_URL:-rtsp://192.168.1.33:554/live}"
PORT="${ESP32_PORT:-/dev/ttyACM0}"
RECORD_S=60
WAIT_S=120
WIDTH=640
HEIGHT=480
MIN_FRAMES=150
FLASH=1
OUT="${STREAM_QUALITY_OUT:-$ROOT/harness/artifacts/stream-quality/$(date -u +%Y%m%dT%H%M%SZ)}"

while [ $# -gt 0 ]; do
  case "$1" in
    --url) URL="${2:?}"; shift 2 ;;
    --port) PORT="${2:?}"; shift 2 ;;
    --seconds) RECORD_S="${2:?}"; shift 2 ;;
    --wait) WAIT_S="${2:?}"; shift 2 ;;
    --width) WIDTH="${2:?}"; shift 2 ;;
    --height) HEIGHT="${2:?}"; shift 2 ;;
    --min-frames) MIN_FRAMES="${2:?}"; shift 2 ;;
    --no-flash) FLASH=0; shift ;;
    --out) OUT="${2:?}"; shift 2 ;;
    -h|--help) sed -n '2,13p' "$0"; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 3 ;;
  esac
done

case "$RECORD_S$WAIT_S$WIDTH$HEIGHT$MIN_FRAMES" in
  ''|*[!0-9]*) echo "seconds and wait must be integers" >&2; exit 3 ;;
esac

mkdir -p "$OUT"
echo "output directory: $OUT"

authority="${URL#*://}"
authority="${authority%%/*}"
host="${authority%%:*}"
tcp_port="${authority##*:}"
case "$tcp_port" in ''|*[!0-9]*) tcp_port=554 ;; esac
[ -n "$host" ] || { echo "cannot parse a host from $URL" >&2; exit 3; }

if [ "$FLASH" = 1 ]; then
  art="${CARGO_TARGET_DIR:-$ROOT/target}/xtensa-esp32s3-espidf/debug"
  if [ ! -x "$art/esp32s3-av-firmware" ]; then
    echo "[1/4] building firmware (no artifact at $art)"
    "$ROOT/scripts/build-firmware.sh" || { echo "build failed" >&2; exit 3; }
  fi
  echo "[1/4] flashing $PORT"
  if espflash flash --non-interactive --port "$PORT" \
       --partition-table "$art/partition-table.bin" "$art/esp32s3-av-firmware"; then
    echo "flash ok; waiting for the board to reboot"
  else
    echo "WARNING: flash failed (serial unavailable?); continuing against $host" >&2
  fi
else
  echo "[1/4] flashing skipped"
fi

echo "[2/4] waiting up to ${WAIT_S}s for $host:$tcp_port"
started=$(date +%s)
while :; do
  if timeout 2 bash -c "echo > /dev/tcp/$host/$tcp_port" 2>/dev/null; then
    echo "stream is up after $(( $(date +%s) - started ))s"
    break
  fi
  if [ $(( $(date +%s) - started )) -ge "$WAIT_S" ]; then
    echo "stream did not come up within ${WAIT_S}s" >&2
    exit 2
  fi
  sleep 2
done

echo "[3/4] recording ${RECORD_S}s from $URL"
if ! ffmpeg -hide_banner -nostdin -rtsp_transport tcp -i "$URL" \
     -t "$RECORD_S" -c copy -y "$OUT/stream.mkv" >"$OUT/record.log" 2>&1; then
  echo "recording failed; see $OUT/record.log" >&2
  exit 3
fi
ffmpeg -hide_banner -loglevel error -nostdin -i "$OUT/stream.mkv" \
  -map 0:v:0 -c copy -f mjpeg "$OUT/video.mjpeg"
ffmpeg -hide_banner -loglevel error -nostdin -i "$OUT/stream.mkv" \
  -map 0:a:0 -c copy -f wav "$OUT/audio.wav" || true

echo "[4/4] analyzing (expect ${WIDTH}x${HEIGHT})"
(cd "$HERE" && cargo run --quiet --bin stream-analyze -- report "$OUT" \
  --width "$WIDTH" --height "$HEIGHT" --min-frames "$MIN_FRAMES")
