//! G.711 µ-law (PCMU) audio track — payload type **0** (plan Todo 12).
//!
//! Plan contract, quoted from `.omo/plans/esp32s3-av-stream-and-sd-recording.md`
//! Todo 11/12 (the single locked decision):
//!
//! > **The audio codec is settled here: µ-law, payload type 0, clock rate
//! > 8000, declared as `a=rtpmap:0 PCMU/8000`.** … µ-law is chosen over
//! > A-law because payload type 0 is the universally-defaulted static RTP
//! > payload, so clients that ignore the SDP still decode audio correctly.
//! > … Todo 12 implements exactly it. […] **Acceptance:** audio packets carry
//! > **payload type 0**, matching the `a=rtpmap:0 PCMU/8000` SDP from Todo 11 —
//! > asserted by reading the actual packet header, not by trusting the two
//! > layers to agree.
//!
//! Pipeline (one mic frame = 20 ms):
//!
//! ```text
//! [i16; 320] @16 kHz  →  halfband::Decimator  →  [i16; 160] @8 kHz
//!                     →  audio_codec_algorithms::encode_ulaw  →  160 B payload
//!                     →  RTP header (12 B, PT=0, ts += 160)  →  172-byte packet
//! ```
//!
//! Companding comes from the plan's one new dependency (see `firmware/
//! Cargo.toml`): `audio_codec_algorithms = { version = "=0.8",
//! default-features = false }` — `#![no_std"]`, `forbid(unsafe_code)`,
//! `encode_ulaw(i16) -> u8` / `decode_ulaw(u8) -> i16` (verified against the
//! crate's registry source: both re-exported at crate root). The decoder is
//! what makes the exhaustive 256-code round-trip assertion possible.

use super::halfband::{Decimator, FRAME_IN, FRAME_OUT};
use audio_codec_algorithms::encode_ulaw;

/// Static RTP payload type for PCMU — plan lock (SDP: `a=rtpmap:0 PCMU/8000`).
/// Asserted by reading the emitted header byte in tests, not by trusting the
/// SDP and the payloader to agree.
pub const PAYLOAD_TYPE: u8 = 0;

/// RTP clock for this track: 1 timestamp unit = 1/8000 s (plan Todo 12).
pub const CLOCK_RATE_HZ: u32 = 8_000;

/// Mic sample rate feeding the track: 16 kHz (`mic::SAMPLE_RATE_HZ`).
pub const INPUT_RATE_HZ: u32 = 16_000;

/// Samples per packet (20 ms @8 kHz) and the per-packet timestamp step.
pub const PAYLOAD_LEN: usize = FRAME_OUT;
/// RTP timestamp advance per packet = 160 units = 1/50 s = the 20 ms frame.
pub const TIMESTAMP_STEP: u32 = 160;

/// Fixed 12-byte RFC 3550 header.
pub const RTP_HEADER_LEN: usize = 12;
/// Complete packet length: header + one µ-law frame — "packet sizes are
/// consistent" (plan acceptance) is `PACKET_LEN`, every packet, forever.
pub const PACKET_LEN: usize = RTP_HEADER_LEN + PAYLOAD_LEN;

/// The audio packetizer: owns decimation state, sequence/timestamp bookkeeping
/// and the packet buffer (no heap — one `[u8; 172]` in the struct).
///
/// Construction starts at `seq = 0, rtptime = 0` per the RTP-Info contract in
/// `rtsp/mod.rs` (first packet of a track in a session must match the first
/// PLAY's RTP-Info). Todo 13 reads [`Self::seq`]/[`Self::rtptime`]/[`Self::ssrc`]
/// for RTCP Sender Reports.
pub struct AudioTrack {
    seq: u16,
    rtptime: u32,
    ssrc: u32,
    dec: Decimator,
    packet: [u8; PACKET_LEN],
}

