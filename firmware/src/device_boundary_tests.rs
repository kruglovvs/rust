//! On-device boundary-case unit tests (plan Todo 15, part 2).
//!
//! Hand-written explicit boundary cases with **named constants**, because
//! property-based testing at scale is not viable on the target. Each test
//! names the exact boundary it pins (`u32::MAX`, `u32::MAX - 1`, `0`,
//! `0x7FFF_FFFF`) so a failure points at the boundary, not at a magic number.
//!
//! Scope note (plan Todo 15, line 535): `firmware/` is a bin-only crate, so
//! `firmware/tests/*.rs` cannot import its modules. This suite therefore lives
//! inside the binary as `#[cfg(test)]`, wired from `main.rs`, and runs on the
//! board via `cargo test --target xtensa-esp32s3-espidf` under
//! `scripts/run-device-tests.sh`.
//!
//! Each module-level suite that already exists (sdp, request, jfif, muxer,
//! recorder, rtcp, audio_track) is left untouched; this file only adds the
//! boundary coverage the plan names that those suites do not pin explicitly.

use crate::recorder::{segment_file_name, DEFAULT_SEGMENT_SIZE_BYTES, SEGMENT_INDEX_WIDTH};
use crate::rtcp::{rtp_delta_units, rtp_delta_us, RtpElapsed, CLOCK_RATE_AUDIO_HZ, CLOCK_RATE_VIDEO_HZ};
use crate::rtsp::sdp::{generate, SdpParams};

/// Named RTP-timestamp boundaries (32-bit wrap).
const TS_ZERO: u32 = 0;
const TS_ONE: u32 = 1;
const TS_HALF: u32 = 0x7FFF_FFFF;
const TS_MAX_MINUS_ONE: u32 = u32::MAX - 1;
const TS_MAX: u32 = u32::MAX;

// ---- A/V timestamp arithmetic: 32-bit wrap at the named boundaries ----------

#[test]
fn rtp_delta_across_the_wrap_boundary_is_forward_distance() {
    // u32::MAX -> 0 is one forward unit, not a 4-billion-unit backward jump.
    assert_eq!(rtp_delta_units(TS_MAX, TS_ZERO), 1);
    // u32::MAX - 1 -> 0 is two forward units.
    assert_eq!(rtp_delta_units(TS_MAX_MINUS_ONE, TS_ZERO), 2);
    // Same value: zero distance, never a full-wrap.
    assert_eq!(rtp_delta_units(TS_MAX, TS_MAX), 0);
    assert_eq!(rtp_delta_units(TS_ZERO, TS_ZERO), 0);
}

#[test]
fn rtp_delta_at_the_half_range_boundary() {
    // Exactly half the 32-bit space forward is 0x7FFF_FFFF units.
    assert_eq!(rtp_delta_units(TS_ZERO, TS_HALF), TS_HALF);
    // One past half is still a forward distance (no sign flip in u32 space).
    assert_eq!(rtp_delta_units(TS_ZERO, TS_HALF + 1), TS_HALF + 1);
}

#[test]
fn rtp_delta_us_converts_units_at_both_clock_rates_across_the_wrap() {
    // 1 unit at 90 kHz video = 11.11… µs, floored by integer math to 11.
    assert_eq!(rtp_delta_us(TS_MAX, TS_ZERO, CLOCK_RATE_VIDEO_HZ), 11);
    // 1 unit at 8 kHz audio = 125 µs exactly.
    assert_eq!(rtp_delta_us(TS_MAX, TS_ZERO, CLOCK_RATE_AUDIO_HZ), 125);
    // A zero clock rate must not divide by zero.
    assert_eq!(rtp_delta_us(TS_MAX, TS_ZERO, 0), 0);
}

#[test]
fn elapsed_accumulates_monotonically_across_the_wrap_boundary() {
    let mut e = RtpElapsed::new();
    // First sample anchors at zero.
    assert_eq!(e.push(TS_MAX_MINUS_ONE), 0);
    // Stepping onto u32::MAX adds one unit.
    assert_eq!(e.push(TS_MAX), 1);
    // Stepping across the wrap to 0 adds one more: total 2, never regresses.
    assert_eq!(e.push(TS_ZERO), 2);
    assert_eq!(e.push(TS_ONE), 3);
    assert_eq!(e.total_units(), 3);
}

#[test]
fn elapsed_never_goes_backwards_on_repeated_timestamp() {
    let mut e = RtpElapsed::new();
    e.push(TS_MAX);
    // A duplicate timestamp is a zero-distance step, not a regression.
    assert_eq!(e.push(TS_MAX), 0);
    assert_eq!(e.total_units(), 0);
}

// ---- Recording segment naming: index boundaries and rollover ----------------

#[test]
fn segment_name_at_index_boundaries() {
    assert_eq!(SEGMENT_INDEX_WIDTH, 4, "plan naming is REC_%04d.avi");
    assert_eq!(segment_file_name(1), "REC_0001.avi");
    // Last four-digit index keeps the fixed width.
    assert_eq!(segment_file_name(9_999), "REC_9999.avi");
    // First five-digit index grows but stays strictly monotonic.
    assert_eq!(segment_file_name(10_000), "REC_10000.avi");
}

#[test]
fn segment_name_at_u32_index_extremes_does_not_panic() {
    // The index is u32; the extreme values must format without panicking and
    // keep the .avi suffix. (Real use never reaches these; the boundary is
    // pinned so a formatter change is caught.)
    assert!(segment_file_name(u32::MAX).starts_with("REC_"));
    assert!(segment_file_name(u32::MAX).ends_with(".avi"));
    assert_eq!(segment_file_name(0), "REC_0000.avi");
}

#[test]
fn rollover_size_threshold_is_256_mib() {
    // Plan: 256 MiB per segment. Pinned as a named constant so a silent
    // change to the rollover size is caught on the board.
    assert_eq!(DEFAULT_SEGMENT_SIZE_BYTES, 256 * 1024 * 1024);
}

// ---- RTSP SDP: the locked rtpmap lines, byte-exact at the boundary ----------

#[test]
fn sdp_audio_rtpmap_is_exactly_pcmu_8000_on_device() {
    let s = generate(&SdpParams {
        session_id: "1",
        version: 1,
        control_base: "rtsp://192.168.1.5:554/mjpeg/1/",
    });
    // Exact line, byte for byte, with the CRLF that terminates it.
    assert!(s.contains("a=rtpmap:0 PCMU/8000\r\n"));
    // And the audio clock rate in the SDP matches the RTP clock constant.
    assert_eq!(CLOCK_RATE_AUDIO_HZ, 8_000);
}

#[test]
fn sdp_session_id_at_u64_boundary_is_emitted_verbatim() {
    // Session id is a string; the version field is u64. Boundary-check the
    // u64 version at its maximum so the origin line never truncates.
    let s = generate(&SdpParams {
        session_id: "0",
        version: u64::MAX,
        control_base: "rtsp://h:554/mjpeg/1/",
    });
    let origin = format!("o=- 0 {} IN IP4 0.0.0.0", u64::MAX);
    assert!(s.lines().any(|l| l == origin));
}

#[test]
fn heartbeat_marker_is_emitted_for_the_runner_watchdog() {
    println!("HEARTBEAT device_boundary_tests");
}
