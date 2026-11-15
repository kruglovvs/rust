//! SD-card storage over SPI — chip-select probe, single mount, clock ladder
//! (plan Todo 7).
//!
//! Design contract (plan Todo 7, "Do, in order"):
//! - (a) **Sweep every candidate CS pin**, not one. A bare trial-mount conflates
//!   "no card", "wrong chip-select" and "present but not FAT" into one opaque
//!   failure, and probing only GPIO21 makes a card wired to GPIO3 read exactly
//!   like no card. [`Storage::bringup`] probes **both** candidates and emits a
//!   per-pin [`ProbeOutcome`] plus an aggregate [`CardProbe`]:
//!   `Detected(GPIO21) | Detected(GPIO3) | Absent`.
//! - (b) **The probe sequence starts from `CMD0` and classifies on a command
//!   that actually responds** (`CMD8`/`SEND_IF_COND`, R7 — expect `0x01` idle
//!   plus the `0xAA` check-pattern echo on SDv2 — or the `ACMD41`/`CMD58`
//!   fallback for legacy cards). **`CMD0`'s R1 is never read as a presence
//!   signal**: `CMD0` sits in the "no response" column of the SD Physical Layer
//!   spec, an earlier draft of this plan mapped `0x04` to "card present", and
//!   on a genuine R1 `0x04` means `ILLEGAL_COMMAND`, not presence. The one
//!   reliable no-response signal is the all-ones read: with the card's CS
//!   deasserted its MISO is pulled high, so `0xFF` throughout means *not
//!   selected* — wrong CS or no card ([`ProbeOutcome::NotSelected`]).
//! - (c) **Mount exactly once, on the detected pin.** The probe already
//!   answered "which pin", so the mount is never retried against the other
//!   candidate. A failure *after* a confirmed card is a **format/volume error,
//!   not an absent card** and the log says exactly that
//!   ([`StorageError::FormatOrVolume`]).
//! - (d) The `Storage` module wraps `SdCardDriver::new_spi` +
//!   `Fatfs::new_sdcard` + `MountedFatfs`, with **32 KiB transfer buffers**
//!   ([`TRANSFER_BUF_BYTES`]). **GPIO21 is the SD card's chip-select — never
//!   drive it as an LED while the card is mounted.**
//! - (e) **Start at 40 MHz, measure, fall back 40 → 20 → 10.** The *negotiated*
//!   clock (read back with `sdspi_host_get_real_freq`, never assumed) is logged
//!   so every throughput number is attributable to a known frequency.
//!
//! ## Mount mechanism — plan (c) vs plan (d)
//!
//! Plan (c) names the C convenience function `esp_vfs_fat_sdspi_mount`; plan
//! (d) names its Rust decomposition `SdCardDriver::new_spi` +
//! `Fatfs::new_sdcard` + `MountedFatfs`. These are the **same five steps**
//! (`sdspi_host_init` → `sdspi_host_init_device` → `sdmmc_card_init` →
//! `ff_diskio_register_sdmmc` → `esp_vfs_fat_register` + `f_mount`) expressed
//! in C and Rust respectively, and they **cannot both run**: each calls
//! `sdmmc_card_init`, so a second one would `CMD0`-reset a card that is
//! already mounted (data corruption), and both attach a device to the same CS
//! GPIO. Verified against ESP-IDF 5.5.2
//! (`components/fatfs/vfs/vfs_fat_sdmmc.c:332`) and esp-idf-svc 0.53.0
//! (`examples/sd_spi.rs`, the canonical `SdCardDriver::new_spi` +
//! `Fatfs::new_sdcard` + `MountedFatfs` usage).
//!
//! This module implements **plan (d) verbatim** and satisfies **every
//! semantic requirement of plan (c)**:
//! | plan (c) requirement | how it is met here |
//! |---|---|
//! | mount exactly once, on the detected pin | [`Storage::bringup`] mounts a single time, after the sweep picks the pin |
//! | mount failure after a confirmed card = format/volume error | [`StorageError::FormatOrVolume`] + the `ERROR:` log line naming it as *not* an absent card |
//! | a fallback attempt must unmount between attempts (single global mount point) | `MountedFatfs::drop` runs `f_mount(NULL)` + `esp_vfs_fat_unregister_path`, i.e. exactly that unmount — and the clock ladder does not remount at all, it retimes the live device with `sdspi_host_set_card_clk` |
//!
//! Plan (c)'s paired symbol `esp_vfs_fat_sdspi_unmount` **does not exist** in
//! ESP-IDF 5.5.2 (the real symbols are `esp_vfs_fat_sdcard_unmount` and the
//! deprecated `esp_vfs_fat_sdmmc_unmount`), so plan (c)'s C-API naming cannot
//! be followed literally in any case.
//!
//! ## Board wiring (verified in plan References)
//!
//! | signal | GPIO | note |
//! |---|---|---|
//! | SCK | 7 | |
//! | MISO (SDI) | 8 | pulled high by R5 when the card is deselected |
//! | MOSI (SDO) | 9 | |
//! | CS primary | 21 | R12 = 0R **fitted**; Seeed `SD.begin(21)`. **Also the user LED** |
//! | CS alternate | 3 | R11 = DNP (XIAO `D2`); `esp-cpp/espp` BSP wrongly hard-codes this |
//! | bus | SPI2_HOST | |
//!
//! Every `unsafe` block lives inside the private [`ffi`] module (same pattern
//! as `camera.rs`/`sensor.rs`); no public path panics — every failure is a
//! typed [`StorageError`].

use core::fmt;

use aligned::{Aligned, A4};
use esp_idf_hal::gpio::{AnyIOPin, AnyOutputPin, Gpio21, Gpio3, Gpio7, Gpio8, Gpio9, PinId};
use esp_idf_hal::sd::config::Configuration as SdCardConfiguration;
use esp_idf_hal::sd::spi::SdSpiHostDriver;
use esp_idf_hal::sd::SdCardDriver;
use esp_idf_hal::spi::config::{Config as SpiDeviceConfig, DriverConfig as SpiDriverConfig};
use esp_idf_hal::spi::{Dma, SpiDeviceDriver, SpiDriver, SPI2};
use esp_idf_hal::units::Hertz;
use esp_idf_svc::fs::fatfs::Fatfs;
use esp_idf_svc::io::vfs::MountedFatfs;

// ---------------------------------------------------------------------------
// Private FFI boundary — the ONLY `unsafe` in this file.
// ---------------------------------------------------------------------------

mod ffi {
    use core::ffi::{c_char, c_int, CStr};
    use core::sync::atomic::{AtomicI32, Ordering};

    use esp_idf_hal::gpio::{AnyOutputPin, PinId};
    use esp_idf_sys::{
        esp_err_t, esp_timer_get_time, esp_vfs_fat_info, gpio_config,
        gpio_config_t, gpio_int_type_t_GPIO_INTR_DISABLE, gpio_mode_t_GPIO_MODE_OUTPUT,
        gpio_pulldown_t_GPIO_PULLDOWN_DISABLE, gpio_pullup_t_GPIO_PULLUP_DISABLE, gpio_set_level,
        sdmmc_command_t, sdspi_dev_handle_t, sdspi_host_get_real_freq, sdspi_host_set_card_clk,
        EspError,
    };

    unsafe extern "C" {
        fn __real_sdspi_host_do_transaction(
            handle: sdspi_dev_handle_t,
            cmdinfo: *mut sdmmc_command_t,
        ) -> esp_err_t;
    }

    static CMD8_LAST_ERROR: AtomicI32 = AtomicI32::new(0);

    /// `esp_err_t` the last real CMD8 (`SEND_IF_COND`) returned during card
    /// init. `0x107` = response timeout (late-answering card), `0x106` =
    /// card answered `ILLEGAL_COMMAND`, `0` = accepted.
    pub(super) fn cmd8_last_error() -> i32 {
        CMD8_LAST_ERROR.load(Ordering::Relaxed)
    }

    /// Link-time interposer for `sdspi_host_do_transaction`, installed with
    /// `-Wl,--wrap=sdspi_host_do_transaction` from `build.rs`.
    ///
    /// Two ESP-IDF 5.5.2 SPI-init steps are skipped here because this card
    /// cannot complete them:
    ///
    /// - **CMD5** (`SD_IO_SEND_OP_COND`) — `sdmmc_card_init` always probes
    ///   SDIO (`sdmmc_init.c` → `sdmmc_init_io`, `io_supported` hard-coded
    ///   `true`). The card answers CMD5 late, the sdspi transaction times out,
    ///   and the stale R1 (`ILLEGAL_COMMAND`) is then consumed as CMD59's
    ///   response, so `sdmmc_init_spi_crc` (`sdmmc_sd.c:613`) aborts with
    ///   `ESP_ERR_NOT_SUPPORTED` (0x106). `sdmmc_init_io` treats any CMD5
    ///   error as "not an IO card" and continues with `is_mem = 1`, so
    ///   reporting it unsupported without touching the bus is exactly
    ///   upstream ESP-IDF's `CONFIG_SD_ENABLE_SDIO_SUPPORT=n` path (absent
    ///   in 5.5.2). The card accepts CMD59 itself (R1 = 0x01 from a clean
    ///   bus, verified with the raw probe).
    ///
    /// - **CMD6** (`MMC_SWITCH`) — `sdmmc_enable_hs_mode_and_check` switches
    ///   the card to SD high-speed and then re-reads CSD, which this card
    ///   fails with `ESP_ERR_INVALID_RESPONSE` (0x108). High-speed mode is a
    ///   parallel-SD concern; the SPI clock is set independently by
    ///   `sdspi_host_set_card_clk`, and `sdmmc_init_card_hs_mode` already
    ///   treats `ESP_ERR_NOT_SUPPORTED` as "card stays at default speed".
    ///
    /// # Safety
    ///
    /// Same contract as the wrapped function: `cmdinfo` must be null or a
    /// valid `sdmmc_command_t`. Every other command is forwarded unchanged.
    #[no_mangle]
    pub(super) unsafe extern "C" fn __wrap_sdspi_host_do_transaction(
        handle: sdspi_dev_handle_t,
        cmdinfo: *mut sdmmc_command_t,
    ) -> esp_err_t {
        if !cmdinfo.is_null() {
            let opcode = (*cmdinfo).opcode;
            if opcode == esp_idf_sys::SD_IO_SEND_OP_COND || opcode == esp_idf_sys::MMC_SWITCH {
                return esp_idf_sys::ESP_ERR_NOT_SUPPORTED;
            }
            if opcode == esp_idf_sys::SD_SEND_IF_COND {
                let ret = unsafe { __real_sdspi_host_do_transaction(handle, cmdinfo) };
                CMD8_LAST_ERROR.store(ret, Ordering::Relaxed);
                return ret;
            }
        }
        unsafe { __real_sdspi_host_do_transaction(handle, cmdinfo) }
    }

