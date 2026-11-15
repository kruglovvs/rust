//! The device gatherer that fills a [`Snapshot`] from ESP-IDF and the mounted
//! card. The pure builder never calls into it.
//!
//! # Contract
//!
//! ```text
//! pub enum SensorState { Reported, Unavailable }
//! pub struct Status { pub snapshot: Snapshot<'static>, pub temperature_sensor: SensorState }
//! pub fn gather(cfg: &Config) -> Status
//! pub fn rec_dir(mount: &str) -> String
//! pub fn newest_recording(dir: &str) -> Option<String>
//! ```
//!
//! This is the **only** module in the crate allowed to call an `esp_*` API.
//! [`gather`] reads the device once and returns a [`Snapshot`] that borrows
//! only `'static` data, so the response builder cannot observe device state
//! changing under it:
//!
//! - `uptime_s` from `esp_timer_get_time` in microseconds, saturated to zero
//!   on a negative reading.
//! - `heap_internal_free` from `heap_caps_get_free_size(MALLOC_CAP_INTERNAL)`
//!   and `heap_spiram_free` from `MALLOC_CAP_SPIRAM` (`0` on a build without
//!   PSRAM).
//! - `ip` from [`net::station_ipv4`]; `"0.0.0.0"` until DHCP assigns one.
//! - `total_bytes`/`free_bytes` from `esp_vfs_fat_info` on the configured
//!   mount point, both `0` when the volume is not mounted.
//! - `mounted` by listing the mount point: a successful `std::fs::read_dir`
//!   means the VFS has the volume.
//! - `clock_khz` from `sdmmc_host_get_real_freq` on the SDSPI host slot
//!   (`SPI2_HOST`, the slot `SDSPI_HOST_DEFAULT` selects), `0` with no card.
//! - `recording_file` from [`newest_recording`] below `<mount>/rec`.
//! - `temperature_c` from the on-die temperature sensor with one decimal;
//!   `None` only when install, enable or read fails at runtime.
//!   [`Status::temperature_sensor`] reports `"reported"` or `"unavailable"`
//!   so a failure is visible in the log rather than implied by a `null`.
//!
//! The identity strings (`version`, `build`, `hostname`, `rtsp_path`) are
//! [`crate::json`]'s documented constants; `rtsp_port`, `rtsp_running` and
//! `recording_enabled` come from the [`Config`] the boot path passed.
//!
//! [`newest_recording`] is plain `std::fs`, not a device API: it lists `dir`,
//! keeps regular files only, and returns the lexicographically largest name,
//! which is the newest because the recorder writes `REC_0001.avi`,
//! `REC_0002.avi`, and so on. Every failure (missing directory, unreadable
//! entry, non-UTF-8 name) is `None`: the status document must still answer
//! when the card is absent.
//!
//! The sensor driver is installed once behind a `std::sync::Mutex` and reused,
//! then disabled after each read, so concurrent requests serialise instead of
//! racing the driver. Every FFI call in this module is a bounded `esp_*`
//! accessor documented in ESP-IDF; the unsafe blocks are the only ones in the
//! crate and each states its precondition.

use std::sync::Mutex;

use esp_idf_sys as sys;

use crate::json::{Snapshot, BUILD, HOSTNAME, RTSP_PATH, VERSION};
use crate::server::Config;

/// The result of trying to read the on-die temperature sensor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SensorState {
    /// The sensor installed, enabled and returned a reading.
    Reported,
    /// The sensor could not be installed, enabled or read this time.
    Unavailable,
}

impl SensorState {
    /// The value reported in [`Status::temperature_sensor`].
    pub const fn as_str(self) -> &'static str {
        match self {
            SensorState::Reported => "reported",
            SensorState::Unavailable => "unavailable",
        }
    }
}

/// One gathered status document plus the sensor outcome.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Status {
    pub snapshot: Snapshot<'static>,
    pub temperature_sensor: SensorState,
}

/// Read the device once and build a [`Snapshot`] borrowing only `'static`
/// data.
pub fn gather(cfg: &Config) -> Status {
    let mount = cfg.mount_point;
    let (temperature_c, temperature_sensor) = read_temperature();
    Status {
        snapshot: Snapshot {
            version: VERSION,
            build: BUILD,
            uptime_s: uptime_seconds(),
            ip: leak(format_ip(net::station_ipv4())),
            hostname: HOSTNAME,
            mounted: std::fs::read_dir(mount).is_ok(),
            total_bytes: volume_info(mount).0,
            free_bytes: volume_info(mount).1,
            clock_khz: sd_clock_khz(),
            rtsp_port: cfg.rtsp_port,
            rtsp_path: RTSP_PATH,
            rtsp_running: cfg.rtsp_running,
            recording_enabled: cfg.recording_enabled,
            recording_file: newest_recording(&rec_dir(mount))
                .map(|name| leak(format!("rec/{name}"))),
            temperature_c,
            heap_internal_free: heap_free(sys::MALLOC_CAP_INTERNAL),
            heap_spiram_free: heap_free(sys::MALLOC_CAP_SPIRAM),
        },
        temperature_sensor,
    }
}

