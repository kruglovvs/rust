//! Todo 10 host-executed proof (plan `esp32s3-av-stream-and-sd-recording.md`).
//!
//! Drives **our** `Recorder` (firmware/src/recorder.rs, `#[path]`-included —
//! pure std + muxer) with synthetic single-clock timestamps and proves, on
//! the host:
//!
//! 1. **Rollover** (`task10_recorder rollover <dir> <seconds>`): a tiny
//!    1 MiB segment bound produces MULTIPLE separate `.avi` files, each
//!    independently ffprobe-valid (two streams), monotonic `REC_%04d.avi`
//!    numbering, and — the IS-5 assertion — **no OpenDML `AVIX` chunk** in
//!    any file, checked structurally (RIFF walker), not by byte grep
//!    (JPEG entropy can contain the ASCII bytes by chance).
//! 2. **Drift** (`task10_recorder drift <out.avi> <seconds>`): one healthy
//!    synthetic take; the caller ffprobes the result and asserts
//!    |dur_v − dur_a| ≤ 250 ms. Prints the accounting the shell asserts on.
//! 3. **Stall** (`task10_recorder stall <out.avi> <seconds>`): a simulated
//!    camera stall (frames withheld, then a stale backlog offered late)
//!    must DROP frames, never duplicate: `video_written + video_dropped ==
//!    video_offered`, written slots strictly increasing, and the written
//!    frame count equals the count of frames offered with fresh slots.
//!
//! Frames are the real QVGA JPEG fixtures from Todo 6 (harness/fixtures).

#[path = "../../firmware/src/muxer.rs"]
mod muxer;
#[path = "../../firmware/src/recorder.rs"]
mod recorder;

use std::f64::consts::PI;

use muxer::{AudioTrack, VideoTrack, AUDIO_CHANNELS, AUDIO_SAMPLE_RATE};
use recorder::{segment_file_name, DirFactory, Recorder, RecorderConfig, VideoOutcome};

const WIDTH: u32 = 320;
const HEIGHT: u32 = 240;
const FRAME_RATE: u32 = 10; // board's proven operating point
const FRAME_PERIOD_US: u64 = 1_000_000 / FRAME_RATE as u64;
const AUDIO_CHUNK_SAMPLES: usize = 320; // 20 ms @ 16 kHz (mic.rs AudioFrame)
const AUDIO_CHUNK_US: u64 = 20_000;

fn fixtures() -> Vec<Vec<u8>> {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures");
    let names = [
        "qvga_422.jpg",
        "qvga_420.jpg",
        "qvga_422_restart.jpg",
        "qvga_422_dyn.jpg",
    ];
    names
        .iter()
        .map(|n| {
            let p = format!("{dir}/{n}");
            std::fs::read(&p).unwrap_or_else(|e| panic!("read fixture {p}: {e}"))
        })
        .collect()
}

fn sine_chunk(start_sample: u64) -> Vec<i16> {
    (0..AUDIO_CHUNK_SAMPLES)
        .map(|i| {
            let t = (start_sample + i as u64) as f64 / AUDIO_SAMPLE_RATE as f64;
            (0.3 * (2.0 * PI * 440.0 * t).sin() * i16::MAX as f64) as i16
        })
        .collect()
}

fn base_config() -> RecorderConfig {
    let video = VideoTrack::new(WIDTH, HEIGHT, FRAME_RATE).expect("video track");
    let audio = AudioTrack::new(AUDIO_SAMPLE_RATE, AUDIO_CHANNELS).expect("audio track");
    RecorderConfig::new(video, audio)
}

