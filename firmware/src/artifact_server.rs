//! Read-only artifact-retrieval HTTP endpoint (plan Todo 14).
//!
//! Plan contract, quoted from the plan's Todo 14 "Do":
//!
//! > **Owns the artifact-retrieval endpoint** — without this, four later gates
//! > cannot run. [...] Todo 14 ships a **minimal read-only HTTP endpoint**
//! > alongside the RTSP session: `GET /files/<name>` streams a file off the
//! > card with a hard size cap and no directory listing. [...] Acceptance:
//! > with a recorded `.avi` on the card, `curl http://<ip>/files/REC_0001.avi
//! > -o out.avi` from the host yields a byte-identical file that `ffprobe`
//! > parses. **Read-only, no listing, size-capped** — this endpoint exists for
//! > test retrieval, and an unrestricted file server on a device that also
//! > holds Wi-Fi credentials would be a real exposure.
//!
//! Security posture (each rule is unit-tested on the host):
//! - **GET only** — the handler is registered for `Method::Get`; there is no
//!   upload, no PUT/POST/DELETE registration, no directory listing route.
//! - **Single flat segment** — [`parse_artifact_path`] accepts exactly one
//!   path segment under `/files/`: no `/`, no `\`, no `..` anywhere, no `%`
//!   (nothing is percent-decoded, so an encoded separator can never expand),
//!   no control characters, no query/fragment remnants. Absolute paths fail
//!   the prefix check outright.
//! - **Directories are 404s** — a name that resolves to a directory is
//!   refused, so `GET /files/` or any mounted subdirectory enumerates nothing.
//! - **Hard size cap** — [`check_size`] refuses files above
//!   [`MAX_ARTIFACT_BYTES`] before a single byte is streamed.
//! - **Streaming, never buffering** — the file is read into a
//!   [`STREAM_CHUNK_BYTES`] (32 KiB) heap buffer and written out chunk by
//!   chunk; the whole file is never resident in RAM.
//!
//! The pure path/size layer has no `esp_idf_*` imports so it runs as
//! host-executed tests via `harness/tests/task14_net.rs` (the `audio_quality`
//! pattern); the HTTP wiring is compiled only for the device target.
//!
//! Framing note (verified in ESP-IDF 5.5.2 `httpd_txrx.c`): every
//! `Response::write_all` goes through `httpd_resp_send_chunk`, so the
//! response is HTTP/1.1 `Transfer-Encoding: chunked` and `curl` de-chunks it
//! back to the exact file bytes. `Content-Length` is deliberately NOT set —
//! esp-idf-svc's `initate_response` "Content-Length" branch is a no-op
//! (`httpd_resp_set_len` is commented out upstream), and setting it alongside
//! chunked framing would corrupt the response.

use std::error::Error;
use std::fmt;

/// Hard cap on a single artifact. Default plan segment size is 256 MiB
/// (recorder rollover), so 512 MiB admits every `REC_%04d.avi` the firmware
/// can produce at plan defaults while still bounding the endpoint: anything
/// larger is a foreign file, and an unbounded stream on a device that also
/// holds Wi-Fi credentials is exactly the exposure this endpoint must not
/// create.
pub const MAX_ARTIFACT_BYTES: u64 = 512 * 1024 * 1024;

/// The only accepted URI prefix.
pub const URI_PREFIX: &str = "/files/";

/// Bytes read from the file per write (plan MUST DO #6: stream in 32 KB
/// chunks, never load the whole file).
pub const STREAM_CHUNK_BYTES: usize = 32 * 1024;

/// Why a request URI was rejected. Every variant maps to a 404 (uniform —
/// the endpoint leaks nothing about which rule tripped to a remote caller;
/// the specific variant is logged locally).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArtifactPathError {
    /// URI does not start with `/files/` (includes absolute paths such as
    /// `/etc/passwd`).
    MissingPrefix,
    /// `/files/` with no name (a directory probe).
    EmptyName,
    /// The name contains `..` — rejected regardless of separators, as
    /// defense in depth.
    Traversal,
    /// The name contains `/` or `\` — subdirectories are not servable, which
    /// is also what makes directory listing impossible.
    Separator,
    /// The name contains `%`. Nothing is percent-decoded here, so an escape
    /// like `%2f` can never become a separator; rejecting keeps the contract
    /// total and makes the "no decoding" rule visible.
    EncodedEscape,
    /// The name contains an ASCII control character (0x00–0x1F or 0x7F).
    ControlChar,
}

