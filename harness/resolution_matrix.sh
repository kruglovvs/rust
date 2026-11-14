#!/usr/bin/env bash
# Resolution/quality matrix: build and measure a list of capture settings.
#
# The question it answers is whether larger sizes become usable once the JPEG
# quality (and therefore bitrate) is lowered, or once the build is optimised.
#
# Usage: harness/resolution_matrix.sh [--release] [--seconds N] [--out DIR]
#   sizes:   QVGA 320x240 | VGA 640x480 | XGA 1024x768 | HD 1280x720 | FHD 1920x1080
#   quality: 8, 12, 20 (VIDEO_JPEG_QUALITY, lower is better/bigger)
#
# Artifacts (gitignored): harness/artifacts/matrix-<timestamp>/<size>-q<quality>/
# Exit: 0 always (it is a measurement), 2 if the build during the run failed.
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
SECONDS_PER_ENTRY=30
RELEASE=0
BUILD_ARGS=()
OUT="$HERE/artifacts/matrix-$(date -u +%Y%m%dT%H%M%SZ)"
PORT="${ESP32_PORT:-/dev/ttyACM0}"

while [ $# -gt 0 ]; do
  case "$1" in
    --release) RELEASE=1; BUILD_ARGS+=(--release); shift ;;
    --seconds) SECONDS_PER_ENTRY="${2:?}"; shift 2 ;;
    --out) OUT="${2:?}"; shift 2 ;;
    -h|--help) sed -n '2,12p' "$0"; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

mkdir -p "$OUT"
echo "artifacts: $OUT"

profile_dir=debug
[ "$RELEASE" = 1 ] && profile_dir=release

SIZES=("QVGA:320:240" "VGA:640:480" "XGA:1024:768" "HD:1280:720" "FHD:1920:1080")
QUALITIES=(8 12 20)

printf '%-6s %-9s %-4s %-8s %-9s %-7s %-7s %s\n' \
  size geometry q fps frames bitrate result | tee "$OUT/summary.txt"

for size in "${SIZES[@]}"; do
  IFS=: read -r name width height <<<"$size"
  for quality in "${QUALITIES[@]}"; do
    VIDEO_WIDTH="$width" VIDEO_HEIGHT="$height" VIDEO_JPEG_QUALITY="$quality" \
      "$ROOT/scripts/build-firmware.sh" "${BUILD_ARGS[@]}" > "$OUT/build-$name-q$quality.log" 2>&1 || {
      echo "build failed for $name q$quality" | tee -a "$OUT/summary.txt"
      continue
    }
    art="$ROOT/firmware/target/xtensa-esp32s3-espidf/$profile_dir"
    espflash flash --non-interactive --port "$PORT" \
      --partition-table "$art/partition-table.bin" \
      "$art/esp32s3-av-firmware" > "$OUT/flash-$name-q$quality.log" 2>&1 || {
      echo "flash failed for $name q$quality" | tee -a "$OUT/summary.txt"
      continue
    }
    sleep 60

    dir="$OUT/$name-q$quality"
    mkdir -p "$dir"
    ffmpeg -hide_banner -nostdin -loglevel error -rtsp_transport tcp \
      -i "${ESP32_RTSP_URL:-rtsp://192.168.1.33:554/live}" \
      -t "$SECONDS_PER_ENTRY" -c copy -y "$dir/stream.mkv" 2>"$dir/ffmpeg.log" || true
    ffmpeg -hide_banner -loglevel error -nostdin -i "$dir/stream.mkv" \
      -map 0:v:0 -c copy -f mjpeg "$dir/video.mjpeg" 2>/dev/null || true
    ffmpeg -hide_banner -loglevel error -nostdin -i "$dir/stream.mkv" \
      -map 0:a:0 -c copy -f wav "$dir/audio.wav" 2>/dev/null || true

    (cd "$HERE" && cargo run --quiet --bin stream-analyze -- report "$dir" \
       --width "$width" --height "$height" --min-frames 1 \
       --min-duration 1 --audio-codec aac --audio-rate 48000) \
       > "$dir/analyze.log" 2>&1 || true

    fps=$(grep -oE '"fps": [0-9.]+' "$dir/metrics.json" 2>/dev/null | head -1 | grep -oE '[0-9.]+' || echo 0)
    frames=$(grep -oE '"frames": [0-9]+' "$dir/metrics.json" 2>/dev/null | head -1 | grep -oE '[0-9]+' || echo 0)
    bitrate=$(grep -oE '"bitrate_bps": [0-9]+' "$dir/metrics.json" 2>/dev/null | head -1 | grep -oE '[0-9]+' || echo 0)
    audio_sec=$(grep -oE '"seconds": [0-9.]+' "$dir/metrics.json" 2>/dev/null | head -1 | grep -oE '[0-9.]+' || echo 0)
    failed=$(grep -oE '"failed_checks": [0-9]+' "$dir/metrics.json" 2>/dev/null | head -1 | grep -oE '[0-9]+' || echo 99)
    result=ok
    [ "$failed" != 0 ] && result=FAIL
    bitrate_kb=$((bitrate / 1000))
    printf '%-6s %-9s %-4s %-8s %-9s %-7s %-7s %s\n' \
      "$name" "${width}x${height}" "$quality" "$fps" "$frames" "${bitrate_kb}" "${audio_sec}s" "$result" \
      | tee -a "$OUT/summary.txt"
  done
done

echo
echo "matrix complete: $OUT/summary.txt"
exit 0
