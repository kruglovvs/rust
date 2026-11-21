//! The `EspHttpServer` glue: one handler per method and URI, the heap guard,
//! and the crate's only entry point.
//!
//! # Contract
//!
//! ```text
//! pub struct Config { pub port: u16, pub mount_point: &'static str,
//!                     pub rtsp_port: u16, pub rtsp_path: &'static str,
//!                     pub rtsp_running: bool, pub recording_enabled: bool }
//! pub enum Error {
//!     Netif(esp_idf_sys::EspError),   // LwIP could not be initialised
//!     Start(EspIOError),              // httpd was not created or a handler was refused
//!     Budget { free: usize, required: usize }, // less internal RAM than the floor
//! }
//! pub struct Server;                  // owns the running httpd
//! pub fn start(config: Config) -> Result<Server, Error>;
//! ```
//!
//! [`Config`] is plain data - port, mount point and the three facts the status
//! document reports, all `Copy`, no handles and no callbacks - so the caller
//! cannot leak device state into the server, and [`start`] is the only thing
//! the rest of the firmware calls. A `mount_point` that is not an absolute VFS
//! path simply serves no files (every download maps to `404`); it is not a
//! reason to refuse to start, because the status endpoint must answer on a boot
//! without a card.
//!
//! # Server
//!
//! [`start`] brings LwIP up and creates one [`EspHttpServer`] with
//! `max_open_sockets: 2`, `stack_size: 16 KiB`, `max_uri_handlers: 10` and
//! wildcard URI matching. It then registers `GET` and `HEAD` for `/`,
//! `/api/v1/status`, `/api/v1/files` and `/api/v1/openapi.json`, and for
//! `/api/v1/files/*` last, because ESP-IDF refuses a handler that a registered
//! wildcard already covers. Every handler runs [`crate::plan::plan`] and sends
//! its result:
//!
//! - `initiate_response(status, reason, headers)` sets the status, the content
//!   type and the custom headers (`Accept-Ranges`, `Content-Range`);
//! - a JSON or static body is written in one `write_all`, a file in
//!   [`crate::body::CHUNK`] pieces through [`crate::body::chunk_plan`] and the
//!   [`crate::body::FileSource`];
//! - a `HEAD`, a `416` and an error body are empty, so httpd answers
//!   `Content-Length: 0`; every non-empty body is `Transfer-Encoding: chunked`.
//!
//! The binding silently ignores a `Content-Length` passed to
//! `initiate_response` and its `write_all` always uses `httpd_resp_send_chunk`,
//! so no route announces a length. This is the accepted trade: a download is
//! chunked, and `Range`/`Content-Range` in [`crate::plan`] are what make it
//! resumable.
//!
//! A request whose URI is registered for another method is answered `405` by
//! `httpd_find_uri_handler` in `components/esp_http_server/src/httpd_uri.c`,
//! which is what [`crate::plan`] selects too; a URI no handler matches is
//! `404`. An unparsable request never reaches a handler and gets httpd's own
//! error response.
//!
//! # Heap guard
//!
//! httpd allocates its task stack with `MALLOC_CAP_INTERNAL`, which the PSRAM
//! cannot pay for. [`start`] therefore reads
//! `heap_caps_get_free_size(MALLOC_CAP_INTERNAL)` before creating the server
//! and again after the handlers are registered, logs both numbers, and refuses
//! to run with less than [`MIN_INTERNAL_FREE`] left. The server is dropped
//! before [`Error::Budget`] is returned, so a refused boot leaves no listener
//! behind. This is the crate's second device call after [`crate::status`]; it
//! exists because the server's own allocation must not starve the media
//! pipeline.

use std::sync::Arc;

use esp_idf_svc::http::server::{Configuration, EspHttpConnection, EspHttpServer, Request};
use esp_idf_svc::http::Method as HttpMethod;
use esp_idf_svc::io::EspIOError;
use esp_idf_sys as sys;

use crate::body::{self, Body, CardSource, FileSource};
use crate::json;
use crate::plan::{self, Method, Request as PlanRequest, View};
use crate::status;

/// Internal SRAM that must remain free after httpd and its handlers exist.
///
/// Measured, not guessed: the server used to spend 24 KiB on its own threads
/// and this check read 83 327 B free. The httpd build gives its single task the
/// same 16 KiB the old worker had: with 6 KiB the status body builder overran
/// the task stack and corrupted the FreeRTOS ready list on the device. The
/// floor stays at the earlier 64 KiB because that is the measured headroom the
/// media pipeline needs above its own 57 552 B footprint - the server is
/// smaller now, so the guard is more conservative, not less.
const MIN_INTERNAL_FREE: usize = 64 * 1024;

