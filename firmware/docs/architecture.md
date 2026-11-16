# ESP32-S3 A/V Firmware Architecture

Status: current source reference, 2026-11-16. This page describes the tree that
is in the repository now. Behaviour that has not been observed on the board is
marked unverified at the end; it is not presented as working.

The ESP32-S3 is the RTSP server and a read-only HTTP server. A client on the
same LAN connects to the board's private IP. There is no public endpoint, relay
or reverse proxy.

## Three Hardware Facts

These facts remove options rather than add them.

1. **The ESP32-S3 has no hardware H.264 encoder.** Software H.264 cannot run
   beside Wi-Fi on this chip. The only real-time video format is MJPEG, which
   the OV3660 encodes in silicon, so frames leave the sensor already compressed.
2. **WebM cannot carry MJPEG.** WebM allows VP8, VP9 and AV1 video only. AVI is
   the container used for recordings.
3. **The sensor emits complete JPEG files.** Frames are DMA'd into PSRAM and
   are ready for the media services without re-encoding on the MCU.

## Boot Path

`firmware/src/main.rs` spawns one thread named `runtime` with a 64 KiB stack and
calls `app::run`, then joins it. If the thread cannot be created it logs
`boot: FATAL: cannot spawn runtime thread: ...` and sleeps.

`firmware/src/app.rs` is the whole order of bring-up:

1. Warn when the `RECORD_MODE` text is not `both`, `stream` or `record`.
2. Initialise the network interface.
3. Take the peripherals, create the SPI2 bus on GPIO7/8/9 and run
   `storage::bringup` (chip-select probe, one mount, clock ladder).
4. Bring up Wi-Fi when credentials are compiled in.
5. Wait up to 60 s for a station IPv4 address, logging progress every 5 s.
6. Start the file server **once** with `fileserver::start(Config { .. })`.
7. Start `media::Pipeline` with the same mount point and record mode.
8. Print one readiness line:

```
boot: ready rtsp=rtsp://<ip>:554/live http://<ip>:80/ recording=<on|off> audio=<on|off> capture=<w>x<h>@<fps>fps
```

The thread then sleeps. With the `rtsp-selftest` feature it runs a loopback
RTSP probe, then loops logging a heap sample and the recording listing every
10 s.

Failure handling is deliberately uneven. A peripheral failure is fatal and
parks the thread. Every other step degrades: a failed SPI bus, mount, Wi-Fi
bring-up, file server or pipeline logs `boot: WARNING: ...` and the remaining
subsystems still start. The readiness line reports the recording and audio
state that actually exists, not the configuration that was requested.

## Media Pipeline

`firmware/components/media` is the ADF/GMF pipeline. Rust configures the
services and never touches a frame:

```text
esp_capture_service (SRC)
   |-- video: OV3660 DVP -> MJPEG at the configured geometry and rate
   |-- audio: PDM mic -> PCM -> AAC-LC encoder inside capture
   |-- esp_muxer_service (AVI sink) -> <mount>/rec/muxed_*.avi
   +-- link -> esp_rtsp_service (ROLE_SERVER) -> rtsp://<ip>:554/live
```

The video track is MJPEG at the requested width, height and rate (defaults
640x480 at 10 fps, JPEG quality 12). The audio track is AAC-LC, mono, 48 kHz and
64 kb/s by default, advertised as RTP payload type 97. G.711 would pin the clock
to 8 kHz, so AAC-LC is used to keep the stream wideband. The PDM microphone uses
GPIO42 for clock and GPIO41 for data; camera SCCB uses GPIO40/39.

`Config::record_mode` selects the consumers at run time:

| Mode | Muxer | RTSP |
|---|---|---|
| `both` (default) | yes | yes |
| `stream` | no | yes |
| `record` | yes | no |

The `avi-recording` feature is a compile-time kill switch and is on by default
in the firmware package; without it no muxer code is linked. Recording also
requires a mounted card: an empty mount point disables it and streaming
continues.

`Pipeline::start` builds every handle, attaches the selected consumers, starts
capture first and then the consumers. A muxer that fails to configure, link or
start is logged as `pipeline: WARNING: recording unavailable: ...` and recording is
skipped; it never prevents RTSP from starting. Any other failure unwinds the
handles already created in reverse order and returns an error naming the failed
step. `Pipeline::stop` is idempotent and also runs from `Drop`.
`Pipeline::recording()` reports whether the muxer was actually started and
`Pipeline::audio()` reports whether the codec device exists.

The vendor muxer writes segments below `<mount>/rec`. With a storage directory
and no storage URL it names them `muxed_0.avi`, `muxed_1.avi` and so on, from
the directory and the slice index. The muxer receives a 16 KiB aligned
internal-RAM write cache.

## Storage

`firmware/components/storage` owns the SD card. It supports **FAT32 only**;
there is no second filesystem path.

