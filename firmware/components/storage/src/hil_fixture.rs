//! Device HIL fixture: one FAT32 format and one mount per test binary.
//!
//! ## Contract
//!
//! ```text
//! pub fn card() -> &'static Storage<'static>
//! ```
//!
//! - The **first** call in a process performs the whole hardware bring-up
//!   exactly once, serialised by [`ONCE`] plus [`LOCK`]:
//!   1. logs `fixture: format-start`;
//!   2. probes the candidate chip-select pins with the same read-only
//!      `CMD0`/`CMD8` sequence [`Storage::bringup`] uses, so the format lands
//!      on the pin the card actually answers on;
//!   3. formats that card FAT32 with `Fatfs::format`
//!      ([`FormatConfiguration`] `fs_type = Fat32`);
//!   4. mounts it **once** through [`Storage::bringup`], which runs the 1 MiB
//!      round-trip and the 4 MiB timed write battery exactly once — that is
//!      the HIL value of this fixture;
//!   5. leaks the `SpiDriver` and the [`Storage`] with `Box::leak` and returns
//!      the `&'static Storage`.
//! - Every **later** call logs `fixture: reused` and returns the same pointer;
//!   it never re-mounts and never re-formats.
//! - While steps 2-4 run, a helper thread logs a line containing `HEARTBEAT`
//!   every 5 s. `scripts/run-device-tests.sh` kills a binary after 60 s of
//!   serial silence and only refreshes that timer on `test ...` lines or lines
//!   containing `HEARTBEAT`; formatting a large card takes minutes, so those
//!   lines are load-bearing, not decoration.
//! - Failure is a panic carrying the typed [`StorageError`]: the fixture never
//!   returns a partly initialised mount, and a failed format is never retried
//!   behind the caller's back.
//! - The volume is destroyed by the format. This module is compiled on every
//!   build (a `#[cfg(test)]` module is invisible to another crate's test
//!   binary) but **nothing in the firmware boot path may call it**.
//!
//! ## Why the format is not part of `Storage::bringup`
//!
//! A normal boot must never reformat the card. `bringup` stays a mount, and
//! this fixture is the only place that writes a filesystem.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Once};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use aligned::{Aligned, A64};
use esp_idf_hal::gpio::{AnyIOPin, AnyOutputPin, Gpio21, Gpio3};
use esp_idf_hal::peripherals::Peripherals;
use esp_idf_hal::sd::config::Configuration as SdCardConfiguration;
use esp_idf_hal::sd::spi::SdSpiHostDriver;
use esp_idf_hal::sd::SdCardDriver;
use esp_idf_hal::spi::SpiDriver;
use esp_idf_svc::fs::fatfs::config::{FatFsType, FormatConfiguration};
use esp_idf_svc::fs::fatfs::Fatfs;

use crate::storage::{
    self, CandidatePins, CsPin, ProbeOutcome, Storage, StorageError, CLOCK_LADDER_KHZ, DRIVE,
};

/// Work buffer handed to the FAT formatter. 4 KiB is the size the previous
/// in-tree format call used and the size of one FAT cluster transfer here.
const FORMAT_BUF_BYTES: usize = 4096;

/// FAT32 cluster size. The card is ~240 GiB: with the 4 KiB default the FAT
/// alone is ~251 MiB, which at the measured ~435 kB/s would spend close to the
/// runner's entire 600 s budget formatting. 32 KiB is the conventional FAT32
/// cluster size for a volume this large and shrinks the FAT to ~31 MiB.
const FORMAT_CLUSTER_BYTES: u32 = 32 * 1024;

/// Same retry budget as [`Storage::bringup`]: cheap cards reject CMD59
/// (`CRC_ON_OFF`) transiently while the SPI host is still settling.
const CARD_INIT_ATTEMPTS: usize = 4;

