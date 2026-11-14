//! Host-side integration helpers for the ESP32-S3 stream.
//!
//! The analysis lives here so the binary and the integration tests share one
//! implementation (no scripting language involved).

pub mod analyze;

/// A fresh directory under `harness/artifacts/` for this run.
///
/// `harness/artifacts/` is gitignored, so recordings from a test or a stress
/// run can be replayed with ffprobe/ffplay after the fact instead of being
/// lost with a temp dir.
pub fn artifacts_dir(name: &str) -> std::path::PathBuf {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("artifacts")
        .join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create artifacts dir");
    dir
}
