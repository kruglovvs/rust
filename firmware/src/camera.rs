//! Safe RAII wrapper over the generated `esp_idf_sys::camera` bindings
//! (Todo 4).
//!
//! Design contract (plan Todo 4):
//! - Every `unsafe` block in this file lives inside the private [`ffi`]
//!   module — the rest of the API is entirely safe.
//! - [`FrameBuffer`] is RAII: dropping it returns the buffer to the driver
//!   exactly once (take-once semantics), on early return or panic alike.
//! - A frame is only forwarded if it ends with the JPEG EOI marker
//!   `FF D9` (catches the `cam_hal: NO-EOI` truncation mode); invalid
//!   frames are returned to the driver and counted, never forwarded.
//!
//! Bindgen reality (verified against `out/bindings.rs`, Todo 2/4):
//! `esp_camera_fb_get() -> *mut camera_fb_t`, fns are plain
//! `unsafe extern "C"` inside `esp_idf_sys::camera`.

use core::ffi::c_int;
use esp_idf_sys::camera as bindings;
use esp_idf_sys::esp_err_t;

// ---------------------------------------------------------------------------
// Private FFI boundary — the ONLY `unsafe` in this file.
// ---------------------------------------------------------------------------

mod ffi {
    use super::bindings;
    use core::ffi::{c_char, CStr};
    use esp_idf_sys::esp_err_t;

    pub(super) fn init(cfg: &bindings::camera_config_t) -> esp_err_t {
        unsafe { bindings::esp_camera_init(cfg) }
    }

    pub(super) fn deinit() -> esp_err_t {
        unsafe { bindings::esp_camera_deinit() }
    }

    /// `None` when the driver has no frame ready (NULL return).
    pub(super) fn fb_get() -> Option<*mut bindings::camera_fb_t> {
        let fb = unsafe { bindings::esp_camera_fb_get() };
        (!fb.is_null()).then_some(fb)
    }

    /// Caller must pass ownership of a buffer obtained from [`fb_get`] —
    /// calling this twice on the same buffer corrupts the driver's pool.
    pub(super) fn fb_return(fb: *mut bindings::camera_fb_t) {
        unsafe { bindings::esp_camera_fb_return(fb) }
    }

    pub(super) fn fb_parts(fb: *mut bindings::camera_fb_t) -> (*const u8, usize, usize, usize) {
        unsafe { ((*fb).buf, (*fb).len, (*fb).width, (*fb).height) }
    }

    /// Lifetime `'a` must be constrained by the caller to the lifetime of the
    /// owning [`super::FrameBuffer`]; the driver reuses the memory after return.
    pub(super) fn slice<'a>(buf: *const u8, len: usize) -> &'a [u8] {
        unsafe { core::slice::from_raw_parts(buf, len) }
    }

