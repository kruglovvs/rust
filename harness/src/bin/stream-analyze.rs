//! CLI wrapper for [`esp32s3_av_harness::analyze`].
//!
//! Usage: stream-analyze report OUTDIR [--width N] [--height N]
//!                                  [--min-frames N] [--min-duration S]
//!                                  [--audio-codec NAME] [--audio-rate HZ]

use std::path::PathBuf;

use esp32s3_av_harness::analyze::{report, Options};

fn main() {
    let mut args = std::env::args().skip(1);
    if args.next().as_deref() != Some("report") {
        eprintln!("usage: stream-analyze report OUTDIR [--width N] [--height N] [--min-frames N] [--min-duration S]");
        std::process::exit(2);
    }
    let Some(outdir) = args.next() else {
        eprintln!("missing OUTDIR");
        std::process::exit(2);
    };
    let mut options = Options::default();
    while let Some(flag) = args.next() {
        let Some(value) = args.next() else {
            eprintln!("{flag} needs a value");
            std::process::exit(2);
        };
        match flag.as_str() {
            "--width" => options.width = value.parse().unwrap_or(options.width),
            "--height" => options.height = value.parse().unwrap_or(options.height),
            "--min-frames" => options.min_frames = value.parse().unwrap_or(options.min_frames),
            "--min-duration" => options.min_duration = value.parse().unwrap_or(options.min_duration),
            "--audio-codec" => options.audio_codec = value,
            "--audio-rate" => options.audio_rate = value.parse().unwrap_or(options.audio_rate),
            other => {
                eprintln!("unknown flag {other}");
                std::process::exit(2);
            }
        }
    }
    match report(&PathBuf::from(outdir), &options) {
        Ok(outcome) => std::process::exit(outcome.verdict.code()),
        Err(error) => {
            eprintln!("analyze: {error}");
            std::process::exit(2);
        }
    }
}
