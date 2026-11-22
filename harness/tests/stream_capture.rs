//! Opt-in live capture test.
//!
//! Records a short RTSP session with ffmpeg, runs the same Rust analyser the
//! `harness/stream_quality.sh` harness uses, and asserts the structural
//! properties a working stream must have (codecs, scale, audio format, frames
//! present). Artefact findings are printed but not asserted, because this
//! board's camera genuinely drops and corrupts frames; the point of the
//! harness is to quantify that, not to hide it.
//!
//! Run with:
//!   ESP32_RTSP_URL=rtsp://<board-ip>:554/live \
//!     cargo test --test stream_capture -- --nocapture
//!
//! STREAM_WIDTH / STREAM_HEIGHT (default 640x480) set the expected scale.
//! Without ESP32_RTSP_URL the test self-skips.

use std::path::PathBuf;
use std::process::Command;

use esp32s3_av_harness::analyze::{report, Options};

fn have(cmd: &str) -> bool {
    Command::new(cmd)
        .arg("-version")
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

#[test]
fn live_stream_matches_the_configured_geometry() {
    let Ok(url) = std::env::var("ESP32_RTSP_URL") else {
        eprintln!("SKIP: ESP32_RTSP_URL is not set (needs a running board)");
        return;
    };
    if !(have("ffmpeg") && have("ffprobe")) {
        eprintln!("SKIP: ffmpeg/ffprobe is not installed");
        return;
    }
    let width: u32 = std::env::var("STREAM_WIDTH")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(640);
    let height: u32 = std::env::var("STREAM_HEIGHT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(480);

    // Kept in harness/artifacts/ (gitignored) for post-run inspection.
    let out = {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("artifacts")
            .join("stream-capture");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create artifacts dir");
        dir
    };

    let record = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-nostdin",
            "-rtsp_transport",
            "tcp",
            "-i",
            &url,
            "-t",
            "10",
            "-c",
            "copy",
            "-y",
        ])
        .arg(out.join("stream.mkv"))
        .output()
        .expect("run ffmpeg");
    assert!(
        record.status.success(),
        "capture failed: {}",
        String::from_utf8_lossy(&record.stderr)
    );

    for (map, file, format) in [
        ("0:v:0", "video.mjpeg", "mjpeg"),
        ("0:a:0", "audio.wav", "wav"),
    ] {
        let status = Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-i"])
            .arg(out.join("stream.mkv"))
            .args(["-map", map, "-c", "copy", "-f", format, "-y"])
            .arg(out.join(file))
            .status()
            .expect("run ffmpeg");
        assert!(status.success(), "failed to extract {file}");
    }

    let audio_codec = std::env::var("STREAM_AUDIO_CODEC").unwrap_or_else(|_| "aac".into());
    let audio_rate: u64 = std::env::var("STREAM_AUDIO_RATE")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(48_000);
    let options = Options {
        width,
        height,
        min_frames: 10,
        min_duration: 5.0,
        audio_codec: audio_codec.clone(),
        audio_rate,
        require_audio: true,
    };
    let _outcome = report(&out, &options).expect("analysis");

    let metrics = std::fs::read_to_string(out.join("metrics.json"))
        .expect("analyser must produce metrics.json");
    assert!(metrics.contains("\"codec\": \"mjpeg\""), "video codec");
    assert!(
        metrics.contains(&format!("\"codec\": \"{audio_codec}\"")),
        "audio codec"
    );
    assert!(
        metrics.contains(&format!("\"width\": {width}")),
        "video width should be {width}"
    );
    assert!(
        metrics.contains(&format!("\"height\": {height}")),
        "video height should be {height}"
    );
    assert!(metrics.contains("\"frames\": "), "frame count missing");
}
