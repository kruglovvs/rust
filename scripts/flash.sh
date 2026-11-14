#!/usr/bin/env bash
# Flash the firmware together with the partition table the build produced.
#
# `espflash flash <elf>` alone does not rewrite the partition table, so the
# board keeps whatever layout was flashed before (the IDF default 8 MB single
# app here). Pass the generated table explicitly so firmware/partitions.csv
# actually takes effect.
#
# Usage: scripts/flash.sh [PORT]
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
fw="$root/firmware"
target="${CARGO_TARGET_DIR:-$fw/target}"
art="$target/xtensa-esp32s3-espidf/debug"
port="${1:-/dev/ttyACM0}"

(cd "$fw" && cargo build --target xtensa-esp32s3-espidf)

exec espflash flash --non-interactive --port "$port" \
  --partition-table "$art/partition-table.bin" \
  "$art/esp32s3-av-firmware"
