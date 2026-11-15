//! Host tests for the firmware's compile-time settings module.
//!
//! `firmware/src/settings.rs` is deliberately free of `esp_idf_*` imports so
//! this harness can `#[path]`-include it and exercise the const parser without
//! an ESP toolchain or a board.

// Only the parser is exercised here; the firmware crate owns the constants.
#[allow(dead_code)]
#[path = "../../firmware/src/settings.rs"]
mod settings;

use settings::{parse_record_mode, parse_u32, record_mode_is_known, RecordMode};

/// Stand-in for a capture-geometry default (`VIDEO_WIDTH`).
const DEFAULT: u32 = 640;

#[test]
fn parses_a_decimal_value() {
    assert_eq!(parse_u32(Some("80"), DEFAULT), 80);
}

#[test]
fn empty_string_falls_back_to_default() {
    assert_eq!(parse_u32(Some(""), DEFAULT), DEFAULT);
}

#[test]
fn non_numeric_falls_back_to_default() {
    assert_eq!(parse_u32(Some("abc"), DEFAULT), DEFAULT);
    assert_eq!(parse_u32(Some(" 80"), DEFAULT), DEFAULT);
}

#[test]
fn overflow_falls_back_to_default() {
    assert_eq!(parse_u32(Some("99999999999"), DEFAULT), DEFAULT);
    assert_eq!(parse_u32(Some("4294967296"), DEFAULT), DEFAULT);
}

#[test]
fn leading_plus_is_accepted() {
    assert_eq!(parse_u32(Some("+12"), DEFAULT), 12);
}

#[test]
fn zero_falls_back_to_default() {
    // The `read_knob` helper this replaces filtered non-positive values, so a
    // zero knob must keep resolving to the default instead of reaching the
    // capture configuration.
    assert_eq!(parse_u32(Some("0"), DEFAULT), DEFAULT);
    assert_eq!(parse_u32(Some("+0"), DEFAULT), DEFAULT);
}

#[test]
fn absent_value_falls_back_to_default() {
    assert_eq!(parse_u32(None, DEFAULT), DEFAULT);
}

#[test]
fn record_mode_defaults_to_both() {
    assert_eq!(parse_record_mode(None), RecordMode::Both);
    assert_eq!(RecordMode::default(), RecordMode::Both);
}

#[test]
fn record_mode_accepts_every_documented_spelling_case_insensitively() {
    assert_eq!(parse_record_mode(Some("both")), RecordMode::Both);
    assert_eq!(parse_record_mode(Some("BOTH")), RecordMode::Both);
    assert_eq!(parse_record_mode(Some("BoTh")), RecordMode::Both);
    assert_eq!(parse_record_mode(Some("stream")), RecordMode::Stream);
    assert_eq!(parse_record_mode(Some("Stream")), RecordMode::Stream);
    assert_eq!(parse_record_mode(Some("record")), RecordMode::Record);
    assert_eq!(parse_record_mode(Some("RECORD")), RecordMode::Record);
}

#[test]
fn unknown_record_mode_falls_back_to_both_and_is_detectable() {
    // A typo must select the documented default and stay visible to the boot
    // path so it can log a warning instead of being silently ignored.
    assert_eq!(parse_record_mode(Some("bogus")), RecordMode::Both);
    assert!(!record_mode_is_known(Some("bogus")));
    assert!(!record_mode_is_known(Some("")));
    assert!(!record_mode_is_known(Some(" both")));
}

#[test]
fn absent_record_mode_is_known_but_never_warns() {
    assert!(record_mode_is_known(None));
    for value in ["both", "stream", "record"] {
        assert!(
            record_mode_is_known(Some(value)),
            "{value} must be recognised"
        );
    }
}

#[test]
fn record_mode_reports_which_consumers_attach() {
    assert!(RecordMode::Both.records() && RecordMode::Both.streams());
    assert!(RecordMode::Stream.streams() && !RecordMode::Stream.records());
    assert!(RecordMode::Record.records() && !RecordMode::Record.streams());
    assert_eq!(RecordMode::Both.as_str(), "both");
    assert_eq!(RecordMode::Stream.as_str(), "stream");
    assert_eq!(RecordMode::Record.as_str(), "record");
}