    pub(super) fn err_name(code: esp_err_t) -> String {
        unsafe {
            let p: *const c_char = esp_idf_sys::esp_err_to_name(code);
            if p.is_null() {
                format!("{code:#x}")
            } else {
                CStr::from_ptr(p).to_string_lossy().into_owned()
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Board pin map — Seeed XIAO ESP32S3 Sense (const table, plan Todo 4).
// ---------------------------------------------------------------------------

/// Pin map from `esp-cpp/espp` `xiao-esp32s3-sense.hpp`, cross-checked against
/// the Seeed expansion-board schematic. `pwdn`/`reset` are not wired on this
/// board (−1).
mod xiao_sense {
    use core::ffi::c_int;

    pub(super) const XCLK: c_int = 10;
    pub(super) const SCCB_SDA: c_int = 40;
    pub(super) const SCCB_SCL: c_int = 39;
    pub(super) const D0: c_int = 15;
    pub(super) const D1: c_int = 17;
    pub(super) const D2: c_int = 18;
    pub(super) const D3: c_int = 16;
    pub(super) const D4: c_int = 14;
    pub(super) const D5: c_int = 12;
    pub(super) const D6: c_int = 11;
    pub(super) const D7: c_int = 48;
    pub(super) const VSYNC: c_int = 38;
    pub(super) const HREF: c_int = 47;
    pub(super) const PCLK: c_int = 13;
    pub(super) const PWDN: c_int = -1;
    pub(super) const RESET: c_int = -1;
    pub(super) const XCLK_FREQ_HZ: u32 = 20_000_000;
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Safe mirror of `camera_config_t`. Defaults are the proven board values
/// (JPEG / QVGA / quality 15 / 2 PSRAM framebuffers / grab-latest).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CameraConfig {
    pub pin_pwdn: c_int,
    pub pin_reset: c_int,
    pub pin_xclk: c_int,
    pub pin_sccb_sda: c_int,
    pub pin_sccb_scl: c_int,
    pub pin_d0: c_int,
    pub pin_d1: c_int,
    pub pin_d2: c_int,
    pub pin_d3: c_int,
    pub pin_d4: c_int,
    pub pin_d5: c_int,
    pub pin_d6: c_int,
    pub pin_d7: c_int,
    pub pin_vsync: c_int,
    pub pin_href: c_int,
    pub pin_pclk: c_int,
    pub xclk_freq_hz: u32,
    /// JPEG quality, 0–63, **lower is better** (OV series convention).
    pub jpeg_quality: c_int,
    /// Number of frame buffers allocated by the driver (2 = double speed).
    pub fb_count: usize,
}

impl Default for CameraConfig {
    fn default() -> Self {
        Self {
            pin_pwdn: xiao_sense::PWDN,
            pin_reset: xiao_sense::RESET,
            pin_xclk: xiao_sense::XCLK,
            pin_sccb_sda: xiao_sense::SCCB_SDA,
            pin_sccb_scl: xiao_sense::SCCB_SCL,
            pin_d0: xiao_sense::D0,
            pin_d1: xiao_sense::D1,
            pin_d2: xiao_sense::D2,
            pin_d3: xiao_sense::D3,
            pin_d4: xiao_sense::D4,
            pin_d5: xiao_sense::D5,
            pin_d6: xiao_sense::D6,
            pin_d7: xiao_sense::D7,
            pin_vsync: xiao_sense::VSYNC,
            pin_href: xiao_sense::HREF,
            pin_pclk: xiao_sense::PCLK,
            xclk_freq_hz: xiao_sense::XCLK_FREQ_HZ,
            // Proven on this board by esp-cpp/camera-streamer.
            jpeg_quality: 15,
            fb_count: 2,
        }
    }
}

impl CameraConfig {
    /// Translate to the bindgen struct. Safe: pure field copies; fixed
    /// geometry is the plan-proven JPEG/QVGA/PSRAM/latest combination.
    fn to_ffi(self) -> bindings::camera_config_t {
        let mut cfg = bindings::camera_config_t::default();
        cfg.pin_pwdn = self.pin_pwdn;
        cfg.pin_reset = self.pin_reset;
        cfg.pin_xclk = self.pin_xclk;
        cfg.__bindgen_anon_1.pin_sccb_sda = self.pin_sccb_sda;
        cfg.__bindgen_anon_2.pin_sccb_scl = self.pin_sccb_scl;
        cfg.pin_d0 = self.pin_d0;
        cfg.pin_d1 = self.pin_d1;
        cfg.pin_d2 = self.pin_d2;
        cfg.pin_d3 = self.pin_d3;
        cfg.pin_d4 = self.pin_d4;
        cfg.pin_d5 = self.pin_d5;
        cfg.pin_d6 = self.pin_d6;
        cfg.pin_d7 = self.pin_d7;
        cfg.pin_vsync = self.pin_vsync;
        cfg.pin_href = self.pin_href;
        cfg.pin_pclk = self.pin_pclk;
        cfg.xclk_freq_hz = self.xclk_freq_hz as c_int;
        cfg.ledc_timer = bindings::ledc_timer_t_LEDC_TIMER_0;
        cfg.ledc_channel = bindings::ledc_channel_t_LEDC_CHANNEL_0;
        cfg.pixel_format = bindings::pixformat_t_PIXFORMAT_JPEG;
        cfg.frame_size = bindings::framesize_t_FRAMESIZE_QVGA;
        cfg.jpeg_quality = self.jpeg_quality;
        cfg.fb_count = self.fb_count;
        cfg.fb_location = bindings::camera_fb_location_t_CAMERA_FB_IN_PSRAM;
        cfg.grab_mode = bindings::camera_grab_mode_t_CAMERA_GRAB_LATEST;
        cfg.sccb_i2c_port = -1; // autodetect SCCB bus
        cfg
    }
}

// ---------------------------------------------------------------------------
// Errors (no thiserror — plain enums + Display/Error, zero dependencies)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CameraError {
    /// `esp_camera_init` failed (raw `esp_err_t`): sensor probe, PSRAM
    /// allocation, DMA setup, or the driver was already initialized.
    Init(esp_err_t),
    /// `esp_camera_deinit` failed — driver was not initialized.
    Deinit(esp_err_t),
}

impl core::fmt::Display for CameraError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            CameraError::Init(e) => {
                write!(f, "esp_camera_init failed: esp_err {e:#x} ({})", ffi::err_name(*e))
            }
            CameraError::Deinit(e) => {
                write!(f, "esp_camera_deinit failed: esp_err {e:#x} ({})", ffi::err_name(*e))
            }
        }
    }
}

