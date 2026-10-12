//! 33-tap equiripple half-band FIR decimator, 16 kHz → 8 kHz (plan Todo 12).
//!
//! Plan contract, quoted from `.omo/plans/esp32s3-av-stream-and-sd-recording.md`
//! Todo 12:
//!
//! > **Decimation is a hand-rolled half-band FIR, not a resampler crate.**
//! > Decimate by exactly 2 with a lowpass at 4 kHz, ~33 taps of equiripple
//! > half-band coefficients. […]
//! > **Filter QA**: passband to 3.4 kHz flat within 0.5 dB, transition
//! > 3.4 → 4.6 kHz (cutoff still 4 kHz, so it remains a true half-band with
//! > zero-valued even taps), and **stopband measured from 4.6 kHz**, exceeding
//! > 40 dB. Measure both with a swept sine. Add the passband-flatness
//! > assertion deliberately: the filter's own passband gain shifts every dBFS
//! > threshold in the plan, so a filter that is correct but flat at +3 dB
//! > would otherwise push a nominal −50 dBFS signal over the presence floor.
//!
//! # Design method and measured response (provenance)
//!
//! **Method**: Kaiser-windowed ideal sinc with the cutoff placed *exactly* at
//! `fs/4` = 4 kHz, so `2·fc/fs = 0.5` and the ideal response
//! `h[k] = sin(πk/2)/(πk)` is a true half-band: every even-offset tap is
//! identically zero (enforced below, not merely approximated), the odd part is
//! 16 free coefficients, and the transition band is symmetric about 4 kHz —
//! which is what makes the 3.4/4.6 kHz spec edges geometrically consistent
//! (a half-band is −6 dB at 4 kHz by construction — measured −6.001 dB).
//! Kaiser `β = 3.66` was selected by a numeric sweep over the f32-rounded
//! coefficients: the smallest transition at N=33 that clears both gates with
//! margin (larger β widened the transition and *raised* the 4.6 kHz edge;
//! smaller β failed the stopband). Coefficients are then renormalised to
//! exactly unity DC gain (0.000 dB — the dBFS-threshold contract) and
//! truncated to `f32`, which is what [`H`] holds and what the swept-sine test
//! re-measures end-to-end through the i16 pipeline.
//!
//! **Measured response** (dense-grid DFT of the shipped `f32` coefficients,
//! `fs = 16 kHz`; the in-file `swept_sine` test re-measures the same numbers
//! through the real decimator and is the binding assertion):
//!
//! | metric | value | plan bound |
//! |---|---|---|
//! | passband 0–3400 Hz, max deviation from unity | **+0.0835 dB** (at 3235 Hz) | ≤ 0.5 dB |
//! | passband 0–3400 Hz, min | **−0.0445 dB** (at 3400 Hz) | ≥ −0.5 dB |
//! | stopband 4600–8000 Hz, worst attenuation | **42.56 dB** (at 4766 Hz) | ≥ 40 dB |
//! | response at 4000 Hz (half-band cutoff) | **−6.001 dB** | ≈ −6 dB |
//! | DC gain | **0.000 dB** (`Σh = 1.0000000`) | 0 dB |
//!
//! Ripple is *not* equiripple-optimal (windowed sinc, not Parks–McClellan);
//! at N=33 the Kaiser design meets both plan gates with ≥2.5 dB stopband
//! margin and ≥6× passband margin, and its zeros-on-even-taps property is
//! exact rather than converged-to — a true half-band as the plan demands.

/// Number of filter taps (plan: "~33 taps"; 33 keeps the centre tap at an
/// even index so the half-band zero pattern is symmetric around it).
pub const TAPS: usize = 33;

/// Centre tap index — `(TAPS - 1) / 2`.
pub const CENTER: usize = 16;

/// Input frame consumed per call: `mic::FRAME_SAMPLES` — 320 samples =
/// 20 ms at 16 kHz (see `firmware/src/mic.rs`; asserted equal there).
pub const FRAME_IN: usize = 320;

/// Output frame produced per call: FRAME_IN / 2 = 160 samples = 20 ms at
/// 8 kHz = exactly one G.711 RTP packet's payload (timestamp step 160).
pub const FRAME_OUT: usize = FRAME_IN / 2;