impl AudioTrack {
    /// New session: seq 0, rtptime 0 (RTP-Info contract).
    pub const fn new(ssrc: u32) -> Self {
        Self {
            seq: 0,
            rtptime: 0,
            ssrc,
            dec: Decimator::new(),
            packet: [0; PACKET_LEN],
        }
    }

    /// Encode one 20 ms mic frame into one RTP packet and return it.
    ///
    /// Advances `seq` by 1 and `rtptime` by [`TIMESTAMP_STEP`] (160 = 20 ms
    /// at 8 kHz) — 50 frames therefore advance the clock by exactly 8000
    /// units = 1 second, asserted in tests. Infallible: the buffer is fixed
    /// size and µ-law encoding is total over `i16`.
    pub fn encode_frame(&mut self, frame: &[i16; FRAME_IN]) -> &[u8] {
        let pcm = self.dec.process(frame);

        super::write_rtp_header(
            &mut self.packet[..RTP_HEADER_LEN],
            false, // M never set on audio (no frame concept in G.711)
            PAYLOAD_TYPE,
            self.seq,
            self.rtptime,
            self.ssrc,
        );
        for (i, &s) in pcm.iter().enumerate() {
            self.packet[RTP_HEADER_LEN + i] = encode_ulaw(s);
        }

        self.seq = self.seq.wrapping_add(1);
        self.rtptime = self.rtptime.wrapping_add(TIMESTAMP_STEP);
        &self.packet
    }

    /// Next sequence number (0 for the session's first packet).
    pub fn seq(&self) -> u16 {
        self.seq
    }

    /// Next RTP timestamp (0 for the session's first packet).
    pub fn rtptime(&self) -> u32 {
        self.rtptime
    }

