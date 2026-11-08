# ESP32-S3 A/V Firmware Architecture

Status: current source reference, 2026-10-18.

## Three Hardware Facts (Decisive Constraints)

These facts eliminate options rather than enable them. Every major design choice stems from one or more of these constraints.

1. **The ESP32-S3 has no hardware H.264 encoder.** Software H.264 encoding cannot run alongside Wi-Fi on this chip. Therefore, the only real-time video format is MJPEG — which the OV3660 encodes in silicon. Frames leave the sensor already JPEG-compressed; the MCU never runs a JPEG encoder.

2. **WebM cannot carry MJPEG.** The WebM specification allows only VP8/VP9/AV1 video codecs. The Rust `webm` crate's `VideoCodecId` is literally `{ VP8, VP9, AV1 }`. AVI is the achievable container for MJPEG + PCM audio.

3. **OV3660 encodes JPEG in silicon, not software.** The sensor emits complete JFIF files at the configured quality level. Frames are reached via DMA into PSRAM and are immediately ready for packetization or recording without re-encoding.

---

The ESP32-S3 is the RTSP server. A client on the same LAN connects directly to
the board's private IP. There is no public endpoint, relay, or reverse proxy.
The runtime boot path is not wired yet. `firmware/src/main.rs` still runs the
camera binding smoke test, so the topology below describes the implemented
subsystems and the boot sequence that remains to be connected.

## Topology

```text
OV3660 camera and PDM microphone
              |
              v
       ESP32-S3 firmware
       |              |
       |              +--> FAT SD card, independent AVI segments
       |
       +--> Wi-Fi station --> private LAN IP
                              |        |
                              |        +--> RTSP client, TCP interleaved or UDP
                              |        +--> optional HTTP artifact client
                              +--> mDNS _rtsp._tcp advertisement
```

The intended server path is `rtsp://<board-private-ip>:554/live`. The RTSP
service supports `OPTIONS`, `DESCRIBE`, `SETUP`, `PLAY`, `TEARDOWN`, and
`GET_PARAMETER`, with one active session. A second client receives RTSP 453
without changing the first session. The service can negotiate TCP interleaved
RTP or UDP transport. The service module and its host tests exist, but no boot
code currently starts the service on a board.

## Capture And Shared Timing

The camera wrapper configures the Seeed XIAO ESP32-S3 Sense pin map, JPEG,
QVGA, quality 15, two PSRAM frame buffers, and `CAMERA_GRAB_LATEST`. `Camera`
owns each frame buffer and returns it to the driver exactly once. Frames that
do not end in JPEG EOI, `FF D9`, are rejected and counted.

The sensor probe reads PID, SCCB address, and driver name after camera
initialization. OV3660, PID `0x3660`, is expected. A different known or unknown
sensor is warned about and would continue with the detected driver. A missing
sensor identity is an error for that probe, not a panic.

The PDM microphone uses GPIO42 for clock and GPIO41 for data. It captures mono
16-bit audio at 16 kHz in 20 ms frames of 320 samples, or 640 bytes. The
capture task measures frames, logs health statistics, and sends `[i16; 320]`
frames to consumers. Timed out and short reads are dropped rather than padded.

The planned runtime uses one monotonic timestamp authority for camera and
audio. The recorder accepts timestamps from its caller rather than reading a
clock itself. `session.rs` suspends capture without a client, backs off for a
slow client or low DMA memory, and stops capture after disconnect or Wi-Fi
loss. These policies are implemented and host-tested. The call sites that
connect them are pending the runtime boot-path work.

## Live RTP Media

### MJPEG video

The camera produces complete JFIF files. `jfif.rs` parses the file, validates
the supported baseline JPEG structure, removes the JFIF headers and trailing
EOI from the RTP payload, and emits RFC 2435 fragments. The scan payload is
reassembled by byte offset. Huffman tables are the RFC 2435 Annex K.3 tables.
Restart markers remain in the payload when DRI is present. The final fragment
alone has the RTP marker bit.

The video SDP is locked to static RTP payload type 26 and
`a=rtpmap:26 JPEG/90000`. `jpeg_track.rs` uses a 90 kHz clock, 1400 byte RTP
payload sizing, and a 900 timestamp step, which represents 100 ms, or 10 fps.
The AVI muxer currently declares a 25 fps default, while the RTP track and
recorder plan describe a 10 fps operating point; runtime wiring must choose one
production cadence and pass it consistently to both paths.
No H.264, VP8, VP9, AV1, or WebM path is part of this design. ADR-0001 records
why `rusty_esp_video` 0.1.2 was rejected and why the in-house RFC 2435 parser
is retained.