/// The bring-up runs on its own thread, mirroring `main.rs`'s 64 KiB runtime
/// stack: libtest threads get the 32 KiB `CONFIG_PTHREAD_TASK_STACK_SIZE_DEFAULT`,
/// and the format buffer plus the mount call chain do not fit in that safely.
const INIT_STACK_BYTES: usize = 64 * 1024;

/// Log a heartbeat this often (the watchdog window is 60 s; 5 s leaves margin).
const HEARTBEAT_PERIOD: Duration = Duration::from_secs(5);

/// Heartbeat thread wake-up, so `Drop` joins in at most one tick.
const HEARTBEAT_TICK: Duration = Duration::from_secs(1);

/// One-time bring-up latch. `Once` is what makes "format exactly once per
/// process" true even under libtest's parallel test threads.
static ONCE: Once = Once::new();

/// The throughput sample taken inside the one-time bring-up. The test body
/// only asserts on it: the existing second-bring-up probe drops the leaked
/// mount's FATFS drive, so I/O after that probe cannot succeed.
#[cfg(test)]
static SPEED_SAMPLE: Mutex<Option<Result<speed::SpeedSample, (String, u64)>>> = Mutex::new(None);

/// The leaked handles. The mutex is the `LOCK` the contract names: it
/// publishes the fixture between threads and keeps readers out while the first
/// call is still initialising.
static LOCK: Mutex<()> = Mutex::new(());

// `Storage` wraps C handles (`*mut FATFS`, `sdmmc_card_t`) and is neither
// `Send` nor `Sync`, so it cannot travel through a `Mutex` or a thread's
// return type. The bring-up thread writes these instead, the starting thread
// reads them after `join()`, and every access holds `LOCK`. Both values are
// leaked, so they outlive the thread that built them.
static mut STORAGE: Option<&'static Storage<'static>> = None;
static mut BUS: Option<&'static SpiDriver<'static>> = None;

fn lock() -> MutexGuard<'static, ()> {
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Return the single FAT32-formatted, mounted SD card of this process.
///
/// See the module contract for the exact one-time behaviour. Panics if the
/// card cannot be formatted or mounted, and on every later call returns the
/// same `&'static Storage` (logging `fixture: reused`).
pub fn card() -> &'static Storage<'static> {
    let mut first_call = false;
    ONCE.call_once(|| {
        first_call = true;
        logging::log!("fixture: format-start");
        let heartbeat = Heartbeat::start("format");
        logging::log!("fixture: HEARTBEAT format-start");

        std::thread::Builder::new()
            .name("fixture-init".into())
            .stack_size(INIT_STACK_BYTES)
            .spawn(publish)
            .expect("fixture: cannot spawn the bring-up thread")
            .join()
            .expect("fixture: card bring-up thread panicked");

        drop(heartbeat);
        logging::log!("fixture: format-complete");
    });

    let storage = {
        let _guard = lock();
        // SAFETY: `ONCE` guarantees `publish` finished before this read, and
        // the guard keeps any other access out.
        unsafe { STORAGE }.expect("fixture: card() initialised the mount before returning")
    };
    if !first_call {
        logging::log!("fixture: reused");
    }
    storage
}

/// Bring-up thread body: format, mount, then publish the leaked handles.
fn publish() {
    match initialise() {
        Ok((storage, bus)) => {
            // Measure before the HIL probe's second bring-up unregisters drive 0.
            #[cfg(test)]
            speed::record();
            let _guard = lock();
            // SAFETY: the only writer, and it holds `LOCK`; both values are
            // `'static` leaks.
            unsafe {
                STORAGE = Some(storage);
                BUS = Some(bus);
            }
        }
        Err(message) => {
            logging::log!("fixture: card bring-up failed: {message}");
            panic!("fixture: card bring-up failed: {message}");
        }
    }
}