/// Feed `seconds` of interleaved video (every FRAME_PERIOD_US) and audio
/// (every AUDIO_CHUNK_US) on one monotonic clock starting at t=0.
/// `stall` simulates a camera stall in [stall_from, stall_to): no video is
/// produced there, and the frames that "would have" been produced are
/// re-offered LATE (old pts) after the stall — the drop-never-duplicate
/// case. Returns the recorder (caller finishes it).
fn feed_take(
    rec: &mut Recorder,
    frames: &[Vec<u8>],
    seconds: u64,
    stall: Option<(u64, u64)>,
) {
    let total_us = seconds * 1_000_000;
    let mut audio_sample_cursor = 0u64;
    let mut video_us = 0u64;
    let mut frame_idx = 0usize;
    let mut stalled_frames: Vec<(u64, usize)> = Vec::new(); // (pts, fixture idx) withheld

    let mut now_us = 0u64;
    while now_us < total_us {
        // Audio first (mic chunks are the steady 20 ms metronome).
        let chunk = sine_chunk(audio_sample_cursor);
        rec.write_audio_samples(now_us, &chunk)
            .unwrap_or_else(|e| panic!("audio write at {now_us} us: {e}"));
        audio_sample_cursor += AUDIO_CHUNK_SAMPLES as u64;

        // Then video when the frame period elapses.
        while video_us <= now_us {
            let in_stall = stall
                .map(|(from, to)| video_us >= from && video_us < to)
                .unwrap_or(false);
            let jpeg = &frames[frame_idx % frames.len()];
            if in_stall {
                // Camera stalled: the frame never leaves the sensor.
                stalled_frames.push((video_us, frame_idx));
            } else {
                let out = rec
                    .write_video_frame(video_us, jpeg)
                    .unwrap_or_else(|e| panic!("video write at {video_us} us: {e}"));
                assert!(
                    matches!(out, VideoOutcome::Written { .. }),
                    "on-time frame at {video_us} us must be written, got {out:?}"
                );
                // Recovery flush: the camera's stale backlog arrives AFTER
                // the fresh frame advanced the write frontier — every stale
                // slot is now behind it, so each must be DROPPED (never
                // duplicated into the gap, never re-timed).
                if let Some((_from, to)) = stall {
                    if video_us >= to && !stalled_frames.is_empty() {
                        for (stale_us, idx) in stalled_frames.drain(..) {
                            let out = rec
                                .write_video_frame(stale_us, &frames[idx % frames.len()])
                                .expect("late write");
                            assert!(
                                matches!(out, VideoOutcome::Dropped { .. }),
                                "stale frame at {stale_us} us must be dropped, got {out:?}"
                            );
                        }
                    }
                }
            }
            frame_idx += 1;
            video_us += FRAME_PERIOD_US;
        }
        now_us += AUDIO_CHUNK_US;
    }
}

/// IS-5 structural assertion: walk the RIFF tree and fail on any `AVIX`
/// fourcc at a chunk-id or LIST-type position (how OpenDML continuations
/// appear). A plain byte grep is NOT sufficient — JPEG payload bytes can
/// contain the ASCII sequence by chance.
fn assert_no_avix(bytes: &[u8], name: &str) {
    fn walk(buf: &[u8], name: &str) {
        let mut pos = 0usize;
        while pos + 8 <= buf.len() {
            let id = &buf[pos..pos + 4];
            let size = u32::from_le_bytes([buf[pos + 4], buf[pos + 5], buf[pos + 6], buf[pos + 7]])
                as usize;
            let body_start = pos + 8;
            let body_end = (body_start + size).min(buf.len());
            assert!(
                id != b"AVIX",
                "{name}: found AVIX chunk id at offset {pos} — OpenDML continuation in an Avi10 file"
            );
            if id == b"LIST" && body_start + 4 <= buf.len() {
                let list_type = &buf[body_start..body_start + 4];
                assert!(
                    list_type != b"AVIX",
                    "{name}: found AVIX LIST type at offset {pos}"
                );
                // Recurse into the list body (skips the 4-byte list type).
                walk(&buf[body_start + 4..body_end], name);
            }
            if id == b"RIFF" && body_start + 4 <= buf.len() {
                let form = &buf[body_start..body_start + 4];
                assert!(
                    form != b"AVIX",
                    "{name}: found RIFF/AVIX form at offset {pos}"
                );
                walk(&buf[body_start + 4..body_end], name);
            }
            pos = body_start + size + (size & 1); // RIFF word alignment
        }
    }
    assert!(
        bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"AVI ",
        "{name}: not a RIFF/AVI file (len={})",
        bytes.len()
    );
    walk(bytes, name);
}

