# Failure Modes

Status: current source reference, 2026-10-18. The recovery descriptions below
are the policies implemented by the subsystem modules. The final column says
whether the behavior is host-covered or still blocked by missing runtime or
hardware verification.

| Area and trigger | Log or diagnostic | Recovery or degradation | Coverage or blocker |
|---|---|---|---|
| Camera initialization fails, including sensor probe, PSRAM allocation, or DMA setup | `CameraError::Init` includes the ESP error code and name | Fatal. The planned boot sequence cannot serve or record without a camera | Camera wrapper tests are host-compilable. Board boot is blocked because `main.rs` is still the binding smoke test |
| Camera returns no frame | `GrabError::NoFrame` | Drop this capture tick and keep the driver alive | Camera logic is unit-tested; runtime loop is not wired |
| Camera frame is short or lacks trailing `FF D9` | `GrabError::InvalidFrame`; rejection counter increments | Return the buffer to the driver and forward nothing | EOI classification is host-tested; no board DMA test exists |
| Sensor PID is not OV3660 `0x3660` | Identity line followed by `WARNING: sensor PID=...` | Continue with the detected driver. Callers must not assume OV3660 registers | PID classification and logging logic are host-tested; live identity is board-pending |
| Sensor driver is not initialized | `ERROR: sensor identity unavailable: ...` | Return `SensorError::NotInitialized`, never panic | Host logic is covered; runtime ordering is blocked by the missing boot path |
| SD card is absent or the candidate CS pin is wrong | Per-pin probe outcome, then aggregate `CardProbe::Absent` | Disable recording and continue streaming if networking is available | Probe and classification are host-tested; real card probing is approval-gated |
| SD card is found but mount or volume is invalid | `StorageError::FormatOrVolume` and an error identifying it as not an absent card | Disable recording. Do not retry the other CS after a confirmed card | Host tests cover classification; FAT mount behavior and SD writes are board-pending |
| SD transfer or negotiated clock is unsuitable | Storage throughput diagnostics include the negotiated frequency | Retune from 40 MHz to 20 MHz, then 10 MHz. If the path still fails, disable recording | Clock ladder logic is present; throughput acceptance requires a real card and explicit SD-write approval |
| SD becomes full, is removed, or a segment cannot open | `RecorderError::SegmentOpen`, `Write`, or `Stopped` identifies the segment | Finish the prior segment where possible, roll once under `ErrorPolicy::Roll`, then stop cleanly if the next segment cannot open | Recorder error and rollover behavior are host-tested; real SD failure injection is not |
| Recorder reaches its AVI 1.0 ceiling or segment limit | `MuxerError::SegmentFull` or recorder segment diagnostic | Finish the current independent AVI and open the next name. Never write past the limit | Host tests cover pre-check and rollover; no board recording exists |
| Microphone setup or enable fails | `MicError::Init` or `MicError::Enable` | Planned runtime logs one specific error and continues video-only | Error types and capture setup compile; runtime degradation branch is not connected |
| Microphone DMA times out or returns a short frame | `WARNING: mic read timeout ...` or `WARNING: mic short read ...` | Drop the incomplete frame, preserve sample alignment, retry after the bounded delay | Host audio quality tests cover pure processing; PDM capture needs board verification |
| Microphone driver read fails | `ERROR: PDM RX read failed ...; retrying in 20 ms` | Keep the capture task alive and retry without a spin | Host-compilable error policy exists; no PDM HIL result exists |
| Microphone receiver is dropped | `mic: frame receiver dropped ...` | End the dedicated capture task cleanly | Task behavior is source-tested where possible; runtime ownership is pending |
| Wi-Fi credentials are absent or invalid | `NetError::SsidInvalid` or `PasswordInvalid` from validation | Take the no-network branch. Recording can still run once runtime wiring exists | Validation and policy are host-tested; no credential literal belongs in this repository |
| Wi-Fi start, connect, or station netif fails | Network error identifies initialization, start, connect, or netif-up stage | Retry with 1, 2, 4, 8, 16, then 30 second backoff. Streaming remains disabled until the link returns | Pure backoff is host-tested; board Wi-Fi is pending and credentials are unavailable here |
| Wi-Fi link drops during a session | Session receives `WifiLost` | Stop capture, tear down the server, and wait for `WifiRestored` before accepting a new session | Session state machine is host-tested; live reconnect is blocked by runtime and hardware |
| SNTP does not sync within 90 seconds | `WARN sntp: not synced ...` | Keep `is_synced=false`, switch to the fallback after 120 seconds, and suppress RTCP Sender Reports. Never fabricate wall time | Clock policy is host-tested; live SNTP is board and network pending |
| mDNS or artifact HTTP service fails | `NetError::MdnsInit`, `MdnsConfig`, or `ArtifactServer` | Keep the network path available where possible, but discovery or artifact retrieval is unavailable | Error mapping exists; no boot path starts these services |
| RTSP client sends malformed request or oversized frame | Request parser maps malformed request, missing or invalid CSeq to 400. Frame decoder rejects oversized headers or bodies | Send an RTSP error when framing permits it, otherwise close the connection. Never panic | Host matrix covers parser and decoder limits; live socket test is pending |
| RTSP client asks for an unsupported method, path, track, or transport | 405, 404, 404, or 461 according to the RTSP status contract | Refuse only the invalid request. A second active client receives 453 without disturbing the first | Host RTSP matrix covers status behavior; board listener bring-up is blocked |
| RTSP control connection is idle too long or disconnects | Service timeout or session disconnect path | Stop capture and tear down the active server session from the disconnect action | Session and service behavior are host-tested; live RTSP is approval-gated |
| RTP send hits a slow client or low DMA memory | Session policy records a write stall or returns a DMA backoff reason | Back off capture for the configured cooldown, or suspend below the DMA thresholds. Drop frames instead of queueing unbounded data | Policy is host-tested; the capture-to-sender call path is not wired |
| Camera input is not a supported baseline JFIF or contains corrupt marker lengths | Typed `JfifError` or `PacketizeError::Jfif` | Emit no RTP packet for the frame and leave sequence and timestamp state unchanged | RFC 2435 negative cases and FFmpeg/GStreamer host gates pass |
| Audio input cannot be represented by the fixed track buffer | Typed track error is returned before emission | Emit no malformed packet; preserve the prior RTP state | PCMU packet size, payload type, and timestamp tests pass on the host |
| Current runtime boot path is invoked | `main.rs` currently prints `esp32s3-av-firmware: camera bindings present ...` and returns | No capture, SD mount, Wi-Fi, SNTP, recorder, or RTSP service starts. This is a blocker, not a successful boot mode | G-1 remains open. Runtime implementation and its build verification are outside this documentation task |

## Reading Test Status

Host tests prove pure parsing, state transitions, packet layout, and synthetic
AVI output. They cannot prove GPIO wiring, camera DMA, PDM signal quality, FAT
mount behavior, negotiated SD frequency, lwIP socket acceptance, Wi-Fi,
SNTP, or an end-to-end client. The device unit-test gate has not produced a
clean `test result:` line, there is no HIL suite, and no end-to-end SD or RTSP
smoke test has run. Those items must remain visibly pending until the runtime
boot path exists and the required approval is granted.