/// Format FAT32, then mount. Runs on the dedicated bring-up thread.
fn initialise() -> Result<(&'static Storage<'static>, &'static SpiDriver<'static>), String> {
    let Peripherals {
        pins, spi2, ..
    } = Peripherals::take().map_err(|error| format!("peripherals already taken: {error}"))?;

    let spi: SpiDriver<'static> = storage::new_bus(spi2, pins.gpio7, pins.gpio9, pins.gpio8)
        .map_err(|error| format!("SPI2 bus init failed: {error}"))?;
    let spi: &'static SpiDriver<'static> = Box::leak(Box::new(spi));

    let cs = detect_cs(spi).map_err(|error| error.to_string())?;
    logging::log!(
        "fixture: card answers on {} ({})",
        cs.label(),
        "probe sequence CMD0/CMD8; see storage: probe lines"
    );

    format_fat32(spi, cs).map_err(|error| format!("FAT32 format failed: {error}"))?;

    logging::log!("fixture: HEARTBEAT mount-start");
    let storage = Storage::bringup(spi, CandidatePins::new(pins.gpio21, pins.gpio3))
        .map_err(|error| format!("mount failed after the format: {error}"))?;
    let storage: &'static Storage<'static> = Box::leak(Box::new(storage));

    logging::log!(
        "fixture: mounted {} total={} B free={} B at {} kHz (CS={})",
        storage.base_path(),
        storage.total_bytes(),
        storage.free_bytes(),
        storage.negotiated_khz(),
        storage.cs().label()
    );
    Ok((storage, spi))
}

/// Find the CS pin the card answers on, using the same read-only probe
/// sequence `Storage::bringup` uses.
fn detect_cs(spi: &'static SpiDriver<'static>) -> Result<CsPin, StorageError> {
    for cs in CsPin::CANDIDATES {
        match storage::probe_pin(spi, cs) {
            Ok(ProbeOutcome::Detected { .. }) => return Ok(cs),
            Ok(outcome) => logging::log!("fixture: probe {} -> {}", cs.label(), outcome.tag()),
            Err(error) => logging::log!("fixture: probe {} failed: {error}", cs.label()),
        }
    }
    Err(StorageError::CardAbsent)
}

/// Tick `CMD0` on the detected pin and write a fresh FAT32 volume.
///
/// The `Fatfs`/`SdCardDriver` pair used here is dropped before
/// [`Storage::bringup`] runs: the fixture formats through a temporary disk-io
/// registration, then the mount owns its own chain. `bringup` is the single
/// authority on mounting, so the volume is validated by its `read_dir` check.
fn format_fat32(spi: &'static SpiDriver<'static>, cs: CsPin) -> Result<(), StorageError> {
    let mut sd_config = SdCardConfiguration::new();
    // Same first rung as `bringup`; the SDSPI host still caps at the card's
    // negotiated maximum.
    sd_config.speed_khz = CLOCK_LADDER_KHZ[0];

    let mut last_error = None;
    for attempt in 1..=CARD_INIT_ATTEMPTS {
        let host = match SdSpiHostDriver::new(
            spi,
            Some(cs_output_pin(cs)),
            AnyIOPin::none(),
            AnyIOPin::none(),
            AnyIOPin::none(),
            None,
        ) {
            Ok(host) => host,
            Err(source) => {
                logging::log!(
                    "fixture: format card init attempt {attempt}/{CARD_INIT_ATTEMPTS} failed: {source}"
                );
                last_error = Some(StorageError::CardInit { cs, source });
                std::thread::sleep(Duration::from_millis(20));
                continue;
            }
        };

        let sd = match SdCardDriver::new_spi(host, &sd_config) {
            Ok(sd) => sd,
            Err(source) => {
                logging::log!(
                    "fixture: format sdmmc_card_init attempt {attempt}/{CARD_INIT_ATTEMPTS} \
                     failed: {source}"
                );
                last_error = Some(StorageError::CardInit { cs, source });
                std::thread::sleep(Duration::from_millis(20));
                continue;
            }
        };

        let mut fatfs =
            Fatfs::new_sdcard(DRIVE, sd).map_err(|source| StorageError::FormatOrVolume {
                cs,
                stage: "fixture format: ff_diskio_register",
                detail: source.to_string(),
            })?;

        // The safe wrapper maps Fat32 to `FM_FAT32` and discards the `FRESULT`
        // (`esp-idf-svc` 0.53.0). A failed format is therefore caught by
        // `bringup`'s `read_dir` volume check rather than here.
        let configuration = FormatConfiguration {
            fs_type: FatFsType::Fat32,
            cluster_size: FORMAT_CLUSTER_BYTES,
            ..Default::default()
        };
        let mut work: Aligned<A64, [u8; FORMAT_BUF_BYTES]> = Aligned([0u8; FORMAT_BUF_BYTES]);

        let started = Instant::now();
        logging::log!(
            "fixture: format-running FAT32, {} B clusters, {} B work buffer",
            FORMAT_CLUSTER_BYTES,
            FORMAT_BUF_BYTES
        );
        fatfs
            .format(&configuration, &mut work[..])
            .map_err(|source| StorageError::FormatOrVolume {
                cs,
                stage: "fixture format: Fatfs::format",
                detail: source.to_string(),
            })?;
        logging::log!(
            "fixture: format-done after {} s ({} B clusters)",
            started.elapsed().as_secs(),
            FORMAT_CLUSTER_BYTES
        );
        return Ok(());
    }

    Err(last_error.expect("CARD_INIT_ATTEMPTS >= 1 ran at least one attempt"))
}

