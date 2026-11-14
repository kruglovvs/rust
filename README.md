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
with `JPEG/90000` and `PCMU/8000`, and 60 s captures at **640x480** deliver
444-571 video frames (7.4-9.5 fps) with **zero corrupt frames** after the
sensor clock was reduced to 10 MHz. At 20 MHz the same sensor produced
74% corrupt JPEGs (decoder `bad vlc`/`error dc`) - the green/coloured bands
seen on screen. The on-device self-check reports PASS.

The 257 GB SD card is formatted and mounts as exFAT: `f_mkfs` (FM_EXFAT |
FM_SFD) is called directly on the registered diskio drive because
`Fatfs::format` never returns on this board. Mount succeeds, a 4 MiB
round-trip is byte-identical, and the write rate is 415-419 kB/s.

Capture geometry is a build-time choice (defaults 640x480 @ 10 fps):

```bash
VIDEO_WIDTH=320 VIDEO_HEIGHT=240 VIDEO_FPS=10 ./scripts/build-firmware.sh
```

The sensor clock is 10 MHz (`dvp_config`). At 20 MHz the OV3660's DVP output
corrupts most VGA frames; 10 MHz is what makes the stream clean.

Known limitation:

- AVI recording is gated behind `avi-recording` and off by default. Enabling
  it links `esp_muxer_service` (the only AVI-capable muxer; the capture
  service has no AVI support), but that sink requests the provider's global
  cache, which starves the RTSP video sink and still leaves `muxed_0.avi`
  empty. The cause is inside the vendor sink, not the pipeline wiring.

## Build and Host Checks

```bash
./scripts/test-harness.sh
./scripts/build-firmware.sh

# End-to-end quality harness: flash, wait for RTSP, record 60 s, analyse
./harness/stream_quality.sh --seconds 60 --width 640 --height 480

# Without re-flashing (board already running):
./harness/stream_quality.sh --no-flash --width 640 --height 480
```

`stream_quality.sh` reports bitrate, resolution, A/V parameters, JPEG
integrity (SOI/EOI, corrupt DC), blur/blockiness means, freeze/black
intervals and >3 sigma colour outliers. Analysis is Rust
(`harness/src/analyze.rs`, binary `stream-analyze`); ffmpeg/ffprobe are the
only external tools.

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
