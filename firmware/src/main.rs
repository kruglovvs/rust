//! Minimal entry point whose only job is to force generation of
//! `esp_idf_sys::camera` bindings (Todo 2 acceptance).
//!
//! Bindgen emits these as plain `unsafe extern "C"` functions inside
//! `esp_idf_sys::camera` (NOT `*_fn` type aliases — esp-idf-sys does not
//! generate those for this component; the original `_fn` imports failed with
//! E0432 and were corrected against the generated `out/bindings.rs`).

/// Safe RAII wrapper over the same bindings (Todo 4). Wave 2 wires it into
/// the boot path; until then its API is intentionally unused here.
#[allow(dead_code)]
mod camera;

/// Runtime sensor-identity probe (Todo 5). Wave 2 calls
/// `log_sensor_identity` from the boot path; until then its API is
/// intentionally unused here.
#[allow(dead_code)]
mod sensor;

/// Wi-Fi STA, mDNS `_rtsp._tcp` discovery, SNTP time source (held handle) and
/// the Wi-Fi supervisor (plan Todo 14). The pure config/clock/backoff layer is
/// host-proven via `harness/tests/task14_net.rs`; boot wiring lands in Todo 20
/// — until then the API is intentionally unused here.
#[allow(dead_code)]
mod net;

/// RTSP session layer (plan Todo 11): `OPTIONS`/`DESCRIBE`/`SETUP`/`PLAY`/
/// `TEARDOWN`/`GET_PARAMETER`, SDP with the plan-locked codec strings
/// (`a=rtpmap:0 PCMU/8000`, `a=rtpmap:26 JPEG/90000`), `Transport:` parsing
/// for TCP-interleaved and UDP, and single-session admission (second client
/// refused with 453 without disturbing the first). Host-proven via
/// `harness/tests/task11_rtsp.rs` and the `task11_errors` evidence bin;
/// Todo 12 consumes `rtsp::RtpSender` for interleaved/UDP RTP.
#[allow(dead_code)]
mod rtsp;

/// RFC 2435 payloader (plan Todo 6) — included as `mod jfif;` exactly as its
/// own doc header specifies ("Included from the firmware as `mod jfif;` and
/// from the harness via `#[path]`"). `rtsp::payload::JpegTrack` (Todo 12) is
/// its consumer; the file itself is unchanged from Todo 6.
#[allow(dead_code)]
mod jfif;

/// RTP payloaders (plan Todo 12): 16 kHz→8 kHz 33-tap half-band decimation +
/// G.711 µ-law PCMU (payload type **0**, SDP lock `a=rtpmap:0 PCMU/8000`) and
/// RFC 2435 JPEG (payload type **26**), emitted as complete RTP datagrams
/// through a sink that is Todo 11's `rtsp::server::RtpSender::send_rtp` on
/// firmware. Pure std + `audio-codec-algorithms` (the plan's one new
/// dependency) — host-proven via `harness/tests/task12_payload.rs` and the
/// `task12_report` evidence bin. Boot wiring lands in Todo 20; until then the
/// API is intentionally unused here.
#[allow(dead_code)]
#[path = "rtsp/payload/mod.rs"]
mod payload;

/// Periodic RTCP Sender Reports (plan Todo 13): hand-rolled RFC 3550 §6.4.1
/// SR (28 bytes, zero new crates), NTP 64-bit mapping from Todo 14's SNTP
/// wallclock (`net::last_sync_utc()` via `SyncSample`/`SrClock` — never
/// synthesised; unsynced ⇒ SRs suppressed entirely with the exact log line
/// `unsynced, SR suppressed`), explicit 32-bit RTP-timestamp wrap arithmetic,
/// the ≥ 5 s scheduler (pure decision fn), per-track packet/octet counters
/// (`counting_sink`) and `spawn_sender_reports` for the device loop against
/// Todo 11's `RtpSender::send_rtcp`. Incoming RRs are parsed/logged only
/// (documented scope limit). Host-proven via `harness/tests/task13_rtcp.rs`;
/// session wiring lands in Todo 20 — until then the API is intentionally
/// unused here.
#[allow(dead_code)]
mod rtcp;

