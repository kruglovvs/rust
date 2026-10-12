#!/usr/bin/env bash
set -e
cd "$(dirname "$0")/../harness"
cargo test "$@"