    /// Re-create a chip-select output handle after its previous owner (the
    /// `SdSpiHostDriver` of a failed `sdmmc_card_init`) has been dropped.
    ///
    /// # Safety
    ///
    /// The caller must ensure no other `AnyOutputPin` for `pin` is alive. The
    /// card-init retry loop only calls this after `SdCardDriver::new_spi`
    /// returned `Err` and dropped the host that owned the pin.
    pub(super) fn steal_output_pin(pin: PinId) -> AnyOutputPin<'static> {
        unsafe { AnyOutputPin::steal(pin) }
    }

    /// Configure `pin` as a push-pull output. Used by the raw probe to drive
    /// chip-select by hand (the probe device is created with no hardware CS so
    /// that CS can be left **deasserted** for the all-ones bus-idle read that
    /// plan (b) requires).
    ///
    /// Configure `pin` as a push-pull output idling **high** (deselected).
    ///
    /// Safe wrapper: `gpio_config`/`gpio_set_level` have no Rust-memory
    /// preconditions — the only obligation is that `pin` names a real board
    /// GPIO, which [`super::CsPin::number`] guarantees. No `GpioNN` handle is
    /// aliased: the ones in [`super::CandidatePins`] stay untouched and are
    /// later consumed by `SdSpiHostDriver::new`, which reconfigures the same
    /// pin as the SDSPI hardware CS.
    pub(super) fn cs_as_output(pin: PinId) -> Result<(), EspError> {
        let cfg = gpio_config_t {
            pin_bit_mask: 1u64 << pin,
            mode: gpio_mode_t_GPIO_MODE_OUTPUT,
            pull_up_en: gpio_pullup_t_GPIO_PULLUP_DISABLE,
            pull_down_en: gpio_pulldown_t_GPIO_PULLDOWN_DISABLE,
            intr_type: gpio_int_type_t_GPIO_INTR_DISABLE,
            ..Default::default()
        };
        esp_idf_sys::esp!(unsafe { gpio_config(&cfg) })?;
        esp_idf_sys::esp!(unsafe { gpio_set_level(pin as _, 1) })?;
        Ok(())
    }

    /// Drive chip-select low (selected). Safe wrapper; requires the pin to
    /// have been prepared by [`cs_as_output`].
    pub(super) fn cs_select(pin: PinId) -> Result<(), EspError> {
        esp_idf_sys::esp!(unsafe { gpio_set_level(pin as _, 0) })
    }

    /// Drive chip-select high (deselected). Left in this state after the probe
    /// so the card stays deselected while the *other* candidate is probed.
    /// Safe wrapper; requires the pin to have been prepared by
    /// [`cs_as_output`].
    pub(super) fn cs_deselect(pin: PinId) -> Result<(), EspError> {
        esp_idf_sys::esp!(unsafe { gpio_set_level(pin as _, 1) })
    }

    /// Monotonic microsecond timestamp (`esp_timer_get_time`).
    pub(super) fn now_us() -> i64 {
        // SAFETY: `esp_timer_get_time` is a plain accessor with no preconditions.
        unsafe { esp_timer_get_time() }
    }

    /// The **negotiated** SPI clock, in kHz, as actually programmed on the
    /// SDSPI device (`sdspi_host_get_real_freq` → `spi_device_get_actual_freq`).
    ///
    /// This is what plan (e) means by "negotiated" — never assume 40 MHz.
    pub(super) fn real_freq_khz(handle: sdspi_dev_handle_t) -> Result<u32, EspError> {
        let mut khz: c_int = 0;
        esp_idf_sys::esp!(unsafe { sdspi_host_get_real_freq(handle, &mut khz) })?;
        Ok(khz as u32)
    }

    /// Retarget the card clock without re-mounting (plan (e) fallback rungs).
    pub(super) fn set_card_clk(handle: sdspi_dev_handle_t, khz: u32) -> Result<(), EspError> {
        esp_idf_sys::esp!(unsafe { sdspi_host_set_card_clk(handle, khz) })
    }

    /// `(total_bytes, free_bytes)` for a registered FATFS VFS mount point.
    pub(super) fn fat_info(base: &CStr) -> Result<(u64, u64), EspError> {
        let mut total: u64 = 0;
        let mut free: u64 = 0;
        esp_idf_sys::esp!(unsafe {
            esp_vfs_fat_info(base.as_ptr() as *const c_char, &mut total, &mut free)
        })?;
        Ok((total, free))
    }

    /// Read one or more sectors from the SD card using raw `sdmmc_read_sectors`.
    ///
    /// SAFETY: `card` must be a valid pointer to an initialized `sdmmc_card_t`,
    /// and `dst` must be a valid mutable buffer of at least `sector_count * 512` bytes.
    pub(super) fn read_sectors(
        card: *mut esp_idf_sys::sdmmc_card_t,
        dst: *mut u8,
        start_sector: usize,
        sector_count: usize,
    ) -> Result<(), EspError> {
        esp_idf_sys::esp!(unsafe {
            esp_idf_sys::sdmmc_read_sectors(card, dst as *mut _, start_sector, sector_count)
        })
    }
}

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// VFS mount point. Single global mount point — plan (c).
pub const BASE_PATH: &str = "/sdcard";

/// FATFS drive number registered by `Fatfs::new_sdcard`.
const DRIVE: u8 = 0;

/// Maximum simultaneously-open files handed to `MountedFatfs::mount`.
const MAX_OPEN_FILES: usize = 5;

/// **32 KiB transfer buffer** (plan (d)). One-sector-at-a-time Arduino writes
/// measure only 0.278 MB/s; large buffers are mandatory to clear the
/// 256 KiB/s throughput gate.
pub const TRANSFER_BUF_BYTES: usize = 32 * 1024;

/// 1 MiB write/read round-trip, asserted byte-identical (acceptance).
pub const ROUNDTRIP_BYTES: u64 = 1024 * 1024;

/// 4 MiB sequential write whose measured rate drives the clock ladder
/// (acceptance + plan (e)).
pub const THROUGHPUT_BYTES: u64 = 4 * 1024 * 1024;

/// Final 4 KiB read-back (acceptance). The 4 MiB write must be followed by a
/// `getfree` check **and** a read-back of the *end* of the file, not just the
/// start — a 32 MiB test file cannot catch a FAT32 capacity bug.
pub const TAIL_READ_BYTES: u64 = 4 * 1024;

/// Throughput gate for the clock ladder: the 4 MiB sequential write must
/// sustain ≥ 256 KiB/s at the trial clock.
///
/// The gate is tied to what the recorder actually needs, not to an unrelated
/// planning figure. Measured recording demand is ~145 kB/s at the top of the
/// envelope (10 fps MJPEG ≈ 62–112 kB/s + 16 kHz/16-bit PCM = 32 kB/s), so
/// 256 KiB/s leaves ~1.8x headroom over 145 kB/s. It also sits comfortably
/// below the ~417 kB/s this card sustains at the negotiated 20 MHz, so a
/// healthy link clears it without falling back.
pub const MIN_WRITE_BYTES_PER_SEC: u64 = 262_144;

/// SPI clock used for the raw probe. The SD spec caps pre-initialisation
/// traffic at 400 kHz; `sdmmc_card_init` later runs its own 400 kHz identify
/// phase before raising the bus to `speed_khz`.
const PROBE_BAUD_HZ: u32 = 400_000;

/// Clock ladder for plan (e): start at 40 MHz, then 20, then 10.
pub const CLOCK_LADDER_KHZ: [u32; 3] = [40_000, 20_000, 10_000];

/// Response window per exchange: 6 command bytes + 32 clocks. `NCR` (the
/// card's response delay) is 1–8 bytes, then R1, then up to 4 R7 payload
/// bytes, so 32 clocks after the command is ample.
const EXCHANGE_LEN: usize = 6 + 32;

// ---------------------------------------------------------------------------
// Candidate chip-select pins
// ---------------------------------------------------------------------------

/// The two chip-select candidates on this board (plan Todo 7: the pin is
/// *disputed in the wild* — Seeed ships R12 = 0R onto GPIO21, R11 = DNP would
/// bring out GPIO3, and the `esp-cpp/espp` BSP hard-codes the GPIO3 bug).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CsPin {
    /// GPIO21 — the shipped path (Seeed `SD.begin(21)`).
    ///
    /// **GPIO21 is also the user LED. Never drive it as an LED while the SD
    /// card is mounted, because that pin is the SD card CS.** (Seeed forum.)
    Gpio21,
    /// GPIO3 — the DNP alternate (XIAO `D2`, Round Display), wrongly used by
    /// `esp-cpp/espp`.
    Gpio3,
}

