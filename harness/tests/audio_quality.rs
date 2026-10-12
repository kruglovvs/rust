//! Host-executed tests for `firmware/src/audio_quality.rs` (plan Todo 8).
//!
//! The statistics module is pure, zero-dependency and FFI-free, so its
//! `#[cfg(test)]` suite runs natively here — the `#[path]` include mirrors
//! the proven `jfif_nostd` pattern (this crate's lib target includes the
//! file as its own crate root; here it is a module so the file keeps its
//! `mod`-level identity).

#[path = "../../firmware/src/audio_quality.rs"]
mod audio_quality;
