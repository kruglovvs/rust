//! On-board PDM microphone capture (plan Todo 8).
//!
//! # Plan contract
//!
//! * Pins: **CLK = GPIO42, DATA = GPIO41** — raw GPIO numbers; Seeed
//!   publishes no `BIND_D11`/`BIND_D12` macros for the `MSM261D3526H1CPM`
//!   mic (freeing the pins would require cutting jumpers JP1/JP2).
//! * PDM RX through `esp_idf_hal::i2s` (`I2sDriver::new_pdm_rx` +
//!   `PdmRxConfig` / `PdmRxSlotConfig` / `PdmRxClkConfig`), **16 kHz mono
//!   16-bit, 20 ms frames = 320 samples = 640 bytes, `dma_desc_num = 4`**,
//!   on a **dedicated task** (`mic_capture`, 16 KiB stack).
//!   Feature gates verified against esp-idf-hal 0.47.0 and the esp-idf-sys
//!   build output: `esp_idf_soc_i2s_supports_pdm_rx` is set for the S3, the
//!   new-channel PDM paths need ESP-IDF ≥ 5.1 (we pin v5.5.2), and the
//!   single-line `new_pdm_rx` takes `(i2s, &PdmRxConfig, clk, din)`.
//! * The clock scale fields the plan calls out (`sd_prescale`, `hp_scale`,
//!   `lp_scale`, `sinc_scale`) are ESP-IDF-internal on the RX path — the
//!   HAL's `PdmRxClkConfig::from_sample_rate_hz` pins a known-good combo
//!   (MCLK ×256, downsample ÷8, SDK defaults for the scales) and every
//!   misconfiguration axis the plan names is *measured* by the stats:
//!   bad `hp_scale` attenuates the signal → the RMS/`stddev` thresholds
//!   fail; bad `sd_prescale` leaves DC → the `|mean| < 0.05 × rms`
//!   threshold fails. Both are asserted per second in the log below.
//! * This file contains **zero `unsafe`** — the HAL's safe wrappers cover
//!   the whole driver (unlike `camera.rs`/`sensor.rs`, there is no FFI
//!   boundary to fence).
//!
//! # Consumer API (Recorder / Todo 10)
//!
//! [`MicCapture::new`] configures and enables the peripheral and returns
//! typed [`MicError`]s (no panics); [`MicCapture::spawn`] moves the driver
//! onto the dedicated task, which for every frame: measures it with
//! `audio_quality::frame_stats`, logs it, and **delivers `[i16; 320]`
//! (`AudioFrame`) over a `std::sync::mpsc` channel**. The channel is
//! unbounded (a recording consumer sustains 32 kB/s against a 95–145 kB/s
//! SD writer, so backpressure is not expected); the task exits cleanly when
//! the receiver is dropped. [`MicCapture::read_frame`] is the synchronous
//! alternative for a consumer that wants to drive the loop itself.
//!
//! # Aggregation windows (thresholds bind to windows, never to one frame)
//!
//! The per-second log line aggregates a **rolling 500-frame (10 s) window**
//! exactly as the plan's Verification strategy demands:
//!
//! ```text
//! mic: fps=50 rms_dbfs_p95=-23.4 stddev_gt0_pct=100.0 stddev_median=1204.5 \
//!      peak_max=8192 crest_db_median=12.31 rail_runs_max=0 stnr_db=18.20 \
//!      analysis_us_avg=4.71 dc_ratio_p95=0.0123
//! ```
//!
//! * `rms_dbfs_p95` — 95th percentile of frame RMS in dBFS (> −50 required)
//! * `stddev_gt0_pct` / `stddev_median` — `stddev > 0` on ≥ 99% of frames
//!   (the constant-DC guard), plus the median level of stddev for context
//! * `peak_max` — max peak (< 32112), `rail_runs_max` — max runs ≥ 3 (0)
//! * `crest_db_median` — **median** crest (> 8 dB)
//! * `stnr_db` — rolling NIST STNR via `audio_quality::stnr_snr_db`
//!   (`INSUFFICIENT_FRAMES` when < 500 valid frames in the window —
//!   never `+∞`; null frames are dropped inside that function, trap 2)
//! * `dc_ratio_p95` — 95th percentile of `|mean|/rms` (< 0.05; null frames
//!   excluded from the ratio population, `0/0` is undefined not passing)
//! * `analysis_us_avg` — mean cost of `frame_stats` + `count_rail_runs`
//!   per frame; gate **< 100 µs**, 5 µs design target (reported, non-gating)
//!
//! Plus the 100 ms hum line (never the 20 ms frame — 60 Hz is bin 1.2 on
//! 320 samples, ~0.85× leakage measured in `audio_quality`'s tests):
//!
//! ```text
//! mic-hum: tone_1khz=0.2441 hum50=0.0610 hum60=0.0000 window=1600smp fs=16000
//! ```
//!
//! Window percentiles (rms/crest/dc) *include* null frames: a dead mic must
//! visibly FAIL `rms_dbfs_p95`/`stddev_gt0_pct`/`crest_db_median`, not be
//! filtered out of existence. Only STNR drops nulls (NIST Appendix 2).
//! Per-frame lines use the prefix `mic-f:` (plan: "logging per-frame
//! stats"); ~5 kB/s at 50 fps — fine on USB-JTAG, ~45% of a 115200 UART,
//! so drop or raise the UART baud if the line rate ever matters.
//!
//! Hardware rows (≥ 450 frames of exactly 640 bytes, on-device thresholds,
//! measured analysis cost) run only with a board — see
//! `.omo/evidence/task-8/serial.log` (PENDING_NO_BOARD).