- The chip-select probe tries GPIO21 first and GPIO3 second, because the
  shipped board wires the card to GPIO21 while some BSPs assume GPIO3. Each
  candidate is logged with its verdict and the aggregate result is
  `CardProbe=Detected(GPIO21)`, `Detected(GPIO3)` or `Absent`.
- SPI uses GPIO7 SCK, GPIO8 MISO and GPIO9 MOSI. The card is mounted once at
  `/sdcard`, and GPIO21 must not be driven as the user LED while it is mounted.
- The clock ladder starts at 40 MHz, falls back to 20 MHz and then 10 MHz. Each
  rung is measured with `sdspi_host_get_real_freq` and accepted only after a
  boot qualification battery: a 1 MiB round trip, a 4 MiB write and a tail read
  at the negotiated clock. A rung that corrupts data or writes below 262144 B/s
  is rejected.
- The component links with `-Wl,--wrap=sdspi_host_do_transaction`. The interposer
  owns chip-select handling around the driver transaction; every component test
  binary links the same wrapper through the shared build script.
- `hil_fixture` formats the card FAT32 and mounts it once per **test binary**.
  It is a normal module so another crate's test binary can use it, but the boot
  path never calls it and a normal boot never formats.

## Network

`firmware/components/net` is Wi-Fi station bring-up plus pure policy:

- `net::bring_up` validates runtime credentials, takes the event loop and NVS,
  starts the station, disables Wi-Fi power save before streaming, connects, and
  advertises `_rtsp._tcp` on port 554 with `path=/live` through mDNS. Failure to
  connect at boot is not fatal; a supervisor retries with backoff.
- `session.rs` is the host-testable lifecycle and backpressure policy. It
  decides when capture is suspended, when a slow client gets a cooldown, and how
  Wi-Fi loss tears a session down.
- The SNTP policy layer is retained for API stability, but the lwIP SNTP client
  is not started: it faults on sync completion on this IDF version. Nothing
  syncs the clock, so `is_synced()` stays false. The pure policy is tested but
  no delivered consumer acts on it; the GMF stack timestamps RTP from its own
  monotonic clock. The layer only logs and never fabricates wall time.

Credentials are compile-time inputs (`WIFI_SSID`, `WIFI_PASSWORD`) read with
`option_env!`. They are absent from the `[env]` table and from every tracked
file; a build without them still compiles, warns, and boots with no network.

## File Server

`firmware/components/fileserver` owns everything between the listen socket and
the card: HTTP parsing, path mapping, `Range` handling, routing, JSON bodies,
status gathering and file streaming. `app.rs` only calls `start`.

Connection model:

- One accept thread with an 8 KiB stack accepts and nothing else. It never reads
  a request byte.
- One worker thread with a 16 KiB stack is created once and lives for the life
  of the server. At most one connection is served at a time.
- When the worker is busy the accept thread writes `HTTP/1.1 503` with the JSON
  error body and closes, so a long download never makes the next client wait.
- Each connection gets a 10 s read timeout and a 10 s write timeout, at most
  8192 bytes of request head, one request, one response, and then close. Every
  response carries `Connection: close`.
- After the threads exist, `start` refuses to run when fewer than 64 KiB of
  internal SRAM remain (`Error::Budget`). The floor is measured against the
  media pipeline's own footprint, not invented.
- One log line per request: `filesrv: <method> <path> -> <status> <bytes>`, and
  `filesrv: 503 busy` for a refused connection.

Request handling is a chain of pure functions: `http::parse`, `router::route`,
`json::status_body` / `json::list_body`, `path::map`, `range::parse` and
`body::write_body`. A malformed download path becomes `404` so it never reveals
whether a file exists. Files are streamed in 4 KiB chunks; the whole file is
never buffered. A response carries an exact `Content-Length` and
`Accept-Ranges: bytes`; a single range produces `206` with `Content-Range`, an
unsatisfiable range produces `416` with `Content-Range: bytes */<total>` and no
body, and a multi-range header is ignored and answered with the whole file.

The API is read-only and JSON only.

| Method | Path | Response |
|---|---|---|
| `GET` | `/` | JSON index of every endpoint |
| `GET` | `/api/v1/status` | status document |
| `GET` | `/api/v1/files` | entries below `<mount>/rec`, at most 200 |
| `GET` | `/api/v1/openapi.json` | the embedded OpenAPI 3.0.3 document |
| `GET`, `HEAD` | `/api/v1/files/{path}` | one file, full or ranged |

The status document always carries the same 17 properties: `version`, `build`,
`uptime_s`, `ip`, `hostname`, `mounted`, `total_bytes`, `free_bytes`,
`clock_khz`, `rtsp_port`, `rtsp_path`, `rtsp_running`, `recording_enabled`,
`recording_file`, `temperature_c`, `heap_internal_free` and
`heap_spiram_free`. `recording_file` and `temperature_c` are nullable. The
document is built from a fresh snapshot that lists the mount and the
`<mount>/rec` directory once but never reads file contents, so it stays
available on a boot without a card. The SD clock field is read only when the
volume is mounted.

