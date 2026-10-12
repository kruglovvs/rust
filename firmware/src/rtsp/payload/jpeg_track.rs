//! RFC 2435 RTP/JPEG video track — payload type **26** (plan Todo 12).
//!
//! Plan contract, quoted from `.omo/plans/esp32s3-av-stream-and-sd-recording.md`
//! Todo 12:
//!
//! > **Do:** wire the payloader chosen in Todo 6 into the video track. […]
//! > The video track is `a=rtpmap:26 JPEG/90000`. […] One RTP timestamp unit
//! > is … 1/90000 s for video.
//!
//! Todo 11's interface contract (from `rtsp/mod.rs`, honoured here):
//!
//! > **TCP interleaved framing is handled here, not in the payloader.** … The
//! > payloader never sees or parses this framing; it just calls
//! > [`crate::rtsp::server::RtpSender::send_rtp`]. **RTP-Info contract**: the
//! > first packet of each track in a session must carry `seq = 0` and
//! > `rtptime = 0`.
//!
//! This module is a thin integration layer over Todo 6's `jfif.rs` (which is
//! reused unchanged — parse → RFC 2435 fragments → header/payload splits) plus
//! the RTP header. Fragment sizing, marker-bit placement, Q-table policy,
//! restart handling and the EOI-exclusion policy are all `jfif`'s, asserted by
//! its own gate-(a) suite; this layer adds seq/rtptime bookkeeping and packet
//! emission.

use super::PacketizeError;
use crate::jfif::{Frame, MAX_HEADER_LEN};
use std::io;

/// Static RTP payload type for JPEG — plan lock (SDP: `a=rtpmap:26 JPEG/90000`).
pub const PAYLOAD_TYPE: u8 = 26;

/// RTP clock for this track: 1 timestamp unit = 1/90000 s (plan Todo 12).
pub const CLOCK_RATE_HZ: u32 = 90_000;

/// RTP header is fixed 12 bytes; the RFC 2435 header follows it inline.
pub const RTP_HEADER_LEN: usize = 12;

/// Max RTP payload per packet: the reference design's 1400-byte MTU
/// (`esp-cpp/camera-streamer` — plan Todo 11 references) — one RTP fragment
/// per packet, fragment count = ceil(scan / (1400 − header)).
pub const MAX_PAYLOAD: usize = 1400;

/// RTP timestamp advance per captured frame: 900 units = 100 ms @90 kHz
/// (plan Todo 12: "seq/timestamp (90 kHz: += 900 per 100 ms frame)"). All
/// fragments of one frame share one timestamp; seq advances per packet.
pub const FRAME_TIMESTAMP_STEP: u32 = 900;

/// Largest datagram this track can emit: header + worst-case RFC 2435 header
/// (main 8 + restart 4 + quant 4 + tables 128) + payload.
pub const PACKET_MAX: usize = RTP_HEADER_LEN + MAX_HEADER_LEN + MAX_PAYLOAD;

/// Sink for already-packetized RTP datagrams — matches
/// `RtpSender::send_rtp(&[u8]) -> io::Result<()>` so firmware wiring is a
/// one-liner: `track.packetize(jpeg, &mut |p| sender.send_rtp(p))`.
pub type RtpSink<'a> = &'a mut dyn FnMut(&[u8]) -> io::Result<()>;

/// The video packetizer: owns seq/rtptime bookkeeping and a fixed-size
/// packet buffer (no heap — `[u8; PACKET_MAX]` in the struct).
///
/// Starts at `seq = 0, rtptime = 0` (RTP-Info contract). Todo 13 reads
/// [`Self::seq`]/[`Self::rtptime`]/[`Self::ssrc`] for RTCP Sender Reports.
pub struct JpegTrack {
    seq: u16,
    rtptime: u32,
    ssrc: u32,
    packet: [u8; PACKET_MAX],
}

impl JpegTrack {
    /// New session: seq 0, rtptime 0 (RTP-Info contract).
    pub const fn new(ssrc: u32) -> Self {
        Self {
            seq: 0,
            rtptime: 0,
            ssrc,
            packet: [0; PACKET_MAX],
        }
    }

