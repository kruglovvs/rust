#!/usr/bin/env bash
set -e
cd "$(dirname "$0")/../firmware"
cargo build --target xtensa-esp32s3-espidf "$@"
