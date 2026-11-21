//! The device gatherer that fills a [`Snapshot`] from ESP-IDF and the mounted
//! card. The pure builder never calls into it.
//!
//! # Contract
//!
//! ```text
//! pub enum SensorState { Reported, Unavailable }
//! pub struct Status { /* owns the run-time strings */ }
//! impl Status {
//!     Status::snapshot(&self) -> Snapshot<'_>;
//!     Status::temperature_sensor(&self) -> SensorState;
//! }
//! pub fn gather(cfg: &Config) -> Status
//! pub fn rec_dir(mount: &str) -> String
//! pub fn newest_recording(dir: &str) -> Option<String>
//! ```
//!
//! This is the **only** module in the crate allowed to call an `esp_*` API.
//! [`gather`] reads the device once and returns a [`Status`] that **owns** the
//! two strings the document needs at run time (the station address and the
//! newest recording's mount-relative path). [`Status::snapshot`] borrows them
//! for the response builder without allocating, so the request path cannot
//! observe device state changing under it and cannot grow the heap:
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
//!   The call is made only when the volume is mounted: the host state it reads
//!   does not exist before a successful mount, and reaching it on a card-less
//!   boot is a `LoadProhibited` fault. Every other `esp_*`/`sdmmc_*` call here
//!   is safe on a card-less boot: `esp_vfs_fat_info` reports an error that
//!   becomes `(0, 0)`, and the timer, heap and temperature accessors do not
//!   touch the card.
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
//! keeps regular files only, and returns the file whose name has the largest
//! numeric suffix (`muxed_10.avi` beats `muxed_9.avi`), falling back to the
//! lexicographically largest name when a name has no suffix. The recorder
//! writes `muxed_0.avi`, `muxed_1.avi`, and so on, so the suffix is the
//! sequence. Every failure (missing directory, unreadable entry, non-UTF-8
//! name) is `None`: the status document must still answer when the card is
//! absent.
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
///
/// The station address and the newest recording path are owned by this value;
/// [`Status::snapshot`] lends them to the response builder. Rebuilding the
/// response therefore allocates nothing and leaks nothing.
#[derive(Clone, Debug, PartialEq)]
pub struct Status {
    ip: String,
    recording_file: Option<String>,
    document: Document,
    temperature_sensor: SensorState,
}

/// The gathered fields that never vary per request.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Document {
    version: &'static str,
    build: &'static str,
    uptime_s: u64,
    hostname: &'static str,
    mounted: bool,
    total_bytes: u64,
    free_bytes: u64,
    clock_khz: u64,
    rtsp_port: u16,
    rtsp_path: &'static str,
    rtsp_running: bool,
    recording_enabled: bool,
    temperature_c: Option<f32>,
    heap_internal_free: u64,
    heap_spiram_free: u64,
}

impl Status {
    /// Borrow the gathered values as the document builder's snapshot.
    ///
    /// Every field is copied or borrowed from `self`; this allocates nothing,
    /// so it is safe to call once per request.
    pub fn snapshot(&self) -> Snapshot<'_> {
        Snapshot {
            version: self.document.version,
            build: self.document.build,
            uptime_s: self.document.uptime_s,
            ip: &self.ip,
            hostname: self.document.hostname,
            mounted: self.document.mounted,
            total_bytes: self.document.total_bytes,
            free_bytes: self.document.free_bytes,
            clock_khz: self.document.clock_khz,
            rtsp_port: self.document.rtsp_port,
            rtsp_path: self.document.rtsp_path,
            rtsp_running: self.document.rtsp_running,
            recording_enabled: self.document.recording_enabled,
            recording_file: self.recording_file.as_deref(),
            temperature_c: self.document.temperature_c,
            heap_internal_free: self.document.heap_internal_free,
            heap_spiram_free: self.document.heap_spiram_free,
        }
    }

    /// Whether the on-die sensor produced a reading for this gather.
    pub const fn temperature_sensor(&self) -> SensorState {
        self.temperature_sensor
    }
}