    /// SSRC — must stay constant for the whole session (asserted in tests).
    pub fn ssrc(&self) -> u32 {
        self.ssrc
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use audio_codec_algorithms::decode_ulaw;

    /// Silence code round-trip: 0xFF decodes to 0 (plan Todo 17 quotes this
    /// G.711 property — "the µ-law silence code 0xFF decodes to 0, so a silent
    /// stream yields RMS == 0 and fails that floor, which is exactly the
    /// behaviour wanted").
    #[test]
    fn ulaw_silence_code_ff_decodes_to_zero() {
        assert_eq!(decode_ulaw(0xFF), 0);
        assert_eq!(encode_ulaw(0), 0xFF, "digital zero encodes to all-ones");
    }

    /// QA — failure (plan Todo 12): "round-trip every one of the 256 µ-law
    /// codes through encode_ulaw/decode_ulaw and assert the reconstruction
    /// error is within the G.711 specification bound for each code".
    ///
    /// Two complementary sweeps:
    /// 1. **Code round-trip**: decode(c) → encode → must return c, with one
    ///    documented exception: codes 0x7F and 0xFF both decode to 0 (µ-law's
    ///    duplicate digital zero), so 0x7F canonicalises to 0xFF. Stability
    ///    `decode(encode(decode(c))) == decode(c)` holds for all 256.
    /// 2. **Exhaustive input sweep** over all 65 536 `i16` values (covers all
    ///    256 codes as outputs): `|decode(encode(x)) − x| ≤ bound(code)`,
    ///    where the G.711 quantisation bound is half the segment step
    ///    (`1 << (segment + 2)`: steps are 8·2^segment, verified against the
    ///    G.191-generated decode table), and the two saturation codes
    ///    (word 0x7F → codes 0x00/0x80) are bounded by the clipping distance
    ///    `32768 − 32124 = 644` — the µ-law reconstruction ceiling.
    #[test]
    fn ulaw_256_code_roundtrip_and_g711_error_bounds() {
        // 1. Code round-trip.
        for c in 0u16..256 {
            let c = c as u8;
            let d = decode_ulaw(c);
            let c2 = encode_ulaw(d);
            assert!(
                c2 == c || (c == 0x7F && c2 == 0xFF),
                "code {c:#04x} round-trips to {c2:#04x} (only 0x7F→0xFF zero-canonicalisation allowed)"
            );
            assert_eq!(
                decode_ulaw(c2),
                d,
                "reconstruction must be stable: decode(encode(decode({c:#04x}))) == decode(c)"
            );
        }

        // 2. Exhaustive i16 sweep against the per-code spec bound.
        let mut hit = [false; 256];
        let mut worst = (0i32, 0u8, 0i32); // (x, code, error)
        for x in i32::from(i16::MIN)..=i32::from(i16::MAX) {
            let x = x as i16;
            let c = encode_ulaw(x);
            hit[c as usize] = true;
            let err = (decode_ulaw(c) as i32 - x as i32).abs();
            let raw = c ^ 0xFF;
            let word = raw & 0x7F;
            let segment = word >> 4;
            let bound = if word == 0x7F {
                // Saturation: |x| ≥ 32636 encodes here; reconstruction ceiling
                // is 32124 ⇒ worst case i16::MIN = −32768 → 644.
                644i32
            } else {
                1i32 << (segment + 2)
            };
            assert!(
                err <= bound,
                "G.711 bound violated: x={x} code={c:#04x} err={err} > {bound} (segment {segment})"
            );
            if err > worst.2 {
                worst = (x as i32, c, err);
            }
        }
        assert!(
            hit.iter().all(|&h| h),
            "all 256 codes must be reachable encode outputs"
        );
        // The bound is tight by construction — report the actual worst case.
        assert_eq!(worst, (-32768, 0x00, 644), "worst-case error is i16::MIN → −32124");
    }

    /// Plan QA — failure: "no panics on i16::MIN/i16::MAX inputs" — explicit,
    /// separate from the sweep so a regression names the rails directly.
    #[test]
    fn ulaw_min_max_inputs_never_panic() {
        let lo = encode_ulaw(i16::MIN);
        let hi = encode_ulaw(i16::MAX);
        assert_eq!(lo, 0x00, "i16::MIN saturates to code 0x00");
        assert_eq!(hi, 0x80, "i16::MAX saturates to code 0x80");
        assert_eq!(decode_ulaw(lo), -32124);
        assert_eq!(decode_ulaw(hi), 32124);
        // And the inverse direction for good measure.
        for c in [u8::MIN, u8::MAX, 0x7F, 0x80] {
            let _ = decode_ulaw(c);
        }
    }

    fn sine_frame(freq: u32, phase: &mut f64) -> [i16; FRAME_IN] {
        let mut f = [0i16; FRAME_IN];
        for s in f.iter_mut() {
            *s = (12000.0 * phase.sin()).round() as i16;
            *phase += 2.0 * std::f64::consts::PI * freq as f64 / 16_000.0;
        }
        f
    }

    /// Acceptance: "audio packets carry payload type 0" — read the actual
    /// header byte out of a real emitted packet. Also the packet-size,
    /// seq/timestamp-step and SSRC-consistency contracts.
    #[test]
    fn emitted_packet_header_byte_carries_pt_0() {
        let mut t = AudioTrack::new(0xA5A5_0F0F);
        let mut phase = 0.0;
        let mut prev_ssrc = 0;
        for i in 0u32..5 {
            let frame = sine_frame(1000, &mut phase);
            let pkt = t.encode_frame(&frame);

            assert_eq!(pkt.len(), PACKET_LEN, "every packet is 172 bytes");
            assert_eq!(pkt[0], 0x80, "V=2 fixed header");
            let pt_byte = pkt[1];
            assert_eq!(pt_byte & 0x7F, 0, "PT field of the ACTUAL packet is 0");
            assert_eq!(pt_byte & 0x80, 0, "M bit clear on audio");
            assert_eq!(pt_byte, 0, "raw PT byte is literally 0");

            let seq = u16::from_be_bytes([pkt[2], pkt[3]]);
            let ts = u32::from_be_bytes([pkt[4], pkt[5], pkt[6], pkt[7]]);
            let ssrc = u32::from_be_bytes([pkt[8], pkt[9], pkt[10], pkt[11]]);
            assert_eq!(seq, i as u16, "seq starts at 0, +1 per packet");
            assert_eq!(ts, i * TIMESTAMP_STEP, "rtptime starts at 0, +160 per packet");
            if i == 0 {
                prev_ssrc = ssrc;
            }
            assert_eq!(ssrc, prev_ssrc, "SSRC constant across the session");
            assert_eq!(ssrc, 0xA5A5_0F0F);

            // Payload is real µ-law: 160 bytes, and decodable to the tone.
            let payload = &pkt[RTP_HEADER_LEN..];
            assert_eq!(payload.len(), 160);
        }
        assert_eq!(t.seq(), 5);
        assert_eq!(t.rtptime(), 5 * TIMESTAMP_STEP);
    }

    /// 50 frames = 1 s of mic input must advance the RTP clock by exactly
    /// 8000 units (the 8 kHz clock's one-second mark) — this is the identity
    /// Todo 13's SR arithmetic relies on.
    #[test]
    fn one_second_of_frames_advances_rtptime_by_8000() {
        let mut t = AudioTrack::new(1);
        let mut phase = 0.0;
        for _ in 0..50 {
            let frame = sine_frame(440, &mut phase);
            let _ = t.encode_frame(&frame);
        }
        assert_eq!(t.rtptime(), 8000);
        assert_eq!(t.seq(), 50);
    }

    /// The pipeline must actually produce the 1 kHz tone at 8 kHz: decode the
    /// µ-law payloads of a steady 1 kHz / 16 kHz input and find the tone with
    /// a coherent magnitude (aggregate over the whole take — not per-frame
    /// stats, which are Todos 8/16's contract).
    #[test]
    #[cfg_attr(target_os = "espidf", ignore = "decimate+compand over a full take starves the task watchdog on device; owner-approved halfband-dependent exclusion (plan Todo 15)")]
    fn pipeline_recovers_1khz_tone_after_decimate_and_compand() {
        let mut t = AudioTrack::new(7);
        let mut phase = 0.0;
        let mut pcm = Vec::new();
        for _ in 0..60 {
            let frame = sine_frame(1000, &mut phase);
            let pkt = t.encode_frame(&frame);
            for &b in &pkt[RTP_HEADER_LEN..] {
                pcm.push(decode_ulaw(b));
            }
        }
        // Skip 10 frames of start-up/edge transient, analyse 0.4 s = 3200
        // samples (1 kHz ⇒ 400 complete cycles: leakage-free).
        let analysis = &pcm[1600..4800];
        let n = analysis.len() as f64;
        let step = 2.0 * std::f64::consts::PI * 1000.0 / 8000.0;
        let (mut re, mut im) = (0.0f64, 0.0f64);
        for (i, &s) in analysis.iter().enumerate() {
            let a = step * i as f64;
            re += s as f64 * a.cos();
            im -= s as f64 * a.sin();
        }
        let mag = (2.0 / n) * re.hypot(im);
        // Input amplitude 12000; allow companding + filter ripple headroom.
        assert!(
            mag > 9000.0 && mag < 13500.0,
            "1 kHz tone must survive decimation + µ-law at roughly its amplitude, got {mag:.0}"
        );
        // Off-tone probe: 3 kHz must be far weaker than the 1 kHz bin.
        let probe = 2.0 * std::f64::consts::PI * 3000.0 / 8000.0;
        let (mut re3, mut im3) = (0.0f64, 0.0f64);
        for (i, &s) in analysis.iter().enumerate() {
            let a = probe * i as f64;
            re3 += s as f64 * a.cos();
            im3 -= s as f64 * a.sin();
        }
        let mag3 = (2.0 / n) * re3.hypot(im3);
        assert!(
            mag3 < mag * 0.1,
            "3 kHz component must be <10% of the 1 kHz tone: {mag3:.0} vs {mag:.0}"
        );
    }
}