impl CsPin {
    /// Sweep order: GPIO21 (shipped) first, GPIO3 (alternate) second.
    pub const CANDIDATES: [CsPin; 2] = [CsPin::Gpio21, CsPin::Gpio3];

    /// GPIO number.
    pub const fn number(self) -> PinId {
        match self {
            CsPin::Gpio21 => 21,
            CsPin::Gpio3 => 3,
        }
    }

    /// Label used in every log line (`GPIO21` / `GPIO3`).
    pub const fn label(self) -> &'static str {
        match self {
            CsPin::Gpio21 => "GPIO21",
            CsPin::Gpio3 => "GPIO3",
        }
    }
}

impl fmt::Display for CsPin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// The CS pin handles, handed to [`Storage::bringup`] so the sweep can probe
/// *both* and the mount can consume exactly the detected one.
pub struct CandidatePins {
    pub gpio21: AnyOutputPin<'static>,
    pub gpio3: AnyOutputPin<'static>,
}

impl CandidatePins {
    /// Build the pair from the typed `Gpio21`/`Gpio3` handles taken out of
    /// `esp_idf_hal::peripherals::Peripherals::take()`'s `pins`.
    pub fn new(gpio21: Gpio21<'static>, gpio3: Gpio3<'static>) -> Self {
        Self {
            gpio21: gpio21.degrade_output(),
            gpio3: gpio3.degrade_output(),
        }
    }

    /// Consume the pair, yielding only the handle for `detected` — the other
    /// candidate's handle is dropped, so a mount can never reach the wrong pin.
    fn take(self, detected: CsPin) -> AnyOutputPin<'static> {
        match detected {
            CsPin::Gpio21 => self.gpio21,
            CsPin::Gpio3 => self.gpio3,
        }
    }
}

/// Build the shared SPI2 bus for the SD card.
///
/// Board wiring (verified, plan Todo 7 references): **SCK = GPIO7,
/// MOSI (SDO) = GPIO9, MISO (SDI) = GPIO8, host = `SPI2_HOST`.** DMA is
/// `Auto(4096)` — the same configuration the official esp-idf-svc
/// `sd_spi.rs` example uses, and required to clear the 256 KiB/s gate (a
/// polling-bound bus cannot sustain it alongside FATFS overhead).
///
/// The returned driver's lifetime is deliberately unconstrained: the caller
/// unifies it with the `&SpiDriver` borrow passed to
/// [`Storage::bringup`].
pub fn new_bus<'d>(
    spi2: SPI2<'static>,
    sck: Gpio7<'static>,
    mosi: Gpio9<'static>,
    miso: Gpio8<'static>,
) -> Result<SpiDriver<'d>, esp_idf_sys::EspError> {
    SpiDriver::new::<SPI2>(
        spi2,
        sck,
        mosi,
        Some(miso),
        &SpiDriverConfig::new().dma(Dma::Auto(4096)),
    )
}

// ---------------------------------------------------------------------------
// Probe verdicts
// ---------------------------------------------------------------------------

/// Aggregate verdict across every candidate CS pin — plan (a):
/// `Detected(GPIO21) | Detected(GPIO3) | Absent`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardProbe {
    /// A card answered the identification sequence on this CS pin.
    Detected(CsPin),
    /// No candidate pin selected a card (all-ones on MISO for **every** pin).
    Absent,
}

impl CardProbe {
    /// The `CardProbe state=` field of the log line (acceptance: the log
    /// reports `card-present` / `not-selected` / `absent`).
    pub const fn state_label(&self) -> &'static str {
        match self {
            CardProbe::Detected(_) => "card-present",
            CardProbe::Absent => "absent",
        }
    }
}

impl fmt::Display for CardProbe {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CardProbe::Detected(cs) => write!(f, "Detected({})", cs),
            CardProbe::Absent => f.write_str("Absent"),
        }
    }
}

// The hardware-free probe classification lives in its own module so the
// host harness can compile and test it. Re-exported here so every existing
// `storage::{...}` path keeps working.
pub use crate::sd_probe::{
    aggregate_detected, classify_cmd8, encode_command, find_r1, is_all_ones,
    legacy_identifies, CardClass, ProbeOutcome, R7,
};

// ---------------------------------------------------------------------------
// Aggregate verdict (plan (a))
// ---------------------------------------------------------------------------

/// Fold the per-pin outcomes into the aggregate [`CardProbe`] — plan (a).
///
/// First detected candidate wins (GPIO21 is probed first, so a card wired to
/// both would still be reported as `Detected(GPIO21)`); `Absent` only when
/// every candidate is `NotSelected`/`Unidentified`.
pub fn aggregate_probe(results: &[(CsPin, ProbeOutcome)]) -> CardProbe {
    match aggregate_detected(results) {
        Some(pin) => CardProbe::Detected(pin),
        None => CardProbe::Absent,
    }
}

/// `bytes * 1e6 / elapsed_us`, saturating at zero elapsed time.
pub fn throughput_bytes_per_sec(bytes: u64, elapsed_us: i64) -> u64 {
    if elapsed_us <= 0 {
        return 0;
    }
    bytes.saturating_mul(1_000_000) / (elapsed_us as u64)
}

// ---------------------------------------------------------------------------
// Clock-ladder state machine (unit-tested without hardware)
// ---------------------------------------------------------------------------

/// Verdict of one clock rung, consumed by [`ClockLadder::record`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClockVerdict {
    /// This clock passed — it is the negotiated clock.
    Accept { khz: u32 },
    /// Fall back to the next rung; `None` means the ladder is exhausted.
    FallBack { next_khz: Option<u32> },
}

/// Plan (e): "Start at 40 MHz, measure, and fall back… retry at 20 MHz, then
/// 10 MHz, and record the clock that actually passed."
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClockLadder {
    idx: usize,
}

impl ClockLadder {
    /// Start at 40 MHz — the fastest rung.
    pub const fn new() -> Self {
        Self { idx: 0 }
    }

    /// The rung to try now.
    pub fn current_khz(&self) -> u32 {
        CLOCK_LADDER_KHZ[self.idx.min(CLOCK_LADDER_KHZ.len() - 1)]
    }

    /// `true` once every rung has been consumed.
    pub const fn exhausted(&self) -> bool {
        self.idx >= CLOCK_LADDER_KHZ.len()
    }

    /// Record a trial. Pass requires **both** no instability (I/O error or
    /// read-back mismatch ⇒ CRC-class failure) **and** the ≥ 256 KiB/s floor.
    /// Anything else advances to the next slower rung.
    ///
    /// Rationale for falling back on *throughput* as well as errors: at
    /// 40 MHz a marginal link spends its budget on retransmission, so the
    /// observed rate drops below the floor even though the "correct" clock
    /// would be faster — removing the errors is exactly what buying a slower
    /// clock does.
    pub fn record(&mut self, bytes_per_sec: u64, unstable: bool) -> ClockVerdict {
        if !unstable && bytes_per_sec >= MIN_WRITE_BYTES_PER_SEC {
            return ClockVerdict::Accept {
                khz: self.current_khz(),
            };
        }
        self.idx += 1;
        ClockVerdict::FallBack {
            next_khz: CLOCK_LADDER_KHZ.get(self.idx).copied(),
        }
    }
}

impl Default for ClockLadder {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Errors (no thiserror — plain enums + Display/Error, zero new dependencies)
// ---------------------------------------------------------------------------

/// Every failure mode of the SD path. **No public path panics.**
#[derive(Debug)]
pub enum StorageError {
    /// All candidate CS pins read all-ones — no card seated (or wired to a pin
    /// we do not know about). Streaming must continue **without** storage.
    CardAbsent,
    /// The SPI device for a probe exchange could not be attached.
    SpiDevice {
        cs: CsPin,
        source: esp_idf_sys::EspError,
    },
    /// A candidate pin could not be prepared for manual chip-select.
    CsPrepare {
        cs: CsPin,
        source: esp_idf_sys::EspError,
    },
    /// A raw probe exchange failed at the transport level.
    Exchange {
        cs: CsPin,
        source: esp_idf_sys::EspError,
    },
    /// `sdmmc_card_init` failed even though raw identification succeeded.
    CardInit {
        cs: CsPin,
        source: esp_idf_sys::EspError,
    },
    /// **A card is confirmed present but the volume did not mount** — a
    /// format/volume error, *not* an absent card. Logged verbatim as such.
    FormatOrVolume {
        cs: CsPin,
        stage: &'static str,
        detail: String,
    },
    /// The SPI clock could not be retimed / read back during the ladder.
    ClockControl { source: esp_idf_sys::EspError },
    /// Every rung of the 40 → 20 → 10 MHz ladder failed.
    LadderExhausted {
        tried_khz: Vec<u32>,
        last_reason: String,
    },
    /// Filesystem I/O failed while the volume was mounted.
    Io {
        path: String,
        source: std::io::Error,
    },
    /// Read-back did not match what was written — silent corruption at this
    /// clock (the failure mode the plan warns about at 40 MHz).
    DataMismatch { path: String, offset: u64 },
    /// Free-space accounting did not move by the 4 MiB written.
    CapacityMismatch {
        before: u64,
        after: u64,
        written: u64,
    },
    /// `esp_vfs_fat_info` failed after a successful mount.
    FreeSpaceQuery { source: esp_idf_sys::EspError },
}

impl fmt::Display for StorageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StorageError::CardAbsent => {
                f.write_str("no candidate CS pin selected a card (MISO all-ones on every pin)")
            }
            StorageError::SpiDevice { cs, source } => {
                write!(f, "could not attach SPI probe device on {cs}: {source}")
            }
            StorageError::CsPrepare { cs, source } => {
                write!(f, "could not configure CS={cs} for the probe: {source}")
            }
            StorageError::Exchange { cs, source } => {
                write!(f, "raw SD exchange failed on {cs}: {source}")
            }
            StorageError::CardInit { cs, source } => {
                write!(
                    f,
                    "sdmmc_card_init failed on {cs} after identification: {source}"
                )
            }
            StorageError::FormatOrVolume { cs, stage, detail } => write!(
                f,
                "card present on {cs} but the volume did not mount at {stage}: \
                 format/volume error, not an absent card ({detail})"
            ),
            StorageError::ClockControl { source } => {
                write!(f, "SPI clock control failed: {source}")
            }
            StorageError::LadderExhausted {
                tried_khz,
                last_reason,
            } => write!(
                f,
                "clock ladder exhausted after {tried_khz:?} kHz; last trial: {last_reason}"
            ),
            StorageError::Io { path, source } => {
                write!(f, "I/O on {path}: {source}")
            }
            StorageError::DataMismatch { path, offset } => {
                write!(
                    f,
                    "read-back of {path} differs from what was written at offset {offset}"
                )
            }
            StorageError::CapacityMismatch {
                before,
                after,
                written,
            } => write!(
                f,
                "getfree delta {before} -> {after} does not account for the {written} bytes written"
            ),
            StorageError::FreeSpaceQuery { source } => {
                write!(f, "esp_vfs_fat_info failed: {source}")
            }
        }
    }
}

