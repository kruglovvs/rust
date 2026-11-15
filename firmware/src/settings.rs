//! Compile-time firmware settings.
//!
//! ## Contract
//!
//! - **Non-secret knobs** come from the `[env]` table in
//!   `firmware/.cargo/config.toml` and are read with `option_env!`. Cargo
//!   applies `[env]` to build scripts, `rustc` invocations and `cargo build`,
//!   so a plain `cargo build` (and every `cargo test --no-run`) resolves the
//!   knobs without a `.env` file or a build-script parser. An exported value
//!   wins over `[env]` unless the table says `force = true`, which keeps
//!   `VIDEO_WIDTH=… ./scripts/build-firmware.sh` working as an override.
//! - **Wi-Fi credentials are secrets.** They are never committed and never
//!   placed in the `[env]` table. `option_env!` keeps them optional: a build
//!   without them still compiles, and the firmware boots without Wi-Fi.
//! - [`parse_u32`] preserves the semantics of the deleted
//!   `build.rs::read_knob` helper: a value is accepted only when it is a
//!   non-empty decimal number greater than zero that fits in `u32`; a leading
//!   `+` is allowed; whitespace is not trimmed. Everything else falls back to
//!   the supplied default.
//!
//! The module is pure (no `esp_idf_*` imports, no allocation), so the host
//! harness `#[path]`-includes it and tests it without a device.

/// Largest JPEG quality the DVP sensor accepts.
const JPEG_QUALITY_MAX: u32 = 63;

/// Parse a decimal knob value, falling back to `default`.
///
/// Returns `default` for `None`, an empty string, a non-digit anywhere, a value
/// that overflows `u32`, and for `0` (the deleted helper filtered non-positive
/// values, so `0` must not reach the capture configuration). A leading `+` is
/// accepted; surrounding whitespace is not trimmed and therefore falls back.
pub const fn parse_u32(value: Option<&str>, default: u32) -> u32 {
    let text = match value {
        Some(text) => text,
        None => return default,
    };
    let bytes = text.as_bytes();
    if bytes.is_empty() {
        return default;
    }
    let mut index = if bytes[0] == b'+' { 1 } else { 0 };
    if index == bytes.len() {
        return default;
    }
    let mut parsed: u32 = 0;
    while index < bytes.len() {
        let digit = bytes[index].wrapping_sub(b'0');
        if digit > 9 {
            return default;
        }
        parsed = match parsed.checked_mul(10) {
            Some(scaled) => match scaled.checked_add(digit as u32) {
                Some(next) => next,
                None => return default,
            },
            None => return default,
        };
        index += 1;
    }
    if parsed == 0 { default } else { parsed }
}

/// Clamp a JPEG quality knob to the range the sensor driver accepts.
const fn clamp_jpeg_quality(value: u32) -> u32 {
    if value > JPEG_QUALITY_MAX {
        JPEG_QUALITY_MAX
    } else {
        value
    }
}

pub const VIDEO_WIDTH: u32 = parse_u32(option_env!("VIDEO_WIDTH"), 640);
pub const VIDEO_HEIGHT: u32 = parse_u32(option_env!("VIDEO_HEIGHT"), 480);
pub const VIDEO_FPS: u32 = parse_u32(option_env!("VIDEO_FPS"), 10);
pub const VIDEO_JPEG_QUALITY: u32 =
    clamp_jpeg_quality(parse_u32(option_env!("VIDEO_JPEG_QUALITY"), 12));
pub const AUDIO_SAMPLE_RATE: u32 = parse_u32(option_env!("AUDIO_SAMPLE_RATE"), 48_000);
pub const AUDIO_BITRATE: u32 = parse_u32(option_env!("AUDIO_BITRATE"), 64_000);
// Consumed by the file server; declared here so every build knob lives in one
// place and a build without it still gets the documented default.
#[allow(dead_code)]
pub const FILE_SERVER_PORT: u32 = parse_u32(option_env!("FILE_SERVER_PORT"), 80);

/// Station SSID, absent unless the build was handed one (never committed).
pub const WIFI_SSID: Option<&str> = option_env!("WIFI_SSID");
/// Station password, absent unless the build was handed one (never committed).
pub const WIFI_PASSWORD: Option<&str> = option_env!("WIFI_PASSWORD");