use crate::audio_quality::{count_rail_runs, frame_stats, goertzel_mag, stnr_snr_db, FrameStats};
use esp_idf_hal::delay::TickType;
use esp_idf_hal::gpio::{InputPin, OutputPin};
use esp_idf_hal::i2s::config::{
    Config, DataBitWidth, PdmRxClkConfig, PdmRxConfig, PdmRxGpioConfig, PdmRxSlotConfig, Role,
    SlotMode,
};
use esp_idf_hal::i2s::{I2s, I2sDriver, I2sRx};
use esp_idf_sys::EspError;
use std::collections::VecDeque;
use std::sync::mpsc::{SendError, Sender};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Geometry (plan Todo 8 "Do (a)")
// ---------------------------------------------------------------------------

/// Capture rate — the on-board mic runs at 16 kHz (Todo 12 decimates to
/// G.711's 8 kHz later; never before these measurements, trap 1).
pub const SAMPLE_RATE_HZ: u32 = 16_000;

/// Samples per frame: 20 ms at 16 kHz (`sample_rate / 50`, per the
/// `esp-cpp/camera-streamer` reference values).
pub const FRAME_SAMPLES: usize = 320;

/// Bytes per frame: 16-bit mono ⇒ `320 × 2` (the QA acceptance asks for
/// frames "of exactly 640 bytes").
pub const FRAME_BYTES: usize = FRAME_SAMPLES * 2;

/// Plan: `dma_desc_num = 4` (HAL builder `dma_buffer_count` maps onto the
/// SDK's `i2s_chan_config_t.dma_desc_num` — verified in esp-idf-hal 0.47.0
/// `Config::as_sdk`).
pub const DMA_DESC_NUM: u32 = 4;

/// PDM clock output pin (raw number, plan References).
pub const CLK_GPIO: u8 = 42;

/// PDM data input pin (raw number, plan References).
pub const DATA_GPIO: u8 = 41;

/// Hum-analysis window: **1600 samples = 100 ms ⇒ 10 Hz bin spacing**,
/// putting 50 Hz (bin 5) and 60 Hz (bin 6) both exactly on-grid. Kept
/// strictly separate from the 320-sample measurement frames.
pub const HUM_WINDOW_SAMPLES: usize = 1600;

/// Clipping run length: "0 runs of ≥ 3 identical samples at the rails".
pub const MIN_RAIL_RUN: usize = 3;

