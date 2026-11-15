#!/usr/bin/env bash
# Long-running stress test: stream for N minutes, keep every 1-minute file,
# analyse each one, and scan the device serial log for errors/warnings.
#
# Usage: harness/stress_test.sh [--minutes N] [--url URL] [--port DEV]
#                               [--width W] [--height H] [--audio-codec C]
#                               [--audio-rate HZ] [--no-flash] [--release]
#                               [--minute-seconds S]
#
# Artifacts (gitignored): harness/artifacts/stress-<timestamp>/
#   minute-001.mkv ...   one file per minute
#   minute-001/metrics.json   analyser output per minute
#   device.log           serial console captured during the run
#   summary.txt          per-minute verdicts + device log findings
#
# Exit: 0 all minutes passed and the device log is clean, 1 otherwise,
#       2 stream never came up, 3 capture/tooling error.
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
URL="${ESP32_RTSP_URL:-rtsp://192.168.1.33:554/live}"
PORT="${ESP32_PORT:-/dev/ttyACM0}"
MINUTES=60
WIDTH=640
HEIGHT=480
AUDIO_CODEC=aac
AUDIO_RATE=48000
FLASH=1
RELEASE=0
MINUTE_SECONDS=60
OUT="$HERE/artifacts/stress-$(date -u +%Y%m%dT%H%M%SZ)"

while [ $# -gt 0 ]; do
  case "$1" in
    --minutes) MINUTES="${2:?}"; shift 2 ;;
    --url) URL="${2:?}"; shift 2 ;;
    --port) PORT="${2:?}"; shift 2 ;;
    --width) WIDTH="${2:?}"; shift 2 ;;
    --height) HEIGHT="${2:?}"; shift 2 ;;
    --audio-codec) AUDIO_CODEC="${2:?}"; shift 2 ;;
    --audio-rate) AUDIO_RATE="${2:?}"; shift 2 ;;
    --no-flash) FLASH=0; shift ;;
    --release) RELEASE=1; shift ;;
    --minute-seconds) MINUTE_SECONDS="${2:?}"; shift 2 ;;
    -h|--help) sed -n '2,16p' "$0"; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 3 ;;
  esac
done

case "$MINUTES$WIDTH$HEIGHT$AUDIO_RATE$MINUTE_SECONDS" in
  ''|*[!0-9]*) echo "minutes/width/height/audio-rate must be integers" >&2; exit 3 ;;
esac

mkdir -p "$OUT"
echo "artifacts: $OUT"

host="${URL#*://}"; host="${host%%/*}"; host="${host%%:*}"
tcp_port="${URL##*:}"; case "$tcp_port" in ''|*[!0-9]*) tcp_port=554 ;; esac

if [ "$FLASH" = 1 ]; then
  profile_dir=debug
  [ "$RELEASE" = 1 ] && profile_dir=release
  art="$ROOT/firmware/target/xtensa-esp32s3-espidf/$profile_dir"
  if [ ! -x "$art/esp32s3-av-firmware" ]; then
    echo "[1/4] building the $profile_dir firmware"
    build_args=()
    [ "$RELEASE" = 1 ] && build_args+=(--release)
    VIDEO_WIDTH="$WIDTH" VIDEO_HEIGHT="$HEIGHT" \
      "$ROOT/scripts/build-firmware.sh" "${build_args[@]}" || {
        echo "build failed" >&2
        exit 3
      }
  fi
  echo "[1/4] flashing $profile_dir"
  espflash flash --non-interactive --port "$PORT" \
    --partition-table "$art/partition-table.bin" "$art/esp32s3-av-firmware" \
    > "$OUT/flash.log" 2>&1 || {
      echo "flash failed; continuing against $host" >&2
    }
fi

echo "[2/4] waiting for $host:$tcp_port"
started=$(date +%s)
until timeout 2 bash -c "echo > /dev/tcp/$host/$tcp_port" 2>/dev/null; do
  if [ $(( $(date +%s) - started )) -ge 180 ]; then
    echo "stream did not come up" >&2
    exit 2
  fi
  sleep 2
