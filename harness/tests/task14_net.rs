//! Host-executed tests for plan Todo 14: `session.rs` (backpressure +
//! lifecycle), `net.rs` (credentials/SNTP clock/backoff) and
//! `artifact_server.rs` (path parsing + size cap).
//!
//! All three files keep their pure logic free of `esp_idf_*` imports (the
//! device glue is `#[cfg(target_os = "espidf")]`-gated inside `net.rs` /
//! `artifact_server.rs`), so the `#[path]` include — the proven
//! `audio_quality` / `task10_recorder` pattern — compiles them unchanged on
//! the host and their `#[cfg(test)]` suites RUN here (no board needed).

#[path = "../../firmware/src/session.rs"]
mod session;

#[path = "../../firmware/src/net.rs"]
mod net;

#[path = "../../firmware/src/artifact_server.rs"]
mod artifact_server;
