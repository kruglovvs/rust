//! Host-executed tests for plan Todo 14: `session.rs` (backpressure +
//! lifecycle) and `net.rs` (credentials/SNTP clock/backoff).
//!
//! Both files keep their pure logic free of `esp_idf_*` imports (the device
//! glue is `#[cfg(target_os = "espidf")]`-gated inside `net.rs`), so the
//! `#[path]` include compiles them unchanged on the host and their
//! `#[cfg(test)]` suites RUN here (no board needed).

#[path = "../../components/net/src/session.rs"]
mod session;

#[path = "../../components/net/src/net.rs"]
mod net;
