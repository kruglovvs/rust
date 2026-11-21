//! The pure request decision: method, path and `Range` in, status, headers and
//! body out.
//!
//! # Contract
//!
//! ```text
//! pub enum Method { Get, Head, Options, Other }
//! pub struct Status(pub u16);
//! pub trait Environment {
//!     fn status(&self, out: &mut String);
//!     fn listing(&self, out: &mut String);
//!     fn len(&self, relative: &str) -> Result<u64, body::Error>;
//! }
//! pub struct View<'a> { pub mount_point: &'a str, pub env: &'a dyn Environment }
//! pub struct Request<'a> { pub method: Method, pub uri_path: &'a str,
//!                          pub range: Option<&'a str> }
//! pub struct Plan { pub status: Status, pub headers: Vec<(&'static str, String)>,
//!                   pub body: Body }
//! pub fn plan(view: &View<'_>, request: Request<'_>) -> Plan;
//! ```
//!
//! [`plan`] is the only decision the socket handler executes. It never touches
//! a socket, an ESP-IDF API or the card directly: the two bodies that depend on
//! device state and the download length come from [`Environment`], which the
//! device implements over [`crate::status`] and [`crate::body::CardSource`] and
//! the tests implement over a [`crate::body::MemorySource`]. The request path
//! is the decoded-free URI path with any query string already removed.
//!
//! ## What it selects
//!
//! - `GET` and `HEAD` select the same operation: [`crate::router::route`] is
//!   consulted with `GET` for both, so a `HEAD` of a JSON endpoint reports the
//!   same status and headers as the `GET` and an empty body. Unknown paths are
//!   `404`, whatever the method.
//! - `OPTIONS` and every other method are `405` before the path is looked at,
//!   so a refused verb cannot probe which paths exist.
//! - A download path is validated by [`crate::path::map`] before the length is
//!   read, so traversal, a NUL, a bad escape or a directory is a `404` and
//!   [`Environment::len`] is never asked about it.
//! - A full download is `200` with `Content-Type: application/octet-stream` and
//!   `Accept-Ranges: bytes`; a selected range is `206` with the same headers
//!   plus `Content-Range: bytes <start>-<end>/<total>`; an unsatisfiable range
//!   is `416` with `Content-Range: bytes */<total>` and an empty body.
//! - A failed length lookup other than "not found" is `500`; the JSON error
//!   body is [`crate::json::error_body`]'s `ApiError` shape.
//! - `HEAD` responses carry no body ([`crate::body::body_for`]), but keep the
//!   status and every header the matching `GET` would send.
//!
//! No header in [`Plan`] is `Content-Length`: the ESP-IDF server owns framing.
//! A body written through its chunk API is `Transfer-Encoding: chunked`, so a
//! download has no announced length; `416` and `HEAD` send an empty body and
//! therefore carry `Content-Length: 0` from httpd.

use crate::body::{self, Body};
use crate::json;
use crate::path;
use crate::range;
use crate::router::{self, Route};

/// The content type of every JSON response.
pub const JSON: &str = "application/json";

/// The content type of a file download.
pub const OCTET_STREAM: &str = "application/octet-stream";

/// The card directory the `/api/v1/files` listing reports.
pub const LIST_DIR: &str = "rec";

/// A request method. Only the three verbs the server distinguishes keep their
/// identity; everything else, including a lowercase `get`, is [`Method::Other`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Method {
    Get,
    Head,
    Options,
    Other,
}

impl Method {
    /// The method as it appeared on the wire for the three known verbs, and
    /// `"OTHER"` for the rest (used by the per-request log line).
    pub const fn as_str(self) -> &'static str {
        match self {
            Method::Get => "GET",
            Method::Head => "HEAD",
            Method::Options => "OPTIONS",
            Method::Other => "OTHER",
        }
    }
}