impl std::error::Error for StorageError {}

// ---------------------------------------------------------------------------
// Owned mount chain (plan (d))
// ---------------------------------------------------------------------------

/// `MountedFatfs` → `Fatfs` → `SdCardDriver` → `SdSpiHostDriver` → the shared
/// SPI2 bus. The bus is *borrowed* (never moved in) so the probe could share
/// it; dropping the chain unmounts VFS + FATFS and detaches the SDSPI device.
type SdChain<'d> = MountedFatfs<Fatfs<SdCardDriver<SdSpiHostDriver<'d, &'d SpiDriver<'d>>>>>;

// ---------------------------------------------------------------------------
// Raw probe — the hal's SPI device, below the VFS layer
// ---------------------------------------------------------------------------

/// The probe's SPI device type: the hal's SPI device sharing the bus by
/// reference, with **no** hardware CS so chip-select can be driven by hand.
type ProbeDevice<'d> = SpiDeviceDriver<'d, &'d SpiDriver<'d>>;

/// One candidate-pin probe: manual CS + `CMD0` → `CMD8` → (`ACMD41`/`CMD58`).
struct RawSd<'d> {
    dev: ProbeDevice<'d>,
    cs: CsPin,
}

impl<'d> RawSd<'d> {
    /// Attach a probe device on `spi` and prepare `cs` as a software
    /// chip-select. The bus runs at [`PROBE_BAUD_HZ`] (spec: ≤ 400 kHz before
    /// the card is initialised).
    fn new(spi: &'d SpiDriver<'d>, cs: CsPin) -> Result<Self, StorageError> {
        ffi::cs_as_output(cs.number()).map_err(|source| StorageError::CsPrepare { cs, source })?;

        let config = SpiDeviceConfig::new().baudrate(Hertz(PROBE_BAUD_HZ));
        let dev = SpiDeviceDriver::new(spi, AnyOutputPin::none(), &config)
            .map_err(|source| StorageError::SpiDevice { cs, source })?;

        Ok(Self { dev, cs })
    }

    /// Clock 8 bytes with chip-select **deasserted** and report whether the
    /// window is all-ones.
    ///
    /// Plan (b): with the card deselected its MISO is pulled high, so every
    /// byte must be `0xFF`. This is the bus-idle sanity read.
    fn bus_idle_is_high(&mut self) -> Result<bool, StorageError> {
        let mut buf = [0xFFu8; 8];
        self.clock(&mut buf, false)?;
        Ok(is_all_ones(&buf))
    }

    /// Issue one command frame and return the full exchange (command bytes
    /// replaced by whatever MISO returned, then the response window).
    fn exchange(&mut self, cmd: [u8; 6]) -> Result<[u8; EXCHANGE_LEN], StorageError> {
        let mut buf = [0xFFu8; EXCHANGE_LEN];
        buf[..6].copy_from_slice(&cmd);
        self.clock(&mut buf, true)?;
        Ok(buf)
    }

    /// Full-duplex clocking around a manual CS transition.
    fn clock(&mut self, buf: &mut [u8], assert_cs: bool) -> Result<(), StorageError> {
        let (cs, pin) = (self.cs, self.cs.number());
        let assert = if assert_cs {
            ffi::cs_select(pin)
        } else {
            ffi::cs_deselect(pin)
        };
        assert.map_err(|source| StorageError::Exchange { cs, source })?;

        let result = self
            .dev
            .transfer_in_place(buf)
            .map_err(|source| StorageError::Exchange { cs, source });

        // Always release CS, even when the transfer failed.
        let release = ffi::cs_deselect(pin).map_err(|source| StorageError::Exchange { cs, source });

        result.and(release)
    }

    /// Pull R1 (+ R7 payload when present) out of a post-command window.
    fn parse_r7(exchange: &[u8]) -> Option<R7> {
        let (idx, r1) = find_r1(&exchange[6..])?;
        let start = 6 + idx + 1;
        let mut payload = [0xFFu8; 4];
        if start + 4 <= exchange.len() {
            payload.copy_from_slice(&exchange[start..start + 4]);
        }
        Some(R7 { r1, payload })
    }
}

impl<'d> Drop for RawSd<'d> {
    fn drop(&mut self) {
        // Best effort: leave the card deselected whatever happened above.
        let _ = ffi::cs_deselect(self.cs.number());
    }
}

