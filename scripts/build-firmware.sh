#!/usr/bin/env bash
# Build the firmware with the container's ESP toolchain.
#
# The script is self-contained: invoking it from a shell with HOME=/root (or
# from an IDE task) still resolves the esp rustup toolchain and the espup
# libclang that bindgen needs for the xtensa target.
set -e

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT/firmware"

# Wi-Fi credentials are secrets: they live in the gitignored `.env` and are
# forwarded explicitly below because `env -i` starts from an empty environment.
# The numeric knobs default to the `[env]` table in `.cargo/config.toml`, but an
# exported value (from `.env` or the caller) is forwarded too, so
# `VIDEO_WIDTH=320 ./scripts/build-firmware.sh` still overrides the default.
if [ -f "$ROOT/.env" ]; then
  set -a
  . "$ROOT/.env"
  set +a
fi

forward=()
for key in WIFI_SSID WIFI_PASSWORD VIDEO_WIDTH VIDEO_HEIGHT VIDEO_FPS \
  VIDEO_JPEG_QUALITY AUDIO_SAMPLE_RATE AUDIO_BITRATE FILE_SERVER_PORT RECORD_MODE; do
  value="${!key:-}"
  if [ -n "$value" ]; then
    forward+=("$key=$value")
  fi
done

esp_home="${ESP_HOME:-/home/esp}"
exec env -i \
  HOME="$esp_home" \
  PATH="$esp_home/.cargo/bin:/usr/local/bin:/usr/bin:/bin" \
  RUSTUP_HOME="$esp_home/.rustup" \
  CARGO_HOME="$esp_home/.cargo" \
  RUSTUP_TOOLCHAIN=esp \
  IDF_PATH="${IDF_PATH:-/opt/esp/idf}" \
  IDF_TOOLS_PATH="${IDF_TOOLS_PATH:-/opt/esp/idf-tools}" \
  LIBCLANG_PATH="$esp_home/.espup/esp-clang" \
  "${forward[@]}" \
  cargo build --target xtensa-esp32s3-espidf "$@"