/// Every URI is registered for both methods `plan` distinguishes.
const ROUTES: [(&str, HttpMethod); 10] = [
    ("/", HttpMethod::Get),
    ("/", HttpMethod::Head),
    ("/api/v1/status", HttpMethod::Get),
    ("/api/v1/status", HttpMethod::Head),
    ("/api/v1/files", HttpMethod::Get),
    ("/api/v1/files", HttpMethod::Head),
    ("/api/v1/openapi.json", HttpMethod::Get),
    ("/api/v1/openapi.json", HttpMethod::Head),
    ("/api/v1/files/*", HttpMethod::Get),
    ("/api/v1/files/*", HttpMethod::Head),
];

/// Everything the server needs to answer requests. Plain data: no handles and
/// no callbacks, so the caller cannot leak device state into the server.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Config {
    pub port: u16,
    pub mount_point: &'static str,
    pub rtsp_port: u16,
    pub rtsp_path: &'static str,
    pub rtsp_running: bool,
    pub recording_enabled: bool,
}

/// Why [`start`] could not leave a server running.
#[derive(Debug)]
pub enum Error {
    /// LwIP could not be initialised, so no socket would work.
    Netif(sys::EspError),
    /// httpd could not be created, or a URI handler was refused.
    Start(EspIOError),
    /// Fewer than [`MIN_INTERNAL_FREE`] bytes of internal SRAM remained after
    /// the server and its handlers were allocated.
    Budget { free: usize, required: usize },
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Netif(error) => write!(f, "network stack unavailable: {error}"),
            Error::Start(error) => write!(f, "http server unavailable: {error}"),
            Error::Budget { free, required } => write!(
                f,
                "internal heap too small: {free} bytes free, {required} required"
            ),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Start(error) => Some(error),
            Error::Netif(_) | Error::Budget { .. } => None,
        }
    }
}

/// A running server. Dropping it stops httpd and removes every handler.
pub struct Server {
    _http: EspHttpServer<'static>,
}

/// Start the server on `config.port`.
///
/// Brings LwIP up (idempotent, so the boot path calling it first is fine),
/// creates httpd, registers the ten method/path pairs, then checks that the
/// internal heap still has [`MIN_INTERNAL_FREE`] bytes. A server that started
/// and then finds the budget gone is dropped before [`Error::Budget`] is
/// returned.
pub fn start(config: Config) -> Result<Server, Error> {
    net::ensure_netif_initialised().map_err(Error::Netif)?;

    let before = internal_free();
    let mut http = EspHttpServer::new(&Configuration {
        http_port: config.port,
        stack_size: 16 * 1024,
        max_open_sockets: 2,
        max_uri_handlers: ROUTES.len(),
        lru_purge_enable: false,
        uri_match_wildcard: true,
        ..Default::default()
    })
    .map_err(Error::Start)?;

    let source = Arc::new(CardSource::new(config.mount_point));
    for (uri, method) in ROUTES {
        register(&mut http, uri, method, config, Arc::clone(&source))?;
    }

    let after = internal_free();
    logging::log!("filesrv: internal heap {before} -> {after} bytes, floor {MIN_INTERNAL_FREE}");
    if after < MIN_INTERNAL_FREE {
        drop(http);
        return Err(Error::Budget {
            free: after,
            required: MIN_INTERNAL_FREE,
        });
    }

    Ok(Server { _http: http })
}

fn register(
    http: &mut EspHttpServer<'static>,
    uri: &str,
    method: HttpMethod,
    config: Config,
    source: Arc<CardSource>,
) -> Result<(), Error> {
    http.fn_handler(uri, method, move |request| serve(request, config, &source))
        .map(|_| ())
        .map_err(|error| Error::Start(error.into()))
}

/// One request: plan, send the head, then the body.
fn serve(
    request: Request<&mut EspHttpConnection<'_>>,
    config: Config,
    source: &CardSource,
) -> Result<(), sys::EspError> {
    let method = match request.method() {
        HttpMethod::Get => Method::Get,
        HttpMethod::Head => Method::Head,
        HttpMethod::Options => Method::Options,
        _ => Method::Other,
    };
    let device = Device {
        config: &config,
        source,
    };
    let view = View {
        mount_point: config.mount_point,
        env: &device,
    };
    let (plan, path) = {
        let uri = request.uri();
        let path = uri.split('?').next().unwrap_or(uri);
        let plan = plan::plan(
            &view,
            PlanRequest {
                method,
                uri_path: path,
                range: request.header("Range"),
            },
        );
        (plan, path.to_owned())
    };

    let headers: Vec<(&str, &str)> = plan
        .headers
        .iter()
        .map(|(name, value)| (*name, value.as_str()))
        .collect();
    let mut response = request
        .into_response(plan.status.code(), Some(plan.status.reason()), &headers)
        .map_err(|error| error.0)?;
    let connection = &mut **response.connection();
    let bytes = write_body(connection, source, &plan.body)?;
    logging::log!(
        "filesrv: {} {path} -> {} {bytes}",
        method.as_str(),
        plan.status.code()
    );
    Ok(())
}

