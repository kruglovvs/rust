//! Deterministic tests of the stream-quality analyser.
//!
//! Each test builds a synthetic MJPEG + G.711 clip with ffmpeg and asserts how
//! the analyser classifies it: a healthy sine track passes, silent audio and an
//! audio/video duration mismatch fail. Skips when ffmpeg/ffprobe are missing.

use std::path::{Path, PathBuf};
use std::process::Command;

use esp32s3_av_harness::analyze::{report, Options, Verdict};

fn have(cmd: &str) -> bool {
    Command::new(cmd)
        .arg("-version")
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

fn run(cmd: &mut Command) {
    let status = cmd.status().expect("spawn");
    assert!(status.success(), "{cmd:?} failed");
}

/// Recordings live in `harness/artifacts/` (gitignored) so a failed run can
/// be inspected with ffprobe/ffplay after the fact.
fn artifacts(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("artifacts")
        .join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create artifacts dir");
    dir
}

/// Build `<dir>/stream.mkv` plus `video.mjpeg` / `audio.wav`.
///
/// `audio` is a full lavfi source spec (it may carry its own duration);
/// `seconds` bounds the video track.
fn build_clip(name: &str, audio: &str, seconds: &str) -> PathBuf {
    let dir = artifacts(name);

    run(Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=320x240:rate=10",
            "-f",
            "lavfi",
            "-i",
            audio,
            "-t",
            seconds,
            "-c:v",
            "mjpeg",
            "-q:v",
            "5",
            "-c:a",
            "pcm_mulaw",
            "-ar",
            "8000",
            "-ac",
            "1",
            "-y",
        ])
        .arg(dir.join("stream.mkv")));

    for (map, file, format) in [
        ("0:v:0", "video.mjpeg", "mjpeg"),
        ("0:a:0", "audio.wav", "wav"),
    ] {
        run(Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-i"])
            .arg(dir.join("stream.mkv"))
            .args(["-map", map, "-c", "copy", "-f", format, "-y"])
            .arg(dir.join(file)));
    }
    dir
}

fn options() -> Options {
    Options {
        width: 320,
        height: 240,
        min_frames: 400,
        min_duration: 55.0,
        audio_codec: "pcm_mulaw".to_string(),
        audio_rate: 8_000,
        require_audio: true,
    }
}

fn analyze(dir: &Path) -> esp32s3_av_harness::analyze::Outcome {
    report(dir, &options()).expect("analysis")
}

#[test]
fn analyzer_passes_a_known_good_recording() {
    if !(have("ffmpeg") && have("ffprobe")) {
        eprintln!("SKIP: ffmpeg/ffprobe is not installed");
        return;
    }
    let dir = build_clip("analyzer-good", "sine=frequency=440:sample_rate=8000", "60");
    let outcome = analyze(&dir);
    assert_eq!(
        outcome.verdict,
        Verdict::Pass,
        "failed: {:?}",
        outcome.failed
    );

    let metrics = std::fs::read_to_string(dir.join("metrics.json")).expect("metrics.json");
    assert!(metrics.contains("\"codec\": \"mjpeg\""));
    assert!(metrics.contains("\"codec\": \"pcm_mulaw\""));
    assert!(metrics.contains("\"failed_checks\": 0"));
    assert!(metrics.contains("\"truncated\": 0"));
    assert!(metrics.contains("\"audio_level\""));
}

#[test]
fn analyzer_rejects_silent_audio() {
    if !(have("ffmpeg") && have("ffprobe")) {
        eprintln!("SKIP: ffmpeg/ffprobe is not installed");
        return;
    }
    let dir = build_clip("analyzer-silent", "anullsrc=r=8000:cl=mono", "60");
    let outcome = analyze(&dir);
    assert_eq!(outcome.verdict, Verdict::Fail);
    assert!(
        outcome
            .failed
            .iter()
            .any(|check| check.starts_with("audio not silent")),
        "expected the silence check to fail, got {:?}",
        outcome.failed
    );
}

#[test]
fn analyzer_rejects_audio_rate_mismatch() {
    if !(have("ffmpeg") && have("ffprobe")) {
        eprintln!("SKIP: ffmpeg/ffprobe is not installed");
        return;
    }
    // 30 s of audio under 60 s of video, the shape a wrong RTP clock produces.
    let dir = build_clip(
        "analyzer-mismatch",
        "sine=frequency=440:sample_rate=8000:duration=30",
        "60",
    );
    let outcome = analyze(&dir);
    assert_eq!(outcome.verdict, Verdict::Fail);
    assert!(
        outcome
            .failed
            .iter()
            .any(|check| check.starts_with("audio duration matches video")),
        "expected the duration check to fail, got {:?}",
        outcome.failed
    );
}

#[test]
fn audio_stats_separate_noise_from_signal() {
    if !(have("ffmpeg") && have("ffprobe")) {
        eprintln!("SKIP: ffmpeg/ffprobe is not installed");
        return;
    }
    // A 1 kHz tone scaled to roughly a quarter of full scale (about -12 dBFS
    // at the sine peak before mu-law, -33 dBFS RMS after the codec).
    // Alternating 0.5 s loud / 0.5 s quiet tone blocks, concatenated: the
    // level changes frame to frame, which is what lets the analyser separate
    // "signal" (99.9th percentile) from "noise floor" (10th).
    let dir = build_clip(
        "analyzer-noise",
        "aevalsrc='if(lt(mod(t,1),0.5),0.25,0.002)*sin(2*PI*1000*t)':s=8000:d=60",
        "60",
    );
    let outcome = analyze(&dir);
    let metrics = std::fs::read_to_string(dir.join("metrics.json")).expect("metrics.json");
    let level = metrics
        .split("\"audio_level\": {")
        .nth(1)
        .and_then(|rest| rest.split('}').next())
        .expect("audio_level block");

    let field = |name: &str| -> f64 {
        level
            .split(&format!("\"{name}\": "))
            .nth(1)
            .and_then(|rest| rest.split([',', '}']).next())
            .and_then(|value| value.trim().parse().ok())
            .unwrap_or_else(|| panic!("missing {name} in {level}"))
    };

    let rms = field("rms_db");
    let peak = field("peak_db");
    let crest = field("crest_db");
    let snr = field("snr_db");
    let noise = field("noise_db");
    let clipping = field("clipped_fraction");

    // 50% duty cycle between 0.25 and 0.002 full scale: the overall RMS lands
    // between the two block levels.
    assert!(
        (-50.0..=-10.0).contains(&rms),
        "rms {rms} should be a mid-level tone"
    );
    assert!(peak > rms, "peak {peak} must exceed rms {rms}");
    assert!(crest > 0.0, "crest {crest} must be positive");
    // 0.25 to 0.002 full scale is about 42 dB, so the two levels must separate.
    assert!(
        snr > 20.0,
        "loud and quiet halves should separate, got {snr} dB SNR"
    );
    assert!(
        noise < rms,
        "noise floor {noise} must sit below the signal {rms}"
    );
    assert!(
        clipping < 0.001,
        "a -12 dBFS tone must not clip, got {clipping}"
    );
    assert_eq!(
        outcome.verdict,
        Verdict::Pass,
        "failed: {:?}",
        outcome.failed
    );
}