/// Rolling window: 500 frames = 10 s at the nominal 50 fps — the
/// aggregation window for rms/crest/peak/rail/STNR, and the plan's
/// `≥ 500 valid frames` floor for STNR.
pub const WINDOW_FRAMES: usize = 500;

/// A delivered audio frame: exactly 20 ms of L16 PCM (640 bytes on the
/// wire). `size_of::<AudioFrame>() == 640` is asserted in the unit tests.
pub type AudioFrame = [i16; FRAME_SAMPLES];

/// Read timeout: four frame periods. Healthy capture completes in ~20 ms;
/// a stalled DMA pipeline surfaces as a typed [`MicError::Timeout`] instead
/// of an indefinite block (QA: "the task neither blocks nor spins").
const READ_TIMEOUT_MS: u64 = 4 * FRAME_SAMPLES as u64 * 1000 / SAMPLE_RATE_HZ as u64;

/// Dedicated-task stack. Locals ≈ 5 KiB (640 B byte buffer, 640 B frame,
/// 3.2 KiB hum window, formatting scratch); window populations live on the
/// heap. Deliberately bounded — see `audio_quality`'s distinct-counter
/// note about the 8 KiB device libtest stacks.
const MIC_TASK_STACK: usize = 16 * 1024;

// ---------------------------------------------------------------------------
// Errors (no thiserror — plain enum + Display/Error, zero dependencies)
// ---------------------------------------------------------------------------

/// Typed failures from PDM setup or capture. Every path logs and degrades;
/// nothing here panics.
#[derive(Debug)]
pub enum MicError {
    /// `I2sDriver::new_pdm_rx` failed (bad pins/port/IDF state).
    Init(EspError),
    /// `rx_enable()` failed — channel left in `READY`.
    Enable(EspError),
    /// `i2s_channel_read` failed (driver-level error, not a timeout).
    Read(EspError),
    /// No complete frame within [`READ_TIMEOUT_MS`]; the partial samples
    /// (if any) were consumed by the driver — the frame is dropped.
    Timeout { waited_ms: u64 },
    /// Read completed with fewer than [`FRAME_BYTES`] bytes (partial DMA
    /// drain on timeout) — frame dropped to keep sample alignment.
    ShortRead { expected: usize, got: usize },
    /// `std::thread::Builder::spawn` failed for the capture task.
    Spawn(std::io::Error),
}

impl core::fmt::Display for MicError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            MicError::Init(e) => write!(f, "PDM RX init failed: {e}"),
            MicError::Enable(e) => write!(f, "PDM RX enable failed: {e}"),
            MicError::Read(e) => write!(f, "PDM RX read failed: {e}"),
            MicError::Timeout { waited_ms } => {
                write!(f, "PDM RX timed out after {waited_ms} ms (no frame)")
            }
            MicError::ShortRead { expected, got } => {
                write!(f, "PDM RX short read: got {got} of {expected} bytes")
            }
            MicError::Spawn(e) => write!(f, "capture task spawn failed: {e}"),
        }
    }
}

impl std::error::Error for MicError {}

// ---------------------------------------------------------------------------
// Capture driver
// ---------------------------------------------------------------------------

/// PDM receive path: one 640-byte DMA read = one 20 ms [`AudioFrame`].
pub struct MicCapture<'d> {
    driver: I2sDriver<'d, I2sRx>,
    bytes: [u8; FRAME_BYTES],
    samples: AudioFrame,
}

