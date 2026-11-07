# Failure Modes

Status: audited against current source on 2026-11-07 (Task 18). Every
diagnostic named below is a real enum variant, `Display` string, or `println!`
format in `firmware/src/`. The coverage column separates behaviour that the
**host harness** actually executes from behaviour that is **device-blocked**:
compiled only for `xtensa-esp32s3-espidf`, or requiring a board, card, radio,
camera, or client that this environment does not have.

## Host test targets (what "host-covered" means)

`cd harness && cargo test` compiles the module through a `#[path]` include and
runs its `#[cfg(test)]` suite on the host:

| Target | Firmware modules |
|---|---|
| `audio_quality` | `audio_quality.rs` |
| `task10_recorder` | `muxer.rs`, `recorder.rs` |
| `task11_rtsp` | `session.rs`, `rtsp/` |
| `task12_payload` | `rtsp/payload/`, `jfif.rs` |
| `task13_rtcp` | `rtcp.rs` |
| `task14_net` | `session.rs`, `net.rs`, `artifact_server.rs` |
| `task18_negative` | `sd_probe.rs` (SD probe classification only) |

`camera.rs`, `sensor.rs`, `mic.rs`, `storage.rs` (mount/clock/throughput),
`runtime.rs` and `main.rs` have **no host target** — they import `esp_idf_*`
unconditionally. Their unit tests are compile-checked only for the espidf
target and need a board to run.