/// The shipped coefficients — see the module header for the design method and
/// measured response table. Even indices (except [`CENTER`]) are *exactly*
/// 0.0: that identity IS the half-band property the plan requires, so it is
/// asserted in tests rather than left to float rounding.
pub const H: [f32; TAPS] = [
    0.000000000e+00, -3.647338133e-03, 0.000000000e+00, 7.280467544e-03,
    0.000000000e+00, -1.276203059e-02, 0.000000000e+00, 2.092028223e-02,
    0.000000000e+00, -3.343584016e-02, 0.000000000e+00, 5.465256795e-02,
    0.000000000e+00, -1.006467342e-01, 0.000000000e+00, 3.170841038e-01,
    5.011090636e-01, 3.170841038e-01, 0.000000000e+00, -1.006467342e-01,
    0.000000000e+00, 5.465256795e-02, 0.000000000e+00, -3.343584016e-02,
    0.000000000e+00, 2.092028223e-02, 0.000000000e+00, -1.276203059e-02,
    0.000000000e+00, 7.280467544e-03, 0.000000000e+00, -3.647338133e-03,
    0.000000000e+00,
];

/// Streaming decimate-by-2 state: the last [`TAPS`] − 1 input samples.
///
/// Output `y[m] = Σ h[k] · x[2m − k]`, so every output uses 32 input samples
/// straddling the frame boundary — the history carries them across calls.
/// The very first frame starts from a zero history (a documented 1-frame
/// start-up transient; irrelevant once the stream is running).
#[derive(Clone)]
pub struct Decimator {
    hist: [i16; TAPS - 1],
}

impl Decimator {
    pub const fn new() -> Self {
        Self { hist: [0; TAPS - 1] }
    }

    /// Filter and decimate one 20 ms mic frame: 320 samples @16 kHz in,
    /// 160 samples @8 kHz out, saturating on round (a passband that peaks at
    /// +0.084 dB can push a full-scale input a few LSB over — clamp, never
    /// wrap).
    pub fn process(&mut self, input: &[i16; FRAME_IN]) -> [i16; FRAME_OUT] {
        let mut out = [0i16; FRAME_OUT];
        for (m, o) in out.iter_mut().enumerate() {
            // Global input index of x[2m] within this frame; negative reaches
            // into the history (the FIR spans 33 samples = 2·16 + 1).
            let center = 2 * m as isize;
            let mut acc = 0.0f32;
            for (k, &c) in H.iter().enumerate() {
                if c == 0.0 {
                    continue; // 16 of 33 taps are exactly zero (half-band)
                }
                let idx = center - k as isize;
                let sample = if idx < 0 {
                    self.hist[(TAPS as isize - 1 + idx) as usize]
                } else {
                    input[idx as usize]
                };
                acc += c * sample as f32;
            }
            *o = acc.round().clamp(i16::MIN as f32, i16::MAX as f32) as i16;
        }
        // Shift the tail (last TAPS-1 input samples) into the history.
        self.hist.copy_from_slice(&input[FRAME_IN - (TAPS - 1)..]);
        out
    }
}

impl Default for Decimator {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Swept-sine verification (plan: "Measure both with a swept sine").
//
// These functions are the *binding* measurement the plan asks for. They live
// in the module (not only in the test) so the harness evidence bin
// (`task12_report filter`) prints the very numbers the test asserts — one
// implementation, two consumers, no drift between evidence and gate.
// ---------------------------------------------------------------------------

/// Input samples per measured tone: 22 frames × 320 (i16 @16 kHz).
pub const MEASURE_IN: usize = 22 * FRAME_IN;
/// Outputs discarded as FIR start-up transient (640 inputs ≫ 33 taps).
pub const MEASURE_WARMUP_OUT: usize = FRAME_OUT;
/// Steady-state outputs analysed: 3200 samples = 0.4 s @8 kHz. A multiple
/// of 8000/gcd ⇒ every test frequency (all multiples of 50 Hz) completes an
/// integer number of cycles, so the coherent magnitude has no leakage.
pub const MEASURE_ANALYSIS_OUT: usize = 3200;

/// Swept-sine results: `(freq_hz, gain_db)` per point. Passband points are
/// input tones ≤ 3.4 kHz (gain vs unity); stopband points are input tones
/// ≥ 4.6 kHz (attenuation, positive dB = rejection).
#[derive(Clone, Debug, Default)]
pub struct SweptSineReport {
    pub passband: Vec<(u32, f32)>,
    pub stopband: Vec<(u32, f32)>,
}

impl SweptSineReport {
    /// Worst absolute deviation from unity gain over the passband — the
    /// plan's passband-flatness number (bound: ≤ 0.5 dB).
    pub fn max_passband_abs_dev_db(&self) -> f32 {
        self.passband.iter().map(|(_, g)| g.abs()).fold(0.0f32, f32::max)
    }

