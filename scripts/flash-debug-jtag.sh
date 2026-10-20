#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
build_dirs=("$root"/target/xtensa-esp32s3-espidf/debug/build/esp-idf-sys-*/out/build)
build_dir=

for candidate in "${build_dirs[@]}"; do
  if [[ -f "$candidate/flasher_args.json" ]] && {
    [[ -z "$build_dir" ]] || [[ "$candidate/flasher_args.json" -nt "$build_dir/flasher_args.json" ]]
  }; then
    build_dir="$candidate"
  fi
done

if [[ -z "$build_dir" ]]; then
  printf 'No ESP-IDF flasher_args.json found under %s/target. Build the firmware first.\n' "$root" >&2
  exit 1
fi

exec /opt/esp/idf-tools/tools/openocd-esp32/v0.12.0-esp32-20250707/openocd-esp32/bin/openocd \
  -s /opt/esp/idf-tools/tools/openocd-esp32/v0.12.0-esp32-20250707/openocd-esp32/share/openocd/scripts \
  -c 'set ESP_RTOS none' \
  -f board/esp32s3-builtin.cfg \
  -c "program_esp_bins $build_dir flasher_args.json verify; reset halt; shutdown"