/// Probe one candidate CS pin and return its [`ProbeOutcome`].
///
/// Sequence (plan (b)):
/// 1. bus-idle read with CS **deasserted** — all-ones is the pull-up resting
///    level;
/// 2. `CMD0` (`GO_IDLE_STATE`, R1 `0x95` CRC) — **issued but its R1 is never
///    used to classify presence**;
/// 3. `CMD8` (`SEND_IF_COND`, R7, arg `0x1AA`, `0x87` CRC) — all-ones here is
///    the *not selected* verdict, otherwise `classify_cmd8`;
/// 4. for a legacy card, `CMD55` + `ACMD41` then `CMD58`.
fn probe_pin<'d>(spi: &'d SpiDriver<'d>, cs: CsPin) -> Result<ProbeOutcome, StorageError> {
    let mut raw = RawSd::new(spi, cs)?;

    // (1) Bus idle with CS deasserted: the card's MISO must be pulled high.
    let idle_ok = raw.bus_idle_is_high()?;
    if !idle_ok {
        println!(
            "storage: probe CS={} bus-idle read was not all-ones (something is driving MISO)",
            cs.label()
        );
    }

    // (2) CMD0 — reset to idle. Explicitly NOT a presence signal.
    let cmd0 = raw.exchange(encode_command(0, 0x0000_0000, 0x95))?;
    let cmd0_note = match RawSd::parse_r7(&cmd0) {
        Some(r) => format!("R1={:#04x}", r.r1),
        None => "no R1 within 32 clocks".to_string(),
    };

    // (3) CMD8 — the command that actually responds. Classify HERE.
    let cmd8 = raw.exchange(encode_command(8, 0x0000_01AA, 0x87))?;
    if is_all_ones(&cmd8[6..]) {
        println!(
            "storage: probe CS={} -> not-selected (MISO all-ones during CMD8; CMD0 {})",
            cs.label(),
            cmd0_note
        );
        return Ok(ProbeOutcome::NotSelected);
    }

    match RawSd::parse_r7(&cmd8).and_then(|r| classify_cmd8(&r).map(|c| (r, c))) {
        Some((_r, class)) => {
            println!(
                "storage: probe CS={} -> detected ({} via CMD8; CMD0 {})",
                cs.label(),
                class,
                cmd0_note
            );
            Ok(ProbeOutcome::Detected { class })
        }
        None => {
            // CMD8 rejected: fall back to ACMD41 / CMD58 (plan (b)).
            let _cmd55 = raw.exchange(encode_command(55, 0x0000_0000, 0x01))?;
            let acmd41 = raw.exchange(encode_command(41, 0x4000_0000, 0x01))?;
            let r_acmd41 = RawSd::parse_r7(&acmd41).map(|r| r.r1);
            let cmd58 = raw.exchange(encode_command(58, 0x0000_0000, 0x01))?;
            let r_cmd58 = RawSd::parse_r7(&cmd58).map(|r| r.r1);

            match (r_acmd41, r_cmd58) {
                (Some(a), Some(b)) if legacy_identifies(a, b) => {
                    println!(
                        "storage: probe CS={} -> detected (legacy via ACMD41/CMD58, \
                         ACMD41 R1={:#04x} CMD58 R1={:#04x}; CMD8 did not identify)",
                        cs.label(),
                        a,
                        b
                    );
                    Ok(ProbeOutcome::Detected {
                        class: CardClass::Legacy,
                    })
                }
                (a, b) => {
                    println!(
                        "storage: probe CS={} -> unidentified (CMD8 R1/payload rejected; \
                         ACMD41={:?} CMD58={:?})",
                        cs.label(),
                        a,
                        b
                    );
                    Ok(ProbeOutcome::Unidentified)
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Storage
// ---------------------------------------------------------------------------

/// Read sector 0 plus the first MBR partition's boot sector and name the
/// filesystem on the card.
///
/// This runs before FatFs sees the card, so it distinguishes a FAT32 volume
/// from "the card has no recognizable volume at all". Without it a failed mount
/// collapses into `ENODEV` and cannot be attributed.
fn detect_filesystem(card: &esp_idf_sys::sdmmc_card_t) -> &'static str {
    let card_ptr = card as *const _ as *mut esp_idf_sys::sdmmc_card_t;
    let mut mbr: Aligned<A4, [u8; 512]> = Aligned([0u8; 512]);
    if ffi::read_sectors(card_ptr, mbr.as_mut_ptr(), 0, 1).is_err() {
        println!("storage: filesystem probe: sector 0 unreadable");
        return "unreadable";
    }
    if mbr[510] != 0x55 || mbr[511] != 0xAA {
        println!("storage: filesystem probe: sector 0 has no 0x55AA MBR signature");
        return "raw";
    }

    let mut start_lba = 0u32;
    let mut partition_type = 0u8;
    for entry in 0..4 {
        let offset = 446 + entry * 16;
        if mbr[offset + 4] != 0 {
            partition_type = mbr[offset + 4];
            start_lba = u32::from_le_bytes([
                mbr[offset + 8],
                mbr[offset + 9],
                mbr[offset + 10],
                mbr[offset + 11],
            ]);
            break;
        }
    }
    if partition_type == 0 {
        println!("storage: filesystem probe: MBR has no populated partition entries");
        return "none";
    }

    let mut boot: Aligned<A4, [u8; 512]> = Aligned([0u8; 512]);
    if ffi::read_sectors(card_ptr, boot.as_mut_ptr(), start_lba as usize, 1).is_err() {
        println!(
            "storage: filesystem probe: partition 1 (type {partition_type:#04x}) at LBA \
             {start_lba} unreadable"
        );
        return "unreadable";
    }
    let b = &boot;
    let kind = if &b[82..90] == b"FAT32   " {
        "FAT32"
    } else {
        "unknown"
    };
    println!(
        "storage: filesystem probe: partition1 type={partition_type:#04x} start_lba={start_lba} \
         fs={kind}"
    );
    kind
}

/// Outcome of one clock-ladder trial (all acceptance checks run together).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Trial {
    bytes_per_sec: u64,
    unstable: bool,
    reason: &'static str,
}

/// Mounted SD storage plus the facts the log and later todos need.
///
/// Dropping this unmounts (VFS unregistration + `f_mount(NULL)` + SDSPI device
/// detach) — plan (c)'s "unmount between attempts" discipline, for free.
pub struct Storage<'d> {
    /// The mounted FAT filesystem. Deliberately unread today: the accessor that
    /// the recorder (Todo 10) and the boot wiring (Todo 20) need does not exist
    /// yet, so this would otherwise be flagged as a dead field.
    #[allow(dead_code)]
    chain: SdChain<'d>,
    probe: CardProbe,
    cs: CsPin,
    negotiated_khz: u32,
    write_bytes_per_sec: u64,
    total_bytes: u64,
    free_bytes: u64,
}

impl<'d> Storage<'d> {
    /// Probe both candidate CS pins, mount **once** on the detected pin, run
    /// the 40 → 20 → 10 MHz ladder with the full acceptance battery at each
    /// rung, and log every acceptance field.
    ///
    /// Returns [`StorageError::CardAbsent`] when no pin selects a card —
    /// the caller then continues streaming **without** storage, never with a
    /// bare "mount failed".
    pub fn bringup(
        spi: &'d SpiDriver<'d>,
        pins: CandidatePins,
    ) -> Result<Storage<'d>, StorageError> {
        // (a) Sweep EVERY candidate CS pin — plan (a).
        let mut outcomes: [(CsPin, ProbeOutcome); 2] = [
            (CsPin::Gpio21, ProbeOutcome::NotSelected),
            (CsPin::Gpio3, ProbeOutcome::NotSelected),
        ];
        for slot in 0..CsPin::CANDIDATES.len() {
            let cs = CsPin::CANDIDATES[slot];
            outcomes[slot].1 = probe_pin(spi, cs)?;
        }

        let probe = aggregate_probe(&outcomes);
        for (cs, outcome) in &outcomes {
            println!("storage: probe CS={} verdict={}", cs.label(), outcome.tag());
        }
        println!("storage: CardProbe={} state={}", probe, probe.state_label());

        let cs = match probe {
            CardProbe::Detected(cs) => cs,
            CardProbe::Absent => {
                println!(
                    "storage: ERROR: CardProbe=Absent state=absent — MISO all-ones on every \
                     candidate pin; continuing WITHOUT storage (not a mount failure)"
                );
                return Err(StorageError::CardAbsent);
            }
        };
        let mut cs_pin = Some(pins.take(cs));

        // Mount exactly once, on the detected pin — plan (c).
        // With optional format-on-mount retry if enabled.
        println!(
            "storage: mount {} on CS={} via SdCardDriver::new_spi + Fatfs::new_sdcard + MountedFatfs",
            BASE_PATH,
            cs.label()
        );

        let mut sd_cfg = SdCardConfiguration::new();
        // Plan (e): START at 40 MHz and measure; fall back only after.
        sd_cfg.speed_khz = CLOCK_LADDER_KHZ[0];

        // ESP-IDF 5.5.2 `sdmmc_card_init` always sends CMD59 (`CRC_ON_OFF`)
        // in SPI mode (`sdmmc_init.c` `SDMMC_INIT_STEP(is_spi,
        // sdmmc_init_spi_crc)`) and treats R1 `ILLEGAL_COMMAND` as fatal
        // (`sdmmc_sd.c:606`, `sdspi_transaction.c:50-53`). Cheap cards that
        // hard-wire CRC reject CMD59, and a stale/late response to the
        // preceding CMD5 (SDIO probe, unconditional in 5.5.2) can make even a
        // conforming card see `ILLEGAL_COMMAND`. Both are transient/benign:
        // CRC16 is optional in SPI mode, and upstream ESP-IDF fixes this by
        // retrying `CRC_ON_OFF` on `ESP_ERR_NOT_SUPPORTED` plus a
        // `SDMMC_HOST_FLAG_SPI_IGNORE_DATA_CRC` opt-out (neither is present in
        // 5.5.2 nor reachable through esp-idf-hal 0.47.0). The only lever here
        // is to run the whole init again after a short delay — the same
        // "second attempt succeeds" path users report.
        const CARD_INIT_ATTEMPTS: usize = 4;
        let mut last_init_error = None;
        let mut sd = None;
        let mut sd_handle = None;

        for attempt in 1..=CARD_INIT_ATTEMPTS {
            // On a failed attempt `SdCardDriver::new_spi` has already dropped
            // the host — and with it the owned CS pin handle — so the retry
            // re-steals that now-unowned GPIO. `cs_pin` covers attempt 1.
            let pin = cs_pin
                .take()
                .unwrap_or_else(|| ffi::steal_output_pin(cs.number()));

            let host = match SdSpiHostDriver::new(
                spi,
                Some(pin),
                AnyIOPin::none(),
                AnyIOPin::none(),
                AnyIOPin::none(),
                None,
            ) {
                Ok(host) => host,
                Err(source) => {
                    let err = StorageError::CardInit { cs, source };
                    println!("storage: ERROR: {err}");
                    return Err(err);
                }
            };

            let attempt_handle = host.handle();
            match SdCardDriver::new_spi(host, &sd_cfg) {
                Ok(driver) => {
                    if attempt > 1 {
                        println!(
                            "storage: sdmmc_card_init succeeded on attempt {attempt}/{} \
                             (SPI CRC_ON_OFF retry)",
                            CARD_INIT_ATTEMPTS
                        );
                    }
                    sd_handle = Some(attempt_handle);
                    sd = Some(driver);
                    break;
                }
                Err(source) => {
                    let kind = if source.code() == esp_idf_sys::ESP_ERR_NOT_SUPPORTED {
                        " (ESP_ERR_NOT_SUPPORTED: an sdmmc_init_* step rejected a command, \
                         typically CMD59 CRC_ON_OFF)"
                    } else {
                        ""
                    };
                    println!(
                        "storage: sdmmc_card_init attempt {attempt}/{} failed: {source}{kind}",
                        CARD_INIT_ATTEMPTS
                    );
                    last_init_error = Some(source);
                    if attempt < CARD_INIT_ATTEMPTS {
                        std::thread::sleep(core::time::Duration::from_millis(20));
                    }
                }
            }
        }

        let sd = match sd {
            Some(sd) => sd,
            None => {
                let source = last_init_error.expect("CARD_INIT_ATTEMPTS >= 1 ran an attempt");
                let suffix = if source.code() == esp_idf_sys::ESP_ERR_NOT_SUPPORTED {
                    " — ESP-IDF 5.5.2 sdmmc_init_spi_crc (CMD59 CRC_ON_OFF) returned \
                     ESP_ERR_NOT_SUPPORTED on every attempt; the card rejects CRC enable \
                     and this IDF version has no Kconfig/flag to skip it \
                     (fixed upstream by retrying and by SDMMC_HOST_FLAG_SPI_IGNORE_DATA_CRC)"
                } else {
                    ""
                };
                let err = StorageError::CardInit { cs, source };
                println!("storage: ERROR: {err}{suffix}");
                return Err(err);
            }
        };

        // Keep the raw handle so the ladder can retime the live device.
        let handle = sd_handle.expect("successful SdCardDriver::new_spi recorded its handle");

        let card = sd.card();
        println!(
            "storage: sdmmc card: csd_ver={} capacity={} sectors sector_size={} \
             ocr={:#010x} sdhc_ccs={} max_freq={}kHz real_freq={}kHz cmd8_err={:#x}",
            card.csd.csd_ver,
            card.csd.capacity,
            card.csd.sector_size,
            card.ocr,
            card.ocr & esp_idf_sys::SD_OCR_SDHC_CAP != 0,
            card.max_freq_khz,
            card.real_freq_khz,
            ffi::cmd8_last_error()
        );
        let filesystem = detect_filesystem(card);

        let fatfs =
            Fatfs::new_sdcard(DRIVE, sd).map_err(|source| StorageError::FormatOrVolume {
                cs,
                stage: "ff_diskio_register",
                detail: source.to_string(),
            })?;

        let chain = MountedFatfs::mount(fatfs, BASE_PATH, MAX_OPEN_FILES).map_err(|source| {
            let e = StorageError::FormatOrVolume {
                cs,
                stage: "MountedFatfs::mount",
                detail: source.to_string(),
            };
            println!("storage: ERROR: {e}");
            e
        })?;

        // `MountedFatfs::mount` issues `f_mount(.., opt=0)` (deferred), so a
        // non-FAT volume only fails on first access. Force that access now so
        // the format error is reported AT mount time, exactly as plan (c)
        // requires, and never mistaken for an absent card.
        if let Err(e) = std::fs::read_dir(BASE_PATH) {
            let err = StorageError::FormatOrVolume {
                cs,
                stage: "volume check (read_dir)",
                detail: e.to_string(),
            };
            println!("storage: ERROR: {err}");

            if e.raw_os_error() == Some(19) {
                println!(
                    "storage: volume ENODEV — sdmmc_card_init succeeded but no mountable \
                     FAT32 volume was found (filesystem probe: {filesystem}). The card is \
                     readable at the sector level; storage is FAT32-only, so the card must \
                     be reformatted as FAT32 before it is inserted."
                );
            }

            return Err(err);
        } else {
            println!(
                "storage: mount ok — card present on {}, volume is mounted (FAT)",
                cs.label()
            );
        }

        // (e) Clock ladder: 40 → 20 → 10, measuring at every rung.
        let mut ladder = ClockLadder::new();
        let mut attempted: Vec<u32> = Vec::new();
        let negotiated_khz;
        let write_bytes_per_sec;
        loop {
            let requested = ladder.current_khz();
            if attempted.is_empty() {
                // First rung was applied by `sdmmc_card_init`.
                println!(
                    "storage: clock rung requested={} kHz (applied by sdmmc_card_init)",
                    requested
                );
            } else {
                ffi::set_card_clk(handle, requested)
                    .map_err(|source| StorageError::ClockControl { source })?;
                println!(
                    "storage: clock rung requested={} kHz (sdspi_host_set_card_clk)",
                    requested
                );
            }
            let real = ffi::real_freq_khz(handle)
                .map_err(|source| StorageError::ClockControl { source })?;
            println!(
                "storage: clock measured requested={} kHz real={} kHz (sdspi_host_get_real_freq)",
                requested, real
            );
            attempted.push(requested);

            let trial = run_acceptance_battery(real)?;
            println!(
                "storage: trial @{} kHz: 4 MiB write = {} B/s (floor {} B/s) unstable={} ({})",
                real, trial.bytes_per_sec, MIN_WRITE_BYTES_PER_SEC, trial.unstable, trial.reason
            );

            match ladder.record(trial.bytes_per_sec, trial.unstable) {
                ClockVerdict::Accept { khz } => {
                    negotiated_khz = khz;
                    write_bytes_per_sec = trial.bytes_per_sec;
                    println!(
                        "storage: PASS at {} kHz — negotiated clock = {} kHz, write = {} B/s",
                        khz, khz, write_bytes_per_sec
                    );
                    break;
                }
                ClockVerdict::FallBack {
                    next_khz: Some(next),
                } => {
                    println!(
                        "storage: clock fallback {} -> {} kHz ({})",
                        requested, next, trial.reason
                    );
                }
                ClockVerdict::FallBack { next_khz: None } => {
                    let reason = format!("{} ({})", trial.reason, trial.bytes_per_sec);
                    println!(
                        "storage: ERROR: clock ladder exhausted at {:?} kHz — {}",
                        attempted, reason
                    );
                    return Err(StorageError::LadderExhausted {
                        tried_khz: attempted,
                        last_reason: reason,
                    });
                }
            }
        }

        let (total_bytes, free_bytes) =
            ffi::fat_info(core::ffi::CStr::from_bytes_with_nul(b"/sdcard\0").unwrap())
                .map_err(|source| StorageError::FreeSpaceQuery { source })?;
        println!(
            "storage: volume total={} B free={} B negotiated_clock={} kHz cs={}",
            total_bytes,
            free_bytes,
            negotiated_khz,
            cs.label()
        );

        Ok(Storage {
            chain,
            probe,
            cs,
            negotiated_khz,
            write_bytes_per_sec,
            total_bytes,
            free_bytes,
        })
    }

    /// The aggregate probe verdict this mount was built from.
    pub fn probe(&self) -> CardProbe {
        self.probe
    }

    /// The CS variant the card was found on.
    pub fn cs(&self) -> CsPin {
        self.cs
    }

    /// The **negotiated** SPI clock in kHz (read back from the hardware —
    /// never the requested value).
    pub fn negotiated_khz(&self) -> u32 {
        self.negotiated_khz
    }

    /// Measured 4 MiB sequential write rate at [`Self::negotiated_khz`].
    pub fn write_bytes_per_sec(&self) -> u64 {
        self.write_bytes_per_sec
    }

    /// `base_path` for `std::fs` (`/sdcard`).
    pub fn base_path(&self) -> &'static str {
        BASE_PATH
    }

    /// Volume capacity reported by `esp_vfs_fat_info`.
    pub fn total_bytes(&self) -> u64 {
        self.total_bytes
    }

    /// Free space reported by `esp_vfs_fat_info`.
    pub fn free_bytes(&self) -> u64 {
        self.free_bytes
    }

    /// Free space in bytes, queried live (`getfree`).
    pub fn free_bytes_now(&self) -> Result<u64, StorageError> {
        let (_total, free) = ffi::fat_info(
            core::ffi::CStr::from_bytes_with_nul(b"/sdcard\0").expect("literal is NUL-terminated"),
        )
        .map_err(|source| StorageError::FreeSpaceQuery { source })?;
        Ok(free)
    }

    /// Write `data` to `path` (relative to [`BASE_PATH`]) using the module's
    /// 32 KiB transfer buffer.
    pub fn write_file(&self, rel: &str, data: &[u8]) -> Result<(), StorageError> {
        use std::io::Write;

        let path = format!("{}/{}", BASE_PATH, rel);
        let mut file = std::fs::File::create(&path).map_err(|source| StorageError::Io {
            path: path.clone(),
            source,
        })?;
        let mut buf = vec![0u8; TRANSFER_BUF_BYTES];
        let mut off = 0usize;
        while off < data.len() {
            let n = core::cmp::min(TRANSFER_BUF_BYTES, data.len() - off);
            buf[..n].copy_from_slice(&data[off..off + n]);
            file.write_all(&buf[..n])
                .map_err(|source| StorageError::Io {
                    path: path.clone(),
                    source,
                })?;
            off += n;
        }
        Ok(())
    }

    /// Read a whole file back (used by the round-trip check).
    pub fn read_file(&self, rel: &str) -> Result<Vec<u8>, StorageError> {
        use std::io::Read;

        let path = format!("{}/{}", BASE_PATH, rel);
        let mut file = std::fs::File::open(&path).map_err(|source| StorageError::Io {
            path: path.clone(),
            source,
        })?;
        let mut out = Vec::new();
        file.read_to_end(&mut out)
            .map_err(|source| StorageError::Io {
                path: path.clone(),
                source,
            })?;
        Ok(out)
    }

    /// Remove a test artefact; a missing file is not an error.
    pub fn remove(&self, rel: &str) {
        let path = format!("{}/{}", BASE_PATH, rel);
        let _ = std::fs::remove_file(&path);
    }
}
/// Deterministic 32 KiB pattern: non-constant so an offset bug cannot pass,
/// and identical on every chunk so a read-back compare needs no bookkeeping.
fn pattern_chunk() -> Vec<u8> {
    (0..TRANSFER_BUF_BYTES).map(|i| (i % 251) as u8).collect()
}