| Area and trigger | Log or diagnostic (source-verified) | Recovery or degradation | Coverage or blocker |
|---|---|---|---|
| Camera initialization fails, including sensor probe, PSRAM allocation, or DMA setup | `CameraError::Init` renders `esp_camera_init failed: esp_err {e:#x} ({})`; runtime adds `boot: FATAL: {message}: {error}` (the caller passes `camera initialization failed`) | Fatal: the runtime loops on a 60 s sleep and never serves or records | `camera.rs` is device-target only; no board run |
| Camera returns no frame | `GrabError::NoFrame` renders `no frame ready (esp_camera_fb_get returned NULL)`; runtime logs `boot: camera frame dropped: {error}` for that tick | Drop this capture tick and keep the driver alive; the runtime loop continues | Device-only module. The connected board has returned NULL on every grab, so this path is observed but not a host test |
| Camera frame is short or lacks trailing `FF D9` | `GrabError::InvalidFrame` renders `frame rejected: buffer does not end with JPEG EOI (FF D9)`; `frames_rejected` increments; runtime logs `boot: camera frame dropped: {error}` | Return the buffer to the driver pool, forward nothing, keep looping | EOI classifier has unit tests in `camera.rs`, but they run only on the espidf target; no board DMA test exists |
| Sensor PID is not OV3660 `0x3660` | One `sensor: PID={:#x} addr={:#x} {} class={:?} frames_grabbed={} frames_rejected={}` line, then `WARNING: sensor PID={:#x} ({}, class={:?}) is not the expected OV3660 (PID={:#x}) — continuing with the detected driver, do not assume OV3660 registers` | Continue with the detected driver; callers must not assume OV3660 registers | `sensor.rs` is device-target only. HIL PID read passes on the connected board (OV3660, `0x3660`, addr `0x3c`); no host test of the pure classifier |
| Sensor driver is not initialized | `ERROR: sensor identity unavailable: {}`; runtime adds `boot: WARNING: sensor probe failed: {error}` | Return `SensorError::NotInitialized`, never panic | Device-target only; runtime ordering exists but no board run proves it |
| SD card is absent or the candidate CS pin is wrong | Per-pin `storage: probe CS={} verdict={}`, aggregate `storage: CardProbe={} state={}`, then `storage: ERROR: CardProbe=Absent state=absent` (the rest of the line says MISO was all-ones on every candidate pin and that storage is disabled, not a mount failure); runtime logs `boot: WARNING: SD storage unavailable: {error}` | Disable recording and keep streaming when the network path is available | Probe/classification logic is now host-tested in `task18_negative`; real GPIO probing is board-only |
| SD card is found but mount or volume is invalid | `StorageError::FormatOrVolume` renders `card present on {cs} but the volume did not mount at {stage}` followed by `format/volume error, not an absent card ({detail})` | Disable recording; do not retry the other CS after a confirmed card | Error mapping is source-verified; FAT mount and SD writes are board-pending |
| SD transfer or negotiated clock is unsuitable | `storage: clock measured requested={} kHz real={} kHz (sdspi_host_get_real_freq)`, `storage: trial @{} kHz:`, then `StorageError::LadderExhausted` if every rung fails | Retune 40 → 20 → 10 MHz; if the path still fails, disable recording | Clock-ladder logic lives in device-only `storage.rs`; throughput acceptance requires a real card and explicit SD-write approval |
| **New observed defect:** the raw probe detects a card, then `sdmmc_card_init` is rejected | `StorageError::CardInit` renders `sdmmc_card_init failed on {cs} after identification: {source}`; on the connected board the observed line ends `ESP_ERR_NOT_SUPPORTED` | Currently the typed error propagates; runtime logs `boot: WARNING: SD storage unavailable: {error}` and disables recording. **Not fixed** | **Open defect.** Occurs after the raw CMD8/ACMD41 probe but **before any filesystem code** (`Fatfs::new_sdcard` / mount / format are never reached). Root cause unknown (card, SPI mode/timing, or driver); see "Open Defects" below |
| SD becomes full, is removed, or a segment cannot open | `RecorderError::SegmentOpen` renders `recorder: cannot open segment {} ({}): {source}`, `RecorderError::Write` renders `recorder: write failed on segment {} ({}): {source}`, `RecorderError::Stopped` renders `recorder: stopped: {reason}` | Roll at most once under `ErrorPolicy::Roll`, closing the prior segment cleanly; stop with a typed error if the next cannot open | Recorder error/roll policy is host-tested in `task10_recorder`; real SD failure injection is not |
| Recorder reaches its AVI 1.0 ceiling or segment limit | `MuxerError::SegmentFull` — checked before the offending packet is written | Finish the current independent AVI and open the next name; never write past the limit | Host-tested in `task10_recorder`; no board recording exists |
| Microphone setup or enable fails | `MicError::Init` renders `PDM RX init failed: {e}`; `MicError::Enable` renders `PDM RX enable failed: {e}`; runtime logs `boot: WARNING: microphone unavailable: {error}` | Continue video-only; `audio_enabled=false` is reported in `boot: ready recording={} audio={} rtsp={}` | Error types are source-verified; `mic.rs` is device-target only |
| Microphone DMA times out or returns a short frame | `WARNING: mic read timeout after {waited_ms} ms — frame dropped (task alive)` or `WARNING: mic short read {got}/{expected} bytes — frame dropped` | Drop the incomplete frame, preserve sample alignment, sleep 20 ms and retry | Pure audio-quality processing is host-tested; PDM capture needs board verification |
| Microphone driver read fails | `MicError::Read` renders `PDM RX read failed: {e}`; the task logs `ERROR: {e} — retrying in 20 ms (task neither blocks nor spins)` | Keep the capture task alive and retry after a bounded sleep | Device-target only; no PDM HIL result |
| Microphone receiver is dropped | `mic: frame receiver dropped — capture task exiting after {} frames` | End the dedicated capture task cleanly | Device-target only; runtime ownership is pending |
| Wi-Fi credentials are absent or invalid | `NetError::SsidInvalid` renders `SSID length {len} outside 1..=32 bytes`; `NetError::PasswordInvalid` renders the corresponding password message; runtime logs `boot: WARNING: invalid Wi-Fi credentials: {error}` or `boot: Wi-Fi credentials unavailable; network disabled` | Take the no-network branch; recording can still run | Validation and policy are host-tested in `task14_net`; no credential literal belongs in this repository |
| Wi-Fi start, connect, or station netif fails | `NetError::WifiStart`/`NetError::WifiConnect`/`NetError::WifiNetifUp` diagnostics; runtime logs `boot: WARNING: network unavailable: {error}` | Retry with 1, 2, 4, 8, 16, then 30 second backoff; streaming stays disabled until the link returns | Backoff maths is host-tested in `task14_net`; board Wi-Fi is pending and credentials are unavailable here |
| Wi-Fi link drops during a session | Session receives `Event::WifiLost` and logs the reconnect attempt/backoff | Stop capture, tear down the server, wait for `Event::WifiRestored` before accepting a new session | Session state machine is host-tested; live reconnect is blocked by hardware |
| SNTP does not sync within 90 seconds | `WARN sntp: not synced ({reason}); is_synced=false, RTCP Sender Reports stay suppressed` | Keep `is_synced=false`, switch to the fallback after 120 s, and suppress RTCP Sender Reports; never fabricate wall time | Clock policy is host-tested in `task14_net`; live SNTP is board/network pending |
| mDNS or artifact HTTP service fails | `NetError::MdnsInit` (`EspMdns::take failed`), `NetError::MdnsConfig`, or `NetError::ArtifactServer`; runtime logs `boot: WARNING: network unavailable: {error}` | Network bring-up fails typed; discovery or artifact retrieval is unavailable while streaming may continue where possible | Error mapping exists; the runtime boot path now starts these services, but no board run verifies them |
| RTSP client sends malformed request or oversized frame | `RequestError::MalformedRequestLine`/`MissingCSeq`/`InvalidCSeq` map to 400; `DecodeError::HeaderTooLarge`/`BodyTooLarge`/`MalformedHeader` close the connection | Send RTSP 400 when framing permits, otherwise close; never panic | Host matrix in `task11_rtsp` covers parser errors, non-numeric length, non-UTF-8 headers, and 8 KiB header / 64 KiB body limits; live socket test is pending |
| RTSP client asks for an unsupported method, path, track, or transport | 405, 404, 404, or 461 according to the RTSP status contract | Refuse only the invalid request; a second active client receives 453 without disturbing the first | Host RTSP matrix covers status behaviour; board listener bring-up is blocked |
| RTSP control connection is idle too long or disconnects | `on_disconnect` returns the teardown actions; the service timeout path runs the same handler | Stop capture and tear down the active server session from the disconnect action | Session/service behaviour is host-tested (socket loopback marked HIL); live RTSP is approval-gated |
| RTP send hits a slow client or low DMA memory | `capture backoff: DMA free {free} B < {} B floor (plan (b))`, `capture backoff: DMA largest block {largest} B < {} B floor (plan (b))`, or `capture backoff: slow-client cooldown until {until_ms} ms (plan (c), frames dropped not queued)` | Back off capture for the cooldown or suspend below the DMA thresholds; drop frames instead of queueing unbounded data | Policy is host-tested in `task14_net`; the capture-to-sender call path needs a board |
| Camera input is not a supported baseline JFIF or contains corrupt marker lengths | Typed `JfifError` or `PacketizeError::Jfif` | Emit no RTP packet for the frame and leave sequence/timestamp state unchanged | RFC 2435 negative cases and FFmpeg/GStreamer host gates pass |
| Audio input cannot be represented by the fixed track buffer | Typed track error is returned before emission | Emit no malformed packet; preserve the prior RTP state | PCMU packet size, payload type, and timestamp tests pass on the host |
| Current runtime boot path is invoked | `boot: ready recording={} audio={} rtsp={}` after ordered bring-up; each subsystem failure logs `boot: WARNING:` followed by the specific message above and degrades | Camera failure is fatal; SD/mic/network/RTSP failures disable only their feature. On the connected board, camera frames are NULL and SD init is rejected, so the observed state is no stream and no recording | Not host-testable (`runtime.rs`/`main.rs` import `esp_idf_*`); F3 remains open |

## Open Defects

1. **SD init rejects a card the raw probe detects — `ESP_ERR_NOT_SUPPORTED`.**
   The probe sequence completes identification `storage: probe CS={} -> detected (` and then `sdmmc_card_init` fails with the typed
   `StorageError::CardInit`, observed as
   `sdmmc_card_init failed on {cs} after identification: {source}` (observed suffix `ESP_ERR_NOT_SUPPORTED`).
   This happens **before any filesystem code**: `Fatfs::new_sdcard`, mount,
   format, and the clock ladder are never reached. The device therefore
   disables recording while streaming can continue. Not fixed here; requires
   board/card investigation and explicit SD-write approval.

## Reading Test Status

Host tests prove pure parsing, state transitions, packet layout, synthetic AVI
output, the SD probe classifier, and every typed diagnostic string quoted
above that lives in a host-compiled module. They cannot prove GPIO wiring,
camera DMA, PDM signal quality, FAT mount behaviour, negotiated SD frequency,
lwIP socket acceptance, Wi-Fi, SNTP, or an end-to-end client. The device
unit-test gate has not produced a clean `test result:` line, there is no HIL
suite, and no end-to-end SD or RTSP smoke test has passed. Those items must
remain visibly pending.