impl core::fmt::Display for Method {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A response status code with its canonical reason phrase.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Status(pub u16);

impl Status {
    pub const OK: Status = Status(200);
    pub const PARTIAL_CONTENT: Status = Status(206);
    pub const NOT_FOUND: Status = Status(404);
    pub const METHOD_NOT_ALLOWED: Status = Status(405);
    pub const RANGE_NOT_SATISFIABLE: Status = Status(416);
    pub const INTERNAL_SERVER_ERROR: Status = Status(500);

    pub const fn code(self) -> u16 {
        self.0
    }

    /// The canonical reason phrase for this code.
    pub const fn reason(self) -> &'static str {
        reason_for(self.0)
    }
}

/// The reason phrase for `code`, or `"Unknown"` for a code this server does not
/// use.
pub const fn reason_for(code: u16) -> &'static str {
    match code {
        200 => "OK",
        206 => "Partial Content",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        416 => "Range Not Satisfiable",
        500 => "Internal Server Error",
        _ => "Unknown",
    }
}

/// The device data a plan reads, provided lazily for the one route that needs
/// it. The method is on the environment rather than the view so a status
/// request does not read the listing and a download does not gather status.
pub trait Environment {
    /// Append the status document for the current device state.
    fn status(&self, out: &mut String);

    /// Append the listing of the recording directory.
    fn listing(&self, out: &mut String);

    /// The length of a mount-relative path already accepted by
    /// [`crate::path::map`], or an error when it is not a readable file.
    fn len(&self, relative: &str) -> Result<u64, body::Error>;
}

/// The non-request inputs: where the card is mounted and the device data
/// behind the bodies.
pub struct View<'a> {
    pub mount_point: &'a str,
    pub env: &'a dyn Environment,
}

/// One request as the socket layer observed it: the query string is already
/// removed from `uri_path` and the header value is the first `Range` line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Request<'a> {
    pub method: Method,
    pub uri_path: &'a str,
    pub range: Option<&'a str>,
}

/// The decision a handler executes: status, response headers and body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub status: Status,
    pub headers: Vec<(&'static str, String)>,
    pub body: Body,
}

impl Plan {
    fn json(status: Status, body: String) -> Plan {
        Plan {
            status,
            headers: vec![("Content-Type", JSON.to_string())],
            body: Body::Json(body),
        }
    }

    fn document(text: &'static str) -> Plan {
        Plan {
            status: Status::OK,
            headers: vec![("Content-Type", JSON.to_string())],
            body: Body::Bytes(text.as_bytes()),
        }
    }

    fn error(status: Status) -> Plan {
        let mut body = String::new();
        json::error_body(status, error_message(status), &mut body);
        Plan::json(status, body)
    }
}

/// Decide the response for one request. See the [module contract](self).
pub fn plan(view: &View<'_>, request: Request<'_>) -> Plan {
    let decided = decide(view, request);
    Plan {
        body: body::body_for(request.method, decided.body),
        ..decided
    }
}

fn decide(view: &View<'_>, request: Request<'_>) -> Plan {
    let selected = match request.method {
        Method::Get | Method::Head => router::route(Method::Get, request.uri_path),
        Method::Options | Method::Other => router::route(request.method, request.uri_path),
    };
    let route = match selected {
        Ok(route) => route,
        Err(status) => return Plan::error(status),
    };

    let mut text = String::new();
    match route {
        Route::Index => json::index_body(&mut text),
        Route::Status => view.env.status(&mut text),
        Route::Files => view.env.listing(&mut text),
        Route::OpenApi => return Plan::document(json::openapi_body()),
        Route::Download => return download(view, request),
    }
    Plan::json(Status::OK, text)
}