/// Write `total` bytes of `pattern`-repetition to `path`, timed.
/// Returns the elapsed microseconds (`i64`, the `esp_timer_get_time` unit).
fn write_repeated(path: &str, pattern: &[u8], total: u64) -> Result<i64, StorageError> {
    use std::io::Write;

    let mut file = std::fs::File::create(path).map_err(|source| StorageError::Io {
        path: path.to_string(),
        source,
    })?;
    let t0 = ffi::now_us();
    let mut written: u64 = 0;
    while written < total {
        let n = core::cmp::min(pattern.len() as u64, total - written) as usize;
        file.write_all(&pattern[..n])
            .map_err(|source| StorageError::Io {
                path: path.to_string(),
                source,
            })?;
        written += n as u64;
    }
    file.sync_all().map_err(|source| StorageError::Io {
        path: path.to_string(),
        source,
    })?;
    Ok(ffi::now_us() - t0)
}

/// Verify `[0, total)` of `path` equals `pattern`-repetition, chunked.
fn verify_repeated(path: &str, pattern: &[u8], total: u64) -> Result<(), StorageError> {
    use std::io::Read;

    let mut file = std::fs::File::open(path).map_err(|source| StorageError::Io {
        path: path.to_string(),
        source,
    })?;
    let mut buf = vec![0u8; pattern.len()];
    let mut offset: u64 = 0;
    while offset < total {
        let want = core::cmp::min(pattern.len() as u64, total - offset) as usize;
        let mut filled = 0usize;
        while filled < want {
            let n = file
                .read(&mut buf[filled..want])
                .map_err(|source| StorageError::Io {
                    path: path.to_string(),
                    source,
                })?;
            if n == 0 {
                return Err(StorageError::Io {
                    path: path.to_string(),
                    source: std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "file shorter than written",
                    ),
                });
            }
            filled += n;
        }
        if buf[..want] != pattern[..want] {
            return Err(StorageError::DataMismatch {
                path: path.to_string(),
                offset,
            });
        }
        offset += want as u64;
    }
    Ok(())
}