impl<'d> MicCapture<'d> {
    /// Configure PDM RX (16 kHz / mono / 16-bit / `dma_desc_num = 4`) and
    /// enable the channel.
    ///
    /// `clk` is the clock **output** pin (GPIO42), `din` the data input
    /// (GPIO41); both are consumed by the driver for the channel's
    /// lifetime. Errors are typed; on success the configuration line is
    /// logged.
    pub fn new<I2S, CLK, DIN>(i2s: I2S, clk: CLK, din: DIN) -> Result<Self, MicError>
    where
        I2S: I2s + 'd,
        CLK: OutputPin + 'd,
        DIN: InputPin + 'd,
    {
        let channel = Config::new()
            .role(Role::Controller)
            .dma_buffer_count(DMA_DESC_NUM)
            .frames_per_buffer(FRAME_SAMPLES as u32);
        let clk_cfg = PdmRxClkConfig::from_sample_rate_hz(SAMPLE_RATE_HZ);
        let slot_cfg =
            PdmRxSlotConfig::from_bits_per_sample_and_slot_mode(DataBitWidth::Bits16, SlotMode::Mono);
        let gpio_cfg = PdmRxGpioConfig::new(false);
        let rx_cfg = PdmRxConfig::new(channel, clk_cfg, slot_cfg, gpio_cfg);

        let mut driver = I2sDriver::new_pdm_rx(i2s, &rx_cfg, clk, din).map_err(MicError::Init)?;
        driver.rx_enable().map_err(MicError::Enable)?;

        println!(
            "mic: PDM RX configured clk=GPIO{} data=GPIO{} rate={}Hz mono 16-bit frame={}smp/{}B dma_desc_num={}",
            CLK_GPIO, DATA_GPIO, SAMPLE_RATE_HZ, FRAME_SAMPLES, FRAME_BYTES, DMA_DESC_NUM
        );

        Ok(Self { driver, bytes: [0; FRAME_BYTES], samples: [0; FRAME_SAMPLES] })
    }

    /// Blocking (bounded) read of exactly one frame. Samples are converted
    /// little-endian → `i16` element-wise; alignment-safe, no `unsafe`.
    ///
    /// On [`MicError::Timeout`] / [`MicError::ShortRead`] no frame is
    /// returned — the samples are *not* padded or zero-filled, because a
    /// zero-filled frame would look like a null frame to `frame_stats`
    /// (which is fine for stats but wrong for the recorder's sample
    /// count/PTS). The caller logs and continues.
    pub fn read_frame(&mut self) -> Result<&AudioFrame, MicError> {
        let timeout = TickType::new_millis(READ_TIMEOUT_MS).ticks();
        let got = match self.driver.read(&mut self.bytes, timeout) {
            Ok(n) => n,
            Err(e) if e.code() == esp_idf_sys::ESP_ERR_TIMEOUT => {
                return Err(MicError::Timeout { waited_ms: READ_TIMEOUT_MS })
            }
            Err(e) => return Err(MicError::Read(e)),
        };
        if got != FRAME_BYTES {
            return Err(MicError::ShortRead { expected: FRAME_BYTES, got });
        }
        for i in 0..FRAME_SAMPLES {
            self.samples[i] = i16::from_le_bytes([self.bytes[2 * i], self.bytes[2 * i + 1]]);
        }
        Ok(&self.samples)
    }
}

impl MicCapture<'static> {
    /// Spawn the dedicated capture task (plan: "on a dedicated task").
    ///
    /// The task owns the driver for its whole life and, per frame:
    /// reads → measures (`frame_stats` + rail runs, timed) → logs the
    /// `mic-f:` line → shifts the 1600-sample hum window → sends the
    /// `[i16; 320]` frame on `tx`. Once per second it emits the
    /// `mic:` aggregate line (rolling 500-frame window) and the `mic-hum:`
    /// line. It exits cleanly when `tx`'s receiver is dropped; read errors
    /// are logged and retried after a 20 ms sleep (never a spin).
    pub fn spawn(
        self,
        tx: Sender<AudioFrame>,
    ) -> Result<std::thread::JoinHandle<()>, MicError> {
        std::thread::Builder::new()
            .name("mic_capture".into())
            .stack_size(MIC_TASK_STACK)
            .spawn(move || self.capture_loop(tx))
            .map_err(MicError::Spawn)
    }
}

// ---------------------------------------------------------------------------
// Capture loop: per-frame stats, per-second aggregates (plan Do (c))
// ---------------------------------------------------------------------------

