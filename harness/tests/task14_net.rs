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

/// Additional rejection cases for the flat artifact namespace. The module's
/// own suite already covers traversal, separators and control characters;
/// these are the remaining boundaries that were not exercised anywhere.
#[test]
fn artifact_path_rejects_prefix_only_and_stray_escapes() {
    use artifact_server::{parse_artifact_path, ArtifactPathError};

    assert_eq!(
        parse_artifact_path("/files"),
        Err(ArtifactPathError::MissingPrefix),
        "the prefix requires the trailing slash"
    );
    assert_eq!(
        parse_artifact_path("/files?x"),
        Err(ArtifactPathError::MissingPrefix)
    );
    assert_eq!(
        parse_artifact_path("/files/?"),
        Err(ArtifactPathError::EmptyName)
    );
    assert_eq!(
        parse_artifact_path("/files/%"),
        Err(ArtifactPathError::EncodedEscape),
        "a bare percent must not be decoded or forwarded"
    );
    assert_eq!(
        parse_artifact_path("/files/a\u{7f}b"),
        Err(ArtifactPathError::ControlChar),
        "DEL (0x7F) is a control character too"
    );
    assert_eq!(parse_artifact_path("/files/a#frag"), Ok("a"));
}
