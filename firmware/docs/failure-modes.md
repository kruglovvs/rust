# Failure Modes

Status: audited against the source on 2026-11-16 (Task 18). Every diagnostic
named below is a real `Display` string, `println!` line or `logging::log!`
format in the delivered tree. The coverage column separates behaviour that the
host harness executes from behaviour that needs the board, a card, a radio or a
camera. Nothing here is a promise about hardware that has not been observed.

## Host Test Targets

`./scripts/test-harness.sh` (a `cargo test` in `harness/`) compiles these
firmware modules through `#[path]` includes and runs their `#[cfg(test)]`
suites on the host:

| Target | Firmware modules |
|---|---|
| `settings` | `firmware/src/settings.rs` (knob parsing, `RECORD_MODE`) |
| `task14_net` | `net/session.rs`, `net/net.rs` (credentials, backoff, SNTP policy) |
| `api_contract` | `fileserver/{http,router,json,body,path,range}.rs` pure functions, plus an optional live board driver |
| `e2e_rtsp` | host RTSP client and SDP parser (the live stream test self-skips without `ESP32_RTSP_URL`) |
| `analyze_stream`, `stream_capture` | harness analyser and capture tooling |

`media`, the storage hardware path, the `net` device glue, `fileserver::{server,status}`
and `app`/`main` have no host target: they import `esp_idf_*` or own sockets and
threads. Their evidence has to come from the device.

## Failure Table

