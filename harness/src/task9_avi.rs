//! Todo 9 host-executed proof (plan `esp32s3-av-stream-and-sd-recording.md`).
//!
//! Muxes ~5 s of real MJPEG frames (the Todo 6 `harness/fixtures/` JPEGs) plus
//! a 16 kHz mono `pcm_s16le` sine through **our** `Muxer` trait / `AviMuxer`
//! (firmware/src/muxer.rs, `#[path]`-included — pure std + oxideav) into an
//! AVI file. `ffprobe` then has to see exactly two streams — `mjpeg` and
//! `pcm_s16le` — with durations ≈ 5 s. That is the host-side half of the IS-3
//! evidence; on-device rows stay PENDING_NO_BOARD.
//!
//! Usage:
//!   task9_avi [out.avi]     mux the 5 s recording to the given path
//!   task9_avi --mock-log    print the MockMuxer packet log for a synthetic
//!                           session (dropped frame included) and exit

#[path = "../../firmware/src/muxer.rs"]
mod muxer;

use std::f64::consts::PI;
use std::io::Write;

use muxer::{
    AviMuxer, AudioTrack, MockMuxer, Muxer, MuxerError, TrackKind, VideoTrack, AUDIO_CHANNELS,
    AUDIO_SAMPLE_RATE, DEFAULT_FRAME_RATE,
};

const SECONDS: u32 = 5;
const WIDTH: u32 = 320;
const HEIGHT: u32 = 240;
const SAMPLES_PER_FRAME: usize = AUDIO_SAMPLE_RATE as usize / DEFAULT_FRAME_RATE as usize;

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

fn sine_frame(start_sample: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(SAMPLES_PER_FRAME * 2);
    for i in 0..SAMPLES_PER_FRAME {
        let t = (start_sample + i as u64) as f64 / AUDIO_SAMPLE_RATE as f64;
        let v = (0.3 * (2.0 * PI * 440.0 * t).sin() * i16::MAX as f64) as i16;
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

fn mux_recording(out_path: &str) -> Result<(), MuxerError> {
    let frames = fixtures();
    let video = VideoTrack::new(WIDTH, HEIGHT, DEFAULT_FRAME_RATE)?;
    let audio = AudioTrack::new(AUDIO_SAMPLE_RATE, AUDIO_CHANNELS)?;
    let file = std::fs::File::create(out_path)
        .unwrap_or_else(|e| panic!("create {out_path}: {e}"));
    let sink: Box<dyn oxideav_core::WriteSeek> = Box::new(file);
    let mut mux = AviMuxer::new(video, audio, sink)?;

    let total_frames = (SECONDS * DEFAULT_FRAME_RATE) as u64;
    for frame in 0..total_frames {
        let jpeg = &frames[(frame as usize) % frames.len()];
        mux.write_video(frame, jpeg)?;
        let pcm = sine_frame(frame * SAMPLES_PER_FRAME as u64);
        mux.write_audio(frame * SAMPLES_PER_FRAME as u64, &pcm)?;
    }
    mux.finish()?;

    println!("task9_avi: muxed {total_frames} MJPEG frames + {} audio packets", total_frames);
    println!("task9_avi: audio = {SAMPLES_PER_FRAME} samples/frame x {total_frames} = {} samples ({} s)",
        total_frames * SAMPLES_PER_FRAME as u64,
        SECONDS
    );
    println!("task9_avi: bytes_written (chunk accounting) = {}", mux.bytes_written());
    println!("task9_avi: finished = {}", mux.is_finished());
    println!("task9_avi: wrote {out_path}");
    Ok(())
}

fn print_mock_log() {
    let video = VideoTrack::new(WIDTH, HEIGHT, DEFAULT_FRAME_RATE).unwrap();
    let audio = AudioTrack::new(AUDIO_SAMPLE_RATE, AUDIO_CHANNELS).unwrap();
    let mut m = MockMuxer::new_mock(
        video,
        audio,
        Box::new(std::io::Cursor::new(Vec::new())),
    );
    for frame in 0..3u64 {
        let jpeg = vec![0xFF, 0xD8, frame as u8, 0xFF, 0xD9];
        m.write_video(frame, &jpeg).unwrap();
        let pcm = sine_frame(frame * SAMPLES_PER_FRAME as u64);
        m.write_audio(frame * SAMPLES_PER_FRAME as u64, &pcm).unwrap();
    }
    // Dropped frame: video packet 3 has no audio partner.
    m.write_video(3, &[0xFF, 0xD8, 9, 0xFF, 0xD9]).unwrap();
    m.finish().unwrap();
    m.finish().unwrap(); // idempotent

    let stdout = std::io::stdout();
    let mut w = stdout.lock();
    let _ = writeln!(w, "MockMuxer packet log (synthetic 3-frame session + 1 dropped frame):");
    for (i, p) in m.packets().iter().enumerate() {
        let kind = match p.kind {
            TrackKind::Video => "VIDEO",
            TrackKind::Audio => "AUDIO",
        };
        let _ = writeln!(
            w,
            "  [{i}] {kind} pts={} payload_len={}",
            p.pts,
            p.payload.len()
        );
    }
    let _ = writeln!(w, "video_packets        = {}", m.video_packets());
    let _ = writeln!(w, "audio_packets        = {}", m.audio_packets());
    let _ = writeln!(w, "audio_samples        = {}", m.audio_samples());
    let _ = writeln!(w, "unmatched_video      = {}", m.unmatched_video_packets());
    let _ = writeln!(w, "bytes_written        = {}", m.bytes_written());
    let _ = writeln!(w, "is_finished          = {}", m.is_finished());
    let after = match m.write_video(4, &[0; 4]) {
        Ok(()) => "Ok".to_string(),
        Err(e) => format!("Err({e})"),
    };
    let _ = writeln!(w, "write after finish   = {after}");
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--mock-log") {
        print_mock_log();
        return;
    }
    let out_path = args
        .get(1)
        .map(String::as_str)
        .unwrap_or("task9-recording.avi");
    mux_recording(out_path).unwrap_or_else(|e| {
        eprintln!("task9_avi: mux failed: {e}");
        std::process::exit(1);
    });
}