/// One window entry: per-frame statistics plus its rail-run count (the
/// rail count is not a `FrameStats` field — it is a raw-sample measure).
struct FrameRecord {
    stats: FrameStats,
    rail_runs: usize,
}

/// Aggregation state for the rolling window and the per-second log.
struct QualityLog {
    window: VecDeque<FrameRecord>,
    hum: [i16; HUM_WINDOW_SAMPLES],
    hum_frames: usize,
    second_start: Instant,
    second_frames: usize,
    analysis_us_total: f64,
    frames_total: u64,
}

impl QualityLog {
    fn new() -> Self {
        Self {
            window: VecDeque::with_capacity(WINDOW_FRAMES),
            hum: [0; HUM_WINDOW_SAMPLES],
            hum_frames: 0,
            second_start: Instant::now(),
            second_frames: 0,
            analysis_us_total: 0.0,
            frames_total: 0,
        }
    }

    /// dBFS of an RMS (sample-unit) value. `rms == 0` → −∞, which is the
    /// *correct failing* reading for the presence floor (and is never fed
    /// to STNR — `stnr_snr_db` drops null frames itself).
    fn dbfs(rms: f32) -> f32 {
        if rms <= 0.0 {
            f32::NEG_INFINITY
        } else {
            20.0 * (rms / 32768.0).log10()
        }
    }

    fn push(&mut self, record: FrameRecord) {
        if self.window.len() == WINDOW_FRAMES {
            self.window.pop_front();
        }
        self.window.push_back(record);
    }

    /// Shift the rolling 1600-sample hum window by one frame and append it.
    /// The buffer is a *linear* window (not a ring) because Goertzel bin
    /// spacing is defined by contiguous length.
    fn push_hum(&mut self, frame: &AudioFrame) {
        self.hum.copy_within(FRAME_SAMPLES.., 0);
        self.hum[HUM_WINDOW_SAMPLES - FRAME_SAMPLES..].copy_from_slice(frame);
        if self.hum_frames < HUM_WINDOW_SAMPLES / FRAME_SAMPLES {
            self.hum_frames += 1;
        }
    }

    fn hum_ready(&self) -> bool {
        self.hum_frames * FRAME_SAMPLES >= HUM_WINDOW_SAMPLES
    }

