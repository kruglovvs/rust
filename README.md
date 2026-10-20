# ESP32-S3 A/V Firmware

This repository contains an ESP32-S3 firmware package and a separate host
harness for an MJPEG plus PCMU RTSP server with optional AVI recording to SD.
The board is the RTSP server. A client connects to the board's private LAN IP.

## Current State

Subsystem modules and host tests exist for camera validation, RFC 2435 JPEG
packetization, PCMU audio, RTSP protocol handling, session policy, SD storage
policy, and AVI recording. The firmware runtime boot path is still blocked:
`firmware/src/main.rs` is currently a camera binding smoke test and does not
start capture, Wi-Fi, SNTP, RTSP, or recording. Do not describe the firmware
as end-to-end verified.

## Safe Host Checks

These commands do not flash a board, connect to RTSP, or write an SD card:

```bash
./scripts/test-harness.sh
./scripts/build-firmware.sh
```

The first command runs the host harness. The second builds for
`xtensa-esp32s3-espidf` and may require the prepared ESP toolchain and ESP-IDF
installation. Equivalent direct commands are:

```bash
(cd harness && cargo test)
(cd firmware && cargo build --target xtensa-esp32s3-espidf)
```

The harness uses its own pinned toolchain. Keep Wi-Fi credentials out of all
commands, source files, logs, and documentation. Runtime credentials are
injected by the eventual boot wiring; never add them to `.env` in a commit.

## Approval Gates

The following actions are not part of normal host verification and require
explicit approval for the specific run:

1. Flashing or running device tests, including
   `scripts/run-device-tests.sh --timeout-seconds 300`, writes firmware to the
   board and requires a connected board.
2. RTSP live tests connect to the board over the LAN and require approval for
   the target IP, client, and network.
3. SD tests mount and write the card. They require approval for the card and
   data location because they mutate removable storage.

No flashing, hardware execution, RTSP live test, or SD write has been run for
the current documentation update. The device test runner also assumes the
runtime path and reports a blocked or incomplete result while `main.rs` is
still the smoke test.

See [the architecture reference](firmware/docs/architecture.md) for data flow
and exact component versions. See [the failure-mode reference](firmware/docs/failure-modes.md)
for diagnostics and recovery boundaries. ADR-0001 documents the RFC 2435
packetization decision.