    /// `(freq, deviation)` of the worst passband point.
    pub fn worst_passband(&self) -> (u32, f32) {
        self.passband
            .iter()
            .copied()
            .max_by(|a, b| a.1.abs().total_cmp(&b.1.abs()))
            .unwrap_or((0, 0.0))
    }

    /// Worst (smallest) stopband attenuation — bound: ≥ 40 dB.
    pub fn min_stopband_db(&self) -> f32 {
        self.stopband.iter().map(|(_, a)| *a).fold(f32::MAX, f32::min)
    }

    /// `(freq, attenuation)` of the worst stopband point.
    pub fn worst_stopband(&self) -> (u32, f32) {
        self.stopband
            .iter()
            .copied()
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .unwrap_or((0, 0.0))
    }
}

/// Coherent (integer-cycle) tone magnitude via direct DFT at a single
/// frequency — `2/N·|Σ x[n]·e^{-jωn}|`. For the analysis windows used here
/// every frequency is coherent, so this equals the tone amplitude exactly
/// (up to quantisation noise).
fn tone_mag(x: &[i16], freq_hz: f64, fs_hz: f64) -> f64 {
    let n = x.len() as f64;
    if n == 0.0 || fs_hz <= 0.0 {
        return 0.0;
    }
    if freq_hz == 0.0 {
        return x.iter().map(|&s| s as f64).sum::<f64>() / n;
    }
    let step = 2.0 * std::f64::consts::PI * freq_hz / fs_hz;
    let (mut re, mut im) = (0.0f64, 0.0f64);
    for (i, &s) in x.iter().enumerate() {
        let a = step * i as f64;
        re += s as f64 * a.cos();
        im -= s as f64 * a.sin();
    }
    (2.0 / n) * re.hypot(im)
}

/// Run one input tone through the real `Decimator` and return its gain in dB
/// at the output (tones above 4 kHz are measured at their `8000 − f` alias —
/// exactly where an unfiltered tone would land, so the number IS the alias
/// rejection a listener would get).
///
/// `cosine` selects cos-phase generation: required for the 4 kHz cutoff
/// probe, where the output lands on the Nyquist bin. At exactly Nyquist the
/// two input sidebands fold onto one real bin — a sine-phase input cancels
/// there (its quadrature has nowhere to go), and `tone_mag`'s 2/N scaling
/// double-counts a real alternating tone. Cos-phase + a factor-1/2 correction
/// recovers the true peak (verified: −6.00 dB at cutoff).
fn measure_tone_phase(freq_hz: u32, cosine: bool) -> f32 {
    const AMPLITUDE: f64 = 16000.0;
    let mut input = vec![0i16; MEASURE_IN];
    if freq_hz == 0 {
        input.fill(AMPLITUDE as i16);
    } else {
        for (i, s) in input.iter_mut().enumerate() {
            let a = 2.0 * std::f64::consts::PI * freq_hz as f64 * i as f64 / 16000.0;
            let v = AMPLITUDE * if cosine { a.cos() } else { a.sin() };
            *s = v.round() as i16;
        }
    }
    let mut dec = Decimator::new();
    let mut out = Vec::with_capacity(MEASURE_IN / 2);
    for frame in input.chunks_exact(FRAME_IN) {
        let arr: &[i16; FRAME_IN] = frame.try_into().expect("chunk is FRAME_IN");
        out.extend_from_slice(&dec.process(arr));
    }
    // Steady state: outputs [WARMUP, WARMUP + ANALYSIS); aligned inputs
    // [2·WARMUP, 2·(WARMUP+ANALYSIS)) — both complete-cycle windows.
    let analysis = &out[MEASURE_WARMUP_OUT..MEASURE_WARMUP_OUT + MEASURE_ANALYSIS_OUT];
    let aligned_in =
        &input[2 * MEASURE_WARMUP_OUT..2 * (MEASURE_WARMUP_OUT + MEASURE_ANALYSIS_OUT)];
    let alias = if freq_hz < 4000 { freq_hz } else { 8000 - freq_hz };
    let mag_in = tone_mag(aligned_in, freq_hz as f64, 16000.0);
    let mut mag_out = tone_mag(analysis, alias as f64, 8000.0);
    if alias == 4000 {
        mag_out *= 0.5; // Nyquist bin: ±f coincide, the 2/N factor counts once too many
    }
    if mag_in <= 0.0 {
        return f32::NEG_INFINITY;
    }
    (20.0 * (mag_out / mag_in).log10()) as f32
}

fn measure_tone(freq_hz: u32) -> f32 {
    measure_tone_phase(freq_hz, false)
}

/// The plan's swept-sine measurement: passband 0…3400 Hz in 50 Hz steps
/// (gain vs unity, bound ≤ 0.5 dB deviation) and stopband 4600…7900 Hz in
/// 100 Hz steps (attenuation, bound ≥ 40 dB). Frequencies are multiples of
/// 50 Hz so both analysis windows are leakage-free.
pub fn swept_sine_report() -> SweptSineReport {
    let mut report = SweptSineReport::default();
    let mut f = 0u32;
    while f <= 3400 {
        report.passband.push((f, measure_tone(f)));
        f += 50;
    }
    let mut f = 4600u32;
    while f <= 7900 {
        // Negate: measure_tone returns gain (dB), stopband stores positive
        // attenuation (rejection), per the struct docs.
        report.stopband.push((f, -measure_tone(f)));
        f += 100;
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn true_halfband_structure_exact_zero_even_taps() {
        for (k, &c) in H.iter().enumerate() {
            if k != CENTER && k % 2 == 0 {
                assert_eq!(c, 0.0, "even-offset tap {k} must be exactly zero (half-band identity)");
            } else if k % 2 == 1 || k == CENTER {
                assert_ne!(c, 0.0, "tap {k} must be non-zero");
            }
        }
        assert_eq!(H[CENTER], 0.5011090636e0_f32, "centre ≈ ideal 0.5 after unity-gain renormalisation");
        // Linear-phase symmetry: h[k] == h[TAPS-1-k].
        for k in 0..TAPS {
            assert_eq!(H[k], H[TAPS - 1 - k], "symmetry at {k}");
        }
        let sum: f64 = H.iter().map(|&v| v as f64).sum();
        assert!(
            (sum - 1.0).abs() < 1e-6,
            "unity DC gain (Σh = {sum}) — a non-unity sum would shift every dBFS threshold"
        );
    }

    #[test]
    fn decimator_halves_frame_and_keeps_dc_at_unity() {
        let mut dec = Decimator::new();
        let dc = [16000i16; FRAME_IN];
        let mut last = [0i16; FRAME_OUT];
        for _ in 0..3 {
            last = dec.process(&dc);
        }
        for &s in last.iter() {
            assert!((s as i32 - 16000).abs() <= 1, "DC must pass at unity, got {s}");
        }
        // Extremes: saturate, never panic or wrap — measured in steady state
        // (the first frame after a polarity step legitimately walks the step
        // through the 33-sample transient, so only settled frames assert).
        let mut dec = Decimator::new();
        for _ in 0..3 {
            dec.process(&[i16::MAX; FRAME_IN]);
        }
        for s in dec.process(&[i16::MAX; FRAME_IN]) {
            assert!(s >= 0, "full-scale positive stays non-negative: {s}");
        }
        for _ in 0..3 {
            dec.process(&[i16::MIN; FRAME_IN]);
        }
        for s in dec.process(&[i16::MIN; FRAME_IN]) {
            assert!(s < 0, "full-scale negative stays negative: {s}");
        }
    }

    #[test]
    fn swept_sine_meets_plan_filter_bounds() {
        let r = swept_sine_report();
        let (pf, pd) = r.worst_passband();
        let (sf, sa) = r.worst_stopband();
        println!(
            "swept-sine: passband worst |dev| = {:.4} dB @ {} Hz; stopband worst = {:.2} dB @ {} Hz; points = {}/{}",
            r.max_passband_abs_dev_db(),
            pf,
            r.min_stopband_db(),
            sf,
            r.passband.len(),
            r.stopband.len()
        );
        assert!(
            r.max_passband_abs_dev_db() <= 0.5,
            "passband flat within 0.5 dB: worst {} dB @ {} Hz (a +3 dB filter would shift every dBFS threshold)",
            pd,
            pf
        );
        assert!(
            r.min_stopband_db() >= 40.0,
            "stopband from 4.6 kHz exceeds 40 dB: worst {:.2} dB @ {} Hz (else 4–8 kHz aliases into the stream)",
            sa,
            sf
        );
        // The 3.4→4.6 transition must actually transition: −6 dB at the
        // cutoff (cos-phase — see measure_tone_phase on the Nyquist folding).
        let at_4k = measure_tone_phase(4000, true);
        assert!(
            (-6.5..=-5.5).contains(&at_4k),
            "half-band cutoff at 4 kHz ≈ −6 dB, got {at_4k} dB"
        );
    }
}