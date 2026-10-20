//! Zero-dependency audio-quality statistics for the on-board PDM microphone
//! (plan Todo 8).
//!
//! **Plan contract.** This module is the measurement instrument for every
//! audio threshold in the plan's *Verification strategy* table. It exposes
//! exactly the published API — [`FrameStats`], [`frame_stats`],
//! [`stnr_snr_db`], [`goertzel_mag`], [`count_rail_runs`] — and **nothing
//! else**: no external crates, no FFT, no compander, no I/O. All four
//! functions are pure and host-testable (the harness runs this file's
//! `#[cfg(test)]` suite natively — see `harness/tests/audio_quality.rs`).
//!
//! # The three traps this module must not fall into (plan §Todo 8)
//!
//! 1. **SNR is computed on L16 PCM, BEFORE the G.711 compander.** NIST:
//!    *"SNR estimates of mu-law encoded speech should be taken with a grain
//!    of salt…"* — measuring downstream makes µ-law's intended lossy
//!    quantisation read as noise. This module therefore contains **no
//!    companding of any kind** (the µ-law encoder lives in Todo 12, after
//!    the SNR numbers are taken); every input is raw `i16` L16 samples at
//!    16 kHz.
//! 2. **Frames with `rms == 0` are DROPPED by [`stnr_snr_db`], never
//!    floored.** NIST Appendix 2: null-sample runs *"lead to large spikes in
//!    the short-time energy histogram at negative infinity dB … these
//!    portions are completely ignored."* A disconnected PDM mic produces
//!    exactly those nulls. Flooring `rms == 0` to a finite floor is not the
//!    fix either — the danger is a `-inf` dB value reaching the percentile
//!    computation, which yields a noise percentile of `-inf` and an SNR of
//!    **`+inf`: a silent pass on a dead mic.** So null frames are removed
//!    from the population before any dB conversion, and if fewer than 500
//!    valid frames survive, the function returns `None` — the caller
//!    reports `INSUFFICIENT_FRAMES`, which is *the* signal for "mic absent"
//!    (plan QA: with the mic disconnected, STNR must report "insufficient
//!    frames" and never `+∞`).
//! 3. **A constant-DC signal defeats an RMS-only check** — RMS looks
//!    perfectly healthy while the standard deviation is exactly zero.
//!    [`FrameStats::stddev`] is therefore a first-class field (an earlier
//!    draft of the struct omitted it while four acceptance criteria
//!    demanded it), and the acceptance threshold `stddev > 0` must be
//!    asserted alongside `rms > 0`, together with the [`FrameStats::distinct`]
//!    counter as secondary sanity.
//!
//! # Aggregation windows — thresholds bind to WINDOWS, not per-frame values
//! (plan Verification strategy; `frame_stats` only *produces* the per-frame
//! inputs)
//!
//! | Property | Threshold | Aggregation window |
//! |---|---|---|
//! | Audio present | RMS > −50 dBFS **and** `stddev > 0` | RMS = 95th percentile over 10 s; `stddev > 0` = ≥ 99% of frames |
//! | DC offset | `\|mean\|` < 0.05 × rms | 95th percentile over 10 s |
//! | Clipping | 0 runs of ≥ 3 identical rail samples; peak < 32112 (−0.2 dBFS) | rail runs = max over window; peak = max |
//! | Crest factor | > 8 dB | **median** over window (a single near-silent frame would drag a mean anywhere) |
//! | Noise floor | NIST STNR ≥ 12 dB (quiet room) | over the 10 s window; ≥ 500 valid frames else `None` ⇒ `INSUFFICIENT_FRAMES` |
//! | Tone purity | tone power / total power > 0.9 | per 20 ms frame of the injected-reference run |
//! | 50/60 Hz hum | each < 0.1 × tone magnitude | **100 ms (1600-sample) window, never the 20 ms frame** (below) |
//! | Analysis cost | **< 100 µs/frame** (0.5% of the 20 ms budget); 5 µs = non-gating design target | mean over the run |
//!
//! Thresholding per 20 ms frame fails on a healthy mic: a frame inside a
//! speech pause is far below −50 dBFS and a 20 ms voiced frame has a crest
//! of 3–6 dB (full-scale sine = 3.01 dB), not > 8 dB. The **aggregate is
//! part of the threshold**. `mic.rs` performs the windowing; this module
//! provides the per-frame numbers and the window statistics the plan
//! names (percentile / median / max are computed by the caller).
//!
//! # Why the hum Goertzel needs a separate 1600-sample (100 ms) buffer
//!
//! 320 samples at 16 kHz gives **50 Hz bin spacing**: 1 kHz lands on bin 20
//! and 50 Hz on bin 1, but **60 Hz lands on bin 1.2 — off-grid.**
//! Rectangular-window leakage at δ = 0.2 bins is ~172× (+47 dB) *relative to
//! the true 60 Hz content*, so a genuine 50 Hz component (or any low-band
//! energy) reads into the 60 Hz bin at ~0.93× its own amplitude — on
//! hardware with **zero** 60 Hz hum the measurement still returns a large
//! non-zero magnitude, and relaxing the threshold would silently delete the
//! hum check. Measuring on a **1600-sample window** gives 10 Hz spacing:
//! **50 Hz = bin 5, 60 Hz = bin 6, both exactly on-grid** (a 50 Hz tone
//! nulled exactly one bin away). Keep 320-sample frames for RMS / peak /
//! crest / STNR; `mic.rs` accumulates a separate 1600-sample rolling buffer
//! solely for the hum Goertzels. See
//! `hum_window_nulls_offgrid_leakage_that_320_frames_show` in the tests.
//!
//! # Cost
//!
//! [`frame_stats`] is single-pass (peak tracked inline — no second loop),
//! accumulates in `i64` (exact; `i32 × i32` product fits `i32`) and does
//! only a handful of `f64` ops per frame, so an unvectorised LX7 stays far
//! under the **< 100 µs/frame** gate; 5 µs is the design target, reported
//! but not asserted. [`goertzel_mag`] is the classic two-term recurrence,
//! O(N) at ~4 flops per sample.

