//! Runtime sensor-identity probe (Todo 5).
//!
//! Design contract (plan Todo 5):
//! - At boot, log `sensor_t->id.PID`, `sensor_t->slv_addr`, and the sensor
//!   name from `esp_camera_sensor_get_info`, plus the frame counters, in one
//!   line.
//! - If the PID is not the expected OV3660 (`0x3660`), log a prominent
//!   warning naming the detected sensor and CONTINUE with the detected
//!   driver — never silently mis-configure.
//! - Every `unsafe` block lives inside the private [`ffi`] module (same
//!   pattern as `camera.rs`); a NULL `esp_camera_sensor_get()` return is a
//!   typed [`SensorError::NotInitialized`], never a panic.
//!
//! Note: `esp_camera_sensor_probe()` / `sensor_t.probe` DO NOT EXIST
//! (plan-verified). The real API is `esp_camera_sensor_get()` →
//! `sensor_t.id.PID` / `.slv_addr`, then `esp_camera_sensor_get_info(&id)` →
//! `camera_sensor_info_t.name`.
//!
//! Bindgen reality (verified against `out/bindings.rs`, Todo 5):
//! - `pub type sensor_t = _sensor;` with `id: sensor_id_t` (`PID: u16`),
//!   `slv_addr: u8`.
//! - `esp_camera_sensor_get_info(id: *mut sensor_id_t) -> *mut
//!   camera_sensor_info_t` (bindgen drops the C `const`), `name: *const
//!   c_char`.
//! - Names come from the driver's static table (`sensor.c`): "OV2640",
//!   "OV3660", "OV5640" — uppercase.
//!
//! Detection truth table (plan Todo 5):
//! | Sensor  | SCCB addr | PID (as `u16`)   |
//! |---------|-----------|-----------------|
//! | OV2640  | `0x30`    | `0x26` (8-bit)  |
//! | OV3660  | `0x3C`    | `0x3660` (16-bit) |
//! | OV5640  | `0x3C`    | `0x5640` (16-bit) |
//! OV3660 and OV5640 share the SCCB address — the PID alone distinguishes
//! them.

use esp_idf_sys::camera as bindings;

// ---------------------------------------------------------------------------
// Private FFI boundary — the ONLY `unsafe` in this file.
// ---------------------------------------------------------------------------

mod ffi {
    use super::bindings;
    use core::ffi::CStr;

    /// Plain-data snapshot of the live `sensor_t` — safe to hold and print.
    pub(super) struct RawIdentity {
        pub pid: u16,
        pub slv_addr: u8,
        /// `camera_sensor_info_t.name`, or `None` when the driver returns
        /// NULL for the info lookup (unknown PID).
        pub name: Option<String>,
    }

    /// Read identity from the global driver instance.
    /// `None` when `esp_camera_sensor_get()` returns NULL (driver not
    /// initialized) — the caller turns this into a typed error, no panic.
    pub(super) fn read_identity() -> Option<RawIdentity> {
        unsafe {
            let s = bindings::esp_camera_sensor_get();
            if s.is_null() {
                return None;
            }
            let pid = (*s).id.PID;
            let slv_addr = (*s).slv_addr;
            // bindgen signature takes `*mut sensor_id_t`; the C
            // implementation only reads `id->PID` (sensor.c: `id->PID ==
            // camera_sensor[i].pid`), so handing it a pointer into the
            // driver's own `sensor_t` is sound.
            let info = bindings::esp_camera_sensor_get_info(core::ptr::addr_of_mut!((*s).id));
            let name = if info.is_null() {
                None
            } else {
                let n = (*info).name;
                if n.is_null() {
                    None
                } else {
                    Some(CStr::from_ptr(n).to_string_lossy().into_owned())
                }
            };
            Some(RawIdentity { pid, slv_addr, name })
        }
    }
}

// ---------------------------------------------------------------------------
// Pure classification logic (unit-tested without hardware)
// ---------------------------------------------------------------------------

/// The sensors this firmware's detection table knows by PID (plan truth
/// table). Anything else is [`SensorId::Unknown`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SensorId {
    /// PID `0x26` — 8-bit value read after the SCCB bank select.
    Ov2640,
    /// PID `0x3660` — the expected sensor for this board.
    Ov3660,
    /// PID `0x5640` — same SCCB address as OV3660; PID alone distinguishes.
    Ov5640,
    /// Any other PID, including `0x0000` (no sensor / probe garbage).
    Unknown,
}

/// The PID this board is expected to carry (OV3660, plan Todo 5).
pub const EXPECTED_PID: u16 = 0x3660;