fn cmd_rollover(dir: &str, seconds: u64) {
    // Clean slate: previous run's segments would be *skipped* by the
    // no-overwrite rule — exactly what we want to prove separately, but
    // this command proves rollover from a fresh directory.
    if std::path::Path::new(dir).exists() {
        std::fs::remove_dir_all(dir).expect("clean dir");
    }
    let mut cfg = base_config();
    cfg.segment_size_bytes = 1024 * 1024; // 1 MiB — rollover quickly
    let mut rec = Recorder::new(cfg, DirFactory::new(dir)).expect("recorder");
    let frames = fixtures();
    feed_take(&mut rec, &frames, seconds, None);
    rec.finish().expect("finish");

    let s = rec.stats();
    println!("task10_recorder: rollover summary:");
    println!("  segments_opened       = {}", s.segments_opened);
    println!("  segments_completed    = {}", s.segments_completed);
    println!("  rolls_size            = {}", s.rolls_size);
    println!("  rolls_wallclock       = {}", s.rolls_wallclock);
    println!("  rolls_segment_full    = {}", s.rolls_segment_full);
    println!("  video_offered/written/dropped = {}/{}/{}",
        s.video_offered, s.video_written, s.video_dropped);
    println!("  audio_samples_written = {}", s.audio_samples_written);
    assert!(
        s.segments_opened >= 2,
        "1 MiB bound over {seconds}s must produce multiple segments, got {}",
        s.segments_opened
    );
    assert_eq!(s.video_dropped, 0, "healthy take drops nothing");
    assert_eq!(s.video_written + s.video_dropped, s.video_offered);

    // Every segment must be a structurally valid Avi10 RIFF with no AVIX.
    for i in 1..=s.current_segment_index {
        let path = format!("{dir}/{}", segment_file_name(i));
        let bytes = std::fs::read(&path)
            .unwrap_or_else(|e| panic!("read segment {path}: {e}"));
        assert_no_avix(&bytes, &path);
        println!(
            "  {}: {} bytes, RIFF/AVI, no AVIX (structural walk)",
            segment_file_name(i),
            bytes.len()
        );
    }
    println!("task10_recorder: rollover PASS");
}

fn cmd_drift(out: &str, seconds: u64) {
    let _ = std::fs::remove_file(out); // stale output from a prior run must not block create_new
    let mut cfg = base_config();
    cfg.segment_size_bytes = u64::MAX / 2; // single segment: prove A/V alignment, not rollover
    let mut rec = Recorder::new(cfg, OneShotFactory { path: out.into() }).expect("recorder");
    let frames = fixtures();
    feed_take(&mut rec, &frames, seconds, None);
    rec.finish().expect("finish");
    let s = rec.stats();
    println!("task10_recorder: drift summary:");
    println!("  segment               = {}", rec.current_segment_name());
    println!("  video_written         = {} ({} us of CFR video)",
        s.video_written, s.video_written as u64 * FRAME_PERIOD_US);
    println!("  audio_samples_written = {} ({} us of audio)",
        s.audio_samples_written, s.audio_samples_written * 1_000_000 / AUDIO_SAMPLE_RATE as u64);
    println!("  av_start_delta_us     = {:?}", s.av_start_delta_us());
    assert_eq!(s.video_dropped, 0);
    assert!(s.av_start_delta_us().expect("both tracks") <= 250_000);
    // The CFR invariant: frames × period ≈ samples / rate, within 250 ms.
    let dur_v_us = s.video_written * FRAME_PERIOD_US;
    let dur_a_us = s.audio_samples_written * 1_000_000 / AUDIO_SAMPLE_RATE as u64;
    let delta = dur_v_us.abs_diff(dur_a_us);
    println!("  |dur_v - dur_a|       = {delta} us");
    assert!(delta <= 250_000, "A/V drift {delta} us exceeds 250 ms");
    println!("task10_recorder: drift PASS (ffprobe assertion runs in the shell)");
}