use std::vec::Vec;

/// Full-scale reference for dBFS: `20·log10(x / 32768)`. A signed 16-bit
/// sample spans ±32768 in mathematical full scale, so RMS of a full-scale
/// square wave reads 0 dBFS and a full-scale sine reads −3.01 dBFS.
const FULL_SCALE: f32 = 32768.0;

/// Minimum number of *valid* (non-null) frames [`stnr_snr_db`] requires
/// before it will answer. Below this it returns `None` and the caller
/// reports `INSUFFICIENT_FRAMES` (plan: ≥ 500 valid frames in the 10 s
/// window, i.e. the ≥ 45 frames/s floor over 10 s).
const MIN_VALID_FRAMES: usize = 500;

/// Open-addressing slots for the [`FrameStats::distinct`] counter. The
/// count is **exact while ≤ 256 distinct values and saturates at 256**
/// (a saturating secondary sanity counter is all the plan asks for — it is
/// explicitly "not a primary threshold"). A 65536-bit exact bitmap would
/// cost 8 KiB of *stack* per call, which would overflow the 8 KiB device
/// libtest pthread stacks (plan Verification strategy fact 4) — this table
/// is 768 bytes instead. Hashing also keeps the worst-case cost O(1)
/// amortised: a cap-N linear scan degrades to ~N probes × 320 samples on
/// broadband audio and would brush the 100 µs analysis gate.
const DISTINCT_SLOTS: usize = 256;

/// Per-frame measurements produced by [`frame_stats`] (plan-published API —
/// field-for-field exactly this shape).
///
/// Field semantics:
/// * `mean` — DC offset, in sample units. Threshold: `|mean| < 0.05 × rms`
///   (95th percentile over the window).
/// * `rms` — level in sample units; convert with `20·log10(rms/32768)`
///   for dBFS. Presence floor: > −50 dBFS (95th percentile over window).
/// * `stddev` — population standard deviation. **The constant-DC guard**:
///   `stddev == 0` with `rms > 0` is a dead/stuck mic (a constant DC voltage
///   looks perfectly healthy to an RMS check). Threshold: `> 0` on ≥ 99% of
///   frames.
/// * `peak` — largest **magnitude** `|sample|` in the frame, so the negative
///   rail clips too. `|i16::MIN| = 32768` exceeds the field type and is
///   clamped to `32767` (documented; the clipping thresholds still fire —
///   32767 ≥ 32112). Threshold: `< 32112` (−0.2 dBFS), max over window.
/// * `crest_db` — `20·log10(peak / rms)`, 0.0 when `rms == 0` (no
///   meaningful crest on silence — such frames are dropped by
///   [`stnr_snr_db`] anyway and should be excluded from the window median).
///   Healthy voiced audio sits at 12–20 dB; clipping collapses it toward 0.
///   Threshold: **median** over the window > 8 dB.
/// * `distinct` — distinct sample values in the frame, exact ≤ 256 then
///   saturating (see [`DISTINCT_SLOTS`]); 1 for a constant-DC frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FrameStats {
    pub mean: f32,
    pub rms: f32,
    pub stddev: f32,
    pub peak: i16,
    pub crest_db: f32,
    pub distinct: usize,
}

