//! Host-executed tests for plan Todo 11: the RTSP session layer
//! (`firmware/src/rtsp/`).
//!
//! The whole layer is pure `std` (blocking sockets, no `esp_idf_*` imports),
//! so the `#[path]` include — the proven `audio_quality` / `task14_net`
//! pattern — compiles it unchanged on the host and every `#[cfg(test)]`
//! suite here RUNS (no board needed): SDP exact-string assertions (the
//! plan-locked `a=rtpmap:0 PCMU/8000` / `a=rtpmap:26 JPEG/90000`), request
//! parsing, the 4xx matrix (400/404/405/406/453/454/461), single-session
//! refusal, CSeq echoing, `$`-interleaved framing, and a real loopback
//! socket lifecycle including a second-client 453.
//!
//! `session.rs` is included first because the RTSP layer consumes Todo 14's
//! lifecycle (`crate::session::with_session` / `SessionAction`) — `crate::`
//! paths resolve to this test crate's root exactly as they do in the
//! firmware binary.

#[path = "../../firmware/src/session.rs"]
mod session;

#[path = "../../firmware/src/rtsp/mod.rs"]
mod rtsp;

/// Wire-framing negatives not covered by the module suite: a numeric field
/// that cannot be a length, and a header block that is not UTF-8. Both must
/// surface a typed decode error so the service drops the connection instead
/// of panicking or treating garbage as a request.
#[test]
fn rtsp_decoder_rejects_unparseable_length_and_non_utf8_headers() {
    use rtsp::service::{DecodeError, FrameDecoder};

    let mut d = FrameDecoder::new();
    d.push(b"OPTIONS rtsp://h/x RTSP/1.0\r\nCSeq: 1\r\nContent-Length: abc\r\n\r\n");
    assert_eq!(d.next(), Err(DecodeError::MalformedHeader));

    let mut d = FrameDecoder::new();
    d.push(b"OPTIONS rtsp://h/x RTSP/1.0\r\nCSeq: 1\r\nContent-Length: -1\r\n\r\n");
    assert_eq!(d.next(), Err(DecodeError::MalformedHeader));

    let mut d = FrameDecoder::new();
    d.push(&[0xFF, 0xFE, b'\r', b'\n', b'\r', b'\n']);
    assert_eq!(d.next(), Err(DecodeError::MalformedHeader));
}