/// Session lifecycle + backpressure policy (plan Todo 14 load-bearing
/// behaviours (b)/(c)/(d): DMA-floor backoff, capture-only-while-a-client-is-
/// connected with a server-provided cooldown, teardown from the disconnect
/// callback). Pure std — host-proven via `harness/tests/task14_net.rs`.
#[allow(dead_code)]
mod session;

/// Read-only `GET /files/<name>` artifact-retrieval endpoint (plan Todo 14):
/// flat path validated against `..`/separators/escapes, hard 512 MiB cap, no
/// directory listing, 32 KiB chunked streaming — the host-retrieval mechanism
/// for the later ffprobe gates. Path logic host-proven via
/// `harness/tests/task14_net.rs`.
#[allow(dead_code)]
mod artifact_server;

/// Segment-rollover A/V recorder (Todo 10): single-clock PTS (drop, never
/// duplicate, video), 256 MiB / 60 min / storage-error rollover into
/// separate `REC_%04d.avi` files. Pure std + `muxer` — host-proven via
/// `harness` (task10_recorder bin + test) with real ffprobe. Storage/camera
/// wiring lands in Todo 20; until then the API is intentionally unused here.
#[allow(dead_code)]
mod recorder;

/// SD-card storage over SPI (Todo 7): CS-variant sweep (`Detected(GPIO21) |
/// Detected(GPIO3) | Absent`), `CMD0`→`CMD8`/`ACMD41`/`CMD58` probe, one
/// mount on the detected pin, and the 40 → 20 → 10 MHz clock ladder with the
/// acceptance battery. The recorder (Todo 10) is the first caller; boot
/// wiring lands there — until then the API is intentionally unused here.
#[allow(dead_code)]
mod storage;

/// Zero-dependency audio-quality statistics (Todo 8): `FrameStats`,
/// NIST STNR, Goertzel, rail-run counting. Pure math — its test suite
/// RUNS on the host via `harness/tests/audio_quality.rs`; the
/// capture-side thresholds (Todo 16) consume it through `mic`.
#[allow(dead_code)]
mod audio_quality;

/// On-board PDM microphone capture (Todo 8): 16 kHz mono 16-bit, 20 ms
/// `[i16; 320]` frames on a dedicated task (CLK=GPIO42, DATA=GPIO41,
/// `dma_desc_num=4`). The recorder (Todo 10) is the first consumer of
/// `MicCapture::spawn`'s channel; boot wiring lands there — until then
/// the API is intentionally unused here.
#[allow(dead_code)]
mod mic;

/// AVI recording muxer behind our own `Muxer` trait (Todo 9). The
/// recorder (Todo 10) is its first caller; until then the API is
/// intentionally unused here. Pure std + oxideav — host-proven via
/// `harness` with real ffprobe.
#[allow(dead_code)]
mod muxer;

#[cfg(test)]
mod device_boundary_tests;

use esp_idf_sys::camera::{
    camera_config_t, camera_fb_t, esp_camera_deinit, esp_camera_fb_get, esp_camera_fb_return,
    esp_camera_init, esp_camera_sensor_get,
};

/// Compile-time proof that every required symbol exists in the generated
/// `esp_idf_sys::camera` module. `stringify!` keeps the names visible in
/// rustdoc/IDE without evaluating them.
const REQUIRED_SYMBOLS: [&str; 6] = [
    "esp_camera_init",
    "esp_camera_fb_get",
    "esp_camera_fb_return",
    "esp_camera_deinit",
    "esp_camera_sensor_get",
    "camera_config_t",
];

fn main() {
    // Each binding resolves by name; taking a value forces the import to be
    // used without ever calling the function or dereferencing a pointer.
    let _init = esp_camera_init;
    let _deinit = esp_camera_deinit;
    let _fb_get = esp_camera_fb_get;
    let _fb_return = esp_camera_fb_return;
    let _sensor_get = esp_camera_sensor_get;
    let _config_size = core::mem::size_of::<camera_config_t>();
    let _fb_size = core::mem::size_of::<camera_fb_t>();

    // The camera module exists and these names resolve at compile time.
    println!(
        "esp32s3-av-firmware: camera bindings present ({} symbols: {:?})",
        REQUIRED_SYMBOLS.len(),
        REQUIRED_SYMBOLS
    );
    let _ = (_init, _deinit, _fb_get, _fb_return, _sensor_get, _config_size, _fb_size);
}