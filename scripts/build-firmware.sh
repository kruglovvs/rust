#!/usr/bin/env bash
# Build the firmware with the container's ESP toolchain.
#
# The script is self-contained: invoking it from a shell with HOME=/root (or
# from an IDE task) still resolves the esp rustup toolchain and the espup
# libclang that bindgen needs for the xtensa target.
set -e
cd "$(dirname "$0")/../firmware"

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
  cargo build --target xtensa-esp32s3-espidf "$@"