/// The download decision, including the `Range` arithmetic over the length the
/// environment reports for a path the card accepted.
fn download(view: &View<'_>, request: Request<'_>) -> Plan {
    let Some(encoded) = router::download_relative(Route::Download, request.uri_path) else {
        return Plan::error(Status::NOT_FOUND);
    };
    let Ok(mapped) = path::map(encoded, view.mount_point) else {
        return Plan::error(Status::NOT_FOUND);
    };
    let Some(relative) = mapped
        .strip_prefix(view.mount_point)
        .and_then(|rest| rest.strip_prefix('/'))
    else {
        return Plan::error(Status::NOT_FOUND);
    };
    let total = match view.env.len(relative) {
        Ok(total) => total,
        Err(body::Error::NotFound) => return Plan::error(Status::NOT_FOUND),
        Err(_) => return Plan::error(Status::INTERNAL_SERVER_ERROR),
    };

    let selected = range::parse(request.range, total);
    let download = body::download(relative, total, selected);
    let mut headers = vec![
        ("Content-Type", OCTET_STREAM.to_string()),
        ("Accept-Ranges", "bytes".to_string()),
    ];
    if let Some(content_range) = &download.content_range {
        headers.push(("Content-Range", content_range.clone()));
    }
    Plan {
        status: download.status,
        headers,
        body: download.body,
    }
}

