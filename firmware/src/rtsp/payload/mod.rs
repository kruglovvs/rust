//! RTP payloaders (plan Todo 12) — G.711 µ-law audio (payload type 0) and
//! RFC 2435 JPEG (payload type 26), wired to Todo 11's `RtpSender`.
//!
//! Plan contract, quoted from `.omo/plans/esp32s3-av-stream-and-sd-recording.md`
//! Todo 12:
//!
//! > **Do:** wire the payloader chosen in Todo 6 into the video track.
//! > Convert 16 kHz 16-bit PCM to 8 kHz 8-bit µ-law for the RTP audio track,
//! > since the Sense's mic runs at 16 kHz and G.711 is defined at 8 kHz.
//! > … **Acceptance:** audio packets carry **payload type 0**, matching the
//! > `a=rtpmap:0 PCMU/8000` SDP from Todo 11 — asserted by reading the actual
//! > packet header, not by trusting the two layers to agree.
//! > One RTP timestamp unit is 1/8000 s for audio and 1/90000 s for video.
//!
//! Locks honoured here:
//!
//! - **Payload types are literal and static**: audio [`audio_track::PAYLOAD_TYPE`]
//!   is `0` (PCMU), video [`jpeg_track::PAYLOAD_TYPE`] is `26` (JPEG) — matching
//!   the SDP strings `sdp::generate` emits. Never dynamic PTs.
//! - **The payloader does NOT do interleaved framing** (rtsp/mod.rs interface
//!   contract): it emits complete RTP datagrams through a caller-supplied sink.
//!   On firmware the sink is `RtpSender::send_rtp` (which wraps the datagram in
//!   RFC 2326 §10.12 `$ | channel | u16-BE len | payload` framing itself); in
//!   tests the sink may be `rtsp::interleaved_frame` or a byte collector.
//! - **RTP-Info contract**: both tracks start a session at `seq = 0`,
//!   `rtptime = 0` (the first packet a track emits matches the first PLAY's
//!   `RTP-Info`, per `rtsp/mod.rs`).
//! - **Pure std / no new crates beyond the plan's one**: `audio_codec_algorithms`
//!   (plan Todo 12, `#![no_std"]` + `forbid(unsafe_code)`) and `crate::jfif`
//!   (Todo 6). No FFT/resampler crate (plan MUST NOT: rustfft/realfft/microfft/
//!   rubato/samplerate/hound/dasp).

pub mod audio_track;
pub mod halfband;
pub mod jpeg_track;

use crate::jfif::JfifError;
use std::io;

/// Errors from the packetizers. `Jfif` covers every `Frame::parse`/
/// `fragments()` typed failure; `Io` carries the sink's write error through
/// so `packetize` is a single `?`-able call for the firmware streaming loop.
#[derive(Debug)]
pub enum PacketizeError {
    Jfif(JfifError),
    Io(io::Error),
}

impl From<JfifError> for PacketizeError {
    fn from(e: JfifError) -> Self {
        Self::Jfif(e)
    }
}

impl From<io::Error> for PacketizeError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

impl std::fmt::Display for PacketizeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Jfif(e) => write!(f, "{e}"),
            Self::Io(e) => write!(f, "rtp sink: {e}"),
        }
    }
}

impl std::error::Error for PacketizeError {}

/// RFC 3550 §5.1 fixed RTP header: 12 bytes, `V=2`, no CSRC, no extension,
/// no padding. Shared by both tracks so the header layout exists exactly once
/// and the PT byte is written in exactly one place — the audio acceptance
/// ("read the actual packet header") reads `dst[1]` written by this function.
pub(crate) fn write_rtp_header(
    dst: &mut [u8],
    marker: bool,
    pt: u8,
    seq: u16,
    timestamp: u32,
    ssrc: u32,
) {
    debug_assert!(dst.len() >= 12, "RTP header needs 12 bytes");
    dst[0] = 0x80; // V=2, P=0, X=0, CC=0
    dst[1] = ((marker as u8) << 7) | (pt & 0x7F);
    dst[2..4].copy_from_slice(&seq.to_be_bytes());
    dst[4..8].copy_from_slice(&timestamp.to_be_bytes());
    dst[8..12].copy_from_slice(&ssrc.to_be_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rtp_header_layout_marker_and_pt_bits() {
        let mut buf = [0u8; 12];
        write_rtp_header(&mut buf, false, 0, 0x1234, 0xDEAD_BEEF, 0x0C0D_EA11);
        assert_eq!(buf[0], 0x80, "V=2, no CSRC/extension/padding");
        assert_eq!(buf[1], 0x00, "M=0, PT=0 literal byte");
        assert_eq!(&buf[2..4], &[0x12, 0x34], "seq big-endian");
        assert_eq!(&buf[4..8], &[0xDE, 0xAD, 0xBE, 0xEF], "timestamp big-endian");
        assert_eq!(&buf[8..12], &[0x0C, 0x0D, 0xEA, 0x11], "SSRC big-endian");

        write_rtp_header(&mut buf, true, 26, 1, 900, 7);
        assert_eq!(buf[1], 0x80 | 26, "M bit in bit 7, PT 26 in low 7 bits");
        // PT is masked to 7 bits: a caller passing 0x80|0 would still put 0
        // in the PT field — the M bit lives outside the payload type.
        write_rtp_header(&mut buf, false, 0x80, 2, 1800, 7);
        assert_eq!(buf[1] & 0x7F, 0, "PT field (low 7 bits) is literal 0");
        assert_eq!(buf[1] & 0x80, 0, "M bit clear");
    }

    #[test]
    fn packetize_error_conversions_preserve_source() {
        let e: PacketizeError = JfifError::NotJfif.into();
        assert!(matches!(e, PacketizeError::Jfif(JfifError::NotJfif)));
        let io_e: PacketizeError = io::Error::new(io::ErrorKind::BrokenPipe, "x").into();
        assert!(matches!(io_e, PacketizeError::Io(_)));
        assert_eq!(e.to_string(), "jfif: missing SOI (FF D8)");
    }
}