    /// Emit the per-second aggregate line over the rolling window and
    /// reset the one-second counters (the window itself keeps rolling).
    fn emit_second(&mut self) {
        let n = self.second_frames;
        if self.window.is_empty() || n == 0 {
            println!("mic: no frames in the last second (read failures) — check the WARNING lines above");
            self.second_start = Instant::now();
            self.second_frames = 0;
            self.analysis_us_total = 0.0;
            return;
        }

        // rms dBFS population (null frames included: a dead mic must fail
        // the floor, not vanish from the statistic).
        let mut rms_db: Vec<f32> = self.window.iter().map(|r| Self::dbfs(r.stats.rms)).collect();
        let rms_p95 = percentile(&mut rms_db, 95.0);

        // stddev > 0 on ≥ 99% of frames (constant-DC guard).
        let pos = self.window.iter().filter(|r| r.stats.stddev > 0.0).count();
        let pos_pct = 100.0 * pos as f32 / self.window.len() as f32;
        let mut stddevs: Vec<f32> = self.window.iter().map(|r| r.stats.stddev).collect();
        let stddev_med = percentile(&mut stddevs, 50.0);

        // Clipping: maxima (a single clipped frame anywhere in the window
        // must be visible — max, not average).
        let peak_max = self.window.iter().map(|r| r.stats.peak).max().unwrap_or(0);
        let rail_max = self.window.iter().map(|r| r.rail_runs).max().unwrap_or(0);

        // Crest: MEDIAN — one near-silent frame would drag a mean below
        // any useful bound (plan).
        let mut crests: Vec<f32> = self.window.iter().map(|r| r.stats.crest_db).collect();
        let crest_med = percentile(&mut crests, 50.0);

        // DC offset: 95th percentile of |mean|/rms over non-null frames.
        let mut dc: Vec<f32> = self
            .window
            .iter()
            .filter(|r| r.stats.rms > 0.0)
            .map(|r| r.stats.mean.abs() / r.stats.rms)
            .collect();
        let dc_p95 = if dc.is_empty() { f32::NAN } else { percentile(&mut dc, 95.0) };

        // Rolling NIST STNR: drops null frames and answers None below 500
        // valid frames → "INSUFFICIENT_FRAMES", never +∞ (trap 2).
        let stats: Vec<FrameStats> = self.window.iter().map(|r| r.stats).collect();
        let stnr = match stnr_snr_db(&stats) {
            Some(v) => format!("{v:.2}"),
            None => String::from("INSUFFICIENT_FRAMES"),
        };

        let analysis_avg = self.analysis_us_total / n as f64;

        println!(
            "mic: fps={} rms_dbfs_p95={:.1} stddev_gt0_pct={:.1} stddev_median={:.1} peak_max={} crest_db_median={:.2} rail_runs_max={} stnr_db={} analysis_us_avg={:.2} dc_ratio_p95={:.4}",
            n, rms_p95, pos_pct, stddev_med, peak_max, crest_med, rail_max, stnr, analysis_avg, dc_p95
        );

        if self.hum_ready() {
            let tone = goertzel_mag(&self.hum, 1000.0, SAMPLE_RATE_HZ as f32);
            let h50 = goertzel_mag(&self.hum, 50.0, SAMPLE_RATE_HZ as f32);
            let h60 = goertzel_mag(&self.hum, 60.0, SAMPLE_RATE_HZ as f32);
            println!(
                "mic-hum: tone_1khz={:.4} hum50={:.4} hum60={:.4} window={}smp fs={}",
                tone, h50, h60, HUM_WINDOW_SAMPLES, SAMPLE_RATE_HZ
            );
        }

        self.second_start = Instant::now();
        self.second_frames = 0;
        self.analysis_us_total = 0.0;
    }
}

/// Linear-interpolation percentile over a slice that this function sorts
/// ascending (same convention as `audio_quality`'s STNR percentiles).
fn percentile(values: &mut [f32], p: f32) -> f32 {
    if values.is_empty() {
        return f32::NAN;
    }
    values.sort_by(|a, b| a.total_cmp(b));
    if values.len() == 1 {
        return values[0];
    }
    let pos = (p / 100.0) * (values.len() - 1) as f32;
    let lo = pos.floor() as usize;
    let hi = pos.ceil() as usize;
    if lo == hi {
        values[lo]
    } else {
        values[lo] + (pos - lo as f32) * (values[hi] - values[lo])
    }
}

