//! The connection model and the crate's only entry point.
//!
//! Only the public shape exists today: the accept loop, the worker budget and
//! the socket timeouts arrive in the `start` implementation change.

/// Everything the server needs to answer requests. Plain data: no handles and
/// no callbacks, so the caller cannot leak device state into the server.
pub struct Config {
    pub port: u16,
    pub mount_point: &'static str,
    pub rtsp_port: u16,
    pub rtsp_path: &'static str,
    pub rtsp_running: bool,
    pub recording_enabled: bool,
}

/// A running server.
pub struct Server;

/// Start the server on `config.port`.
///
/// The accept loop is not implemented yet, so this reports the gap instead of
/// pretending to serve.
pub fn start(_config: Config) -> Result<Server, std::io::Error> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "fileserver: server not implemented yet",
    ))
}