| Area and trigger | Log or diagnostic (source-verified) | Recovery or degradation | Coverage or blocker |
|---|---|---|---|
| Peripherals cannot be taken | `boot: FATAL: peripheral initialization failed: <error>` | Fatal: the runtime thread parks on a 60 s sleep and serves nothing | Device only; `app.rs` |
| SPI2 bus creation fails | `boot: WARNING: SD SPI initialization failed: <error>` | Storage is skipped; Wi-Fi and streaming still start | Device only |
| No card, or the card is wired to an unknown pin | Per-pin `storage: probe CS=<pin> verdict=<tag>`, aggregate `storage: CardProbe=<probe> state=<state>`, then `storage: ERROR: CardProbe=Absent state=absent`; the error renders `no candidate CS pin selected a card (MISO all-ones on every pin)` | Recording is disabled; streaming continues | Probe classification is host-testable; GPIO probing is device only |
| A card is identified but `sdmmc_card_init` rejects it | `sdmmc_card_init failed on <cs> after identification: <source>` (`StorageError::CardInit`) | Recording is disabled; the boot line reports `recording=off` | Observed on the board (`ESP_ERR_TIMEOUT`); not host-testable |
| A card is present but the volume does not mount | `card present on <cs> but the volume did not mount at <stage>: format/volume error, not an absent card (<detail>)` | Recording is disabled; the other chip-select pin is not tried after a confirmed card | Error mapping is source-verified; mount behaviour is device only |
| The card is stuck: the raw probe still identifies it (`CMD8` returns SDv2) but `ACMD41` times out on every attempt, every boot | The probe logs `storage: probe CS=... verdict=detected`, then bring-up fails with `sdmmc_card_init failed on <cs> after identification: ESP_ERR_TIMEOUT`; no mount, format or clock ladder runs | Recording and file download are unavailable for that boot; HTTP status and `/api/v1/files` still answer with empty storage. Recovery is physical: re-seat the card or power-cycle the board | Observed on the connected board; diagnosis in `.omo/evidence/sd-diagnosis.txt` |
| Every clock rung is rejected by the qualification battery | `storage: clock measured requested=<khz> kHz real=<khz> kHz`, `storage: trial @<khz> kHz: 4 MiB write = <bps> B/s (floor 262144 B/s) unstable=<bool> (<reason>)`, then `clock ladder exhausted after [...] kHz; last trial: <reason>` | Lower the rung 40 -> 20 -> 10 MHz; if all fail, recording is disabled | Device only; requires a card |
| A rung writes but reads back differently | `read-back of <path> differs from what was written at offset <n>` (`StorageError::DataMismatch`) | That rung is rejected and the ladder falls back | Device only |
| Wi-Fi credentials are absent from the build | `boot: Wi-Fi credentials unavailable; network disabled` | No network, so no LAN client can reach RTSP or HTTP; recording can still run locally | Host-tested parsing; the boot branch is device only |
| Wi-Fi credentials are invalid | `boot: WARNING: invalid Wi-Fi credentials: <error>`; the error names the SSID or password length rule | Take the no-network branch | Host-tested in `task14_net` |
| Wi-Fi bring-up fails before the supervisor exists | `boot: WARNING: network unavailable: <error>` | No network for that boot; RTSP and the file server bind but are unreachable from the LAN | Device only |
| No station IP arrives within 60 s | `boot: waiting for station IP (<n>s elapsed)` every 5 s, then `boot: WARNING: no station IP after 60s; starting pipeline anyway` | The pipeline and server start with `0.0.0.0` as the printed address; they are unreachable until an address exists | Device only |
| The file server cannot initialise LwIP | `boot: WARNING: fileserver unavailable: network stack unavailable: <error>` | Server stays down; RTSP and the pipeline are unaffected | Typed error path is source-verified; live failure is device only |
| The file server cannot bind its port | `boot: WARNING: fileserver unavailable: listen socket unavailable: <error>` | Server stays down; RTSP keeps the media path alive | Device only; a conflicting port is the trigger |
| Fewer than 64 KiB of internal SRAM remain after the server stacks | `boot: WARNING: fileserver unavailable: internal heap too small: <free> bytes free, <required> required` | Server stays down by design, so it cannot starve the media pipeline. This is the measured guard path, not a crash | Proven on the board during the memory ladder (`.omo/evidence/task-22-firmware-slim-and-file-server/`) |
| A second request arrives while the single worker is busy | `filesrv: 503 busy` on the accept thread; the client receives `HTTP/1.1 503` with a JSON `ApiError` body | The connection is refused at once instead of waiting; the active download is not disturbed | Test lives in the crate's device test binary; see Open Defects for run status |
| The request head is oversized, incomplete or silent | `filesrv: - - -> 400 <bytes>` and a `400` response; a silent connection is closed after the 10 s read timeout | One connection is dropped; the worker slot is released | Test lives in the crate's device test binary; see Open Defects for run status |
| A download path contains `..`, NUL, bad escapes or an empty segment | The response is `404`; the log line still records the method and path | No file is opened and no existence information leaks | Pure `path::map` tests, host and device |
| A `Range` header selects no byte | `416` with `Content-Range: bytes */<total>` and a zero-length body | The client can retry with a valid range; the server stays up | Pure `range::parse` tests plus the contract test |
| The card cannot be read during a download | `filesrv: body aborted: <error>` and a `500` JSON body when the failure happens before the head; a short source read is a typed error | The response is not silently truncated as a successful `200` | Device only for real card errors |
| Status is requested on a boot with no card | `200` JSON with `"mounted":false`, `total_bytes`, `free_bytes` and `clock_khz` all `0`, `recording_file:null`; the SD clock FFI is never called and the chip does not fault | The status endpoint stays available and the board keeps answering | Previously crashed the chip; fixed and proven on a card-less boot (`.omo/evidence/task-23-firmware-slim-and-file-server/`) |
| The on-die temperature sensor cannot be installed, enabled or read | The status body carries `"temperature_c":null` and the gatherer reports the sensor as unavailable in its log | The rest of the status document still answers | Device only; no host target for `status.rs` |
| `RECORD_MODE` is empty or misspelled | `boot: WARNING: RECORD_MODE="<text>" is not one of both\|stream\|record; using both` | The build proceeds with the `both` default; the warning is the only signal | Pure parser is host-tested in `settings` |
| The AVI muxer cannot be configured or linked | `pipeline: WARNING: recording unavailable: <error>`, then the boot line reports `recording=off` | RTSP still starts; the muxer is skipped | Device only |
| The muxer fails to start after being linked | `pipeline: WARNING: muxer start failed: <error>; continuing without recording` | RTSP continues, `recording=off` | Device only |
| The vendor camera driver drops a frame as `NO-SOI` or `NO-EOI` | The EOI patch trims the frame to the real EOI by scanning forward from the SOI; a real SOI failure logs `cam_hal: NO-SOI len=<n> head=<8 bytes>` on the first reject and every 100th | The frame is dropped and capture continues. If the patch did not apply, the driver falls back to its tail-only EOI search and the plain warning | Device only; the vendor driver has no host target |
| The RTSP service fails to start | `pipeline: rtsp start returned err=<error>` from the start thread | The readiness line still names the configured RTSP URL. It is a configuration report, not proof that port 554 accepts connections | Device only |
| LwIP runs out of sockets or listening-TCP entries | The file server logs `listen socket unavailable: <error>`; the RTSP start thread logs `pipeline: rtsp start returned err=<error>` | `sdkconfig.defaults` raises `CONFIG_LWIP_MAX_SOCKETS` and `CONFIG_LWIP_MAX_LISTENING_TCP` to 16. If the limit is still hit, the affected listener stays down and the rest of the boot continues | Device only; limit values are build-verified |
| Pipeline construction fails at any other step | `boot: WARNING: GMF pipeline unavailable: <error>`, where the error names the step (`i2c_bus`, `capture_setup`, `rtsp_service`, and so on) | HTTP stays up; streaming and recording are absent | Device only |
| Recording and streaming run in the same boot (`RECORD_MODE=both`) | No warning is emitted; the risk is behavioural. The vendor muxer and the RTSP sink both consume the capture service, and starvation has not been measured on this board | Use `RECORD_MODE=stream` to stream only, or `RECORD_MODE=record` to record only, when the default misbehaves | **Unmeasured.** No 60 s recording has been produced by this tree |
| A build patch cannot find its vendor source block | `cargo:warning=... patch skipped` or `... may not be linked` during `cargo build` | The build continues with the vendor behaviour (tail-only EOI scan, plain `NO-SOI`, vendor JPEG quality) | Build-time only; messages are in `build.rs` |

