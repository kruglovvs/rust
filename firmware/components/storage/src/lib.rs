//! SD-card storage over SPI: chip-select probe, single FAT32 mount, and
//! the hardware-proven `sdspi_host_do_transaction` interposer.
//!
//! `storage` owns the mount flow and re-exports the hardware-free probe
//! classification from `sd_probe`.

/// Device-test fixture: formats the card FAT32 and mounts it once per binary.
/// Compiled on every build so another crate's test binary can use it, but the
/// firmware boot path must never call it.
#[doc(hidden)]
pub mod hil_fixture;

pub mod sd_probe;
pub mod storage;

pub use storage::*;