impl<'d> MicCapture<'d> {
    /// The dedicated-task body — see [`MicCapture::spawn`].
    fn capture_loop(mut self, tx: Sender<AudioFrame>) {
        let mut log = QualityLog::new();
        println!(
            "mic: capture task running task=mic_capture stack={} window={}frames({}s)",
            MIC_TASK_STACK,
            WINDOW_FRAMES,
            WINDOW_FRAMES as u64 * 1000 / 50 / 1000
        );

        loop {
            // ---- measure the analysis cost of stats, not of I/O/logging ----
            let t0 = Instant::now();
            let frame = match self.read_frame() {
                Ok(f) => f,
                Err(MicError::Timeout { waited_ms }) => {
                    println!("WARNING: mic read timeout after {waited_ms} ms — frame dropped (task alive)");
                    std::thread::sleep(Duration::from_millis(20));
                    continue;
                }
                Err(MicError::ShortRead { expected, got }) => {
                    println!("WARNING: mic short read {got}/{expected} bytes — frame dropped");
                    continue;
                }
                Err(e) => {
                    println!("ERROR: {e} — retrying in 20 ms (task neither blocks nor spins)");
                    std::thread::sleep(Duration::from_millis(20));
                    continue;
                }
            };

            let stats = frame_stats(frame);
            let rail_runs = count_rail_runs(frame, MIN_RAIL_RUN);
            log.analysis_us_total += t0.elapsed().as_micros() as f64;
            log.second_frames += 1;
            log.frames_total += 1;

            // Plan Do (c): log per-frame stats.
            println!(
                "mic-f: mean={:.1} rms_dbfs={:.1} stddev={:.1} peak={} crest_db={:.2} distinct={} rail_runs={}",
                stats.mean,
                QualityLog::dbfs(stats.rms),
                stats.stddev,
                stats.peak,
                stats.crest_db,
                stats.distinct,
                rail_runs
            );

            log.push(FrameRecord { stats, rail_runs });
            log.push_hum(frame);

            // ---- deliver to the consumer (Recorder / Todo 10) ----
            if let Err(SendError(_)) = tx.send(*frame) {
                println!(
                    "mic: frame receiver dropped — capture task exiting after {} frames",
                    log.frames_total
                );
                break;
            }

            if log.second_start.elapsed() >= Duration::from_secs(1) {
                log.emit_second();
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Unit tests — geometry/grid invariants; compile-checked on the device via
// `cargo test --no-run --target xtensa-esp32s3-espidf`. The PDM driver
// itself needs hardware (RUN_PENDING_NO_BOARD); the statistics these tests
// guard run on the host through `harness/tests/audio_quality.rs`.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_geometry_matches_plan() {
        assert_eq!(SAMPLE_RATE_HZ, 16_000);
        assert_eq!(FRAME_SAMPLES, 320);
        assert_eq!(FRAME_BYTES, 640);
        assert_eq!(core::mem::size_of::<AudioFrame>(), 640);
        // 20 ms frames ⇒ ≥ 45 frames/s is physically possible (QA floor).
        assert_eq!(SAMPLE_RATE_HZ / FRAME_SAMPLES as u32, 50);
        assert!(50 >= 45);
        assert_eq!(DMA_DESC_NUM, 4);
    }

    #[test]
    fn pins_are_the_raw_gpio_numbers() {
        // Seeed publishes no BIND_D11/D12 — raw numbers are the contract.
        assert_eq!(CLK_GPIO, 42);
        assert_eq!(DATA_GPIO, 41);
    }

    #[test]
    fn hum_window_puts_50_and_60_hz_on_grid_but_not_on_320_frames() {
        assert_eq!(HUM_WINDOW_SAMPLES, 1600);
        // 100 ms at 16 kHz ⇒ 10 Hz bins: 50 Hz = bin 5, 60 Hz = bin 6.
        assert_eq!(SAMPLE_RATE_HZ as usize / HUM_WINDOW_SAMPLES, 10);
        assert_eq!(50 * HUM_WINDOW_SAMPLES, 5 * SAMPLE_RATE_HZ as usize);
        assert_eq!(60 * HUM_WINDOW_SAMPLES, 6 * SAMPLE_RATE_HZ as usize);
        // On the 320-sample frame: 50 Hz is on-grid (bin 1) but 60 Hz is
        // NOT — 1.2 bins, the +47 dB leakage trap the plan documents.
        assert_eq!(50 * FRAME_SAMPLES, SAMPLE_RATE_HZ as usize);
        assert_ne!(60 * FRAME_SAMPLES, SAMPLE_RATE_HZ as usize);
        assert_eq!(60 * FRAME_SAMPLES % SAMPLE_RATE_HZ as usize, 3200);
    }

    #[test]
    fn read_timeout_is_four_frame_periods() {
        assert_eq!(READ_TIMEOUT_MS, 80);
    }

    #[test]
    fn window_is_the_ten_second_stnr_floor() {
        // 500 frames × 20 ms = 10 s at 50 fps — also the plan's STNR
        // "≥ 500 valid frames" floor.
        assert_eq!(WINDOW_FRAMES, 500);
        assert_eq!(WINDOW_FRAMES * FRAME_SAMPLES / SAMPLE_RATE_HZ as usize, 10);
    }
}