/// Read back exactly the **final 4 KiB** and compare it against the pattern
/// (acceptance: "reading back the final 4 KiB of the file returns the bytes
/// written").
fn verify_tail(path: &str, pattern: &[u8], total: u64) -> Result<(), StorageError> {
    use std::io::{Read, Seek, SeekFrom};

    if total < TAIL_READ_BYTES {
        return Err(StorageError::Io {
            path: path.to_string(),
            source: std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "tail read larger than file",
            ),
        });
    }
    let mut file = std::fs::File::open(path).map_err(|source| StorageError::Io {
        path: path.to_string(),
        source,
    })?;
    file.seek(SeekFrom::Start(total - TAIL_READ_BYTES))
        .map_err(|source| StorageError::Io {
            path: path.to_string(),
            source,
        })?;

    let mut buf = vec![0u8; TAIL_READ_BYTES as usize];
    file.read_exact(&mut buf)
        .map_err(|source| StorageError::Io {
            path: path.to_string(),
            source,
        })?;

    // `total` is a whole number of `pattern` chunks, so the tail is the last
    // `TAIL_READ_BYTES` of the pattern itself.
    let tail = &pattern[pattern.len() - TAIL_READ_BYTES as usize..];
    if buf[..] != tail[..] {
        return Err(StorageError::DataMismatch {
            path: path.to_string(),
            offset: total - TAIL_READ_BYTES,
        });
    }
    Ok(())
}

/// The full acceptance battery, run once per clock rung:
///
/// 1. 1 MiB write/read round-trip, byte-identical;
/// 2. `getfree` **before**;
/// 3. timed 4 MiB sequential write (throughput);
/// 4. `getfree` **after** — the delta is reported but not gated (see below);
/// 5. read-back of the **final 4 KiB**.
///
/// Any I/O error or mismatch marks the rung *unstable* (CRC-class failure at
/// this clock) rather than failing the whole bring-up, so the ladder can fall
/// back exactly as plan (e) prescribes. Free-space accounting is reported for
/// diagnostics only; it is not a pass/fail signal.
fn run_acceptance_battery(_real_khz: u32) -> Result<Trial, StorageError> {
    let pattern = pattern_chunk();
    let roundtrip = format!("{}/_t7_roundtrip.bin", BASE_PATH);
    let bulk = format!("{}/_t7_bulk.bin", BASE_PATH);

    // --- 1 MiB byte-identical round-trip -----------------------------------
    let mut unstable = false;
    let mut reason: &'static str = "ok";
    let roundtrip_res = (|| -> Result<(), StorageError> {
        write_repeated(&roundtrip, &pattern, ROUNDTRIP_BYTES)?;
        verify_repeated(&roundtrip, &pattern, ROUNDTRIP_BYTES)?;
        Ok(())
    })();
    if let Err(e) = roundtrip_res {
        println!("storage: round-trip 1 MiB FAILED: {e}");
        unstable = true;
        reason = "1 MiB round-trip failed";
    } else {
        println!(
            "storage: round-trip 1 MiB byte-identical OK (buffer {} B)",
            TRANSFER_BUF_BYTES
        );
    }
    let _ = std::fs::remove_file(&roundtrip);

    // --- getfree before -----------------------------------------------------
    let (before, _total_before) =
        ffi::fat_info(core::ffi::CStr::from_bytes_with_nul(b"/sdcard\0").expect("literal"))
            .map_err(|source| StorageError::FreeSpaceQuery { source })?;

    // --- timed 4 MiB sequential write --------------------------------------
    let (bytes_per_sec, write_res) = match write_repeated(&bulk, &pattern, THROUGHPUT_BYTES) {
        Ok(elapsed_us) => (
            throughput_bytes_per_sec(THROUGHPUT_BYTES, elapsed_us),
            Ok(()),
        ),
        Err(e) => {
            println!("storage: 4 MiB write FAILED: {e}");
            (0u64, Err(e))
        }
    };

    // --- getfree after ------------------------------------------------------
    let (after, _total_after) =
        ffi::fat_info(core::ffi::CStr::from_bytes_with_nul(b"/sdcard\0").expect("literal"))
            .map_err(|source| StorageError::FreeSpaceQuery { source })?;
    let delta = before.saturating_sub(after);
    println!(
        "storage: getfree before={} B after={} B delta={} B (expected >= {} B)",
        before, after, delta, THROUGHPUT_BYTES
    );
    if delta < THROUGHPUT_BYTES && write_res.is_ok() && !unstable {
        // FatFs does not always update the free-cluster count synchronously, so
        // this delta is advisory; the 1 MiB byte-identical round-trip and the
        // 4 KiB tail read-back stay authoritative.
        println!(
            "storage: WARNING: getfree delta {} B does not account for {} B written",
            delta, THROUGHPUT_BYTES
        );
    }

    // --- read back the final 4 KiB -----------------------------------------
    if write_res.is_ok() {
        match verify_tail(&bulk, &pattern, THROUGHPUT_BYTES) {
            Ok(()) => println!(
                "storage: final {} B read-back byte-identical OK (file {} B)",
                TAIL_READ_BYTES, THROUGHPUT_BYTES
            ),
            Err(e) => {
                println!("storage: final {} B read-back FAILED: {e}", TAIL_READ_BYTES);
                unstable = true;
                reason = "final 4 KiB read-back mismatch";
            }
        }
    }

    if let Err(e) = &write_res {
        unstable = true;
        if reason == "ok" {
            reason = "4 MiB write failed";
        }
        let _ = e;
    }

    let _ = std::fs::remove_file(&bulk);

    Ok(Trial {
        bytes_per_sec,
        unstable,
        reason,
    })
}

