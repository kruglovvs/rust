# ESP32-S3 A/V Firmware

This repository contains an ESP32-S3 firmware package and a separate host
harness for an MJPEG plus PCMU RTSP server with optional AVI recording to SD.
The board is the RTSP server. A client connects to the board's private LAN IP.

## Current State

The firmware is built on Espressif's ADF/GMF media services, called from Rust
through bindings generated directly from the C headers (`bindings-gmf.h` →
`esp_idf_sys::gmf`). There is no hand-written C or C++ shim, and no
hand-written RTSP, RTP, JPEG, camera, microphone or muxer code.

```
esp_capture_service (DVP OV3660 -> MJPEG, PDM mic -> G711U)
    -> esp_media_service_link -> esp_rtsp_service (ROLE_SERVER)
    -> rtsp://<board-ip>:554/live

    (feature `avi-recording`, off by default)
    -> esp_media_service_link -> esp_muxer_service -> /sdcard/rec/*.avi
```

`firmware/src/main.rs` is 24 lines; `firmware/src/app.rs` brings up netif,
storage and Wi-Fi, waits for a station address, then starts `media::Pipeline`
and idles. Reusable Rust libraries live in `firmware/components/{logging,net,storage,media}`.

Verified on hardware: `OPTIONS` returns `200 OK`, `DESCRIBE` returns an SDP
with `JPEG/90000` and `PCMU/8000`, and live 10 s sessions over TCP
interleaved delivered 498-504 audio frames, 87-91 video frames (the gate is
90) and a 53-56 ms A/V start delta. The on-device self-check reports PASS.

The 257 GB SD card is formatted and mounts as exFAT: `f_mkfs` (FM_EXFAT |
FM_SFD) is called directly on the registered diskio drive because
`Fatfs::format` never returns on this board. Mount succeeds, a 4 MiB
round-trip is byte-identical, and the write rate is 415-419 kB/s.

Known limitations (see the failure-mode reference):

- About 10-14% of camera frames arrive without a JPEG SOI and are dropped by
  `cam_hal`; the rejected payload is garbage rather than zeros. That loss is
  what puts the video rate near the 90-frame gate.
- AVI recording is gated behind `avi-recording` and off by default. Enabling
  it links `esp_muxer_service` (the only AVI-capable muxer; the capture
  service has no AVI support), but that sink requests the provider's global
  cache, which starves the RTSP video sink (86 -> 1-3 frames per 10 s) and
  still leaves `muxed_0.avi` empty. The cause is inside the vendor sink's
  frame stealing, not in the pipeline wiring.

## Build and Host Checks

```bash
./scripts/test-harness.sh
./scripts/build-firmware.sh
```

Equivalent direct commands:

```bash
(cd harness && cargo test)
(cd firmware && cargo build --target xtensa-esp32s3-espidf)
```

The harness uses its own pinned toolchain.

Flashing must include the partition table, otherwise the board keeps the
image already in flash (the IDF default 8 MB single-app layout) and
`firmware/partitions.csv` never takes effect:

```bash
./scripts/flash.sh              # builds, then flashes elf + partition table to /dev/ttyACM0
./scripts/flash.sh /dev/ttyUSB0 # or pass a port
```

## Wi-Fi Credentials

`WIFI_SSID` and `WIFI_PASSWORD` live in `.env` at the repository root and are
committed deliberately so builds are reproducible; `firmware/build.rs` embeds
them into `OUT_DIR/wifi_credentials.rs`. `.env.*` remains ignored for
per-environment overrides. Do not add any other secret to `.env`.

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

Flashing and the live RTSP session were run with the board attached and the
user's approval; the measurements above come from that run. No SD write has
been attempted.

See [the architecture reference](firmware/docs/architecture.md) for data flow
and component versions, and [the failure-mode reference](firmware/docs/failure-modes.md)
for diagnostics and recovery boundaries.