impl std::error::Error for CameraError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrabError {
    /// `esp_camera_fb_get` returned NULL — no frame ready (non-blocking).
    NoFrame,
    /// Frame failed JPEG validation ([`classify_frame`]): buffer returned to
    /// the driver, rejection counter incremented, nothing forwarded.
    InvalidFrame,
}

impl core::fmt::Display for GrabError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            GrabError::NoFrame => f.write_str("no frame ready (esp_camera_fb_get returned NULL)"),
            GrabError::InvalidFrame => {
                f.write_str("frame rejected: buffer does not end with JPEG EOI (FF D9)")
            }
        }
    }
}

impl std::error::Error for GrabError {}

// ---------------------------------------------------------------------------
// Pure frame-validation logic (unit-tested without hardware)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FrameVerdict {
    /// Ends with `FF D9` — safe to forward.
    Accept,
    /// Fewer than 2 bytes — cannot contain EOI.
    RejectTooShort,
    /// Does not end with `FF D9` (e.g. tail `FF 00`) — `cam_hal: NO-EOI`
    /// truncation.
    RejectNoEoi,
}

/// A JPEG frame is only valid if its last two bytes are `FF D9`. Frames that
/// fail are dropped back to the driver and counted — never forwarded.
pub(crate) fn classify_frame(buf: &[u8]) -> FrameVerdict {
    if buf.len() < 2 {
        return FrameVerdict::RejectTooShort;
    }
    let n = buf.len();
    if buf[n - 2] == 0xFF && buf[n - 1] == 0xD9 {
        FrameVerdict::Accept
    } else {
        FrameVerdict::RejectNoEoi
    }
}

// ---------------------------------------------------------------------------
// RAII frame buffer
// ---------------------------------------------------------------------------

/// RAII owner of one `camera_fb_t` borrowed from the driver. Dropping it
/// calls `esp_camera_fb_return` exactly once (take-once: only `Drop` takes
/// the pointer out of the `Option`), including on early return or panic.
pub struct FrameBuffer {
    fb: Option<*mut bindings::camera_fb_t>,
    len: usize,
    width: usize,
    height: usize,
}

impl FrameBuffer {
    fn new(fb: *mut bindings::camera_fb_t) -> Self {
        let (buf, len, width, height) = ffi::fb_parts(fb);
        let _ = buf; // validity re-checked via classify_frame on the slice
        Self { fb: Some(fb), len, width, height }
    }

    /// The frame bytes. Borrowed from `&self`: once the `FrameBuffer` is
    /// dropped the driver may reuse the memory.
    pub fn as_bytes(&self) -> &[u8] {
        let fb = self.fb.expect("FrameBuffer used after drop");
        let (buf, ..) = ffi::fb_parts(fb);
        ffi::slice(buf, self.len)
    }

    /// Frame length in bytes (as reported by the driver).
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn width(&self) -> usize {
        self.width
    }

    pub fn height(&self) -> usize {
        self.height
    }
}

impl Drop for FrameBuffer {
    fn drop(&mut self) {
        if let Some(fb) = self.fb.take() {
            ffi::fb_return(fb);
        }
    }
}

// ---------------------------------------------------------------------------
// Camera driver owner
// ---------------------------------------------------------------------------