/// Read the device once and build a [`Status`] that owns its run-time strings.
pub fn gather(cfg: &Config) -> Status {
    let mount = cfg.mount_point;
    let mounted = std::fs::read_dir(mount).is_ok();
    let (total_bytes, free_bytes) = volume_info(mount);
    let (temperature_c, temperature_sensor) = read_temperature();
    Status {
        ip: format_ip(net::station_ipv4()),
        recording_file: newest_recording(&rec_dir(mount)).map(|name| format!("rec/{name}")),
        document: Document {
            version: VERSION,
            build: BUILD,
            uptime_s: uptime_seconds(),
            hostname: HOSTNAME,
            mounted,
            total_bytes,
            free_bytes,
            clock_khz: sd_clock_khz(mounted),
            rtsp_port: cfg.rtsp_port,
            rtsp_path: RTSP_PATH,
            rtsp_running: cfg.rtsp_running,
            recording_enabled: cfg.recording_enabled,
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

/// The newest (largest numeric suffix, then name) regular file in `dir`.
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
        if newest
            .as_ref()
            .is_none_or(|current| is_newer_recording(&name, current))
        {
            newest = Some(name);
        }
    }
    newest
}

/// The numeric suffix of a recording name (`muxed_10.avi` -> `Some(10)`).
fn recording_sequence(name: &str) -> Option<u64> {
    let stem = name.rsplit_once('.').map_or(name, |(stem, _)| stem);
    let prefix_len = stem
        .trim_end_matches(|character: char| character.is_ascii_digit())
        .len();
    let digits = &stem[prefix_len..];
    if digits.is_empty() {
        None
    } else {
        digits.parse().ok()
    }
}

/// Whether `candidate` should replace `current` as the newest recording.
fn is_newer_recording(candidate: &str, current: &str) -> bool {
    match (recording_sequence(candidate), recording_sequence(current)) {
        (Some(candidate_sequence), Some(current_sequence))
            if candidate_sequence != current_sequence =>
        {
            candidate_sequence > current_sequence
        }
        _ => candidate > current,
    }
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
                clk_src:
                    sys::soc_periph_temperature_sensor_clk_src_t_TEMPERATURE_SENSOR_CLK_SRC_DEFAULT,
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

/// The real SD SPI clock in kHz, or `0` when no volume is mounted.
///
/// `sdmmc_host_get_real_freq` dereferences the SDMMC host state, which only
/// exists after a successful mount has initialised a card on the slot. On a
/// card-less boot that state was never created and the call faults
/// (`LoadProhibited` in `sdmmc_ll_get_clock_div`), so it must not be reached.
/// `mounted` is the same precondition the volume size and recording fields
/// use; without it the document reports `0`, which is the truth.
fn sd_clock_khz(mounted: bool) -> u64 {
    if !mounted {
        return 0;
    }
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
    #[ignore = "HIL: needs a mounted SD card"]
    fn newest_recording_picks_the_largest_name() {
        let scratch = Scratch::new("largest");
        for name in ["muxed_1.avi", "muxed_10.avi", "muxed_9.avi"] {
            std::fs::write(scratch.join(name), b"x").expect("write");
        }
        assert_eq!(
            newest_recording(&scratch.path()),
            Some("muxed_10.avi".to_string()),
            "the sequence is numeric, not lexicographic"
        );
    }

    #[test]
    #[ignore = "HIL: needs a mounted SD card"]
    fn newest_recording_skips_directories_and_absent_dirs() {
        let scratch = Scratch::new("skip");
        std::fs::create_dir(scratch.join("muxed_9999.avi")).expect("dir");
        std::fs::write(scratch.join("muxed_1.avi"), b"x").expect("write");
        assert_eq!(
            newest_recording(&scratch.path()),
            Some("muxed_1.avi".to_string()),
            "a directory named like a recording is not a recording"
        );
        assert_eq!(
            newest_recording(&scratch.join("missing")),
            None,
            "an absent directory is no recording, not a panic"
        );
    }

    #[test]
    fn recording_names_order_by_numeric_suffix() {
        assert_eq!(recording_sequence("muxed_10.avi"), Some(10));
        assert_eq!(recording_sequence("muxed_9.avi"), Some(9));
        assert_eq!(recording_sequence("muxed.avi"), None);
        assert!(is_newer_recording("muxed_10.avi", "muxed_9.avi"));
        assert!(!is_newer_recording("muxed_9.avi", "muxed_10.avi"));
        assert!(is_newer_recording("REC_0002.avi", "REC_0001.avi"));
        assert!(is_newer_recording("b.avi", "a.avi"));
    }

    #[test]
    fn status_snapshot_is_stable_across_calls() {
        let status = Status {
            ip: "192.168.1.50".to_string(),
            recording_file: Some("rec/muxed_10.avi".to_string()),
            document: Document {
                version: VERSION,
                build: BUILD,
                uptime_s: 42,
                hostname: HOSTNAME,
                mounted: true,
                total_bytes: 31_914_983_424,
                free_bytes: 29_000_000_000,
                clock_khz: 20_000,
                rtsp_port: 554,
                rtsp_path: RTSP_PATH,
                rtsp_running: true,
                recording_enabled: true,
                temperature_c: Some(47.5),
                heap_internal_free: 200_000,
                heap_spiram_free: 7_000_000,
            },
            temperature_sensor: SensorState::Reported,
        };

        let first = status.snapshot();
        let second = status.snapshot();
        assert_eq!(first, second, "the builder adds nothing between calls");
        assert_eq!(first.ip, "192.168.1.50");
        assert_eq!(first.recording_file, Some("rec/muxed_10.avi"));
        assert_eq!(first.temperature_c, Some(47.5));
        assert_eq!(status.temperature_sensor(), SensorState::Reported);
    }

    #[test]
    fn repeated_gathers_do_not_grow_the_internal_heap() {
        const ITERATIONS: u64 = 1000;
        const SLACK: u64 = 4 * 1024;

        let cfg = Config {
            port: 0,
            mount_point: storage::BASE_PATH,
            rtsp_port: 554,
            rtsp_path: RTSP_PATH,
            rtsp_running: true,
            recording_enabled: true,
        };
        // Warm the sensor driver and the allocator's caches before measuring.
        let _ = gather(&cfg).snapshot();

        let before = heap_free(sys::MALLOC_CAP_INTERNAL);
        for _ in 0..ITERATIONS {
            let status = gather(&cfg);
            std::hint::black_box(status.snapshot());
        }
        let after = heap_free(sys::MALLOC_CAP_INTERNAL);

        let consumed = before.saturating_sub(after);
        assert!(
            consumed <= SLACK,
            "{ITERATIONS} gathers consumed {consumed} bytes of internal heap \
             ({before} -> {after}); the request path must not leak"
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
    fn sd_clock_khz_is_zero_without_a_mounted_volume() {
        // On a card-less boot the SDSPI host slot was never initialised;
        // calling the driver there is the Guru Meditation (LoadProhibited) this
        // guard exists to prevent. A `false` precondition must short-circuit
        // before any `sdmmc_*` call, which this assertion proves on the device:
        // if the FFI ran, the binary would fault instead of returning.
        assert_eq!(sd_clock_khz(false), 0);
    }

    #[test]
    fn unmounted_status_renders_zero_storage_fields() {
        let unmounted = Snapshot {
            version: VERSION,
            build: BUILD,
            uptime_s: 0,
            ip: "0.0.0.0",
            hostname: HOSTNAME,
            mounted: false,
            total_bytes: 0,
            free_bytes: 0,
            clock_khz: sd_clock_khz(false),
            rtsp_port: 554,
            rtsp_path: RTSP_PATH,
            rtsp_running: false,
            recording_enabled: false,
            recording_file: None,
            temperature_c: None,
            heap_internal_free: 0,
            heap_spiram_free: 0,
        };
        let mut body = String::new();
        crate::json::status_body(&unmounted, &mut body);
        let value: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");

        assert_eq!(value["mounted"], false);
        for name in ["total_bytes", "free_bytes", "clock_khz"] {
            let field = value
                .get(name)
                .unwrap_or_else(|| panic!("{name} must be present, not omitted"));
            assert_eq!(*field, 0, "{name} renders 0 without a mounted volume");
            assert!(
                field.is_number(),
                "{name} stays a JSON number when unmounted"
            );
        }
        assert!(value["recording_file"].is_null());
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
        let snapshot = status.snapshot();
        assert!(!snapshot.version.is_empty());
        assert_eq!(snapshot.rtsp_port, 554);
        assert_eq!(snapshot.rtsp_path, RTSP_PATH);
        assert_eq!(snapshot.hostname, HOSTNAME);
        match status.temperature_sensor() {
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
