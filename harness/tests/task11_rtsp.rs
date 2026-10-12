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