/// The `rec` directory below `mount`, without a trailing separator.
pub fn rec_dir(mount: &str) -> String {
    format!("{mount}/rec")
}

/// The newest (lexicographically largest) regular file in `dir`, or `None`.
pub fn newest_recording(dir: &str) -> Option<String> {
    let entries = std::fs::read_dir(dir).ok()?;
    let mut newest: Option<String> = None;
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_file() {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if newest.as_ref().is_none_or(|current| name > *current) {
            newest = Some(name);
        }
    }
    newest
}

/// Install (once), enable, read and disable the on-die temperature sensor.
///
/// `None` means this read is unusable: the driver failed to install, the
/// enable failed, or the body rejected the value. The handle stays installed
/// for the next call; only `temperature_sensor_enable`/`_disable` bracket a
/// read.
fn with_sensor<T>(body: impl FnOnce(sys::temperature_sensor_handle_t) -> Option<T>) -> Option<T> {
    static SENSOR: Mutex<Option<usize>> = Mutex::new(None);
    let mut slot = SENSOR.lock().ok()?;
    let handle = match *slot {
        Some(address) => address as sys::temperature_sensor_handle_t,
        None => {
            let config = sys::temperature_sensor_config_t {
                range_min: -10,
                range_max: 80,
                // The default clock source (`TEMPERATURE_SENSOR_CLK_SRC_DEFAULT`,
                // i.e. RC_FAST) and the range are validated by the driver.
                clk_src: sys::soc_periph_temperature_sensor_clk_src_t_TEMPERATURE_SENSOR_CLK_SRC_DEFAULT,
                flags: sys::temperature_sensor_config_t__bindgen_ty_1 { allow_pd: 0 },
            };
            let mut handle: sys::temperature_sensor_handle_t = std::ptr::null_mut();
            // SAFETY: `config` outlives the call and `handle` is a valid out
            // pointer; the driver copies the configuration.
            let installed = unsafe { sys::temperature_sensor_install(&config, &mut handle) };
            if installed != sys::ESP_OK || handle.is_null() {
                return None;
            }
            *slot = Some(handle as usize);
            handle
        }
    };
    // SAFETY: `handle` came from `temperature_sensor_install` above and the
    // mutex keeps every enable/read/disable sequence exclusive.
    let enabled = unsafe { sys::temperature_sensor_enable(handle) };
    if enabled != sys::ESP_OK {
        return None;
    }
    let reading = body(handle);
    // SAFETY: same handle, still owned by the static slot; the disable pairs
    // with the enable above.
    unsafe {
        let _ = sys::temperature_sensor_disable(handle);
    }
    reading
}

/// The sensor reading with one decimal, or `None` when the sensor fails.
fn read_temperature() -> (Option<f32>, SensorState) {
    let reading = with_sensor(|handle| {
        let mut celsius: f32 = 0.0;
        // SAFETY: `celsius` is a valid out pointer and the sensor is enabled.
        let read = unsafe { sys::temperature_sensor_get_celsius(handle, &mut celsius) };
        if read == sys::ESP_OK && celsius.is_finite() {
            Some((celsius * 10.0).round() / 10.0)
        } else {
            None
        }
    });
    match reading {
        Some(celsius) => (Some(celsius), SensorState::Reported),
        None => (None, SensorState::Unavailable),
    }
}

/// Seconds since the timer subsystem started.
fn uptime_seconds() -> u64 {
    // SAFETY: `esp_timer_get_time` is a plain getter with no preconditions.
    let micros = unsafe { sys::esp_timer_get_time() };
    u64::try_from(micros).unwrap_or(0) / 1_000_000
}

/// Free bytes in the heap pool selected by `caps`.
fn heap_free(caps: u32) -> u64 {
    // SAFETY: `heap_caps_get_free_size` is a plain getter with no
    // preconditions; an unknown mask simply returns zero.
    let free = unsafe { sys::heap_caps_get_free_size(caps) };
    u64::try_from(free).unwrap_or(0)
}

/// The mounted FAT volume's size and free space, or `(0, 0)`.
fn volume_info(mount: &str) -> (u64, u64) {
    let Ok(path) = std::ffi::CString::new(mount) else {
        return (0, 0);
    };
    let mut total: u64 = 0;
    let mut free: u64 = 0;
    // SAFETY: `path` is NUL-terminated and outlives the call; both out
    // pointers are valid for writes.
    let result = unsafe { sys::esp_vfs_fat_info(path.as_ptr(), &mut total, &mut free) };
    if result == sys::ESP_OK {
        (total, free)
    } else {
        (0, 0)
    }
}