/// Map a raw `sensor_t.id.PID` to a known sensor. Pure — no FFI, no panic.
///
/// - `0x26`   → [`SensorId::Ov2640`] (8-bit PID after bank-select)
/// - `0x3660` → [`SensorId::Ov3660`] (expected)
/// - `0x5640` → [`SensorId::Ov5640`] (shares `0x3C` with OV3660; PID decides)
/// - anything else, including `0x0000` → [`SensorId::Unknown`]
pub fn classify_pid(pid: u16) -> SensorId {
    match pid {
        0x26 => SensorId::Ov2640,
        0x3660 => SensorId::Ov3660,
        0x5640 => SensorId::Ov5640,
        _ => SensorId::Unknown,
    }
}

/// `true` only for the expected OV3660 PID. The boot log emits its warning
/// line exactly when this is `false` (plan: non-OV3660 must warn, never
/// silently mis-configure).
pub fn is_expected_pid(pid: u16) -> bool {
    pid == EXPECTED_PID
}

// ---------------------------------------------------------------------------
// Errors (no thiserror — plain enums + Display/Error, zero dependencies)
// ---------------------------------------------------------------------------

/// Failure to obtain a sensor identity at boot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SensorError {
    /// `esp_camera_sensor_get()` returned NULL — the camera driver has not
    /// been initialized (or was deinitialized). Logged, never panicked.
    NotInitialized,
}

impl core::fmt::Display for SensorError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            SensorError::NotInitialized => f.write_str(
                "esp_camera_sensor_get() returned NULL (camera driver not initialized)",
            ),
        }
    }
}

impl std::error::Error for SensorError {}

// ---------------------------------------------------------------------------
// Identity snapshot + boot logging
// ---------------------------------------------------------------------------

/// What the boot log saw: raw PID/SCCB address, the driver's own name for
/// the sensor, and our classification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SensorIdentity {
    pub pid: u16,
    pub slv_addr: u8,
    /// `camera_sensor_info_t.name` ("OV3660"), `None` when the lookup
    /// returned NULL (unknown PID).
    pub name: Option<String>,
    pub class: SensorId,
}

impl SensorIdentity {
    /// Name to print: the driver's name, or `UNKNOWN` for an unrecognized
    /// PID where `esp_camera_sensor_get_info` returned NULL.
    pub fn name_or_unknown(&self) -> &str {
        self.name.as_deref().unwrap_or("UNKNOWN")
    }
}

/// Probe the live sensor and emit the boot-log lines (plan Todo 5):
///
/// 1. One identity line — `PID`, SCCB `addr`, driver `name`, classification
///    and the frame counters from [`super::camera::Camera`]:
///    `sensor: PID=0x3660 addr=0x3c OV3660 class=Ov3660 frames_grabbed=0 frames_rejected=0`
/// 2. If the PID is not [`EXPECTED_PID`], a prominent `WARNING:` line naming
///    the detected sensor — and execution CONTINUES with the detected driver
///    (the `Ok` return is the "continue" signal).
///
/// Returns [`SensorError::NotInitialized`] (after logging an `ERROR:` line)
/// when the driver is not up — typed, never a panic.
pub fn log_sensor_identity(
    frames_grabbed: u64,
    frames_rejected: u64,
) -> Result<SensorIdentity, SensorError> {
    let raw = match ffi::read_identity() {
        Some(raw) => raw,
        None => {
            println!("ERROR: sensor identity unavailable: {}", SensorError::NotInitialized);
            return Err(SensorError::NotInitialized);
        }
    };
    let class = classify_pid(raw.pid);
    let name = raw.name.as_deref().unwrap_or("UNKNOWN");

    println!(
        "sensor: PID={:#x} addr={:#x} {} class={:?} frames_grabbed={} frames_rejected={}",
        raw.pid, raw.slv_addr, name, class, frames_grabbed, frames_rejected
    );

    if !is_expected_pid(raw.pid) {
        // Prominent, names the detected sensor, and states the policy:
        // continue with the detected driver rather than assuming OV3660.
        println!(
            "WARNING: sensor PID={:#x} ({}, class={:?}) is not the expected OV3660 (PID={:#x}) — continuing with the detected driver, do not assume OV3660 registers",
            raw.pid, name, class, EXPECTED_PID
        );
    }

    Ok(SensorIdentity { pid: raw.pid, slv_addr: raw.slv_addr, name: raw.name, class })
}