## Open Defects and Unmeasured Behaviour

1. **The SD card is stuck after an aborted format.** The raw probe identifies
   it with `CMD8` but `ACMD41` times out on every attempt, so bring-up never
   reaches the filesystem. A physical re-seat or full power-cycle is required.
   Diagnosis: `.omo/evidence/sd-diagnosis.txt`.
2. **Recording during streaming is unmeasured.** `RECORD_MODE=both` is the
   default and compiles, but whether the vendor muxer starves the RTSP sink, or
   whether the file ends up empty, has not been observed. The two single-purpose
   modes are the documented fallback and must not be described as tested either.
3. **No recording has been downloaded over HTTP.** The ranged path has device
   tests and an in-process contract run, but no real file has crossed the wire.
   The test board's card is unreadable, so `/api/v1/files` returns an empty list
   and the live contract exercises the documented `404` instead of a transfer.
4. **The device test gate is not clean.** The serial runner is implemented and
   the binaries build, but the last recorded device run ended in a task stack
   overflow, and the runs that needed the card could not mount it. No device run
   has produced `VERDICT 0`; the only `VERDICT 0` on record is the runner's
   synthetic self-test.
5. **`clock_khz` on a mounted volume is unverified.** The field reads the SDMMC
   host slot, while this firmware mounts through SDSPI. The card-less branch is
   safe (it returns `0`); the mounted branch has not run on hardware.

## Reading Test Status

The host suite proves the pure layers: settings parsing, the net policy, HTTP
and path parsing, range arithmetic, JSON bodies, chunk planning, the in-process
API contract and the harness analyser. It cannot prove GPIO wiring, SD bring-up,
the card clock, LwIP socket acceptance, the file server's threads, Wi-Fi, mDNS,
camera DMA or a real recording. Those require the board and, for anything that
formats or writes the card, a deliberate device run. Commands and their expected
verdicts are listed in `README.md`.
