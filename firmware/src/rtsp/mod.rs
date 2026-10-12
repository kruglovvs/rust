//! RTSP session layer (plan Todo 11).
//!
//! Plan contract, quoted from `.omo/plans/esp32s3-av-stream-and-sd-recording.md`
//! Todo 11:
//!
//! > **References:** No Rust RTSP server crate of any maturity exists — […]
//! > Expect roughly 300 lines.
//! > **Do:** add `firmware/src/rtsp/` implementing `OPTIONS`, `DESCRIBE`,
//! > `SETUP`, `PLAY`, `TEARDOWN` (and `GET_PARAMETER` if needed), SDP
//! > generation, plus `Transport:` header parsing for both TCP-interleaved
//! > and UDP. Single session at a time; a second client is rejected cleanly
//! > rather than corrupting the first.
//! > **Acceptance:** `ffprobe rtsp://<ip>:554/mjpeg/1` connects, reports both
//! > streams, and returns cleanly on quit. A second concurrent client is
//! > refused without disturbing the first.
//!
//! Locks honoured here:
//! - **SDP codec strings** (plan round-1 fix): `a=rtpmap:0 PCMU/8000` and
//!   `a=rtpmap:26 JPEG/90000` — generated only by [`sdp::generate`], asserted
//!   byte-exact in its unit tests and again by `harness/tests/task11_rtsp.rs`
//!   (Todo 15 re-asserts on-device). No PCMA, no L16, no dynamic PTs.
//! - **Path**: [`STREAM_PATH`] is `/mjpeg/1`, matching the mDNS `txt=path`
//!   advertised by `net.rs` (Todo 14) — discovery and DESCRIBE agree by
//!   construction.
//! - **Single session**: [`server::RtspServer`] admits one session; every
//!   refusal path leaves the active session and the Todo 14 lifecycle state
//!   untouched (asserted).
//! - **No new crates**: blocking `std::net` sockets on threads (plan R14 —
//!   no tokio). [`service::Service::spawn`] binds the listener and runs one
//!   thread per connection.
//!
//! # Interface for Todo 12 (RTP JPEG + G.711 payloaders)
//!
//! After `PLAY`, obtain the per-track send hook:
//!
//! ```ignore
//! // service::Service keeps the state machine behind Arc<Mutex<..>>;
//! let sender = server.lock()...sender(Track::Video).expect("SETUP done");
//! sender.send_rtp(&rtp_packet)?;     // one already-packetized RTP datagram
//! sender.send_rtcp(&sr_packet)?;     // Todo 13 sender reports
//! ```
//!
//! **TCP interleaved framing is handled here, not in the payloader.**
//! RFC 2326 §10.12 frames each chunk on the RTSP connection as:
//!
//! ```text
//! '$' (0x24) | channel (1 byte) | length (2 bytes, big-endian) | payload
//! ```
//!
//! Video uses channels 0 (RTP) / 1 (RTCP), audio 2 / 3 — assigned by the
//! server at SETUP and echoed in the `Transport:` response (clients re-offer
//! `interleaved=0-1` for every track; the server reassigns so tracks cannot
//! collide). The payloader never sees or parses this framing; it just calls
//! [`server::RtpSender::send_rtp`].
//!
//! **RTP-Info contract**: the first packet of each track in a session must
//! carry `seq = 0` and `rtptime = 0`, matching the `RTP-Info` the first PLAY
//! response emits; call [`server::RtspServer::note_rtp_sent`] after that to
//! keep later PLAY responses in step.
//!
//! **Lifecycle (Todo 14)**: the service applies the [`crate::session::SessionAction`]s
//! returned by every dispatch — `PLAY` produces `StartCapture`,
//! `TEARDOWN`/disconnect produce `StopCapture` + `TearDownServer`
//! (behaviour (d): teardown from the disconnect callback). Lock order is
//! always *rtsp state → lifecycle*, never the reverse.

pub mod request;
pub mod sdp;
pub mod server;
pub mod service;
pub mod transport;

/// Value of the `Server:` header on every response (plan MUST DO #2:
/// "`Server:` header present").
pub const SERVER_HEADER: &str = "esp32s3-av/1.0";

/// The presentation path — must equal the mDNS `txt=path` advertised by
/// `net.rs` (Todo 14: `("path", "/mjpeg/1")`).
pub const STREAM_PATH: &str = "/mjpeg/1";

/// `Public:` / `Allow:` header value: exactly the methods this server
/// implements (anything else answers 405).
pub const PUBLIC_METHODS: &str = "OPTIONS, DESCRIBE, SETUP, PLAY, TEARDOWN, GET_PARAMETER";

/// RFC 2326 §10.12 interleaved framing: `'$' | channel | length (u16 BE) |
/// payload`. Length is capped at `u16::MAX` by construction — RTP packets
/// (≤ 1400-byte payloads per the reference design) and RTCP reports are far
/// below it; larger inputs are a programming error upstream and are clamped
/// rather than silently truncated mid-header.
pub fn interleaved_frame(channel: u8, payload: &[u8]) -> Vec<u8> {
    let len = u16::try_from(payload.len()).unwrap_or(u16::MAX);
    let mut frame = Vec::with_capacity(4 + len as usize);
    frame.push(b'$');
    frame.push(channel);
    frame.extend_from_slice(&len.to_be_bytes());
    frame.extend_from_slice(payload);
    frame
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interleaved_frame_is_dollar_channel_be_length_payload() {
        assert_eq!(
            interleaved_frame(0, &[0x80, 0x00]),
            vec![0x24, 0x00, 0x00, 0x02, 0x80, 0x00]
        );
        // 300-byte payload: header is '$' | 3 | 0x012C (300, big-endian).
        let frame = interleaved_frame(3, &[0xAB; 300]);
        assert_eq!(frame[..4], [0x24, 0x03, 0x01, 0x2C]);
        assert_eq!(&frame[4..], &[0xAB; 300][..]);
    }

    #[test]
    fn interleaved_frame_empty_payload_has_zero_length() {
        assert_eq!(interleaved_frame(1, &[]), vec![0x24, 0x01, 0x00, 0x00]);
    }

    #[test]
    fn path_constant_matches_mdns_advertisement() {
        // net.rs Todo 14: &[("path", "/mjpeg/1")] — keep the two in lockstep.
        assert_eq!(STREAM_PATH, "/mjpeg/1");
    }
}
