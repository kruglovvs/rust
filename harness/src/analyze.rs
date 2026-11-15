//! Stream-quality analysis: bitrate, scale, A/V parameters and artefacts.
//!
//! Everything here is dependency-free Rust plus `ffprobe`/`ffmpeg` calls:
//!
//! * JPEG integrity - each frame of the concatenated MJPEG stream is split on
//!   SOI/EOI markers. A frame without EOI is truncated, which makes a decoder
//!   paint the missing MCU rows green.
//! * `blurdetect` / `blockdetect` - ffmpeg's Laplacian-variance sharpness and
//!   8x8 blockiness estimators.
//! * `freezedetect` / `blackdetect` - interval detectors.
//! * `signalstats` - per-frame luma and saturation means; corrupt frames show
//!   up as >3 sigma outliers.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::Path;
use std::process::Command;

/// Expected stream properties.
#[derive(Debug, Clone)]
pub struct Options {
    pub width: u32,
    pub height: u32,
    pub min_frames: u64,
    pub min_duration: f64,
    /// Expected audio codec name as reported by ffprobe (`aac`, `pcm_mulaw`, ...).
    pub audio_codec: String,
    /// Expected audio sample rate in Hz.
    pub audio_rate: u64,
    /// Fail when the audio is effectively silent. Off during long stress runs:
    /// a quiet room is not a stream defect.
    pub require_audio: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            width: 640,
            height: 480,
            min_frames: 150,
            min_duration: 55.0,
            audio_codec: "aac".to_string(),
            audio_rate: 48_000,
            require_audio: true,
        }
    }
}

/// Outcome of an analysis run.
#[derive(Debug, Clone)]
pub struct Outcome {
    pub verdict: Verdict,
    pub failed: Vec<String>,
    pub warnings: Vec<String>,
}

/// Overall verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Pass,
    Fail,
}

impl Verdict {
    pub fn code(self) -> i32 {
        match self {
            Verdict::Pass => 0,
            Verdict::Fail => 1,
        }
    }
}

#[derive(Debug, Default, Clone)]
struct Probe {
    streams: Vec<HashMap<String, String>>,
    format: HashMap<String, String>,
}

impl Probe {
    fn video(&self) -> Option<&HashMap<String, String>> {
        self.streams.iter().find(|s| s.get("codec_type").map(String::as_str) == Some("video"))
    }

    fn audio(&self) -> Option<&HashMap<String, String>> {
        self.streams.iter().find(|s| s.get("codec_type").map(String::as_str) == Some("audio"))
    }
}

fn field<'a>(map: &'a HashMap<String, String>, key: &str) -> Option<&'a str> {
    map.get(key).map(String::as_str)
}

fn number(map: &HashMap<String, String>, key: &str) -> Option<f64> {
    field(map, key).and_then(|value| value.parse().ok())
}

fn integer(map: &HashMap<String, String>, key: &str) -> Option<u64> {
    field(map, key).and_then(|value| value.parse().ok())
}