/// Compute [`FrameStats`] for one frame in a **single pass** — the peak is
/// tracked inline (plan: "single pass; peak tracked inline"), there is no
/// second loop over the buffer.
///
/// Exactness: sums accumulate in `i64` (`i32 × i32` square fits `i32`, so
/// the per-sample multiply stays single-word), and only the final
/// division/sqrt run in `f64`. For the 320-sample capture frame this is
/// ~1.5 k integer ops — single-digit microseconds on the LX7, far inside
/// the < 100 µs/frame analysis gate.
///
/// Edges: an empty slice reports all zeros (`rms == 0` ⇒ the frame is a
/// null frame and is dropped by [`stnr_snr_db`]).
pub fn frame_stats(x: &[i16]) -> FrameStats {
    let n = x.len();
    if n == 0 {
        return FrameStats {
            mean: 0.0,
            rms: 0.0,
            stddev: 0.0,
            peak: 0,
            crest_db: 0.0,
            distinct: 0,
        };
    }

    // Single pass: sums, peak and distinct are all maintained inline.
    let mut sum: i64 = 0;
    let mut sum_sq: i64 = 0;
    let mut peak: u16 = 0;

    // Distinct-value table (open addressing, linear probe).
    let mut occupied = [false; DISTINCT_SLOTS];
    let mut keys = [0i16; DISTINCT_SLOTS];
    let mut distinct: usize = 0;

    for &v in x {
        sum += v as i64;
        // |v| <= 32768 ⇒ v² <= 2^30 fits i32 exactly.
        sum_sq += (v as i32 * v as i32) as i64;

        let mag = v.unsigned_abs();
        if mag > peak {
            peak = mag;
        }

        if distinct < DISTINCT_SLOTS {
            // Multiplicative hash: any odd constant works; >>8 folds u16 to
            // a 0..=255 slot. Probing always terminates: while the table is
            // not full an empty slot exists, and the counter guards the
            // (unreachable) pathological wrap.
            let mut slot = ((v as u16).wrapping_mul(0x9E37) >> 8) as usize;
            let mut probes = 0usize;
            loop {
                if !occupied[slot] {
                    occupied[slot] = true;
                    keys[slot] = v;
                    distinct += 1;
                    break;
                }
                if keys[slot] == v {
                    break;
                }
                probes += 1;
                if probes >= DISTINCT_SLOTS {
                    break;
                }
                slot = (slot + 1) & (DISTINCT_SLOTS - 1);
            }
        }
        // Table full ⇒ `distinct` already saturated at 256; membership
        // queries can no longer change the answer, so skip (this is what
        // keeps broadband frames O(1) instead of 256 probes × 320).
    }

    let nf = n as f64;
    let mean64 = sum as f64 / nf;
    let mean_sq = sum_sq as f64 / nf;
    // Floating rounding can push the variance microscopically negative.
    let var = (mean_sq - mean64 * mean64).max(0.0);

    let mean = mean64 as f32;
    let rms = mean_sq.sqrt() as f32;
    let stddev = var.sqrt() as f32;
    // |i16::MIN| = 32768 does not fit i16 — clamp to the type's max
    // (documented on FrameStats::peak; clipping thresholds unaffected).
    let peak = if peak > i16::MAX as u16 {
        i16::MAX
    } else {
        peak as i16
    };
    let crest_db = if rms > 0.0 {
        20.0 * (peak as f32 / rms).log10()
    } else {
        0.0
    };

    FrameStats { mean, rms, stddev, peak, crest_db, distinct }
}