/// The device [`plan::Environment`]: the status gatherer and the card, read
/// only for the route that asks for them.
struct Device<'a> {
    config: &'a Config,
    source: &'a CardSource,
}

impl plan::Environment for Device<'_> {
    fn status(&self, out: &mut String) {
        let gathered = status::gather(self.config);
        json::status_body(&gathered.snapshot(), out);
    }

    fn listing(&self, out: &mut String) {
        json::list_body(plan::LIST_DIR, self.source, out, json::LIST_CAP);
    }

    fn len(&self, relative: &str) -> Result<u64, body::Error> {
        self.source.len(relative)
    }
}

fn write_body(
    connection: &mut EspHttpConnection<'_>,
    source: &CardSource,
    body: &Body,
) -> Result<u64, sys::EspError> {
    match body {
        Body::Bytes(bytes) => {
            connection.write_all(bytes)?;
            Ok(bytes.len() as u64)
        }
        Body::Json(text) => {
            connection.write_all(text.as_bytes())?;
            Ok(text.len() as u64)
        }
        Body::File { path, start, len } => stream_file(connection, source, path, *start, *len),
    }
}

fn stream_file(
    connection: &mut EspHttpConnection<'_>,
    source: &CardSource,
    path: &str,
    start: u64,
    len: u64,
) -> Result<u64, sys::EspError> {
    let mut buffer = [0u8; body::CHUNK];
    let mut written = 0u64;
    for (offset, want) in body::chunk_plan(len, body::CHUNK) {
        let at = start + offset;
        let got = match source.read_at(path, at, &mut buffer[..want]) {
            Ok(got) if got == want => got,
            Ok(got) => {
                return fail(&format!(
                    "{path}: short read at {at}: wanted {want}, got {got}"
                ))
            }
            Err(error) => return fail(&format!("{path}: read failed at {at}: {error}")),
        };
        connection.write_all(&buffer[..got])?;
        written += got as u64;
    }
    Ok(written)
}

fn fail(message: &str) -> Result<u64, sys::EspError> {
    logging::log!("filesrv: body aborted: {message}");
    Err(sys::EspError::from_infallible::<{ sys::ESP_FAIL }>())
}

