# ESP32-S3 A/V Streamer

XIAO ESP32-S3 Sense as a LAN RTSP server: MJPEG video + G.711 µ-law audio, plus
optional SD recording. The board is the server; a client pulls
`rtsp://<board-ip>:554/live`.

## Stack

| Layer | Choice |
|---|---|
| SoC | ESP32-S3R8, 8 MiB octal PSRAM, 8 MiB flash, OV3660 DVP camera, PDM mic |
| Media | Espressif **ADF / GMF**: `esp_capture_service`, `esp_rtsp_service` (one stream, MJPEG + PCMU), optional `esp_muxer_service` (AVI) |
| Language | **Rust** (`std`, ESP-IDF target `xtensa-esp32s3-espidf`) |
| Bindings | `esp-idf-sys` bindgen over the C headers; **no C or C++ shim** |
| FS | FatFs exFAT (patched at build time), `f_mkfs` for on-device format |

## Layout

```
firmware/   own cargo workspace: xtensa target, ESP-IDF, components/*, sdkconfig
harness/    own cargo workspace: host x86_64, stream capture + analysis
scripts/    build / flash / device-test entry points
docs:       firmware/docs/architecture.md, failure-modes.md
```

Each workspace pins its own `.cargo/config.toml` and `rust-toolchain.toml`, so
host tests never inherit the ESP target.

## Firmware Architecture

`main.rs` (24 lines) starts `app.rs`, which brings up the platform and hands
control to the media pipeline:

```mermaid
flowchart TD
    M["main.rs"] --> A["app.rs"]
    A --> N["net (Wi-Fi STA)"]
    N --> S["storage (SD / exFAT)"]
    S --> P["media::Pipeline"]
```

`media::Pipeline` links Espressif services; frames never pass through Rust:

```mermaid
flowchart LR
    CAM["OV3660 DVP<br/>MJPEG"] --> CAP["esp_capture_service"]
    MIC["PDM mic<br/>PCM to G711U"] --> CAP
    CAP -->|"esp_media_service_link"| RTSP["esp_rtsp_service<br/>ROLE_SERVER"]
    RTSP --> C["rtsp://ip:554/live"]
    CAP -.->|"feature avi-recording"| MUX["esp_muxer_service"]
    MUX -.-> SD[("/sdcard/rec/*.avi")]
```

Reusable Rust crates: `firmware/components/{logging,net,storage,media}`.
Capture geometry is build-time: `VIDEO_WIDTH`, `VIDEO_HEIGHT`, `VIDEO_FPS`
(defaults 640×480 @ 10). The DVP clock is 10 MHz — at 20 MHz the sensor emits
~74% corrupt VGA JPEGs (green/coloured bands).

## Harness Architecture

Host-side tooling that drives the real board and measures the result:

```mermaid
flowchart LR
    Q["stream_quality.sh"] --> F["flash.sh<br/>ELF + partition table"]
    F --> B[("board")]
    Q --> W["wait for TCP 554<br/>up to 120 s"]
    W --> R["ffmpeg record 60 s<br/>-c copy"]
    R --> AN["stream-analyze (Rust)<br/>bitrate, scale, JPEG integrity,<br/>blur/block, freeze/black"]
    AN --> J["metrics.json<br/>PASS / FAIL"]
```

Host tests (`harness/`) exercise the same analysis code plus a live RTSP client:

```mermaid
flowchart LR
    T1["analyze_stream<br/>synthetic clip"] --> L["harness/src/analyze.rs"]
    T2["stream_capture<br/>opt-in live"] --> L
    T3["e2e_rtsp<br/>client + SDP"] --> L
    T4["task14_net<br/>pure policy"] --> L
```

## Devcontainer

Image `espressif/idf-rust:esp32s3_latest` plus a containerfile that installs
ESP-IDF v5.5.2 (`/opt/esp/idf`, tools in `/opt/esp/idf-tools`), ffmpeg and
serial diagnostics. Environment (set in `containerEnv`):
`IDF_PATH`, `IDF_TOOLS_PATH`, `RUSTUP_HOME=/home/esp/.rustup`,
`CARGO_HOME=/home/esp/.cargo`. `/dev` is bind-mounted so `ttyACM*` appears
without restarting the container.

After changing `containerfile`/`devcontainer.json`: **Dev Containers: Rebuild
Container** (the first build clones IDF, so it is slow). rust-analyzer analyses
both workspaces via `rust-analyzer.linkedProjects`.

## Test Runners

| Runner | Command | Runs on | Covers |
|---|---|---|---|
| Host tests | `./scripts/test-harness.sh` | host | analyzer, RTSP client, net policy |
| Device unit tests | `./scripts/run-device-tests.sh` | board (serial) | 54 tests in 5 binaries |
| Stream quality | `./harness/stream_quality.sh` | host + board | record 60 s, artefact metrics |
| Test Explorer | rust-analyzer | both | host tests instant; device tests flash per click |

Device tests are also runnable from the Test Explorer: the cargo runner
(`scripts/device-test-runner.sh`) flashes the test binary, streams libtest
output and exits with the device result. The device runs every test in a
binary (filters do not reach `app_main`), and a click costs one flash (~40 s).

## Build, Flash, Connect

```bash
./scripts/build-firmware.sh          # VIDEO_WIDTH=320 VIDEO_HEIGHT=240 ...
./scripts/flash.sh [PORT]            # flashes ELF + partition table
./scripts/monitor.sh [PORT]          # serial console (add --after no-reset to keep it running)
./harness/stream_quality.sh          # flash, wait, record 60 s, analyse
ffplay -rtsp_transport tcp rtsp://<board-ip>:554/live
```

The serial log prints the address: `boot: RTSP advertising <ip>:554`, followed
by `boot: ready recording=… audio=… rtsp=…` and `selftest: PASS`.

Flashing must include the partition table; `espflash flash <elf>` alone keeps
the previous layout in flash.

## Configuration

`.env` at the repository root holds `WIFI_SSID` / `WIFI_PASSWORD` and is
committed so builds are reproducible; `firmware/build.rs` embeds them. Add no
other secret there.

## Known Limitations

- **AVI recording is gated off** (`avi-recording`, default off): the
  `esp_muxer_service` sink requests the capture provider's global cache, which
  starves the RTSP video sink, and the file stays empty. The cause is inside
  the vendor sink.
- Device tests, flashing and the live capture require the board attached.