/// NIST short-time SNR (STNR) over a window of per-frame statistics:
/// **85th-percentile minus 15th-percentile of the frame RMS levels in
/// dBFS** — speech level above the noise floor.
///
/// Trap 2, structurally enforced (plan: "drops null frames internally"):
///
/// * Frames with `rms == 0` are **removed from the population before the
///   dB conversion** — never floored. `20·log10(0)` would be `-inf`, which
///   drives the 15th percentile to `-inf` and the difference to **`+inf` —
///   a silent pass on a dead mic.** Because the input is `&[FrameStats]`
///   (raw measurements, not pre-converted dB), the implementation — not the
///   caller — owns the drop rule; a `-inf` cannot be smuggled in.
/// * Returns `None` when fewer than [`MIN_VALID_FRAMES`] (500) valid frames
///   survive: the caller logs `INSUFFICIENT_FRAMES`. `None` on a
///   disconnected mic (all-null frames ⇒ zero valid) rather than `Some(+)∞`.
///
/// Percentiles use linear interpolation between closest ranks (the numpy
/// default), on the sorted dB values.
///
/// Acceptance: ≥ 12 dB in a quiet room, computed over the 10 s window.
pub fn stnr_snr_db(frames: &[FrameStats]) -> Option<f32> {
    // Capacity: at most one dB value per input frame.
    let mut db = Vec::with_capacity(frames.len());
    for f in frames {
        // ---- trap 2: drop, never floor ----
        if f.rms == 0.0 {
            continue;
        }
        // ---- trap 1: dB on raw L16 PCM — no companding anywhere ----
        db.push(20.0 * (f.rms / FULL_SCALE).log10());
    }
    if db.len() < MIN_VALID_FRAMES {
        // "Insufficient frames" is the signal — never invent a number.
        return None;
    }
    db.sort_by(|a, b| a.total_cmp(b));
    let p15 = percentile_sorted(&db, 15.0);
    let p85 = percentile_sorted(&db, 85.0);
    Some(p85 - p15)
}

/// Linear-interpolation percentile over an **ascending-sorted** slice of dB
/// values. `pos = (p/100)·(n−1)`; the result interpolates between the two
/// neighbouring ranks (identical to numpy's default `linear` method).
fn percentile_sorted(sorted: &[f32], p: f32) -> f32 {
    debug_assert!(!sorted.is_empty());
    let n = sorted.len();
    if n == 1 {
        return sorted[0];
    }
    let pos = (p / 100.0) * (n - 1) as f32;
    let lo = pos.floor() as usize;
    let hi = pos.ceil() as usize;
    if lo == hi {
        sorted[lo]
    } else {
        let frac = pos - lo as f32;
        sorted[lo] + frac * (sorted[hi] - sorted[lo])
    }
}

/// Goertzel filter — an exact single-bin DFT via a two-term recurrence,
/// O(N) at ~4 flops per sample. The measurement target is a *known injected
/// tone*, so a single bin beats an FFT (which is additionally disqualified
/// here: `rustfft` has no `no_std`, and a spectrum is never needed).
///
/// Returns the **amplitude estimate at `freq_hz` in full-scale units**
/// (samples scaled by 1/32768, result normalised `2/N`): a full-scale
/// sine at exactly `freq_hz` returns ≈ 1.0. Amplitude, not power — the
/// plan's hum threshold "< 0.1 × tone magnitude" and "tone power / total
/// power > 0.9" are both evaluated against this scale by the caller.
///
/// **Bin-grid caveat (plan):** the result is only leakage-clean when
/// `freq_hz` sits on the window's bin grid (`fs_hz / N`). At 16 kHz a
/// 320-sample frame has 50 Hz spacing (50 Hz = bin 1 on-grid, **60 Hz =
/// bin 1.2 off-grid, +47 dB leakage**); a 1600-sample window has 10 Hz
/// spacing (50 Hz = bin 5, 60 Hz = bin 6, both on-grid). Feed hum checks a
/// 1600-sample buffer — see the module docs.
pub fn goertzel_mag(x: &[i16], freq_hz: f32, fs_hz: f32) -> f32 {
    if x.is_empty() || fs_hz <= 0.0 {
        return 0.0;
    }
    let omega = 2.0 * core::f32::consts::PI * freq_hz / fs_hz;
    let coeff = 2.0 * omega.cos();
    let (mut s1, mut s2) = (0.0f32, 0.0f32);
    for &v in x {
        let v = v as f32 / FULL_SCALE;
        let s0 = v + coeff * s1 - s2;
        s2 = s1;
        s1 = s0;
    }
    // |X(k)| from the recurrence's final two states. (cos ω, not the
    // 2·cos ω recurrence coefficient — using `coeff` here is the classic
    // off-by-factor bug.)
    let real = s1 - s2 * omega.cos();
    let imag = s2 * omega.sin();
    let mag = (real * real + imag * imag).sqrt();
    // A pure tone of amplitude A exactly on this bin yields |X| = A·N/2.
    2.0 * mag / x.len() as f32
}