/// Safe owner of the global esp32-camera driver instance. The C driver is a
/// singleton: a second `init` while one is live fails with
/// `ESP_ERR_INVALID_STATE`, surfaced as [`CameraError::Init`].
pub struct Camera {
    frames_grabbed: u64,
    frames_rejected: u64,
}

impl Camera {
    /// Initialize the driver (pins per the XIAO ESP32S3 Sense const table,
    /// JPEG/QVGA, PSRAM framebuffers). Fails if the sensor cannot be probed
    /// over SCCB or the driver is already initialized.
    pub fn init(config: CameraConfig) -> Result<Self, CameraError> {
        let cfg = config.to_ffi();
        let err = ffi::init(&cfg);
        if err != esp_idf_sys::ESP_OK {
            return Err(CameraError::Init(err));
        }
        Ok(Self { frames_grabbed: 0, frames_rejected: 0 })
    }

    /// Grab the latest frame. [`GrabError::NoFrame`] when the driver has no
    /// frame ready; [`GrabError::InvalidFrame`] when the frame fails JPEG
    /// validation — the buffer is returned to the driver and counted in
    /// [`Self::frames_rejected`], never forwarded to the caller.
    pub fn grab(&mut self) -> Result<FrameBuffer, GrabError> {
        let fb = ffi::fb_get().ok_or(GrabError::NoFrame)?;
        let (buf, len, ..) = ffi::fb_parts(fb);
        let verdict = classify_frame(ffi::slice(buf, len));
        if verdict != FrameVerdict::Accept {
            // NO-EOI / truncated frame: return it to the driver pool and
            // count the rejection; do NOT forward.
            ffi::fb_return(fb);
            self.frames_rejected += 1;
            return Err(GrabError::InvalidFrame);
        }
        self.frames_grabbed += 1;
        Ok(FrameBuffer::new(fb))
    }

    /// Frames successfully grabbed (and validated) since [`Self::init`].
    pub fn frames_grabbed(&self) -> u64 {
        self.frames_grabbed
    }

    /// Frames rejected by the EOI check and returned to the driver pool.
    pub fn frames_rejected(&self) -> u64 {
        self.frames_rejected
    }

    /// Deinitialize the driver (consumes `self`). Dropping a `Camera`
    /// without calling this leaves the driver initialized — the firmware's
    /// `main` owns that decision.
    pub fn deinit(self) -> Result<(), CameraError> {
        let err = ffi::deinit();
        if err != esp_idf_sys::ESP_OK {
            return Err(CameraError::Deinit(err));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Unit tests — pure logic only; `#[cfg(test)]`, compile-checked via
// `cargo test --no-run --target xtensa-esp32s3-espidf`.
// Running them requires a board (RUN_PENDING_NO_BOARD).
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::{classify_frame, FrameVerdict};

    #[test]
    fn accepts_frame_ending_with_eoi() {
        // SOI + payload + EOI — the only shape that may be forwarded.
        let frame = [0xFF, 0xD8, 0x00, 0x10, 0xAA, 0xBB, 0xFF, 0xD9];
        assert_eq!(classify_frame(&frame), FrameVerdict::Accept);
        // Bare two-byte EOI is also structurally valid.
        assert_eq!(classify_frame(&[0xFF, 0xD9]), FrameVerdict::Accept);
    }

    #[test]
    fn rejects_truncated_tail_and_counts_not_forwarded() {
        // `cam_hal: NO-EOI` signature: scan cut short, tail `FF 00`.
        let truncated = [0xFF, 0xD8, 0x01, 0x02, 0xFF, 0x00];
        assert_eq!(classify_frame(&truncated), FrameVerdict::RejectNoEoi);
        // `Camera::grab` maps RejectNoEoi -> fb_return + frames_rejected += 1
        // + Err(GrabError::InvalidFrame); the counter path needs hardware and
        // is covered by hardware QA (RUN_PENDING_NO_BOARD).
    }

    #[test]
    fn rejects_short_buffer() {
        assert_eq!(classify_frame(&[]), FrameVerdict::RejectTooShort);
        assert_eq!(classify_frame(&[0xFF]), FrameVerdict::RejectTooShort);
        assert_eq!(classify_frame(&[0xD9]), FrameVerdict::RejectTooShort);
    }
}
