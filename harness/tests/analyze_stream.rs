//! Deterministic test of the stream-quality analyser.
//!
//! Builds a synthetic MJPEG + G.711 clip with ffmpeg and asserts the analyser
//! accepts it. Exercises bitrate/scale/A-V probing, JPEG marker parsing and
//! the ffmpeg artefact filters without needing a board.
//!
//! Skips (with a printed reason) when ffmpeg/ffprobe are unavailable.

use std::path::PathBuf;
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

#[test]
fn analyzer_passes_a_known_good_recording() {
    if !(have("ffmpeg") && have("ffprobe")) {
        eprintln!("SKIP: ffmpeg/ffprobe is not installed");
        return;
    }

    let out = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("analyzer-known-good");
    let _ = std::fs::remove_dir_all(&out);
    std::fs::create_dir_all(&out).expect("create test dir");

    run(Command::new("ffmpeg").args([
        "-hide_banner", "-loglevel", "error",
        "-f", "lavfi", "-i", "testsrc2=size=320x240:rate=10",
        "-f", "lavfi", "-i", "sine=frequency=440:sample_rate=16000",
        "-t", "60", "-c:v", "mjpeg", "-q:v", "5",
        "-c:a", "pcm_mulaw", "-ar", "8000", "-ac", "1", "-y",
    ]).arg(out.join("stream.mkv")));

    for (map, file, format) in [
        ("0:v:0", "video.mjpeg", "mjpeg"),
        ("0:a:0", "audio.wav", "wav"),
    ] {
        run(Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-i"])
            .arg(out.join("stream.mkv"))
            .args(["-map", map, "-c", "copy", "-f", format, "-y"])
            .arg(out.join(file)));
    }

    let options = Options {
        width: 320,
        height: 240,
        min_frames: 400,
        min_duration: 55.0,
    };
    let verdict = report(&out, &options).expect("analysis");
    assert_eq!(verdict, Verdict::Pass, "analyser rejected a known-good clip");

    let metrics = std::fs::read_to_string(out.join("metrics.json")).expect("metrics.json");
    assert!(metrics.contains("\"codec\": \"mjpeg\""));
    assert!(metrics.contains("\"codec\": \"pcm_mulaw\""));
    assert!(metrics.contains("\"failed_checks\": 0"));
    assert!(metrics.contains("\"truncated\": 0"));
}