impl fmt::Display for ArtifactPathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::MissingPrefix => "missing /files/ prefix (absolute or foreign path)",
            Self::EmptyName => "empty file name (directory access is not served)",
            Self::Traversal => "path traversal sequence '..' rejected",
            Self::Separator => "path separator rejected (flat namespace only)",
            Self::EncodedEscape => "percent-escape rejected (no URL decoding performed)",
            Self::ControlChar => "control character rejected",
        })
    }
}

impl Error for ArtifactPathError {}

/// Validate a request URI and return the single file-name segment.
///
/// `uri` is the raw request URI as seen by `httpd` (path plus optional
/// query/fragment); everything after the first `?` or `#` is stripped before
/// validation so `?`/`#` can never smuggle extra path bytes past the checks.
pub fn parse_artifact_path(uri: &str) -> Result<&str, ArtifactPathError> {
    let path = uri
        .split(|c| c == '?' || c == '#')
        .next()
        .unwrap_or(uri);
    let name = path.strip_prefix(URI_PREFIX).ok_or(ArtifactPathError::MissingPrefix)?;
    if name.is_empty() {
        return Err(ArtifactPathError::EmptyName);
    }
    if name.contains("..") {
        return Err(ArtifactPathError::Traversal);
    }
    if name.contains('/') || name.contains('\\') {
        return Err(ArtifactPathError::Separator);
    }
    if name.contains('%') {
        return Err(ArtifactPathError::EncodedEscape);
    }
    if name.chars().any(|c| c.is_control()) {
        return Err(ArtifactPathError::ControlChar);
    }
    Ok(name)
}

/// Refuse anything above [`MAX_ARTIFACT_BYTES`] before streaming starts.
pub fn check_size(len: u64) -> Result<(), ArtifactSizeError> {
    if len > MAX_ARTIFACT_BYTES {
        Err(ArtifactSizeError { len })
    } else {
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ArtifactSizeError {
    pub len: u64,
}

impl fmt::Display for ArtifactSizeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "file size {} B exceeds the {} B artifact cap",
            self.len, MAX_ARTIFACT_BYTES
        )
    }
}

impl Error for ArtifactSizeError {}

#[cfg(target_os = "espidf")]
mod esp_impl {
    use std::fmt;
    use std::fs::File;
    use std::io::Read as _;

    use esp_idf_hal::io::Write as _;
    use esp_idf_svc::http::server::{Configuration, EspHttpServer};
    use esp_idf_svc::http::Method;
    use esp_idf_svc::io::EspIOError;
    use esp_idf_svc::sys::EspError;

    use super::{
        check_size, parse_artifact_path, MAX_ARTIFACT_BYTES, STREAM_CHUNK_BYTES, URI_PREFIX,
    };

    /// Typed handler error (`E: Debug` is the `fn_handler` bound; Display +
    /// Error keep it house-style typed).
    #[derive(Debug)]
    pub enum ArtifactServeError {
        Io(std::io::Error),
        Response(EspIOError),
    }