    /// Packetize one complete JFIF file into RTP datagrams and hand each to
    /// `send`. Returns the packet count.
    ///
    /// Per packet: V=2, M = `jfif`'s fragment marker (last fragment of the
    /// frame only), PT 26, seq +1 per packet, timestamp = this frame's
    /// `rtptime` (shared by every fragment), SSRC constant. After the frame:
    /// `rtptime += `[`FRAME_TIMESTAMP_STEP`] (900 per 100 ms frame).
    /// Errors: any `jfif` typed parse/fragment failure, or the sink's `io`.
    pub fn packetize(&mut self, jpeg: &[u8], send: RtpSink<'_>) -> Result<usize, PacketizeError> {
        let frame = Frame::parse(jpeg)?;
        let mut sent = 0usize;
        for frag in frame.fragments(MAX_PAYLOAD)? {
            let total = RTP_HEADER_LEN + frag.header_len + frag.payload.len();
            super::write_rtp_header(
                &mut self.packet[..RTP_HEADER_LEN],
                frag.marker,
                PAYLOAD_TYPE,
                self.seq,
                self.rtptime,
                self.ssrc,
            );
            self.packet[RTP_HEADER_LEN..RTP_HEADER_LEN + frag.header_len]
                .copy_from_slice(&frag.header[..frag.header_len]);
            self.packet[RTP_HEADER_LEN + frag.header_len..total].copy_from_slice(frag.payload);
            send(&self.packet[..total])?;
            self.seq = self.seq.wrapping_add(1);
            sent += 1;
        }
        self.rtptime = self.rtptime.wrapping_add(FRAME_TIMESTAMP_STEP);
        Ok(sent)
    }

    /// Next sequence number (0 for the session's first packet).
    pub fn seq(&self) -> u16 {
        self.seq
    }

    /// Next frame's RTP timestamp (0 for the session's first frame).
    pub fn rtptime(&self) -> u32 {
        self.rtptime
    }

    /// SSRC — constant for the whole session.
    pub fn ssrc(&self) -> u32 {
        self.ssrc
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Header/field contracts that do not need a real JPEG: constant locks,
    /// PT 26 in the actual header byte, and sink-error propagation.
    #[test]
    fn constants_match_plan_locks() {
        assert_eq!(PAYLOAD_TYPE, 26, "static PT for JPEG (SDP a=rtpmap:26 JPEG/90000)");
        assert_eq!(CLOCK_RATE_HZ, 90_000, "video clock 1/90000 s");
        assert_eq!(FRAME_TIMESTAMP_STEP, 900, "900 units = 100 ms @90 kHz");
        assert_eq!(MAX_PAYLOAD, 1400, "reference-design MTU payload");
    }

    #[test]
    fn header_writer_places_pt_26_and_marker_in_actual_byte() {
        let mut buf = [0u8; 12];
        super::super::write_rtp_header(&mut buf, false, PAYLOAD_TYPE, 0, 0, 0x55);
        assert_eq!(buf[1], 26, "M=0 → raw byte is exactly the PT");
        super::super::write_rtp_header(&mut buf, true, PAYLOAD_TYPE, 6, 900, 0x55);
        assert_eq!(buf[1], 0x80 | 26, "M=1 sets bit 7, PT 26 in low bits");
        assert_eq!(buf[1] & 0x7F, 26);
    }

    #[test]
    fn non_jpeg_input_is_typed_error_and_sink_is_never_called() {
        let mut t = JpegTrack::new(1);
        let mut called = 0usize;
        let err = t
            .packetize(&[0x00, 0x01, 0x02, 0x03], &mut |_| {
                called += 1;
                Ok(())
            })
            .expect_err("not a JPEG");
        assert!(matches!(err, PacketizeError::Jfif(_)));
        assert_eq!(called, 0, "no packet may be emitted for an unparseable frame");
        assert_eq!(t.rtptime(), 0, "failed frame must not advance the clock");
        assert_eq!(t.seq(), 0);
    }

    #[test]
    fn parse_error_precedes_any_sink_call() {
        let mut t = JpegTrack::new(1);
        let mut seen = 0usize;
        let err = t
            .packetize(&[0xFF, 0xD8, 0x00, 0x01], &mut |_| {
                seen += 1;
                Ok(())
            })
            .expect_err("SOI then a corrupt segment: typed parse failure");
        assert!(matches!(err, PacketizeError::Jfif(_)));
        assert_eq!(seen, 0, "the sink must not see a packet for a failed parse");
        assert_eq!(t.rtptime(), 0, "failed frame must not advance the clock");
        assert_eq!(t.seq(), 0);
    }
}