// ---------------------------------------------------------------------------
// Unit tests — pure logic only; `#[cfg(test)]`, compile-checked via
// `cargo test --no-run --target xtensa-esp32s3-espidf`.
// Running them requires a board (RUN_PENDING_NO_BOARD).
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // --- command framing ----------------------------------------------------

    #[test]
    fn encodes_cmd0_with_the_real_crc() {
        // CMD0 is one of the two frames whose CRC the card checks in SPI mode.
        assert_eq!(
            encode_command(0, 0x0000_0000, 0x95),
            [0x40, 0x00, 0x00, 0x00, 0x00, 0x95]
        );
    }

    #[test]
    fn encodes_cmd8_with_arg_0x1aa_and_its_crc() {
        assert_eq!(
            encode_command(8, 0x0000_01AA, 0x87),
            [0x48, 0x00, 0x00, 0x01, 0xAA, 0x87]
        );
    }

    #[test]
    fn masks_command_index_into_six_bits() {
        // Bit 6 (start) is forced on, bit 5 (transmission) stays clear.
        assert_eq!(encode_command(0xFF, 0, 0x01)[0] & 0xC0, 0x40);
        assert_eq!(encode_command(41, 0, 0x01)[0], 0x40 | 41);
    }

    // --- all-ones / R1 location --------------------------------------------

    #[test]
    fn all_ones_is_the_not_selected_signal() {
        // Plan (b): MISO pulled high while deselected ⇒ all-ones ⇒ not selected.
        assert!(is_all_ones(&[0xFF; 16]));
        assert!(is_all_ones(&[0xFF]));
        assert!(!is_all_ones(&[0xFF, 0x01]));
        assert!(!is_all_ones(&[0x01, 0xFF]));
        assert!(!is_all_ones(&[]), "empty window proves nothing");
    }

    #[test]
    fn finds_r1_after_the_ncr_filler() {
        // 3 × 0xFF NCR bytes, then R1 = 0x01 (idle), then R7 payload.
        let mut win = [0xFFu8; 12];
        win[3] = 0x01;
        let (idx, r1) = find_r1(&win).expect("R1 must be found");
        assert_eq!((idx, r1), (3, 0x01));
    }

    #[test]
    fn no_r1_when_the_card_never_answers() {
        // All-ones window: the card is not selected, so there is no R1.
        assert_eq!(find_r1(&[0xFFu8; 32]), None);
    }

    #[test]
    fn r1_with_msb_clear_is_never_confused_with_filler() {
        // 0x00 and 0x7F are valid R1 values (0x7F = every error flag set).
        assert_eq!(find_r1(&[0xFF, 0x00]), Some((1, 0x00)));
        assert_eq!(find_r1(&[0xFF, 0x7F]), Some((1, 0x7F)));
    }

    // --- CMD8 classification (plan (b)) -------------------------------------

    #[test]
    fn cmd8_idle_plus_voltage_plus_echo_classifies_sdv2() {
        let r = R7 {
            r1: 0x01,
            payload: [0x00, 0x00, 0x01, 0xAA],
        };
        assert_eq!(classify_cmd8(&r), Some(CardClass::Sdv2));
    }

    #[test]
    fn cmd8_illegal_command_falls_through_to_legacy() {
        // 0x04 = ILLEGAL_COMMAND (0x01 idle | 0x04 illegal = 0x05) on a
        // pre-SDv2 card. The plan's whole point: this is a *card*, not absence.
        let r = R7 {
            r1: 0x05,
            payload: [0xFF; 4],
        };
        assert_eq!(classify_cmd8(&r), Some(CardClass::Legacy));
    }

    #[test]
    fn cmd8_garbage_identifies_nothing() {
        // Not illegal, but not a valid SDv2 R7 either.
        let r = R7 {
            r1: 0x00,
            payload: [0x00, 0x00, 0x01, 0xAA],
        };
        assert_eq!(classify_cmd8(&r), None);
        // Right R1 but wrong echo pattern.
        let r = R7 {
            r1: 0x01,
            payload: [0x00, 0x00, 0x01, 0x55],
        };
        assert_eq!(classify_cmd8(&r), None);
        // Right echo but an error flag set in R1.
        let r = R7 {
            r1: 0x09,
            payload: [0x00, 0x00, 0x01, 0xAA],
        };
        assert_eq!(classify_cmd8(&r), None);
    }

    #[test]
    fn r1_alone_is_never_a_presence_signal() {
        // The trap the plan reviewer proved wrong: `0x04` on a genuine R1 is
        // ILLEGAL_COMMAND, not "card present and selected". A bare R1 must
        // never classify anything — only CMD8's full R7 (or the ACMD41/CMD58
        // fallback) may.
        let only_r1 = R7 {
            r1: 0x04,
            payload: [0xFF, 0xFF, 0xFF, 0xFF],
        };
        // 0x04 has ILLEGAL set, so it routes to Legacy — but only because the
        // legacy fallback is *verified* with ACMD41/CMD58 afterwards; a bare
        // R1 never produces Sdv2.
        assert_ne!(classify_cmd8(&only_r1), Some(CardClass::Sdv2));
    }

    // --- legacy fallback -----------------------------------------------------

    #[test]
    fn legacy_identification_needs_both_answers() {
        assert!(legacy_identifies(0x01, 0x00), "idle ACMD41 + clean CMD58");
        assert!(legacy_identifies(0x00, 0x00));
        // CMD58 reporting ILLEGAL_COMMAND ⇒ not a usable legacy identification.
        assert!(!legacy_identifies(0x01, 0x04));
        // No answer at all (MSB set) ⇒ not selected / unresponsive.
        assert!(!legacy_identifies(0xFF, 0x00));
        assert!(!legacy_identifies(0x01, 0xFF));
    }

    // --- aggregate probe (plan (a)) -----------------------------------------

    #[test]
    fn detects_gpio21_when_primary_answers() {
        let results = [
            (
                CsPin::Gpio21,
                ProbeOutcome::Detected {
                    class: CardClass::Sdv2,
                },
            ),
            (CsPin::Gpio3, ProbeOutcome::NotSelected),
        ];
        assert_eq!(
            aggregate_probe(&results),
            CardProbe::Detected(CsPin::Gpio21)
        );
    }

    #[test]
    fn detects_gpio3_when_only_the_alternate_answers() {
        // The case a single-pin probe cannot distinguish from no card at all.
        let results = [
            (CsPin::Gpio21, ProbeOutcome::NotSelected),
            (
                CsPin::Gpio3,
                ProbeOutcome::Detected {
                    class: CardClass::Sdv2,
                },
            ),
        ];
        assert_eq!(aggregate_probe(&results), CardProbe::Detected(CsPin::Gpio3));
    }

    #[test]
    fn absent_only_when_every_candidate_is_silent() {
        for second in [ProbeOutcome::NotSelected, ProbeOutcome::Unidentified] {
            let results = [
                (CsPin::Gpio21, ProbeOutcome::NotSelected),
                (CsPin::Gpio3, second),
            ];
            assert_eq!(aggregate_probe(&results), CardProbe::Absent);
        }
        let empty: [(CsPin, ProbeOutcome); 0] = [];
        assert_eq!(aggregate_probe(&empty), CardProbe::Absent);
    }

    #[test]
    fn probe_state_labels_match_the_acceptance_words() {
        assert_eq!(
            CardProbe::Detected(CsPin::Gpio21).state_label(),
            "card-present"
        );
        assert_eq!(CardProbe::Absent.state_label(), "absent");
        assert_eq!(ProbeOutcome::NotSelected.tag(), "not-selected");
        assert_eq!(
            ProbeOutcome::Detected {
                class: CardClass::Sdv2
            }
            .tag(),
            "detected"
        );
    }

    #[test]
    fn candidate_order_is_gpio21_first() {
        // GPIO21 is the shipped path; the sweep must try it first so a card
        // present on both reports Detected(GPIO21).
        assert_eq!(CsPin::CANDIDATES, [CsPin::Gpio21, CsPin::Gpio3]);
        assert_eq!(CsPin::Gpio21.number(), 21);
        assert_eq!(CsPin::Gpio3.number(), 3);
        assert_eq!(CsPin::Gpio21.label(), "GPIO21");
        assert_eq!(CsPin::Gpio3.label(), "GPIO3");
        assert_eq!(CsPin::Gpio21.to_string(), "GPIO21");
    }

    // --- clock ladder (plan (e)) --------------------------------------------

    #[test]
    fn ladder_starts_at_40mhz() {
        let l = ClockLadder::new();
        assert_eq!(l.current_khz(), 40_000);
        assert_eq!(CLOCK_LADDER_KHZ, [40_000, 20_000, 10_000]);
        assert!(!l.exhausted());
    }

    #[test]
    fn fast_and_clean_run_accepts_the_first_rung() {
        let mut l = ClockLadder::new();
        // 40 MHz gives ~2 MB/s and no errors ⇒ accept, no fallback.
        assert_eq!(
            l.record(2_000_000, false),
            ClockVerdict::Accept { khz: 40_000 }
        );
        assert_eq!(l.current_khz(), 40_000, "accept must not advance the rung");
    }

    #[test]
    fn sub_floor_throughput_falls_back_40_20_10_then_exhausts() {
        let mut l = ClockLadder::new();
        // Plan (e): below the floor ⇒ retry at 20, then 10.
        assert_eq!(
            l.record(MIN_WRITE_BYTES_PER_SEC - 1, false),
            ClockVerdict::FallBack {
                next_khz: Some(20_000)
            }
        );
        assert_eq!(l.current_khz(), 20_000);
        assert_eq!(
            l.record(0, false),
            ClockVerdict::FallBack {
                next_khz: Some(10_000)
            }
        );
        assert_eq!(l.current_khz(), 10_000);
        assert_eq!(
            l.record(0, false),
            ClockVerdict::FallBack { next_khz: None }
        );
        assert!(l.exhausted());
    }

    #[test]
    fn instability_forces_fallback_even_at_high_throughput() {
        // A CRC-class failure at 40 MHz can still show a high *attempted*
        // rate; instability alone must drop the clock.
        let mut l = ClockLadder::new();
        assert_eq!(
            l.record(5_000_000, true),
            ClockVerdict::FallBack {
                next_khz: Some(20_000)
            }
        );
    }

    #[test]
    fn exactly_at_the_floor_accepts() {
        let mut l = ClockLadder::new();
        assert_eq!(
            l.record(MIN_WRITE_BYTES_PER_SEC, false),
            ClockVerdict::Accept { khz: 40_000 }
        );
    }

    #[test]
    fn a_later_rung_can_be_the_one_that_passes() {
        // "record the clock that actually passed".
        let mut l = ClockLadder::new();
        l.record(200_000, false);
        assert_eq!(
            l.record(400_000, false),
            ClockVerdict::Accept { khz: 20_000 }
        );
    }

    // --- throughput arithmetic ----------------------------------------------

    #[test]
    fn throughput_conversion_is_bytes_per_second() {
        assert_eq!(throughput_bytes_per_sec(4_194_304, 2_000_000), 2_097_152);
        assert_eq!(throughput_bytes_per_sec(1_000_000, 1_000_000), 1_000_000);
        // Degenerate timing must not divide by zero or panic.
        assert_eq!(throughput_bytes_per_sec(4_194_304, 0), 0);
        assert_eq!(throughput_bytes_per_sec(4_194_304, -5), 0);
    }

    #[test]
    fn acceptance_sizes_and_floor_match_the_plan() {
        assert_eq!(ROUNDTRIP_BYTES, 1_048_576, "1 MiB");
        assert_eq!(THROUGHPUT_BYTES, 4_194_304, "4 MiB");
        assert_eq!(TAIL_READ_BYTES, 4_096, "final 4 KiB");
        assert_eq!(TRANSFER_BUF_BYTES, 32_768, "32 KiB transfer buffer");
        assert_eq!(MIN_WRITE_BYTES_PER_SEC, 262_144, "256 KiB/s floor");
        assert_eq!(BASE_PATH, "/sdcard");
    }

    #[test]
    fn pattern_chunk_is_32k_and_not_constant() {
        let p = pattern_chunk();
        assert_eq!(p.len(), TRANSFER_BUF_BYTES);
        assert!(
            p.iter().any(|&b| b != p[0]),
            "constant pattern proves nothing"
        );
        // Deterministic: two calls agree (read-back compares against it).
        assert_eq!(p, pattern_chunk());
    }
}