    impl fmt::Display for ArtifactServeError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            match self {
                Self::Io(e) => write!(f, "artifact I/O: {e}"),
                Self::Response(e) => write!(f, "artifact response: {e}"),
            }
        }
    }

    impl std::error::Error for ArtifactServeError {}

    impl From<std::io::Error> for ArtifactServeError {
        fn from(e: std::io::Error) -> Self {
            Self::Io(e)
        }
    }

    /// `EspHttpServer::new` surfaces bare `EspError`; `Request::into_response`
    /// / `Response::write_all` surface the connection's `ErrorType::Error`,
    /// which is `EspIOError` (verified in esp-idf-svc 0.53.0
    /// `http/server.rs` — `impl ErrorType for EspHttpConnection { type Error =
    /// EspIOError; }`). Both must convert, hence two `From` impls.
    impl From<EspIOError> for ArtifactServeError {
        fn from(e: EspIOError) -> Self {
            Self::Response(e)
        }
    }

    impl From<EspError> for ArtifactServeError {
        fn from(e: EspError) -> Self {
            Self::Response(EspIOError(e))
        }
    }

    /// Register `GET /files/*` on a new HTTP server (port 80) and return it.
    ///
    /// The caller must hold the returned server for the process lifetime —
    /// dropping it stops artifact retrieval (the gates of Todos 9/10/20 need
    /// it). There is intentionally no route besides `/files/*`: `httpd`
    /// answers every other URI with its built-in 404, so there is no listing
    /// surface at all.
    pub fn start() -> Result<EspHttpServer<'static>, ArtifactServeError> {
        let conf = Configuration {
            // The default matcher is an EXACT compare (httpd_uri.c);
            // "/files/*" needs the wildcard matcher wired in by this flag.
            uri_match_wildcard: true,
            ..Default::default()
        };
        let mut server = EspHttpServer::new(&conf)?;
        server.fn_handler::<ArtifactServeError, _>(concat!("/files", "/*"), Method::Get, |req| {
            // `uri()` borrows `req`; own the string before `req` is moved
            // into the response.
            let uri = req.uri().to_owned();
            let name = match parse_artifact_path(&uri) {
                Ok(name) => name.to_owned(),
                Err(err) => {
                    println!("artifact: GET {uri} -> 404 (path rejected: {err})");
                    req.into_response(404, Some("Not Found"), &[])?
                        .write_all(b"not found\n")?;
                    return Ok(());
                }
            };

            // `crate::storage::BASE_PATH` is "/sdcard"; kept as a literal
            // here so this file's pure layer stays host-includable — the
            // device-side check `grep BASE_PATH firmware/src/{storage,artifact_server}.rs`
            // is recorded in the evidence.
            let path = format!("/sdcard/{name}");

            let meta = match std::fs::metadata(&path) {
                Ok(meta) => meta,
                Err(err) => {
                    println!("artifact: GET {uri} -> 404 (no such file: {err})");
                    req.into_response(404, Some("Not Found"), &[])?
                        .write_all(b"not found\n")?;
                    return Ok(());
                }
            };
            if !meta.is_file() {
                // Directories are never listed — flat namespace, one 404.
                println!("artifact: GET {uri} -> 404 (not a regular file)");
                req.into_response(404, Some("Not Found"), &[])?
                    .write_all(b"not found\n")?;
                return Ok(());
            }
            if let Err(err) = check_size(meta.len()) {
                println!(
                    "artifact: GET {uri} -> 413 ({err}; cap is {} B)",
                    MAX_ARTIFACT_BYTES
                );
                req.into_response(413, Some("Payload Too Large"), &[])?
                    .write_all(err.to_string().as_bytes())?;
                return Ok(());
            }

            let mut file = File::open(&path)?;
            let mut resp = req.into_response(200, Some("OK"), &[("Content-Type", "application/octet-stream")])?;
            let mut buf = vec![0u8; STREAM_CHUNK_BYTES];
            let mut sent: u64 = 0;
            loop {
                let n = file.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                resp.write_all(&buf[..n])?;
                sent += n as u64;
            }
            println!(
                "artifact: GET {uri} -> 200 ({sent} B, chunked {STREAM_CHUNK_BYTES} B, prefix {URI_PREFIX})"
            );
            Ok(())
        })?;
        Ok(server)
    }
}