/// Owning chip-select handle for the temporary format chain.
///
/// SAFETY: this fixture is the only SD owner in the test binary. The format
/// step runs before `bringup` takes the real `Gpio21`/`Gpio3` handles, and the
/// failure probe in the tests re-drives the pin only through a second
/// `bringup`, using the same aliasing pattern `bringup` itself uses on retry.
fn cs_output_pin(cs: CsPin) -> AnyOutputPin<'static> {
    match cs {
        CsPin::Gpio21 => unsafe { Gpio21::steal() }.into(),
        CsPin::Gpio3 => unsafe { Gpio3::steal() }.into(),
    }
}

/// A second `Storage::bringup` on the already-mounted volume, for the failure
/// probe. Test-only: outside `cfg(test)` nothing may re-drive the card.
#[cfg(test)]
fn second_bringup() -> Result<Storage<'static>, StorageError> {
    let bus = {
        let _guard = lock();
        // SAFETY: `card()` published the bus before the test called this.
        unsafe { BUS }.expect("fixture: card() must run before the failure probe")
    };
    let pins = CandidatePins::new(unsafe { Gpio21::steal() }, unsafe { Gpio3::steal() });
    Storage::bringup(bus, pins)
}

/// Background `HEARTBEAT` logger. Dropping it stops and joins the thread.
struct Heartbeat {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Heartbeat {
    fn start(stage: &'static str) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let thread = std::thread::Builder::new()
            .name("fixture-heartbeat".into())
            .stack_size(16 * 1024)
            .spawn(move || {
                let mut since_beat = Duration::ZERO;
                let mut beat = 0u32;
                while !flag.load(Ordering::Relaxed) {
                    std::thread::sleep(HEARTBEAT_TICK);
                    if flag.load(Ordering::Relaxed) {
                        break;
                    }
                    since_beat += HEARTBEAT_TICK;
                    if since_beat >= HEARTBEAT_PERIOD {
                        since_beat = Duration::ZERO;
                        beat += 1;
                        logging::log!("fixture: HEARTBEAT {stage} beat={beat}");
                    }
                }
            })
            .expect("fixture: cannot spawn the heartbeat thread (idle watchdog would kill the run)");
        Self {
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for Heartbeat {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod speed {
    //! On-device SD write/read throughput measurement.
    //!
    //! Measured once, inside [`super::publish`], on the fixture's single mount.
    //! The test body only asserts on the stored result: the existing HIL
    //! probe's second bring-up unregisters FATFS drive 0 when its temporary
    //! `Fatfs` drops, so a card-touching test that measured on its own could
    //! not run after it under libtest's parallel scheduler.

    use std::io::{Read, Write};

    use crate::storage::{
        ffi, pattern_chunk, throughput_bytes_per_sec, StorageError, BASE_PATH,
        MIN_WRITE_BYTES_PER_SEC, TRANSFER_BUF_BYTES,
    };

    const SPEED_BYTES: usize = 4 * 1024 * 1024;
    const SPEED_FILE: &str = "_sd_speed.bin";

    #[derive(Clone, Copy)]
    pub(super) struct SpeedSample {
        pub(super) write_bytes_per_sec: u64,
        pub(super) read_bytes_per_sec: u64,
        pub(super) bytes: usize,
    }

    /// The first offset that does not match, as `(error, offset)`.
    type Mismatch = (StorageError, u64);

    /// Write then read back [`SPEED_BYTES`] in [`TRANSFER_BUF_BYTES`] chunks
    /// and publish the sample. The write loop is timed through `sync_all`, the
    /// read loop through the last compared chunk; the file is removed on both
    /// the success and failure paths.
    pub(super) fn record() {
        let result = measure().map_err(|(error, offset)| (error.to_string(), offset));
        logging::log!(
            "sd-speed: write_kib_s={} read_kib_s={} bytes={}",
            result.as_ref().map_or(0, |s| s.write_bytes_per_sec / 1024),
            result.as_ref().map_or(0, |s| s.read_bytes_per_sec / 1024),
            SPEED_BYTES
        );
        *super::SPEED_SAMPLE
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(result);
    }

    fn io_error(path: &str, source: std::io::Error, offset: usize) -> Mismatch {
        (
            StorageError::Io {
                path: path.to_string(),
                source,
            },
            offset as u64,
        )
    }

    fn measure() -> Result<SpeedSample, Mismatch> {
        let path = format!("{BASE_PATH}/{SPEED_FILE}");
        let pattern = pattern_chunk();

        let mut file = std::fs::File::create(&path).map_err(|source| io_error(&path, source, 0))?;
        let mut written = 0usize;
        let write_t0 = ffi::now_us();
        while written < SPEED_BYTES {
            if let Err(source) = file.write_all(&pattern) {
                let _ = std::fs::remove_file(&path);
                return Err(io_error(&path, source, written));
            }
            written += pattern.len();
        }
        if let Err(source) = file.sync_all() {
            let _ = std::fs::remove_file(&path);
            return Err(io_error(&path, source, written));
        }
        let write_us = ffi::now_us() - write_t0;
        drop(file);

        let mut file = std::fs::File::open(&path).map_err(|source| io_error(&path, source, 0))?;
        let mut chunk = vec![0u8; TRANSFER_BUF_BYTES];
        let mut offset = 0usize;
        let read_t0 = ffi::now_us();
        while offset < SPEED_BYTES {
            let want = core::cmp::min(TRANSFER_BUF_BYTES, SPEED_BYTES - offset);
            if let Err(source) = file.read_exact(&mut chunk[..want]) {
                let _ = std::fs::remove_file(&path);
                return Err(io_error(&path, source, offset));
            }
            if let Some(index) = chunk[..want]
                .iter()
                .zip(&pattern[..want])
                .position(|(got, expected)| got != expected)
            {
                let bad = offset + index;
                let _ = std::fs::remove_file(&path);
                return Err((
                    StorageError::DataMismatch {
                        path: path.clone(),
                        offset: bad as u64,
                    },
                    bad as u64,
                ));
            }
            offset += want;
        }
        let read_us = ffi::now_us() - read_t0;
        drop(file);
        let _ = std::fs::remove_file(&path);

        Ok(SpeedSample {
            write_bytes_per_sec: throughput_bytes_per_sec(SPEED_BYTES as u64, write_us),
            read_bytes_per_sec: throughput_bytes_per_sec(SPEED_BYTES as u64, read_us),
            bytes: SPEED_BYTES,
        })
    }

    #[test]
    fn hil_sd_write_and_read_throughput() {
        // Joins the one-time mount: no second format, no second mount.
        let _ = super::card();
        let sample = {
            let guard = super::SPEED_SAMPLE
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            match guard.as_ref() {
                Some(Ok(sample)) => *sample,
                Some(Err((error, offset))) => {
                    panic!("SD speed measurement failed at offset {offset}: {error}")
                }
                None => panic!("the one-time bring-up must have recorded the throughput sample"),
            }
        };

        assert_eq!(sample.bytes, SPEED_BYTES);
        assert!(
            sample.write_bytes_per_sec >= MIN_WRITE_BYTES_PER_SEC,
            "SD write {} B/s is below the {} B/s boot-ladder floor",
            sample.write_bytes_per_sec,
            MIN_WRITE_BYTES_PER_SEC
        );
        assert!(
            sample.read_bytes_per_sec >= MIN_WRITE_BYTES_PER_SEC,
            "SD read {} B/s is below the {} B/s floor",
            sample.read_bytes_per_sec,
            MIN_WRITE_BYTES_PER_SEC
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::BASE_PATH;

    const PROBE_FILE: &str = "_fixture_probe.bin";

    /// The storage crate's one card-touching HIL test.
    ///
    /// It is deliberately a single function: the second-bring-up probe resets
    /// the card and unregisters FATFS drive 0 when its temporary `Fatfs`
    /// drops, so every other card access has to happen before it. One test
    /// body makes that order deterministic under libtest's parallel scheduler
    /// without a lock that other tests could forget to take.
    #[test]
    fn formats_and_mounts_once_then_rejects_a_second_bringup() {
        // First call formats + mounts + leaks; the second must reuse it.
        let first = card();
        logging::log!("fixture: HEARTBEAT hil-test");
        let second = card();
        assert!(
            core::ptr::eq(first, second),
            "card() must return the same leaked Storage on every call"
        );
        assert_eq!(first.base_path(), BASE_PATH);
        assert!(first.total_bytes() > 0, "the mounted volume reports no capacity");
        assert!(
            first.negotiated_khz() >= CLOCK_LADDER_KHZ[CLOCK_LADDER_KHZ.len() - 1],
            "negotiated clock {} kHz is below the ladder floor",
            first.negotiated_khz()
        );

        // The fixture's volume is really usable: a 64 KiB pattern must come
        // back byte-identical. This is what the fileserver HIL tests rely on.
        let payload: Vec<u8> = (0..64 * 1024).map(|offset| (offset % 251) as u8).collect();
        first
            .write_file(PROBE_FILE, &payload)
            .expect("writing to the fixture volume");
        let read_back = first.read_file(PROBE_FILE).expect("reading back");
        assert_eq!(
            read_back.as_slice(),
            payload.as_slice(),
            "fixture volume read-back differs from what was written"
        );
        first.remove(PROBE_FILE);

        // Failure path (todo-5 adaptation 2): exFAT is gone, so prove the
        // error path by mounting a second time on the still-mounted volume.
        // `Storage::bringup` must return a typed error, bounded in time, with
        // no hang and no panic.
        let started = Instant::now();
        let error = match second_bringup() {
            Err(error) => error,
            Ok(_) => panic!("a second bringup on /sdcard must fail, but it succeeded"),
        };
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(60),
            "second bringup took {elapsed:?}; it must fail fast, not hang"
        );
        assert!(
            matches!(
                error,
                StorageError::FormatOrVolume { .. } | StorageError::CardInit { .. }
            ),
            "second bringup returned an unexpected error variant: {error:?}"
        );
        logging::log!(
            "fixture: HEARTBEAT second-bringup-rejected after {elapsed:?}: {error}"
        );
    }
}