/// The real SD SPI clock in kHz, or `0` when the host has no card.
fn sd_clock_khz() -> u64 {
    let mut real_freq_khz: std::ffi::c_int = 0;
    // SAFETY: the slot constant is the one `SDSPI_HOST_DEFAULT` selects on
    // this chip, and the out pointer is valid for one write.
    let result = unsafe {
        sys::sdmmc_host_get_real_freq(
            sys::spi_host_device_t_SPI2_HOST as std::ffi::c_int,
            &mut real_freq_khz,
        )
    };
    if result == sys::ESP_OK && real_freq_khz > 0 {
        real_freq_khz as u64
    } else {
        0
    }
}

/// Render the station address, or the unspecified address before DHCP.
fn format_ip(ip: Option<std::net::Ipv4Addr>) -> String {
    match ip {
        Some(ip) => ip.to_string(),
        None => "0.0.0.0".to_string(),
    }
}

/// Intern `text` for the lifetime of the process.
///
/// [`Snapshot`] borrows its strings and is rebuilt per request. Recycling one
/// buffer would need a lock around the whole response (and still make the
/// borrow unsound if a second request wrote into it); leaking a handful of
/// short strings per request is the bounded cost that keeps the builder pure.
fn leak(text: String) -> &'static str {
    Box::leak(text.into_boxed_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// A scratch directory under the temp dir, removed on drop.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(label: &str) -> Scratch {
            let path = std::env::temp_dir()
                .join(format!("fileserver-status-{label}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("scratch dir");
            Scratch(path)
        }

        fn join(&self, name: &str) -> String {
            self.0.join(name).to_string_lossy().into_owned()
        }

        fn path(&self) -> String {
            self.0.to_string_lossy().into_owned()
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn newest_recording_picks_the_largest_name() {
        let scratch = Scratch::new("largest");
        for name in ["REC_0001.avi", "REC_0003.avi", "REC_0002.avi"] {
            std::fs::write(scratch.join(name), b"x").expect("write");
        }
        assert_eq!(
            newest_recording(&scratch.path()),
            Some("REC_0003.avi".to_string())
        );
    }

    #[test]
    fn newest_recording_skips_directories_and_absent_dirs() {
        let scratch = Scratch::new("skip");
        std::fs::create_dir(scratch.join("REC_9999.avi")).expect("dir");
        std::fs::write(scratch.join("REC_0001.avi"), b"x").expect("write");
        assert_eq!(
            newest_recording(&scratch.path()),
            Some("REC_0001.avi".to_string()),
            "a directory named like a recording is not a recording"
        );
        assert_eq!(
            newest_recording(&scratch.join("missing")),
            None,
            "an absent directory is no recording, not a panic"
        );
    }

    #[test]
    fn rec_dir_has_one_separator() {
        assert_eq!(rec_dir("/sdcard"), "/sdcard/rec");
        assert_eq!(rec_dir("/mnt/card"), "/mnt/card/rec");
    }

    #[test]
    fn format_ip_reports_unspecified_before_dhcp() {
        assert_eq!(format_ip(None), "0.0.0.0");
        assert_eq!(
            format_ip(Some(std::net::Ipv4Addr::new(192, 168, 1, 50))),
            "192.168.1.50"
        );
    }

    #[test]
    fn sensor_state_names_are_stable() {
        assert_eq!(SensorState::Reported.as_str(), "reported");
        assert_eq!(SensorState::Unavailable.as_str(), "unavailable");
    }

    #[test]
    fn gather_reports_a_real_snapshot_on_the_device() {
        let cfg = Config {
            port: 0,
            mount_point: storage::BASE_PATH,
            rtsp_port: 554,
            rtsp_path: RTSP_PATH,
            rtsp_running: true,
            recording_enabled: true,
        };
        let status = gather(&cfg);
        let snapshot = status.snapshot;
        assert!(!snapshot.version.is_empty());
        assert_eq!(snapshot.rtsp_port, 554);
        assert_eq!(snapshot.rtsp_path, RTSP_PATH);
        assert_eq!(snapshot.hostname, HOSTNAME);
        match status.temperature_sensor {
            SensorState::Reported => {
                let celsius = snapshot.temperature_c.expect("reported implies a reading");
                assert!(celsius.is_finite(), "{celsius}");
                assert!(
                    (-40.0..=125.0).contains(&celsius),
                    "on-die reading out of range: {celsius}"
                );
                assert_eq!(
                    (celsius * 10.0).round() / 10.0,
                    celsius,
                    "the reading is rounded to one decimal"
                );
            }
            SensorState::Unavailable => assert_eq!(snapshot.temperature_c, None),
        }
        let mut body = String::new();
        crate::json::status_body(&snapshot, &mut body);
        serde_json::from_str::<serde_json::Value>(&body).expect("gathered status is valid JSON");
    }
}