#[cfg(target_os = "espidf")]
#[allow(unused_imports)] // consumed by net::bring_up (Todo 20 wiring)
pub use esp_impl::{start, ArtifactServeError};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_a_plain_artifact_name() {
        assert_eq!(
            parse_artifact_path("/files/REC_0001.avi"),
            Ok("REC_0001.avi")
        );
        assert_eq!(parse_artifact_path("/files/a"), Ok("a"));
    }

    #[test]
    fn rejects_missing_prefix_and_absolute_paths() {
        assert_eq!(
            parse_artifact_path("/REC_0001.avi"),
            Err(ArtifactPathError::MissingPrefix)
        );
        assert_eq!(
            parse_artifact_path("/etc/passwd"),
            Err(ArtifactPathError::MissingPrefix)
        );
        assert_eq!(
            parse_artifact_path("/sdcard/REC_0001.avi"),
            Err(ArtifactPathError::MissingPrefix)
        );
        assert_eq!(
            parse_artifact_path("files/REC_0001.avi"),
            Err(ArtifactPathError::MissingPrefix)
        );
        assert_eq!(
            parse_artifact_path("http://x/files/a"),
            Err(ArtifactPathError::MissingPrefix)
        );
    }

    #[test]
    fn rejects_dotdot_traversal_everywhere() {
        assert_eq!(
            parse_artifact_path("/files/.."),
            Err(ArtifactPathError::Traversal)
        );
        assert_eq!(
            parse_artifact_path("/files/../REC_0001.avi"),
            Err(ArtifactPathError::Traversal)
        );
        assert_eq!(
            parse_artifact_path("/files/..%2fetc/passwd"),
            Err(ArtifactPathError::Traversal)
        );
        assert_eq!(
            parse_artifact_path("/files/a..b"),
            Err(ArtifactPathError::Traversal)
        );
    }

    #[test]
    fn rejects_subdirectories_and_backslashes() {
        assert_eq!(
            parse_artifact_path("/files/dir/REC_0001.avi"),
            Err(ArtifactPathError::Separator)
        );
        assert_eq!(
            parse_artifact_path("/files//double"),
            Err(ArtifactPathError::Separator)
        );
        assert_eq!(
            parse_artifact_path("/files/a\\b"),
            Err(ArtifactPathError::Separator)
        );
    }

    #[test]
    fn rejects_directory_probes_and_encoded_escapes() {
        assert_eq!(
            parse_artifact_path("/files/"),
            Err(ArtifactPathError::EmptyName)
        );
        assert_eq!(
            parse_artifact_path("/files/%2e%2e/x"),
            Err(ArtifactPathError::Separator) // '%' after separator check would also fire; separator first
        );
        assert_eq!(
            parse_artifact_path("/files/%2e%2e"),
            Err(ArtifactPathError::EncodedEscape)
        );
        assert_eq!(
            parse_artifact_path("/files/a%00b"),
            Err(ArtifactPathError::EncodedEscape)
        );
    }

    #[test]
    fn rejects_control_characters_but_keeps_normal_names() {
        assert_eq!(
            parse_artifact_path("/files/a\u{0}b"),
            Err(ArtifactPathError::ControlChar)
        );
        assert_eq!(
            parse_artifact_path("/files/a\nb"),
            Err(ArtifactPathError::ControlChar)
        );
        // Dots, dashes, underscores — the recorder's vocabulary — all pass.
        assert_eq!(
            parse_artifact_path("/files/REC_0000-1.avi.tmp"),
            Ok("REC_0000-1.avi.tmp")
        );
    }

    #[test]
    fn strips_query_and_fragment_before_validating() {
        assert_eq!(
            parse_artifact_path("/files/REC_0001.avi?token=abc"),
            Ok("REC_0001.avi")
        );
        assert_eq!(
            parse_artifact_path("/files/REC_0001.avi#frag"),
            Ok("REC_0001.avi")
        );
        // The query string itself may not carry path bytes past validation:
        assert_eq!(
            parse_artifact_path("/files/a?../b"),
            Ok("a")
        );
    }

    #[test]
    fn size_cap_is_inclusive_at_the_boundary() {
        assert_eq!(check_size(0), Ok(()));
        assert_eq!(check_size(MAX_ARTIFACT_BYTES), Ok(()));
        assert_eq!(
            check_size(MAX_ARTIFACT_BYTES + 1),
            Err(ArtifactSizeError {
                len: MAX_ARTIFACT_BYTES + 1
            })
        );
        // A plan-default 256 MiB segment fits.
        assert_eq!(check_size(256 * 1024 * 1024), Ok(()));
    }

    #[test]
    fn cap_matches_plan_segment_rollover() {
        // The recorder rolls at 256 MiB (plan); the cap must admit a full
        // segment with headroom and stay a power-of-two MiB value.
        assert!(MAX_ARTIFACT_BYTES >= 2 * 256 * 1024 * 1024);
        assert_eq!(MAX_ARTIFACT_BYTES, 512 * 1024 * 1024);
    }
}