fn internal_free() -> usize {
    // SAFETY: `heap_caps_get_free_size` is a plain getter with no preconditions.
    unsafe { sys::heap_caps_get_free_size(sys::MALLOC_CAP_INTERNAL) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_is_data_only() {
        fn plain<T: Copy + Send + Sync + 'static>() {}
        plain::<Config>();
        assert!(
            !std::mem::needs_drop::<Config>(),
            "Config owns nothing that needs dropping"
        );

        let config = Config {
            port: 8080,
            mount_point: storage::BASE_PATH,
            rtsp_port: 554,
            rtsp_path: "/live",
            rtsp_running: true,
            recording_enabled: true,
        };
        assert_eq!(config.port, 8080);
        assert_eq!(config.mount_point, storage::BASE_PATH);
        assert_eq!(config.rtsp_port, 554);
        assert_eq!(config.rtsp_path, "/live");
        assert!(config.rtsp_running);
        assert!(config.recording_enabled);
        let copy = config;
        assert_eq!(copy, config, "Config is Copy, not a handle");
    }

    #[test]
    fn budget_error_names_both_numbers() {
        let text = Error::Budget {
            free: 1000,
            required: MIN_INTERNAL_FREE,
        }
        .to_string();
        assert!(text.contains("1000"), "{text}");
        assert!(text.contains(&MIN_INTERNAL_FREE.to_string()), "{text}");
    }
}

/// Hardware-in-the-loop tests: they run in the device test binary, not on a
/// host substitute. The loopback tests need LwIP (and the ranged one a mounted
/// card); the route check parses the document embedded in the same image.
#[cfg(test)]
mod hil {
    use std::io::{Read, Write};
    use std::net::{Ipv4Addr, SocketAddr, TcpStream};
    use std::sync::{Mutex, MutexGuard};
    use std::time::Duration;

    use super::*;

    /// One server at a time: httpd keeps a control socket per instance.
    static SOCKETS: Mutex<()> = Mutex::new(());
    const TEST_PORT: u16 = 18080;
    const MIB: u64 = 1024 * 1024;
    const FIXTURE_NAME: &str = "_hil_range.bin";
    const FIXTURE_BYTES: u64 = 2 * MIB;
    const RANGE_START: u64 = 524_288;
    const RANGE_END: u64 = 1_048_575;
    const RANGE_LEN: u64 = RANGE_END - RANGE_START + 1;

    fn sockets() -> MutexGuard<'static, ()> {
        SOCKETS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn config(port: u16) -> Config {
        Config {
            port,
            mount_point: storage::BASE_PATH,
            rtsp_port: 554,
            rtsp_path: "/live",
            rtsp_running: true,
            recording_enabled: true,
        }
    }

    /// Send one request and read until httpd closes; returns the raw response.
    fn exchange(method: &str, path: &str, range: Option<&str>) -> Vec<u8> {
        let address = SocketAddr::new(Ipv4Addr::LOCALHOST.into(), TEST_PORT);
        let mut stream =
            TcpStream::connect(address).expect("the loopback client connects to httpd");
        stream
            .set_read_timeout(Some(Duration::from_secs(20)))
            .expect("the client read timeout");
        let mut head = format!("{method} {path} HTTP/1.1\r\nHost: board\r\nConnection: close\r\n");
        if let Some(range) = range {
            head.push_str(&format!("Range: {range}\r\n"));
        }
        head.push_str("\r\n");
        stream
            .write_all(head.as_bytes())
            .expect("the request is written");
        let mut raw = Vec::new();
        let mut buffer = [0u8; 1024];
        loop {
            match stream.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => {
                    raw.extend_from_slice(&buffer[..read]);
                    if response_complete(&raw) {
                        break;
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::ConnectionReset
                            | std::io::ErrorKind::WouldBlock
                            | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    break
                }
                Err(error) => panic!("reading the response failed: {error}"),
            }
        }
        raw
    }

    /// Whether the buffered response is complete, so the test does not depend
    /// on httpd closing the socket to find the end of the body.
    fn response_complete(raw: &[u8]) -> bool {
        let Some(boundary) = raw.windows(4).position(|window| window == b"\r\n\r\n") else {
            return false;
        };
        let head = String::from_utf8_lossy(&raw[..boundary]).to_ascii_lowercase();
        let body = &raw[boundary + 4..];
        if head.contains("transfer-encoding: chunked") {
            return body.windows(5).any(|window| window == b"0\r\n\r\n");
        }
        let length = head.lines().find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())?
        });
        length.is_some_and(|length| body.len() >= length)
    }

    fn split_head(raw: &[u8]) -> (String, &[u8]) {
        let boundary = raw
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .unwrap_or_else(|| panic!("the response has no head terminator"));
        let head = String::from_utf8(raw[..boundary].to_vec()).expect("the head is UTF-8");
        (head, &raw[boundary + 4..])
    }

    fn header_value<'a>(head: &'a str, name: &str) -> Option<&'a str> {
        head.lines().find_map(|line| {
            let (field, value) = line.split_once(':')?;
            field
                .eq_ignore_ascii_case(name)
                .then(|| value.trim_start_matches(' '))
        })
    }

    /// Decode a chunked body; httpd emits one chunk per `write_all`.
    fn decode_chunked(body: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut rest = body;
        loop {
            let line_end = rest
                .windows(2)
                .position(|window| window == b"\r\n")
                .unwrap_or_else(|| {
                    panic!("no chunk-size line in {:?}", String::from_utf8_lossy(rest))
                });
            let text = core::str::from_utf8(&rest[..line_end]).expect("the chunk size is ASCII");
            let size = usize::from_str_radix(text.trim(), 16)
                .unwrap_or_else(|error| panic!("chunk size {text:?}: {error}"));
            rest = &rest[line_end + 2..];
            if size == 0 {
                return out;
            }
            out.extend_from_slice(&rest[..size]);
            rest = &rest[size + 2..];
        }
    }

    fn body_bytes(head: &str, body: &[u8]) -> Vec<u8> {
        match header_value(head, "Transfer-Encoding") {
            Some(value) if value.eq_ignore_ascii_case("chunked") => decode_chunked(body),
            _ => body.to_vec(),
        }
    }

    fn pattern_byte(offset: u64) -> u8 {
        (offset % 251) as u8
    }

    #[test]
    fn hil_loopback_status_serves_json() {
        let _guard = sockets();
        let before = internal_free();
        let server = start(config(TEST_PORT)).expect("HIL: httpd starts on the test port");
        let after = internal_free();
        logging::log!("test: fileserver heap before={before} after={after}");

        let raw = exchange("GET", "/api/v1/status", None);
        drop(server);

        let (head, body) = split_head(&raw);
        assert!(
            head.starts_with("HTTP/1.1 200"),
            "GET /api/v1/status answered {head:?}"
        );
        assert_eq!(
            header_value(&head, "Content-Type"),
            Some("application/json"),
            "GET /api/v1/status: Content-Type"
        );
        let body = body_bytes(&head, body);
        let document: serde_json::Value =
            serde_json::from_slice(&body).expect("the status body is JSON");
        assert!(document.get("version").is_some(), "{document}");
        assert!(document.get("mounted").is_some(), "{document}");
    }

    #[test]
    #[ignore = "HIL: needs a mounted SD card"]
    fn hil_loopback_ranged_download_is_byte_exact() {
        let card = storage::hil_fixture::card();
        assert_eq!(
            card.base_path(),
            storage::BASE_PATH,
            "HIL: the fixture mounts the card where the server expects it"
        );
        let path = format!("{}/{FIXTURE_NAME}", storage::BASE_PATH);
        write_fixture(&path);

        let _guard = sockets();
        let server = start(config(TEST_PORT)).expect("HIL: httpd starts on the test port");
        let raw = exchange(
            "GET",
            "/api/v1/files/_hil_range.bin",
            Some(&format!("bytes={RANGE_START}-{RANGE_END}")),
        );
        drop(server);
        let _ = std::fs::remove_file(&path);

        let (head, body) = split_head(&raw);
        assert!(
            head.starts_with("HTTP/1.1 206"),
            "the ranged download answered {head:?}"
        );
        assert_eq!(
            header_value(&head, "Content-Range"),
            Some(format!("bytes {RANGE_START}-{RANGE_END}/{FIXTURE_BYTES}").as_str()),
            "Content-Range"
        );
        assert_eq!(header_value(&head, "Accept-Ranges"), Some("bytes"));
        let body = body_bytes(&head, body);
        assert_eq!(
            body.len() as u64,
            RANGE_LEN,
            "expected {RANGE_LEN} body bytes, got {}",
            body.len()
        );
        for (index, received) in body.iter().enumerate() {
            let offset = RANGE_START + index as u64;
            assert_eq!(
                *received,
                pattern_byte(offset),
                "body byte at file offset {offset} differs"
            );
        }
    }

    fn write_fixture(path: &str) {
        let mut file = std::fs::File::create(path)
            .unwrap_or_else(|error| panic!("cannot create {path}: {error}"));
        let mut buffer = [0u8; 32 * 1024];
        let mut offset = 0u64;
        while offset < FIXTURE_BYTES {
            let len = core::cmp::min(buffer.len() as u64, FIXTURE_BYTES - offset) as usize;
            for (index, byte) in buffer[..len].iter_mut().enumerate() {
                *byte = pattern_byte(offset + index as u64);
            }
            file.write_all(&buffer[..len])
                .unwrap_or_else(|error| panic!("write to {path} at {offset}: {error}"));
            offset += len as u64;
        }
        file.sync_all()
            .unwrap_or_else(|error| panic!("sync of {path}: {error}"));
    }

    #[test]
    fn hil_router_matches_openapi_paths() {
        let document: serde_json::Value = serde_json::from_str(crate::json::openapi_body())
            .unwrap_or_else(|error| panic!("the embedded openapi.json does not parse: {error}"));
        assert_eq!(document["openapi"].as_str(), Some("3.0.3"));

        let mut documented = Vec::new();
        for (path, item) in document["paths"]
            .as_object()
            .expect("the embedded document has a paths object")
        {
            for (method, operation) in item.as_object().expect("a path item is an object") {
                if operation.is_object() && matches!(method.as_str(), "get" | "head") {
                    documented.push((method.to_ascii_uppercase(), path.clone()));
                }
            }
        }
        for (method, path) in &documented {
            assert!(
                crate::router::ROUTES.contains(&(method.as_str(), path.as_str())),
                "openapi.json documents {method} {path} but ROUTES does not answer it"
            );
        }
        for (method, path) in crate::router::ROUTES {
            assert!(
                documented.iter().any(|(m, p)| m == method && p == path),
                "ROUTES answers {method} {path} but openapi.json does not document it"
            );
        }
        assert_eq!(documented.len(), crate::router::ROUTES.len());
    }
}