fn run(cmd: &mut Command) -> Result<String, String> {
    let output = cmd.output().map_err(|error| format!("{cmd:?}: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "{cmd:?} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn parse_key_values(text: &str) -> HashMap<String, String> {
    text.lines()
        .filter_map(|line| line.split_once('='))
        .map(|(key, value)| (key.trim().to_string(), value.trim().to_string()))
        .collect()
}

fn probe(path: &Path) -> Result<Probe, String> {
    let streams_raw = run(Command::new("ffprobe").args([
        "-v",
        "error",
        "-count_frames",
        "-show_entries",
        "stream=index,codec_name,codec_type,width,height,sample_rate,channels,avg_frame_rate,nb_read_frames",
        "-of",
        "default=noprint_wrappers=1",
    ]).arg(path))?;
    let format_raw = run(Command::new("ffprobe").args([
        "-v",
        "error",
        "-show_entries",
        "format=format_name,duration",
        "-of",
        "default=noprint_wrappers=1",
    ]).arg(path))?;

    let mut streams: Vec<HashMap<String, String>> = Vec::new();
    for line in streams_raw.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim() == "index" {
            streams.push(HashMap::new());
        }
        if let Some(current) = streams.last_mut() {
            current.insert(key.trim().to_string(), value.trim().to_string());
        }
    }
    Ok(Probe {
        streams,
        format: parse_key_values(&format_raw),
    })
}

fn video_bitrate(path: &Path, duration: f64) -> Result<f64, String> {
    let raw = run(Command::new("ffprobe").args([
        "-v",
        "error",
        "-select_streams",
        "v:0",
        "-show_entries",
        "packet=size",
        "-of",
        "csv=p=0",
    ]).arg(path))?;
    let bytes: u64 = raw
        .lines()
        .filter_map(|line| line.trim().parse::<u64>().ok())
        .sum();
    Ok(if duration > 0.0 {
        bytes as f64 * 8.0 / duration
    } else {
        0.0
    })
}

fn ffmpeg_log(path: &Path, filter: &str) -> Result<String, String> {
    let output = Command::new("ffmpeg")
        .args(["-hide_banner", "-nostdin", "-i"])
        .arg(path)
        .args(["-vf", filter, "-an", "-f", "null", "-"])
        .output()
        .map_err(|error| format!("ffmpeg -vf {filter}: {error}"))?;
    Ok(String::from_utf8_lossy(&output.stderr).into_owned())
}

fn number_after(text: &str, marker: &str) -> Option<f64> {
    text.split(marker)
        .nth(1)?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

fn sum_after(text: &str, marker: &str) -> f64 {
    text.match_indices(marker)
        .filter_map(|(index, _)| {
            text[index + marker.len()..]
                .split_whitespace()
                .next()?
                .parse::<f64>()
                .ok()
        })
        .sum()
}

#[derive(Debug, Default, Clone)]
struct DecodeWarnings {
    corrupt_dc: u64,
    invalid_size: u64,
    overread: u64,
    eoi_missing: u64,
    other: u64,
}

impl DecodeWarnings {
    fn total(&self) -> u64 {
        self.corrupt_dc + self.invalid_size + self.overread + self.eoi_missing + self.other
    }
}

fn classify_warnings(path: &Path) -> Result<DecodeWarnings, String> {
    let output = Command::new("ffmpeg")
        .args(["-hide_banner", "-nostdin", "-v", "warning", "-i"])
        .arg(path)
        .args(["-f", "null", "-"])
        .output()
        .map_err(|error| format!("ffmpeg warning scan: {error}"))?;
    let mut classes = DecodeWarnings::default();
    for line in String::from_utf8_lossy(&output.stderr).lines() {
        if line.contains("mjpeg_decode_dc: bad vlc") {
            classes.corrupt_dc += 1;
        } else if line.contains("Picture size") && line.contains("invalid") {
            classes.invalid_size += 1;
        } else if line.contains("overread") {
            classes.overread += 1;
        } else if line.contains("EOI missing") {
            classes.eoi_missing += 1;
        } else if !line.trim().is_empty()
            && !line.contains("Guessed Channel")
            && !line.contains("Stream mapping")
        {
            classes.other += 1;
        }
    }
    Ok(classes)
}

#[derive(Debug, Default, Clone)]
struct JpegStats {
    frames: u64,
    truncated: u64,
    garbage: u64,
    short: u64,
    median: usize,
    min: usize,
    max: usize,
    dims: Vec<(u16, u16)>,
}

fn jpeg_dims(frame: &[u8]) -> Option<(u16, u16)> {
    let mut i = 2;
    while i + 9 <= frame.len() {
        if frame[i] != 0xFF {
            i += 1;
            continue;
        }
        match frame[i + 1] {
            0xC0 | 0xC1 | 0xC2 => {
                let height = u16::from_be_bytes([frame[i + 5], frame[i + 6]]);
                let width = u16::from_be_bytes([frame[i + 7], frame[i + 8]]);
                return Some((width, height));
            }
            0xD8 | 0xD9 => {
                i += 2;
                continue;
            }
            _ => {
                if i + 3 >= frame.len() {
                    break;
                }
                let len = u16::from_be_bytes([frame[i + 2], frame[i + 3]]) as usize;
                if len < 2 {
                    break;
                }
                i += 2 + len;
            }
        }
    }
    None
}

fn jpeg_stats(path: &Path) -> Result<JpegStats, String> {
    let data = std::fs::read(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let mut sizes: Vec<usize> = Vec::new();
    let mut stats = JpegStats::default();
    let mut dims: HashMap<(u16, u16), u64> = HashMap::new();
    let mut i = 0usize;
    loop {
        let Some(offset) = data[i..].windows(2).position(|w| w == [0xFF, 0xD8]) else {
            stats.garbage += (data.len() - i) as u64;
            break;
        };
        let start = i + offset;
        stats.garbage += (start - i) as u64;
        let Some(end_offset) = data[start + 2..].windows(2).position(|w| w == [0xFF, 0xD9]) else {
            sizes.push(data.len() - start);
            stats.truncated += 1;
            break;
        };
        let end = start + 2 + end_offset + 2;
        let frame = &data[start..end];
        sizes.push(frame.len());
        if let Some((width, height)) = jpeg_dims(frame) {
            *dims.entry((width, height)).or_default() += 1;
        }
        i = end;
        if i >= data.len() {
            break;
        }
    }
    stats.frames = sizes.len() as u64;
    let mut sorted = sizes.clone();
    sorted.sort_unstable();
    stats.median = sorted
        .get(sorted.len() / 2)
        .copied()
        .unwrap_or_default();
    stats.min = sorted.first().copied().unwrap_or_default();
    stats.max = sorted.last().copied().unwrap_or_default();
    stats.short = sizes
        .iter()
        .filter(|size| stats.median > 0 && **size < stats.median / 2)
        .count() as u64;
    stats.dims = dims.keys().copied().collect();
    Ok(stats)
}

#[derive(Debug, Default, Clone)]
struct SignalStats {
    frames: u64,
    outlier_frames: u64,
    yavg_mean: Option<f64>,
    satavg_mean: Option<f64>,
}

fn signal_stats(path: &Path) -> Result<SignalStats, String> {
    let output = Command::new("ffmpeg")
        .args(["-hide_banner", "-nostdin", "-i"])
        .arg(path)
        .args(["-vf", "signalstats,metadata=print:file=-", "-an", "-f", "null", "-"])
        .output()
        .map_err(|error| format!("ffmpeg signalstats: {error}"))?;
    let text = String::from_utf8_lossy(&output.stdout);
    let mut series: HashMap<&str, Vec<f64>> = HashMap::new();
    for line in text.lines() {
        for key in ["YAVG", "SATAVG", "HUEAVG"] {
            let tag = format!("lavfi.signalstats.{key}=");
            if let Some(value) = line.split(&tag).nth(1) {
                if let Ok(value) = value.trim().parse::<f64>() {
                    series.entry(key).or_default().push(value);
                }
            }
        }
    }
    let mut stats = SignalStats {
        frames: series.get("YAVG").map_or(0, |values| values.len() as u64),
        ..SignalStats::default()
    };
    let mut outliers = 0u64;
    for (key, values) in &series {
        if values.len() < 2 {
            continue;
        }
        let mean = values.iter().sum::<f64>() / values.len() as f64;
        let variance = values
            .iter()
            .map(|value| (value - mean).powi(2))
            .sum::<f64>()
            / values.len() as f64;
        let stdev = variance.sqrt();
        let high = values
            .iter()
            .filter(|value| stdev > 0.0 && (*value - mean).abs() > 3.0 * stdev)
            .count() as u64;
        outliers = outliers.max(high);
        match *key {
            "YAVG" => stats.yavg_mean = Some(mean),
            "SATAVG" => stats.satavg_mean = Some(mean),
            _ => {}
        }
    }
    stats.outlier_frames = outliers;
    Ok(stats)
}

fn dims_text(dims: &[(u16, u16)]) -> String {
    if dims.is_empty() {
        return "none".into();
    }
    let mut sorted = dims.to_vec();
    sorted.sort_unstable();
    sorted
        .iter()
        .map(|(width, height)| format!("{width}x{height}"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn json_escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

#[derive(Debug, Default, Clone)]
struct AudioStats {
    seconds: f64,
    /// Mean of the raw samples (DC offset, in sample units).
    dc: f64,
    /// RMS of the AC part, dBFS.
    ac_rms_db: f64,
    /// Highest absolute sample, in sample units and dBFS.
    peak: i32,
    peak_db: f64,
    /// Crest factor: peak over RMS, in dB. Low values indicate compression or
    /// clipping, high values a peaky signal.
    crest_db: f64,
    /// 99.9th percentile of per-frame RMS (100 ms frames), dBFS: the loud part
    /// of the signal, insensitive to single-sample spikes.
    signal_db: f64,
    /// 10th percentile of per-frame RMS, dBFS: the noise floor between speech.
    noise_db: f64,
    /// Signal minus noise, in dB. Below ~10 dB the microphone is noise-bound.
    snr_db: f64,
    /// Fraction of samples at or beyond full scale.
    clipped_fraction: f64,
    /// Longest run of 100 ms frames below -60 dBFS, in seconds.
    longest_silence_s: f64,
    /// Energy split: below 1 kHz, 1-3 kHz and above 3 kHz, in dB relative to
    /// the total. Shows whether the mic is band-limited.
    band_low_db: f64,
    band_mid_db: f64,
    band_high_db: f64,
}

fn percentile(sorted: &[f64], fraction: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NEG_INFINITY;
    }
    let index = ((sorted.len() as f64 - 1.0) * fraction).round() as usize;
    sorted[index.min(sorted.len() - 1)]
}

fn audio_stats(path: &Path, sample_rate: u64) -> Result<Option<AudioStats>, String> {
    if sample_rate == 0 {
        return Ok(None);
    }
    let output = Command::new("ffmpeg")
        .args(["-hide_banner", "-nostdin", "-loglevel", "error", "-i"])
        .arg(path)
        .args(["-map", "0:a:0", "-f", "s16le", "-acodec", "pcm_s16le", "-"])
        .output()
        .map_err(|error| format!("ffmpeg audio decode: {error}"))?;
    if !output.status.success() || output.stdout.len() < 2 {
        return Ok(None);
    }
    let samples: Vec<i32> = output
        .stdout
        .chunks_exact(2)
        .map(|pair| i16::from_le_bytes([pair[0], pair[1]]) as i32)
        .collect();
    let count = samples.len() as f64;
    let mean = samples.iter().map(|&s| s as f64).sum::<f64>() / count;
    let ac_rms = (samples
        .iter()
        .map(|&s| (s as f64 - mean).powi(2))
        .sum::<f64>()
        / count)
        .sqrt();
    let peak = samples.iter().map(|s| s.abs()).max().unwrap_or(0);
    let clipped = samples.iter().filter(|s| s.abs() >= 32_760).count() as f64 / count;

    // Per-frame (100 ms) RMS gives a signal/noise split without a VAD.
    let frame = (sample_rate / 10).max(1) as usize;
    let mut frames: Vec<f64> = samples
        .chunks(frame)
        .filter(|chunk| !chunk.is_empty())
        .map(|chunk| {
            let power = chunk
                .iter()
                .map(|&s| (s as f64 - mean).powi(2))
                .sum::<f64>()
                / chunk.len() as f64;
            20.0 * (power.sqrt().max(1.0) / 32768.0).log10()
        })
        .collect();
    let mut longest_run = 0usize;
    let mut run = 0usize;
    for level in &frames {
        if *level < -60.0 {
            run += 1;
            longest_run = longest_run.max(run);
        } else {
            run = 0;
        }
    }
    let longest_silence_s = longest_run as f64 * 0.1;
    frames.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let signal_db = percentile(&frames, 0.999);
    let noise_db = percentile(&frames, 0.10);

    let (low, mid, high) = band_split(&samples, sample_rate as f64, mean);
    let total = low + mid + high;
    let to_db = |energy: f64| {
        if total > 0.0 {
            10.0 * (energy / total).max(1e-12).log10()
        } else {
            f64::NEG_INFINITY
        }
    };

    Ok(Some(AudioStats {
        seconds: count / sample_rate as f64,
        dc: mean,
        ac_rms_db: 20.0 * (ac_rms.max(1.0) / 32768.0).log10(),
        peak,
        peak_db: 20.0 * (peak.max(1) as f64 / 32768.0).log10(),
        crest_db: 20.0 * (peak.max(1) as f64 / ac_rms.max(1.0)).log10(),
        signal_db,
        noise_db,
        snr_db: signal_db - noise_db,
        clipped_fraction: clipped,
        longest_silence_s,
        band_low_db: to_db(low),
        band_mid_db: to_db(mid),
        band_high_db: to_db(high),
    }))
}

/// Split signal energy into three bands with one-pole filters (no FFT
/// dependency): below 1 kHz, 1-3 kHz, above 3 kHz. Energy is the sum of the
/// squared high-pass residuals, so the three parts are disjoint.
fn band_split(samples: &[i32], rate: f64, mean: f64) -> (f64, f64, f64) {
    let alpha = |cutoff: f64| {
        let dt = 1.0 / rate;
        let rc = 1.0 / (2.0 * std::f64::consts::PI * cutoff);
        dt / (rc + dt)
    };
    let a1k = alpha(1_000.0);
    let a3k = alpha(3_000.0);
    // y1 tracks the part of x below 1 kHz and y3 the part below 3 kHz, so the
    // bands are the differences between successive trackers.
    let (mut y1, mut y3) = (0.0f64, 0.0f64);
    let (mut low, mut mid, mut high) = (0.0f64, 0.0f64, 0.0f64);
    for &sample in samples {
        let x = sample as f64 - mean;
        y1 += a1k * (x - y1);
        y3 += a3k * (x - y3);
        let below_1k = y1;
        let between = y3 - y1;
        let above_3k = x - y3;
        low += below_1k * below_1k;
        mid += between * between;
        high += above_3k * above_3k;
    }
    (low, mid, high)
}

/// Analyze a recording directory and print a human report.
///
/// Expects `stream.mkv`, `video.mjpeg` (concatenated JPEG frames) and
/// optionally `audio.wav` in `outdir`. Writes `metrics.json` there and
/// returns a [`Verdict`].
pub fn report(outdir: &Path, opts: &Options) -> Result<Outcome, String> {
    let mkv = outdir.join("stream.mkv");
    let mjpeg = outdir.join("video.mjpeg");
    let probe = probe(&mkv)?;
    let video = probe.video().ok_or("no video stream in the recording")?.clone();
    let audio = probe.audio().cloned();
    let duration = number(&probe.format, "duration").unwrap_or(0.0);
    let frames = integer(&video, "nb_read_frames").unwrap_or(0);
    let fps = field(&video, "avg_frame_rate")
        .and_then(|value| {
            let (num, den) = value.split_once('/')?;
            let num: f64 = num.parse().ok()?;
            let den: f64 = den.parse().ok()?;
            (den != 0.0).then(|| num / den)
        })
        .unwrap_or(0.0);
    let bitrate = video_bitrate(&mkv, duration)?;

    let blur_log = ffmpeg_log(&mkv, "blurdetect")?;
    let block_log = ffmpeg_log(&mkv, "blockdetect")?;
    let freeze_log = ffmpeg_log(&mkv, "freezedetect=n=0.001:d=2")?;
    let black_log = ffmpeg_log(&mkv, "blackdetect=d=1:pix_th=0.10")?;
    let warnings = classify_warnings(&mkv)?;
    let jpeg = jpeg_stats(&mjpeg)?;
    let colour = signal_stats(&mkv)?;
    let audio_rate = audio
        .as_ref()
        .and_then(|stream| integer(stream, "sample_rate"))
        .unwrap_or(0);
    let levels = audio_stats(&mkv, audio_rate)?;

    let blur_mean = number_after(&blur_log, "blur mean:");
    let block_mean = number_after(&block_log, "block mean:");
    let freeze_total = sum_after(&freeze_log, "freeze_duration:").max(0.0);
    let black_total = sum_after(&black_log, "black_duration:").max(0.0);

    let mut checks: Vec<(String, bool, String)> = Vec::new();
    let mut warns: Vec<String> = Vec::new();
    let mut check = |name: String, ok: bool, detail: String| {
        checks.push((name, ok, detail));
    };
    check("container".into(), probe.format.contains_key("format_name"),
        field(&probe.format, "format_name").unwrap_or("?").to_string());
    check(
        format!("duration >= {} s", opts.min_duration),
        duration >= opts.min_duration,
        format!("{duration:.1}s"),
    );
    check("video codec mjpeg".into(), field(&video, "codec_name") == Some("mjpeg"),
        field(&video, "codec_name").unwrap_or("?").to_string());
    check(
        format!("video scale {}x{}", opts.width, opts.height),
        integer(&video, "width") == Some(opts.width as u64)
            && integer(&video, "height") == Some(opts.height as u64),
        format!(
            "{}x{}",
            field(&video, "width").unwrap_or("?"),
            field(&video, "height").unwrap_or("?")
        ),
    );
    check(
        format!("video frames >= {}", opts.min_frames),
        frames >= opts.min_frames,
        format!("{frames} (fps {fps:.1})"),
    );
    match &audio {
        Some(audio) => {
            check(
                format!("audio codec {}", opts.audio_codec),
                field(audio, "codec_name") == Some(opts.audio_codec.as_str()),
                field(audio, "codec_name").unwrap_or("?").to_string(),
            );
            check(
                format!("audio {} Hz mono", opts.audio_rate),
                integer(audio, "sample_rate") == Some(opts.audio_rate)
                    && integer(audio, "channels") == Some(1),
                format!(
                    "{} Hz, {} ch",
                    field(audio, "sample_rate").unwrap_or("?"),
                    field(audio, "channels").unwrap_or("?")
                ),
            );
        }
        None => check("audio stream present".into(), false, "missing".into()),
    }

    if let Some(levels) = &levels {
        if duration > 0.0 {
            let ratio = levels.seconds / duration;
            check(
                format!(
                    "audio duration matches video ({:.2}s vs {:.2}s)",
                    levels.seconds, duration
                ),
                (0.9..=1.1).contains(&ratio),
                format!("ratio {ratio:.2}"),
            );
        }
        if opts.require_audio {
            check(
                "audio not silent".into(),
                levels.ac_rms_db > -60.0,
                format!("{:.1} dBFS AC RMS", levels.ac_rms_db),
            );
        }
        check(
            "audio not clipping".into(),
            levels.clipped_fraction < 0.001,
            format!("{:.3}% at full scale", levels.clipped_fraction * 100.0),
        );
        check(
            "audio has headroom".into(),
            levels.peak_db < -1.0,
            format!("peak {:.1} dBFS", levels.peak_db),
        );
        if levels.snr_db < 10.0 {
            warns.push(format!(
                "audio SNR {:.1} dB (noise floor {:.1} dBFS) - microphone is noise-bound \
                 even without a signal",
                levels.snr_db, levels.noise_db
            ));
        }
        if levels.crest_db < 6.0 {
            warns.push(format!("crest factor {:.1} dB - signal sounds compressed", levels.crest_db));
        }
        if levels.band_high_db < -40.0 && levels.band_low_db > -3.0 {
            warns.push(format!(
                "audio is band-limited below 3 kHz (>3 kHz at {:.1} dB) - 48 kHz buys nothing",
                levels.band_high_db
            ));
        }
        if levels.ac_rms_db <= -60.0 {
            warns.push(format!(
                "audio is effectively silent ({:.1} dBFS) - quiet room?",
                levels.ac_rms_db
            ));
        }
        if levels.ac_rms_db < -40.0 {
            warns.push(format!(
                "audio is quiet ({:.1} dBFS AC RMS, peak {})",
                levels.ac_rms_db, levels.peak
            ));
        }
    }

    let corrupt_ratio = if frames > 0 {
        warnings.corrupt_dc as f64 / frames as f64
    } else {
        1.0
    };
    check("jpeg payload clean (<10% corrupt frames)".into(), corrupt_ratio <= 0.10,
        format!("{}/{} frames ({:.0}%) with corrupt DC", warnings.corrupt_dc, frames, corrupt_ratio * 100.0));
    check("jpeg header valid".into(), warnings.invalid_size == 0,
        format!("{} frames with invalid picture size", warnings.invalid_size));

    if jpeg.truncated > 0 {
        warns.push(format!(
            "{} truncated JPEG frame(s) - green bands",
            jpeg.truncated
        ));
    }
    if jpeg.frames > 0 && jpeg.short as f64 > 0.10 * jpeg.frames as f64 {
        warns.push(format!("{} short frames ({} of median size)", jpeg.short, jpeg.median));
    }
    if jpeg.garbage > 1024 {
        warns.push(format!("{} stray bytes between frames", jpeg.garbage));
    }
    if let Some(blur) = blur_mean {
        if blur > 15.0 {
            warns.push(format!("blur mean {blur:.1} (> 15 is soft)"));
        }
    }
    if let Some(block) = block_mean {
        if block > 200.0 {
            warns.push(format!("block mean {block:.1} (> 200 is blocky)"));
        }
    }
    if freeze_total > 5.0 {
        warns.push(format!("{freeze_total:.1} s frozen"));
    }
    if black_total > 5.0 {
        warns.push(format!("{black_total:.1} s black"));
    }
    if warnings.total() > 20 {
        warns.push(format!(
            "{} decoder warnings (dc={} size={} overread={} eoi={})",
            warnings.total(), warnings.corrupt_dc, warnings.invalid_size,
            warnings.overread, warnings.eoi_missing
        ));
    }
    if colour.frames > 0 && colour.outlier_frames as f64 > 0.05 * colour.frames as f64 {
        warns.push(format!(
            "{}/{} frames are >3 sigma colour outliers (corruption)",
            colour.outlier_frames, colour.frames
        ));
    }
    if !(50_000.0..=3_000_000.0).contains(&bitrate) {
        warns.push(format!("video bitrate {:.0} kb/s outside 50-3000", bitrate / 1000.0));
    }

    println!("file            {}  ({:.2} MB)", mkv.display(),
        std::fs::metadata(&mkv).map(|m| m.len()).unwrap_or(0) as f64 / 1e6);
    println!("duration        {duration:.1} s");
    println!("video           {} {}x{} @ {fps:.1} fps, {frames} frames, {:.0} kb/s",
        field(&video, "codec_name").unwrap_or("?"), field(&video, "width").unwrap_or("?"),
        field(&video, "height").unwrap_or("?"), bitrate / 1000.0);
    if let Some(audio) = &audio {
        let rate = integer(audio, "sample_rate").unwrap_or(0);
        let channels = integer(audio, "channels").unwrap_or(0);
        let audio_bitrate = integer(audio, "bit_rate")
            .unwrap_or_else(|| rate.saturating_mul(channels).saturating_mul(8));
        println!("audio           {} {} Hz {} ch {} kb/s",
            field(audio, "codec_name").unwrap_or("?"), rate, channels,
            audio_bitrate as f64 / 1000.0);
    }
    if let Some(levels) = &levels {
        println!(
            "audio level     RMS {:.1} dBFS, peak {:.1} dBFS ({}), crest {:.1} dB, DC {:.0}",
            levels.ac_rms_db, levels.peak_db, levels.peak, levels.crest_db, levels.dc
        );
        println!(
            "audio noise     floor {:.1} dBFS, signal {:.1} dBFS, SNR {:.1} dB, clip {:.3}%",
            levels.noise_db, levels.signal_db, levels.snr_db, levels.clipped_fraction * 100.0
        );
        println!(
            "audio spectrum  <1kHz {:.1} dB, 1-3kHz {:.1} dB, >3kHz {:.1} dB, silence {:.1} s",
            levels.band_low_db, levels.band_mid_db, levels.band_high_db,
            levels.longest_silence_s
        );
    }
    println!("jpeg frames     {} ({} B median, {}-{} B)", jpeg.frames, jpeg.median, jpeg.min, jpeg.max);
    println!("jpeg integrity  {} truncated, {} short, {} stray B, dims {}",
        jpeg.truncated, jpeg.short, jpeg.garbage, dims_text(&jpeg.dims));
    println!("sharpness       blur mean {:?} (lower is sharper)", blur_mean);
    println!("blockiness      block mean {:?} (0-100, lower is cleaner)", block_mean);
    println!("freeze / black  {freeze_total:.1} s / {black_total:.1} s");
    println!("decoder         {} warnings: {} corrupt DC, {} invalid size, {} overread, {} EOI missing",
        warnings.total(), warnings.corrupt_dc, warnings.invalid_size,
        warnings.overread, warnings.eoi_missing);
    println!("colour          {}/{} frames >3 sigma; YAVG {:?} SATAVG {:?}",
        colour.outlier_frames, colour.frames, colour.yavg_mean, colour.satavg_mean);

    println!("\nchecks:");
    let mut failed = 0;
    for (name, ok, detail) in &checks {
        println!("  [{}] {name}: {detail}", if *ok { "PASS" } else { "FAIL" });
        if !ok {
            failed += 1;
        }
    }
    for warn in &warns {
        println!("  [WARN] {warn}");
    }

    let mut json = String::new();
    let _ = writeln!(json, "{{");
    let _ = writeln!(json, "  \"duration_s\": {duration:.3},");
    let _ = writeln!(json, "  \"video\": {{\"codec\": \"{}\", \"width\": {}, \"height\": {}, \"fps\": {fps:.3}, \"frames\": {frames}, \"bitrate_bps\": {bitrate:.0}}},",
        json_escape(field(&video, "codec_name").unwrap_or("")),
        integer(&video, "width").unwrap_or(0),
        integer(&video, "height").unwrap_or(0));
    match &audio {
        Some(audio) => {
            let rate = integer(audio, "sample_rate").unwrap_or(0);
            let channels = integer(audio, "channels").unwrap_or(0);
            let audio_bitrate = integer(audio, "bit_rate")
                .unwrap_or_else(|| rate.saturating_mul(channels).saturating_mul(8));
            let _ = writeln!(json, "  \"audio\": {{\"codec\": \"{}\", \"rate\": {}, \"channels\": {}, \"bitrate\": {}}},",
                json_escape(field(audio, "codec_name").unwrap_or("")),
                rate, channels, audio_bitrate);
        }
        None => {
            let _ = writeln!(json, "  \"audio\": null,");
        }
    }
    if let Some(levels) = &levels {
        let _ = writeln!(
            json,
            "  \"audio_level\": {{\"seconds\": {:.3}, \"rms_db\": {:.2}, \"peak\": {}, \"peak_db\": {:.2}, \"crest_db\": {:.2}, \"dc\": {:.1}, \"noise_db\": {:.2}, \"signal_db\": {:.2}, \"snr_db\": {:.2}, \"clipped_fraction\": {:.5}, \"silence_s\": {:.2}, \"band_low_db\": {:.2}, \"band_mid_db\": {:.2}, \"band_high_db\": {:.2}}},",
            levels.seconds, levels.ac_rms_db, levels.peak, levels.peak_db,
            levels.crest_db, levels.dc, levels.noise_db, levels.signal_db,
            levels.snr_db, levels.clipped_fraction, levels.longest_silence_s,
            levels.band_low_db, levels.band_mid_db, levels.band_high_db
        );
    }
    let _ = writeln!(json, "  \"jpeg\": {{\"frames\": {}, \"truncated\": {}, \"short\": {}, \"garbage_bytes\": {}, \"median_bytes\": {}, \"dimensions\": \"{}\"}},",
        jpeg.frames, jpeg.truncated, jpeg.short, jpeg.garbage, jpeg.median, dims_text(&jpeg.dims));
    let _ = writeln!(json, "  \"blur_mean\": {}, \"block_mean\": {},",
        blur_mean.map_or("null".to_string(), |v| format!("{v:.4}")),
        block_mean.map_or("null".to_string(), |v| format!("{v:.4}")));
    let _ = writeln!(json, "  \"freeze_s\": {freeze_total:.3}, \"black_s\": {black_total:.3},");
    let _ = writeln!(json, "  \"decode_warnings\": {},", warnings.total());
    let _ = writeln!(json, "  \"colour\": {{\"frames\": {}, \"outlier_frames\": {}}},",
        colour.frames, colour.outlier_frames);
    let _ = writeln!(json, "  \"failed_checks\": {failed}");
    let _ = writeln!(json, "}}");
    std::fs::write(outdir.join("metrics.json"), json).map_err(|error| error.to_string())?;

    let failed_names: Vec<String> = checks
        .iter()
        .filter(|(_, ok, _)| !ok)
        .map(|(name, _, _)| name.clone())
        .collect();
    if failed > 0 {
        println!("\nRESULT: FAIL ({failed} check(s))");
        Ok(Outcome {
            verdict: Verdict::Fail,
            failed: failed_names,
            warnings: warns,
        })
    } else {
        println!("\nRESULT: PASS{}", if warns.is_empty() { "" } else { " with warnings" });
        Ok(Outcome {
            verdict: Verdict::Pass,
            failed: Vec::new(),
            warnings: warns,
        })
    }
}