/// Count runs of **at least `min_run` identical consecutive samples at the
/// rails** (`i16::MIN` or `i16::MAX`) — the clipping detector. Acceptance
/// uses `min_run = 3`: 0 runs of ≥ 3 in a healthy window; a run at `MIN`
/// and a run at `MAX` are separate runs even when adjacent.
///
/// Runs shorter than `min_run` are not counted (a single −32768 sample is
/// a legitimate waveform extreme; three consecutive ones are clipping).
pub fn count_rail_runs(x: &[i16], min_run: usize) -> usize {
    let mut runs = 0usize;
    let mut cur: Option<i16> = None;
    let mut len = 0usize;
    for &v in x {
        if v == i16::MIN || v == i16::MAX {
            if cur == Some(v) {
                len += 1;
            } else {
                if cur.is_some() && len >= min_run {
                    runs += 1;
                }
                cur = Some(v);
                len = 1;
            }
        } else if cur.is_some() {
            if len >= min_run {
                runs += 1;
            }
            cur = None;
            len = 0;
        }
    }
    if cur.is_some() && len >= min_run {
        runs += 1;
    }
    runs
}

// ---------------------------------------------------------------------------
// Unit tests — synthetic buffers only, pure math, no hardware. Executed on
// the host through the harness test target (`harness/tests/audio_quality.rs`
// `#[path]`-includes this file), and also compile-checked for the device via
// `cargo test --no-run --target xtensa-esp32s3-espidf`.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use core::f32::consts::PI;

    /// Integer-rounded sine; `amp` in sample units, `n` samples at `fs`.
    fn sine(freq: f32, amp: f32, n: usize, fs: f32) -> Vec<i16> {
        (0..n)
            .map(|i| {
                let v = amp * (2.0 * PI * freq * i as f32 / fs).sin();
                v.round() as i16
            })
            .collect()
    }

    /// Two-tone buffer (1 kHz tone + hum) for the Goertzel tests.
    fn tone_plus(freq_a: f32, amp_a: f32, freq_b: f32, amp_b: f32, n: usize, fs: f32) -> Vec<i16> {
        (0..n)
            .map(|i| {
                let t = i as f32 / fs;
                let v = amp_a * (2.0 * PI * freq_a * t).sin() + amp_b * (2.0 * PI * freq_b * t).sin();
                v.round() as i16
            })
            .collect()
    }

    // ---- trap 3: constant DC defeats an RMS-only check ------------------

    #[test]
    fn constant_dc_buffer_zero_stddev_with_nonzero_rms_is_flagged() {
        let buf = vec![1000i16; 320];
        let s = frame_stats(&buf);
        assert!((s.mean - 1000.0).abs() < 1e-3, "mean = {}", s.mean);
        assert!((s.rms - 1000.0).abs() < 1e-2, "rms = {}", s.rms);
        // THE trap: RMS is perfectly healthy while stddev is exactly zero.
        assert_eq!(s.stddev, 0.0);
        assert!(s.rms > 0.0);
        // The plan's required assertion `stddev > 0` must FAIL here —
        // this is the dead-mic signature an RMS-only check would pass.
        assert!(!(s.stddev > 0.0), "stddev>0 must reject constant DC");
        // The DC-offset threshold |mean| < 0.05*rms also rejects it.
        assert!(!(s.mean.abs() < 0.05 * s.rms), "DC check must reject constant DC");
        // Secondary counter: exactly one distinct value.
        assert_eq!(s.distinct, 1);
    }

    #[test]
    fn zeroed_frame_is_a_null_frame_rms_zero() {
        // The disconnected-mic sample pattern: everything 0.
        let s = frame_stats(&[0i16; 320]);
        assert_eq!(s.rms, 0.0);
        assert_eq!(s.mean, 0.0);
        assert_eq!(s.stddev, 0.0);
        assert_eq!(s.peak, 0);
        assert_eq!(s.crest_db, 0.0);
        assert_eq!(s.distinct, 1);
    }

    // ---- trap (d): all-i16::MAX => rail runs + peak fire ----------------

    #[test]
    fn all_max_buffer_fires_rail_run_and_peak_assertions() {
        let buf = vec![i16::MAX; 320];
        let s = frame_stats(&buf);
        assert_eq!(s.peak, i16::MAX);
        // Plan clipping threshold: peak < 32112 — must FAIL here.
        assert!(s.peak >= 32112, "peak {} must fire the clipping bound", s.peak);
        assert_eq!(count_rail_runs(&buf, 3), 1);

        // Two runs separated by a gap; MIN rail counts too; short runs do not.
        let mut two = vec![i16::MAX; 5];
        two.push(0);
        two.extend_from_slice(&[i16::MAX; 4]);
        assert_eq!(count_rail_runs(&two, 3), 2);
        assert_eq!(count_rail_runs(&two[..2], 3), 0, "run of 2 < min_run 3");
        assert_eq!(count_rail_runs(&[i16::MIN; 10], 3), 1);
        // Adjacent MIN-run and MAX-run are separate runs.
        let both: Vec<i16> = [vec![i16::MIN; 4], vec![i16::MAX; 4]].concat();
        assert_eq!(count_rail_runs(&both, 3), 2);
        // |i16::MIN| = 32768 clamps to 32767 — still fires peak < 32112.
        assert_eq!(frame_stats(&[i16::MIN; 320]).peak, i16::MAX);
    }

    #[test]
    fn rail_run_runs_are_counted_not_samples() {
        // 7 rail samples = one run of 7, not two runs of 3.
        assert_eq!(count_rail_runs(&[i16::MAX; 7], 3), 1);
        // min_run larger than the run => nothing.
        assert_eq!(count_rail_runs(&[i16::MAX; 7], 8), 0);
        assert_eq!(count_rail_runs(&[], 3), 0);
    }

    // ---- trap (e): alternating ±FS => crest ≈ 0 dB ----------------------

    #[test]
    fn alternating_full_scale_reads_zero_db_crest() {
        let buf: Vec<i16> = (0..320)
            .map(|i| if i % 2 == 0 { i16::MAX } else { i16::MIN })
            .collect();
        let s = frame_stats(&buf);
        // peak ≈ rms (both full scale) => crest ≈ 0 dB. The −32768 rail is
        // clamped in `peak`, which moves crest by < 0.01 dB.
        assert!(s.crest_db.abs() < 0.01, "crest = {} dB", s.crest_db);
        // It is an AC signal, so stddev > 0 and distinct = 2.
        assert!(s.stddev > 0.0);
        assert_eq!(s.distinct, 2);
        // Runs are length 1 (MAX, MIN, MAX, …): no run reaches the ≥3
        // clipping bound — the alternating pattern is AC, not clipping.
        assert_eq!(count_rail_runs(&buf, 1), 320);
        assert_eq!(count_rail_runs(&buf, 3), 0);
    }

    // ---- QA (a): injected 1 kHz reference, RMS within 5% ----------------

    #[test]
    fn injected_1khz_sine_recovers_rms_within_5_percent() {
        let amp = 16384.0f32;
        let buf = sine(1000.0, amp, 320, 16000.0);
        let s = frame_stats(&buf);
        let theory = amp / 2.0f32.sqrt(); // A/√2
        let err = (s.rms - theory).abs() / theory;
        assert!(err < 0.05, "rms {} vs theory {} (err {:.3}%)", s.rms, theory, err * 100.0);
        // 320 samples = exactly 20 cycles of 1 kHz at 16 kHz, and a sample
        // lands on the peak => crest = 3.01 dB (full-scale sine law).
        assert!((s.crest_db - 3.0103).abs() < 0.05, "crest = {}", s.crest_db);
        assert!(s.stddev > 0.0);
        // 16 samples/cycle symmetric about 0 ⇒ 9 distinct quantised values
        // ({0, ±6270, ±11585, ±15137, ±16384}); NOT 1 (constant DC) and not
        // saturated — a real waveform sits well above the DC signature.
        assert_eq!(s.distinct, 9, "distinct = {}", s.distinct);
    }

    #[test]
    #[cfg_attr(target_os = "espidf", ignore = "excluded from device gate: heavy STNR/goertzel starves watchdog and stack (task-15 scope)")]
    fn goertzel_recovers_on_grid_tone_amplitude() {
        let buf = sine(1000.0, 0.5 * 32767.0, 320, 16000.0);
        let mag = goertzel_mag(&buf, 1000.0, 16000.0);
        assert!((mag - 0.5).abs() / 0.5 < 0.02, "mag = {}", mag);
    }

    // ---- QA (f): hum detector actually detects hum ----------------------

    #[test]
    fn hum_detector_reads_50hz_above_tenth_of_tone() {
        // 1 kHz tone + 50 Hz at 1/4 the amplitude, 1600-sample (100 ms)
        // window: all three bins on-grid (bins 100, 5, 6).
        let amp_tone = 8000.0f32;
        let buf = tone_plus(1000.0, amp_tone, 50.0, amp_tone / 4.0, 1600, 16000.0);
        let tone = goertzel_mag(&buf, 1000.0, 16000.0);
        let hum50 = goertzel_mag(&buf, 50.0, 16000.0);
        let hum60 = goertzel_mag(&buf, 60.0, 16000.0);
        assert!(hum50 > 0.1 * tone, "hum50 {} must exceed 0.1×tone {}", hum50, 0.1 * tone);
        // No 60 Hz content present ⇒ the on-grid null keeps it below the
        // 0.1× threshold too.
        assert!(hum60 < 0.1 * tone, "hum60 {} must stay under 0.1×tone {}", hum60, 0.1 * tone);
    }

    // ---- the 1600-sample hum-window rationale, demonstrated -------------

    #[test]
    fn hum_window_nulls_offgrid_leakage_that_320_frames_show() {
        // Pure 50 Hz, NO 60 Hz content anywhere.
        let a50 = 8000.0f32;

        // 320-sample frame: 60 Hz is bin 1.2 (off-grid) => the 50 Hz tone
        // (bin 1) leaks into the 60 Hz bin at ~sinc(0.2) ≈ 0.93× amplitude.
        let f320 = sine(50.0, a50, 320, 16000.0);
        let leak320 = goertzel_mag(&f320, 60.0, 16000.0);
        let true50_320 = goertzel_mag(&f320, 50.0, 16000.0);
        assert!(
            leak320 > 0.5 * true50_320,
            "off-grid leakage must be large: {} vs {}",
            leak320,
            true50_320
        );

        // 1600-sample window: 50 Hz = bin 5, 60 Hz = bin 6 — exactly one
        // bin apart, rectangular window nulls it to float noise.
        let f1600 = sine(50.0, a50, 1600, 16000.0);
        let leak1600 = goertzel_mag(&f1600, 60.0, 16000.0);
        let true50_1600 = goertzel_mag(&f1600, 50.0, 16000.0);
        assert!(
            leak1600 < 0.02 * true50_1600,
            "on-grid null failed: {} vs {}",
            leak1600,
            true50_1600
        );
    }

    #[test]
    #[cfg_attr(target_os = "espidf", ignore = "excluded from device gate: heavy STNR/goertzel starves watchdog and stack (task-15 scope)")]
    fn goertzel_edges() {
        assert_eq!(goertzel_mag(&[], 1000.0, 16000.0), 0.0);
        assert_eq!(goertzel_mag(&[0i16; 16], 1000.0, 16000.0), 0.0);
        assert_eq!(goertzel_mag(&[1i16; 16], 1000.0, 0.0), 0.0);
    }

    // ---- trap 2: rms==0 frames dropped, never floored -------------------

    #[test]
    #[cfg_attr(target_os = "espidf", ignore = "excluded from device gate: heavy STNR/goertzel starves watchdog and stack (task-15 scope)")]
    fn stnr_drops_null_frames_and_matches_the_valid_only_population() {
        // 500 valid frames (varying amplitude ⇒ a real dB spread).
        let valid: Vec<FrameStats> = (0..500u32)
            .map(|i| frame_stats(&sine(1000.0, 1000.0 + i as f32 * 40.0, 320, 16000.0)))
            .collect();
        assert!(valid.iter().all(|f| f.rms > 0.0));

        let baseline = stnr_snr_db(&valid).expect("500 valid frames must produce a value");
        assert!(baseline.is_finite(), "STNR must never be ±inf: {}", baseline);
        assert!(baseline > 0.0);

        // Prepend 100 null (all-zero) frames — the dead-mic pattern.
        let mut mixed = vec![frame_stats(&[0i16; 320]); 100];
        mixed.extend_from_slice(&valid);
        let with_nulls = stnr_snr_db(&mixed).expect("nulls must be dropped, 500 valid remain");
        // Identical result ⇒ the null frames were REMOVED, not floored
        // (flooring would drag p15 down and change the answer).
        assert_eq!(with_nulls, baseline);
        assert!(with_nulls.is_finite());
    }

    #[test]
    #[cfg_attr(target_os = "espidf", ignore = "excluded from device gate: heavy STNR/goertzel starves watchdog and stack (task-15 scope)")]
    fn stnr_all_null_frames_returns_none_not_infinity() {
        // 600 frames of a disconnected mic (all zeros): 0 valid frames.
        let dead = vec![frame_stats(&[0i16; 320]); 600];
        assert_eq!(stnr_snr_db(&dead), None, "dead mic must be None (INSUFFICIENT_FRAMES), never +inf");
    }

    #[test]
    #[cfg_attr(target_os = "espidf", ignore = "excluded from device gate: heavy STNR/goertzel starves watchdog and stack (task-15 scope)")]
    fn stnr_below_500_valid_frames_is_none_and_at_500_is_some() {
        // Yield periodically: this CPU-bound loop otherwise starves IDLE0 and trips the TWDT on-device.
        let mut frames: Vec<FrameStats> = Vec::with_capacity(500);
        for i in 0..500u32 {
            if i % 50 == 0 {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            frames.push(frame_stats(&sine(1000.0, 1200.0 + i as f32 * 30.0, 320, 16000.0)));
        }
        assert!(stnr_snr_db(&frames).is_some(), "500 valid frames must answer");
        assert!(stnr_snr_db(&frames[..499]).is_none(), "499 valid frames must be None");
        // …and even 600 frames of which only 499 are valid still fail.
        let mut mostly_dead = vec![frame_stats(&[0i16; 320]); 101];
        mostly_dead.extend_from_slice(&frames[..499]);
        assert_eq!(stnr_snr_db(&mostly_dead), None);
        assert!(stnr_snr_db(&[]).is_none());
    }

    #[test]
    #[cfg_attr(target_os = "espidf", ignore = "excluded from device gate: heavy STNR/goertzel starves watchdog and stack (task-15 scope)")]
    fn stnr_percentiles_are_15th_and_85th_linear_interpolation() {
        // 1000 frames on an exactly linear dB ladder from -60 to -34 dBFS.
        // p15 sits at pos 0.15·999 = 149.85, p85 at 0.85·999 = 849.15.
        let (d0, d1) = (-60.0f32, -34.0f32);
        let frames: Vec<FrameStats> = (0..1000usize)
            .map(|i| {
                let db = d0 + (d1 - d0) * i as f32 / 999.0;
                let rms = FULL_SCALE * 10f32.powf(db / 20.0);
                FrameStats { mean: 0.0, rms, stddev: 1.0, peak: 0, crest_db: 0.0, distinct: 2 }
            })
            .collect();
        let snr = stnr_snr_db(&frames).expect("1000 valid");
        let expected = (849.15f32 - 149.85) * (d1 - d0) / 999.0;
        assert!((snr - expected).abs() < 0.01, "snr {} vs {}", snr, expected);
    }

    #[test]
    #[cfg_attr(target_os = "espidf", ignore = "excluded from device gate: heavy STNR/goertzel starves watchdog and stack (task-15 scope)")]
    fn stnr_never_returns_nan_on_finite_inputs() {
        // Constant-amplitude valid frames: p15 == p85 ⇒ STNR = 0 dB, not NaN.
        let frames: Vec<FrameStats> = (0..600).map(|_| frame_stats(&sine(1000.0, 4096.0, 320, 16000.0))).collect();
        let snr = stnr_snr_db(&frames).unwrap();
        assert!(snr.is_finite());
        assert!(snr.abs() < 1e-2, "identical frames ⇒ ~0 dB, got {}", snr);
    }

    // ---- misc edges / API shape -----------------------------------------

    #[test]
    fn frame_stats_empty_slice_is_all_zero() {
        let s = frame_stats(&[]);
        assert_eq!(
            s,
            FrameStats { mean: 0.0, rms: 0.0, stddev: 0.0, peak: 0, crest_db: 0.0, distinct: 0 }
        );
    }

    #[test]
    fn distinct_counts_up_to_256_then_saturates() {
        // 256 distinct values present, counter exact.
        let buf: Vec<i16> = (0..256i32).map(|i| (i * 257 - 32768) as i16).collect();
        let s = frame_stats(&buf);
        assert_eq!(s.distinct, 256, "exact up to the 256-slot table");

        // Many more distinct values than slots ⇒ saturates, does not hang
        // or undercount below the cap.
        let many: Vec<i16> = (0..512i32).map(|i| (i * 131 - 32768) as i16).collect();
        let s2 = frame_stats(&many);
        assert_eq!(s2.distinct, 256, "saturates at the table size");
    }
}
