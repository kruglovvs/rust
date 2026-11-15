//! SD-card storage over SPI: chip-select probe, single FAT32 mount, and
//! the hardware-proven `sdspi_host_do_transaction` interposer.
//!
//! `storage` owns the mount flow and re-exports the hardware-free probe
//! classification from `sd_probe`.

pub mod sd_probe;
pub mod storage;

pub use storage::*;