### PCMU audio

Each 16 kHz microphone frame is half-band decimated to 160 samples at 8 kHz.
`audio_codec_algorithms` encodes those samples as 160 bytes of G.711 mu-law.
The RTP packet is 12 bytes of RFC 3550 header plus 160 bytes of payload, and
uses static payload type 0. SDP is locked to
`a=rtpmap:0 PCMU/8000`. The audio timestamp advances by 160 per 20 ms frame.
The track does not emit PCMA, L16, or a dynamic payload type.

## AVI Recording

The recorder writes one independent AVI 1.0 segment per file through
`oxideav-avi`. Video is MJPEG with `MJPG` FourCC. Audio is mono 16 kHz
`pcm_s16le`. Segment names are `REC_0001.avi`, `REC_0002.avi`, and so on;
existing names are never overwritten. The default segment limit is 256 MiB,
with a 60 minute wall clock safety limit. A storage error or the AVI 1.0
ceiling closes the current file and rolls according to `ErrorPolicy`.

The recorder's video timestamps are clock-derived slots and late frames are
dropped, never duplicated or retimed. Audio timestamps are contiguous sample
counts. Host tests create playable two-stream AVI files and verify them with
`ffprobe`. A real five minute SD recording has not been performed. The
runtime does not yet open the recorder because the boot path is unimplemented.

## Platform Services

The SD path probes GPIO21 first and GPIO3 second, then mounts once on the
detected chip select. SPI uses GPIO7 SCK, GPIO8 MISO, and GPIO9 MOSI. The
clock ladder is 40 MHz, 20 MHz, then 10 MHz, and the negotiated clock is read
back. The mount point is `/sdcard`. GPIO21 is also the user LED, so it must not
be driven as an LED while the card is mounted.

The network module validates runtime-injected Wi-Fi credentials, initializes
the ESP netif before sockets, disables Wi-Fi power save, starts station mode,
advertises `_rtsp._tcp` through mDNS, and starts SNTP. The primary SNTP name
is `pool.ntp.org` and the fallback is `time.cloudflare.com`; SNTP remains UTC
and resynchronizes every 60 minutes. RTCP Sender Reports are suppressed until
the clock has synced. These are module-level behaviors, not evidence that a
board has completed network bring-up.

The artifact server exposes only one safe file-name segment below `/files/`.
It rejects traversal, separators, percent escapes, and non-regular files. It
must not be treated as an authenticated file service.

## Exact Component Versions

| Component | Exact version or pin | Role |
|---|---:|---|
| `esp-idf-sys` | `=0.38.1` | ESP-IDF bindings and build integration |
| `esp-idf-hal` | `=0.47.0` | GPIO, I2S, SPI, and SD HAL |
| `esp-idf-svc` | `=0.53.0` | Wi-Fi, mDNS, SNTP, FAT VFS, and HTTP services |
| `oxideav-avi` | `=0.0.10` | AVI 1.0 muxer |
| `oxideav-core` | `=0.1.39` | Direct type surface required by the AVI adapter |
| `audio-codec-algorithms` | `=0.8`, default features disabled | G.711 mu-law encoder and decoder |
| `espressif/esp32-camera` | `2.1.2` | Remote camera component |
| `espressif/mdns` | `1.14.0` | Remote mDNS component |
| ESP-IDF | `5.5.2` | Board SDK used by the generated bindings |

The firmware package declares Rust edition 2021 and currently states a
`rust-version` of 1.85. The firmware build uses the `esp` toolchain,
nightly-1.93.0, with `-Zbuild-std`. The harness is a separate workspace pinned
to toolchain 1.89. These toolchain facts are distinct from the package MSRV.

## Credentials And Verification Boundaries

Wi-Fi SSID and password are runtime inputs. They must come from the local
environment or another untracked injection mechanism. They must never be
written to source, documentation, test fixtures, command examples, logs, or
`.env` committed to the repository. The repository's `.env` is untracked and
must not be edited by this documentation.

Host verification covers parser logic, packetization, audio conversion,
session policy, recorder behavior, and synthetic AVI inspection. It does not
prove camera DMA, sensor identity, PDM wiring, SD throughput, Wi-Fi, SNTP,
RTSP sockets on lwIP, or a real recording. Device unit tests, HIL tests, SD
writes, flashing, and host-to-board RTSP tests remain approval-gated or
blocked. Until a runtime implementation replaces the smoke-test `main`, no
claim of end-to-end firmware operation is valid.