fn cmd_stall(out: &str, seconds: u64) {
    let _ = std::fs::remove_file(out); // stale output from a prior run must not block create_new
    let mut cfg = base_config();
    cfg.segment_size_bytes = u64::MAX / 2;
    let mut rec = Recorder::new(cfg, OneShotFactory { path: out.into() }).expect("recorder");
    let frames = fixtures();
    // Stall the camera for 1 s in the middle of the take; the withheld
    // frames are re-offered late (stale pts) when the stall ends.
    let stall = (seconds * 1_000_000 / 4, seconds * 1_000_000 / 4 + 1_000_000);
    feed_take(&mut rec, &frames, seconds, Some(stall));
    rec.finish().expect("finish");
    let s = rec.stats();
    println!("task10_recorder: stall summary:");
    println!("  stall window          = [{}, {}) us", stall.0, stall.1);
    println!("  video_offered         = {}", s.video_offered);
    println!("  video_written         = {}", s.video_written);
    println!("  video_dropped         = {}", s.video_dropped);
    println!("  audio_samples_written = {}", s.audio_samples_written);
    // The drop-never-duplicate invariants.
    assert_eq!(
        s.video_written + s.video_dropped,
        s.video_offered,
        "every offered frame is either written exactly once or dropped"
    );
    assert!(
        s.video_dropped >= stall.1.saturating_sub(stall.0) / FRAME_PERIOD_US,
        "the withheld backlog must be dropped, not written: dropped={}",
        s.video_dropped
    );
    // Written count must equal the number of on-time (non-withheld) frames:
    // total slots in `seconds` minus the stall window slots.
    let total_slots = seconds * 1_000_000 / FRAME_PERIOD_US;
    let stall_slots = (stall.1 - stall.0) / FRAME_PERIOD_US;
    assert_eq!(
        s.video_written,
        total_slots - stall_slots,
        "stall drops frames; it never duplicates or invents them"
    );
    // And the file is still a valid single-segment Avi10 (honest short
    // video, correct audio).
    let bytes = std::fs::read(out).expect("read stall file");
    assert_no_avix(&bytes, out);
    println!("task10_recorder: stall PASS (frames dropped, never duplicated)");
}

/// One-shot factory that creates exactly one named file (for the drift and
/// stall commands, which target a specific output path).
struct OneShotFactory {
    path: std::path::PathBuf,
}

impl recorder::SegmentFactory for OneShotFactory {
    fn exists(&self, _name: &str) -> std::io::Result<bool> {
        Ok(false)
    }
    fn create(&self, _name: &str) -> std::io::Result<Box<dyn oxideav_core::WriteSeek>> {
        let f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&self.path)?;
        Ok(Box::new(f))
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("rollover") => {
            let dir = args.get(2).map(String::as_str).unwrap_or("task10-rollover");
            let seconds: u64 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(20);
            cmd_rollover(dir, seconds);
        }
        Some("drift") => {
            let out = args.get(2).map(String::as_str).unwrap_or("task10-drift.avi");
            let seconds: u64 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(5);
            cmd_drift(out, seconds);
        }
        Some("stall") => {
            let out = args.get(2).map(String::as_str).unwrap_or("task10-stall.avi");
            let seconds: u64 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(5);
            cmd_stall(out, seconds);
        }
        _ => {
            eprintln!("usage: task10_recorder rollover|drift|stall [path] [seconds]");
            std::process::exit(2);
        }
    }
}