fn error_message(status: Status) -> &'static str {
    match status {
        Status::METHOD_NOT_ALLOWED => "GET or HEAD only",
        Status::NOT_FOUND => "no such endpoint",
        _ => "request refused",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::body::{Error as BodyError, FileSource, MemorySource};
    use crate::json::Snapshot;
    use std::cell::Cell;

    const MOUNT: &str = "/sdcard";
    const FILE: &str = "rec/REC_0001.avi";
    const TARGET: &str = "/api/v1/files/rec/REC_0001.avi";

    static RECORDING: [u8; 4096] = {
        let mut data = [0u8; 4096];
        let mut index = 0;
        while index < 4096 {
            data[index] = (index % 251) as u8;
            index += 1;
        }
        data
    };

    static FILES: [(&str, &[u8]); 1] = [(FILE, &RECORDING)];

    /// Counts `len` calls so a rejected path can prove it never reached the
    /// card.
    struct TestEnv {
        source: MemorySource<'static>,
        lengths: Cell<usize>,
    }

    impl TestEnv {
        fn new() -> TestEnv {
            TestEnv {
                source: MemorySource::new(&FILES),
                lengths: Cell::new(0),
            }
        }
    }

    fn snapshot() -> Snapshot<'static> {
        Snapshot {
            version: "0.1.0",
            build: "test",
            uptime_s: 42,
            ip: "192.168.1.33",
            hostname: "esp32s3-av",
            mounted: true,
            total_bytes: 1024,
            free_bytes: 512,
            clock_khz: 20_000,
            rtsp_port: 554,
            rtsp_path: "/live",
            rtsp_running: true,
            recording_enabled: true,
            recording_file: Some(FILE),
            temperature_c: None,
            heap_internal_free: 120_000,
            heap_spiram_free: 7_000_000,
        }
    }

    impl Environment for TestEnv {
        fn status(&self, out: &mut String) {
            json::status_body(&snapshot(), out);
        }

        fn listing(&self, out: &mut String) {
            json::list_body(LIST_DIR, &self.source, out, json::LIST_CAP);
        }

        fn len(&self, relative: &str) -> Result<u64, BodyError> {
            self.lengths.set(self.lengths.get() + 1);
            self.source.len(relative)
        }
    }

    fn decide(env: &TestEnv, method: Method, path: &str, range: Option<&str>) -> Plan {
        plan(
            &View {
                mount_point: MOUNT,
                env,
            },
            Request {
                method,
                uri_path: path,
                range,
            },
        )
    }

    fn header<'a>(plan: &'a Plan, name: &str) -> Option<&'a str> {
        plan.headers
            .iter()
            .find(|(field, _)| field.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    fn body_text(plan: &Plan) -> &str {
        match &plan.body {
            Body::Json(text) => text,
            Body::Bytes(bytes) => core::str::from_utf8(bytes).expect("static bytes are UTF-8"),
            Body::File { .. } => panic!("expected a buffered body, got a file"),
        }
    }

    fn stream(plan: &Plan, source: &MemorySource<'_>) -> Vec<u8> {
        let mut out = Vec::new();
        body::write_body(&plan.body, source, &mut out).expect("the body streams");
        out
    }

    #[test]
    fn index_is_200_json() {
        let env = TestEnv::new();
        let plan = decide(&env, Method::Get, "/", None);
        assert_eq!(plan.status, Status::OK);
        assert_eq!(header(&plan, "Content-Type"), Some(JSON));
        let document: serde_json::Value =
            serde_json::from_str(body_text(&plan)).expect("the index is JSON");
        assert!(document["endpoints"]
            .as_array()
            .is_some_and(|list| list.len() == 6));
        assert_eq!(env.lengths.get(), 0, "the index reads no file length");
    }

    #[test]
    fn status_is_200_json() {
        let env = TestEnv::new();
        let plan = decide(&env, Method::Get, "/api/v1/status", None);
        assert_eq!(plan.status, Status::OK);
        assert_eq!(header(&plan, "Content-Type"), Some(JSON));
        let document: serde_json::Value =
            serde_json::from_str(body_text(&plan)).expect("the status is JSON");
        assert_eq!(document["version"], "0.1.0");
        assert_eq!(document["rtsp_running"], true);
        assert_eq!(env.lengths.get(), 0, "status reads no file length");
    }

    #[test]
    fn listing_is_200_json() {
        let env = TestEnv::new();
        let plan = decide(&env, Method::Get, "/api/v1/files", None);
        assert_eq!(plan.status, Status::OK);
        let document: serde_json::Value =
            serde_json::from_str(body_text(&plan)).expect("the listing is JSON");
        assert_eq!(document["files"][0]["path"], "/rec/REC_0001.avi");
        assert_eq!(document["files"][0]["size"], 4096);
    }

    #[test]
    fn openapi_is_the_embedded_document() {
        let env = TestEnv::new();
        let plan = decide(&env, Method::Get, "/api/v1/openapi.json", None);
        assert_eq!(plan.status, Status::OK);
        assert_eq!(body_text(&plan), json::openapi_body());
    }

    #[test]
    fn full_download_is_200_octet_stream_with_accept_ranges() {
        let env = TestEnv::new();
        let plan = decide(&env, Method::Get, TARGET, None);
        assert_eq!(plan.status, Status::OK);
        assert_eq!(header(&plan, "Content-Type"), Some(OCTET_STREAM));
        assert_eq!(header(&plan, "Accept-Ranges"), Some("bytes"));
        assert_eq!(header(&plan, "Content-Range"), None);
        assert_eq!(
            plan.body,
            Body::File {
                path: FILE.to_string(),
                start: 0,
                len: 4096,
            }
        );
        assert_eq!(stream(&plan, &env.source), RECORDING);
    }

    #[test]
    fn partial_download_is_206_with_content_range() {
        let env = TestEnv::new();
        let plan = decide(&env, Method::Get, TARGET, Some("bytes=1024-2047"));
        assert_eq!(plan.status, Status::PARTIAL_CONTENT);
        assert_eq!(header(&plan, "Content-Type"), Some(OCTET_STREAM));
        assert_eq!(header(&plan, "Accept-Ranges"), Some("bytes"));
        assert_eq!(header(&plan, "Content-Range"), Some("bytes 1024-2047/4096"));
        assert_eq!(stream(&plan, &env.source), RECORDING[1024..2048]);
    }

    #[test]
    fn unsatisfiable_range_is_416_with_an_empty_body() {
        let env = TestEnv::new();
        let plan = decide(&env, Method::Get, TARGET, Some("bytes=9999-"));
        assert_eq!(plan.status, Status::RANGE_NOT_SATISFIABLE);
        assert_eq!(header(&plan, "Content-Range"), Some("bytes */4096"));
        assert_eq!(plan.body, Body::empty());
        assert_eq!(stream(&plan, &env.source), b"");
    }

    #[test]
    fn head_keeps_status_and_headers_but_drops_the_body() {
        let env = TestEnv::new();
        let get = decide(&env, Method::Get, TARGET, Some("bytes=0-9"));
        let head = decide(&env, Method::Head, TARGET, Some("bytes=0-9"));
        assert_eq!(head.status, get.status);
        assert_eq!(head.headers, get.headers);
        assert_eq!(head.body, Body::empty());
        assert_eq!(stream(&head, &env.source), b"");

        let index = decide(&env, Method::Head, "/", None);
        assert_eq!(index.status, Status::OK);
        assert_eq!(index.body, Body::empty());
    }

    #[test]
    fn unknown_path_is_404() {
        let env = TestEnv::new();
        for path in ["/nope", "/api/v1", "/api/v1/status/", "/api/v2/status"] {
            let plan = decide(&env, Method::Get, path, None);
            assert_eq!(plan.status, Status::NOT_FOUND, "{path}");
            assert_eq!(header(&plan, "Content-Type"), Some(JSON));
        }
        assert_eq!(env.lengths.get(), 0, "no unknown path reads a file length");
    }

    #[test]
    fn wrong_method_is_405_before_the_path_is_read() {
        let env = TestEnv::new();
        for path in ["/", "/api/v1/status", "/does/not/exist", TARGET] {
            let plan = decide(&env, Method::Other, path, None);
            assert_eq!(plan.status, Status::METHOD_NOT_ALLOWED, "{path}");
        }
        assert_eq!(
            decide(&env, Method::Options, "/api/v1/status", None).status,
            Status::METHOD_NOT_ALLOWED
        );
        assert_eq!(env.lengths.get(), 0);
    }

    #[test]
    fn traversal_is_404_without_asking_the_card() {
        let env = TestEnv::new();
        for encoded in [
            "rec/../../etc/passwd",
            "%2e%2e%2fetc%2fpasswd",
            "rec/%00x",
            "rec\\a",
            "a//b",
            "rec/",
        ] {
            let path = format!("/api/v1/files/{encoded}");
            let plan = decide(&env, Method::Get, &path, None);
            assert_eq!(plan.status, Status::NOT_FOUND, "{path}");
        }
        assert_eq!(
            env.lengths.get(),
            0,
            "a rejected path must not reach the card"
        );
    }

    #[test]
    fn absent_file_is_404() {
        let env = TestEnv::new();
        let plan = decide(&env, Method::Get, "/api/v1/files/rec/absent.avi", None);
        assert_eq!(plan.status, Status::NOT_FOUND);
        assert_eq!(env.lengths.get(), 1, "the card was asked once");
    }

    #[test]
    fn a_card_error_is_500() {
        struct Failing;
        impl Environment for Failing {
            fn status(&self, _: &mut String) {}
            fn listing(&self, _: &mut String) {}
            fn len(&self, _: &str) -> Result<u64, BodyError> {
                Err(BodyError::Io(std::io::Error::other("io")))
            }
        }
        let plan = plan(
            &View {
                mount_point: MOUNT,
                env: &Failing,
            },
            Request {
                method: Method::Get,
                uri_path: TARGET,
                range: None,
            },
        );
        assert_eq!(plan.status, Status::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn reasons_match_the_codes() {
        assert_eq!(Status::OK.reason(), "OK");
        assert_eq!(Status::PARTIAL_CONTENT.reason(), "Partial Content");
        assert_eq!(Status::NOT_FOUND.reason(), "Not Found");
        assert_eq!(Status::METHOD_NOT_ALLOWED.reason(), "Method Not Allowed");
        assert_eq!(
            Status::RANGE_NOT_SATISFIABLE.reason(),
            "Range Not Satisfiable"
        );
        assert_eq!(Status(299).reason(), "Unknown");
    }
}
