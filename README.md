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
esp_capture_service (DVP OV3660 -> MJPEG, PDM mic -> G711U, AVI muxer)
    -> esp_media_service_link -> esp_rtsp_service (ROLE_SERVER)
    -> rtsp://<board-ip>:554/live
```

`firmware/src/main.rs` is 24 lines; `firmware/src/app.rs` brings up netif,
storage and Wi-Fi, waits for a station address, then starts `media::Pipeline`
and idles. Reusable Rust libraries live in `components/{logging,net,storage,media}`.

Verified on hardware: `OPTIONS` returns `200 OK`, `DESCRIBE` returns an SDP
with `JPEG/90000` and `PCMU/8000`, and a live 10 s session over TCP
interleaved delivered 504 audio frames and 86 video frames with a 56 ms A/V
start delta.

Known hardware limitations (see the failure-mode reference):

- About 14% of camera frames arrive without a JPEG SOI and are dropped by
  `cam_hal`; the rejected payload is garbage rather than zeros.
- The SD card does not mount, so recording stays disabled
  (`recording=false`) even though the on-board muxer is configured.

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
