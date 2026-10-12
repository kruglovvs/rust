//! Host-executed tests for `firmware/src/recorder.rs` (plan Todo 10).
//!
//! The recorder is pure std + muxer, so its `#[cfg(test)]` suite (naming,
//! no-overwrite, wall-clock/size/SegmentFull triggers, drop-never-duplicate
//! accounting, error policies) runs natively here — the `#[path]` includes
//! mirror the proven `jfif` / `audio_quality` pattern. The file-level
//! ffprobe proofs (rollover, drift, stall) run in the `task10_recorder` bin.

#[path = "../../firmware/src/muxer.rs"]
mod muxer;

#[path = "../../firmware/src/recorder.rs"]
mod recorder;
