//! Host tests for the firmware's compile-time settings module.
//!
//! `firmware/src/settings.rs` is deliberately free of `esp_idf_*` imports so
//! this harness can `#[path]`-include it and exercise the const parser without
//! an ESP toolchain or a board.

// Only the parser is exercised here; the firmware crate owns the constants.
#[allow(dead_code)]
#[path = "../../firmware/src/settings.rs"]
mod settings;

use settings::parse_u32;

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