`firmware/api/openapi.json` is the contract. `harness/tests/api_contract.rs`
validates the live board against it, and the firmware serves the same bytes at
`/api/v1/openapi.json` from `include_str!`.

## Configuration

All non-secret build knobs are `[env]` entries in
`firmware/.cargo/config.toml`, read with `option_env!` in
`firmware/src/settings.rs`: `VIDEO_WIDTH`, `VIDEO_HEIGHT`, `VIDEO_FPS`,
`VIDEO_JPEG_QUALITY`, `AUDIO_SAMPLE_RATE`, `AUDIO_BITRATE`, `FILE_SERVER_PORT`
and `RECORD_MODE`. Cargo exports the table to rustc, so a plain `cargo build`
resolves every knob without a `.env` parser or a generated settings file. An
exported value wins unless the entry says `force = true`; the build script
sources `.env` before invoking Cargo, so a key defined there is exported and
takes precedence over the table.

`settings::parse_u32` keeps the old knob semantics: an empty value, a
non-digit, an overflow and `0` all fall back to the default, a leading `+` is
accepted and whitespace is not trimmed. `VIDEO_JPEG_QUALITY` is clamped to the
sensor's 0 to 63 range. `RECORD_MODE` is matched case-insensitively; a typo
falls back to `both` and logs a warning at boot.

Secrets are the exception. `WIFI_SSID` and `WIFI_PASSWORD` are compile-time
inputs supplied by `scripts/build-firmware.sh` from a gitignored `.env`;
`.env.example` lists the key names with empty values. `cargo build` without them
still links and the boot path reports that the network is disabled.

## Build

`firmware/build_support.rs` holds the single ESP-IDF link-arg forwarder and its
argument splitter. The five component build scripts are two lines each: include
the shared file and call `forward_and_emit_link_args()`. `firmware/build.rs`
includes the same file, applies its C-source patches, then emits.

`firmware/build.rs` still patches three vendor C sources on purpose:

1. `cam_hal.c`: search the whole frame buffer for JPEG EOI instead of only the
   tail, because the DMA length can lag the bytes actually written and drop a
   complete frame as `NO-EOI`.
2. `cam_hal.c`: replace the `NO-SOI` warning with a dump of the frame length and
   the first eight bytes, so a rejected frame is diagnosable.
3. `esp_capture`: set the JPEG quality from `VIDEO_JPEG_QUALITY` instead of the
   vendor's fixed value.

Each patch is idempotent, warns and skips when the expected source block is not
found, and reruns the component's ninja rule so the Rust link picks up the
rebuilt archive.

## Component Versions

| Component | Version | Role |
|---|---:|---|
| `esp-idf-sys` | `=0.38.1` | ESP-IDF bindings and build integration |
| `esp-idf-hal` | `=0.47.0` | GPIO, I2S, I2C, SPI and SD HAL |
| `esp-idf-svc` | `=0.53.0` | Wi-Fi, mDNS, FAT VFS |
| `serde_json` | `1` | JSON bodies in `fileserver` only |
| `espressif/esp32-camera` | `2.1.2` | Camera component (patched at build time) |
| `espressif/mdns` | `1.14.0` | mDNS component |
| ESP-IDF | `5.5.2` | Board SDK behind the bindings |

Both workspaces pin the `esp` toolchain (rustc 1.93 nightly): the firmware for
`xtensa-esp32s3-espidf` with `-Zbuild-std`, the harness for the host. The
declared package MSRV is 1.85, which is separate from the toolchain pin.

Historical decision records that no longer describe the tree are kept for
provenance, starting with `adr-0001-packetization.md`.

## What Is Not Proven

The code below exists and compiles, but this repository has no hardware evidence
for it:

- **Recording while streaming.** The default mode attaches both the AVI muxer
  and RTSP to one capture service. Whether the vendor muxer starves the RTSP
  sink on this board is unmeasured. `RECORD_MODE=stream` and `RECORD_MODE=record`
  exist as the documented fallback, and no boot has yet produced a recording
  that `ffprobe` reads.
- **Downloading a recording over HTTP.** The ranged-download path has device
  tests and an in-process contract test, but no recording has been fetched from
  the card on this board. The SD card attached to the test board
  answers `CMD8` and then times out on `ACMD41`, so the card never mounts; the
  file list is empty and no byte-range transfer has run against real data.
- **The SD clock in the status document.** `clock_khz` is read from the SDMMC
  host slot after a successful mount. This firmware mounts through SDSPI, and
  the mounted branch has not run on hardware, so the value is unverified.
- **Device test gate.** The serial runner exists and the test binaries build,
  but the last recorded device run ended in a task stack overflow, and the runs
  that needed the card could not mount it. No device run has produced
  `VERDICT 0`; the only `VERDICT 0` on record is the runner's synthetic
  self-test.
- **Wi-Fi, mDNS and RTSP under load.** The live board has answered HTTP and
  joined the network, but long-run streaming behaviour is covered only by the
  host harness and earlier measurements, not by this tree's device tests.
