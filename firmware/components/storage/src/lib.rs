//! SD-card storage over SPI: chip-select probe, single FAT/exFAT mount, and
//! the hardware-proven `sdspi_host_do_transaction` interposer.
//!
//! `storage` owns the mount flow and re-exports the hardware-free probe
//! classification from `sd_probe`.

pub mod sd_probe;
pub mod storage;

pub use storage::*;

#[cfg(all(test, feature = "sd-format-on-mount"))]
pub(crate) static HIL_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