// ---------------------------------------------------------------------------
// Unit tests — pure logic only; `#[cfg(test)]`, compile-checked via
// `cargo test --no-run --target xtensa-esp32s3-espidf`.
// Running them requires a board (RUN_PENDING_NO_BOARD).
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::{classify_pid, is_expected_pid, SensorId, EXPECTED_PID};

    #[test]
    fn classifies_ov2640_pid_0x26() {
        // 8-bit PID read after the SCCB bank-select — recognised, but NOT
        // the expected sensor for this board.
        assert_eq!(classify_pid(0x26), SensorId::Ov2640);
        assert!(!is_expected_pid(0x26));
    }

    #[test]
    fn classifies_expected_ov3660_pid_0x3660() {
        assert_eq!(classify_pid(0x3660), SensorId::Ov3660);
        assert!(is_expected_pid(0x3660));
        // The constant the log quotes is pinned to the plan truth table.
        assert_eq!(EXPECTED_PID, 0x3660);
    }

    #[test]
    fn classifies_ov5640_distinctly_despite_shared_sccb_address() {
        // OV5640 and OV3660 both sit at SCCB 0x3C — per the plan truth
        // table the PID value alone distinguishes them.
        assert_eq!(classify_pid(0x5640), SensorId::Ov5640);
        assert_ne!(classify_pid(0x5640), classify_pid(0x3660));
        // Known sensor, but still not the expected one -> warning path.
        assert!(!is_expected_pid(0x5640));
    }

    #[test]
    fn classifies_zero_pid_as_unknown_and_triggers_warning() {
        // 0x0000 = no sensor / probe garbage -> Unknown, never a known id.
        assert_eq!(classify_pid(0x0000), SensorId::Unknown);
        // QA gate: proves the warning branch exists — the expected sensor
        // and the null PID classify differently, and every non-0x3660 PID
        // (0x0000 included) takes the WARNING path.
        assert_ne!(classify_pid(0x3660), classify_pid(0x0000));
        for pid in [0x0000u16, 0x26, 0x5640, 0x1234] {
            assert!(!is_expected_pid(pid), "PID {pid:#x} must trigger the warning");
        }
        assert!(is_expected_pid(0x3660));
    }

    #[test]
    fn unrecognised_pid_is_unknown() {
        assert_eq!(classify_pid(0x1234), SensorId::Unknown);
        assert_eq!(classify_pid(0xFFFF), SensorId::Unknown);
    }

    /// Hardware presence check: is a camera actually wired to the board?
    ///
    /// Reads the sensor's 16-bit PID straight off the SCCB bus (SDA=GPIO40,
    /// SCL=GPIO39, address 0x3C) so it proves the module is physically
    /// connected and responding, independently of whether `esp_video`
    /// managed to register `/dev/video2`.
    #[cfg(target_os = "espidf")]
    #[test]
    fn hil_camera_is_connected_over_sccb() {
        use esp_idf_hal::delay::BLOCK;
        use esp_idf_hal::i2c::*;
        use esp_idf_hal::peripherals::Peripherals;

        const SCCB_ADDR: u8 = 0x3C;
        const REG_PID_HIGH: [u8; 2] = [0x30, 0x0A];
        const REG_PID_LOW: [u8; 2] = [0x30, 0x0B];

        let _guard = crate::HIL_LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
        let peripherals = Peripherals::take().expect("take I2C0, SCCB pins and LEDC");

        // The OV3660 will not answer on SCCB until its master clock runs, so
        // generate XCLK on GPIO10 before probing (same clock esp_video drives).
        let xclk_timer = esp_idf_hal::ledc::LedcTimerDriver::new(
            peripherals.ledc.timer0,
            &esp_idf_hal::ledc::config::TimerConfig::new()
                .frequency(esp_idf_hal::units::Hertz(20_000_000))
                .resolution(esp_idf_hal::ledc::config::Resolution::Bits1),
        )
        .expect("LEDC timer0 for camera XCLK");
        let mut xclk = esp_idf_hal::ledc::LedcDriver::new(
            peripherals.ledc.channel0,
            xclk_timer,
            peripherals.pins.gpio10,
        )
        .expect("LEDC channel0 on XCLK GPIO10");
        let half = xclk.get_max_duty() / 2;
        xclk.set_duty(half).expect("start camera XCLK");

        let mut i2c = I2cDriver::new(
            peripherals.i2c0,
            peripherals.pins.gpio40,
            peripherals.pins.gpio39,
            &I2cConfig::new().baudrate(esp_idf_hal::units::Hertz(100_000)),
        )
        .expect("SCCB I2C0 on SDA=GPIO40 SCL=GPIO39");

        let mut high = [0u8; 1];
        i2c.write(SCCB_ADDR, &REG_PID_HIGH, BLOCK)
            .expect("no ACK from the camera on SCCB address 0x3C: camera not connected");
        i2c.read(SCCB_ADDR, &mut high, BLOCK).expect("read OV3660 PID high byte");

        let mut low = [0u8; 1];
        i2c.write(SCCB_ADDR, &REG_PID_LOW, BLOCK).expect("select OV3660 PID low byte");
        i2c.read(SCCB_ADDR, &mut low, BLOCK).expect("read OV3660 PID low byte");

        let pid = (u16::from(high[0]) << 8) | u16::from(low[0]);
        crate::log!("hil-camera: SCCB addr=0x{SCCB_ADDR:02x} PID=0x{pid:04x}");
        assert_eq!(
            pid, EXPECTED_PID,
            "camera responded on SCCB but its PID is not the expected OV3660"
        );
    }
}
