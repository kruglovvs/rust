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

/// Diagnostic and policy-contract checks the module suite leaves implicit:
/// the documented default is `ErrorPolicy::Roll`, and every typed recorder
/// failure must name a segment (or the stop reason) in its Display string so
/// the device log is self-describing.
#[test]
fn recorder_diagnostics_are_self_describing_and_default_policy_is_roll() {
    use muxer::{AudioTrack, VideoTrack};
    use recorder::{segment_file_name, ErrorPolicy, RecorderConfig, RecorderError};

    let cfg = RecorderConfig::new(
        VideoTrack::new(320, 240, 10).unwrap(),
        AudioTrack::new(16_000, 1).unwrap(),
    );
    assert_eq!(cfg.error_policy, ErrorPolicy::Roll);

    let open = RecorderError::SegmentOpen {
        index: 7,
        source: std::io::Error::new(std::io::ErrorKind::NotFound, "card removed"),
    };
    let text = open.to_string();
    assert!(text.contains("cannot open segment REC_0007.avi"), "{text}");
    assert!(text.contains("card removed"), "{text}");

    assert_eq!(
        RecorderError::Stopped("write failed".into()).to_string(),
        "recorder: stopped: write failed"
    );
    assert_eq!(
        RecorderError::Finished.to_string(),
        "recorder: already finished"
    );
    assert_eq!(
        RecorderError::InvalidConfig("bad track".into()).to_string(),
        "recorder config: bad track"
    );
    assert_eq!(segment_file_name(7), "REC_0007.avi");
}