done

echo "[3/4] waiting 60 s for the board to boot"
sleep 60
echo "[3/4] capturing $MINUTES minute(s) and tailing the serial log"
espflash monitor --non-interactive --port "$PORT" --after no-reset > "$OUT/device.log" 2>&1 &
mon=$!
trap 'kill "$mon" 2>/dev/null || true' EXIT
sleep 3

pass=0
fail=0
declare -a failed_minutes=()

for minute in $(seq -w 1 "$MINUTES"); do
  file="$OUT/minute-$minute.mkv"
  printf '  minute %s: ' "$minute"
  # A single failed connect (board still booting, Wi-Fi blip) must not fail a
  # minute: retry until most of the window was actually captured.
  captured=0
  for attempt in 1 2 3 4; do
    if ffmpeg -hide_banner -nostdin -loglevel error -rtsp_transport tcp \
         -i "$URL" -t "$MINUTE_SECONDS" -c copy -y "$file" \
         2>"$OUT/minute-$minute.ffmpeg.log"; then
      captured=1
      break
    fi
    sleep 5
  done
  if [ "$captured" = 0 ]; then
    echo "capture failed after 4 attempts"
    fail=$((fail + 1))
    failed_minutes+=("$minute (capture)")
    continue
  fi

  dir="$OUT/minute-$minute"
  mkdir -p "$dir"
  mv "$file" "$dir/stream.mkv"
  ffmpeg -hide_banner -loglevel error -nostdin -i "$dir/stream.mkv" \
    -map 0:v:0 -c copy -f mjpeg "$dir/video.mjpeg" 2>/dev/null || true
  ffmpeg -hide_banner -loglevel error -nostdin -i "$dir/stream.mkv" \
    -map 0:a:0 -c copy -f wav "$dir/audio.wav" 2>/dev/null || true

  set +e
  (cd "$HERE" && cargo run --quiet --bin stream-analyze -- report "$dir" \
     --width "$WIDTH" --height "$HEIGHT" --min-frames 1 --min-duration 1 \
     --audio-codec "$AUDIO_CODEC" --audio-rate "$AUDIO_RATE" --no-require-audio) \
     > "$dir/analyze.log" 2>&1
  rc=$?
  set -e
  if [ "$rc" -eq 0 ]; then
    echo "ok"
    pass=$((pass + 1))
  else
    echo "FAIL"
    fail=$((fail + 1))
    failed_minutes+=("$minute")
  fi
done

kill "$mon" 2>/dev/null || true
wait "$mon" 2>/dev/null || true

echo "[4/4] scanning the device log"
grep -cE " E \(|ERROR|Guru Meditation|panic|abort\(\)" "$OUT/device.log" > "$OUT/.errors" || true
grep -cE " W \(|WARNING" "$OUT/device.log" > "$OUT/.warnings" || true
errors=$(cat "$OUT/.errors")
warnings=$(cat "$OUT/.warnings")
rm -f "$OUT/.errors" "$OUT/.warnings"

{
  echo "url:        $URL"
  echo "geometry:   ${WIDTH}x${HEIGHT}, audio ${AUDIO_CODEC}@${AUDIO_RATE}"
  echo "minutes:    $MINUTES (pass $pass, fail $fail)"
  echo "device log: $errors error lines, $warnings warning lines"
  if [ "$fail" -gt 0 ]; then
    echo "failed:     ${failed_minutes[*]}"
  fi
  echo
  echo "device errors:"
  grep -nE " E \(|ERROR|Guru Meditation|panic|abort\(\)" "$OUT/device.log" | head -40 || true
  echo "device warnings:"
  grep -nE " W \(|WARNING" "$OUT/device.log" | head -40 || true
} > "$OUT/summary.txt"

cat "$OUT/summary.txt"

if [ "$fail" -eq 0 ] && [ "$errors" -eq 0 ]; then
  echo "STRESS: PASS"
  exit 0
fi
echo "STRESS: FAIL"
exit 1
