//! Live LAN and in-process contract test for the firmware's JSON API
//! (plan Todo 16).
//!
//! One assertion set runs over two drivers:
//!
//! * [`InProcess`] builds the exact response bytes from the crate's pure
//!   router, JSON, path, body and range modules (`#[path]` includes, no
//!   socket), so it runs on a machine with no board.
//! * [`LiveBoard`] sends the same requests over `std::net::TcpStream` to
//!   `ESP32_BOARD_URL`, which is the only thing that proves the device answers.
//!
//! Both drivers end at [`parse_reply`], so a head-format difference between the
//! in-process mirror and the device is a test failure rather than a blind spot.
//!
//! The set checks, per documented operation, that the response status is
//! documented, that `Content-Length` equals the body length, that a
//! `Content-Range` body is exactly the announced span, and that every JSON body
//! validates against the schema in `firmware/api/openapi.json`. The document is
//! parsed with `openapiv3` and the bodies are checked with `jsonschema`.
//!
//! # Running
//!
//! ```text
//! # No board: in-process driver only (the live test prints SKIP and returns)
//! cd /workspaces/rust/harness && cargo test --test api_contract -- --nocapture
//!
//! # Live board: both drivers, and an unset URL is a hard failure
//! ESP32_BOARD_URL=http://<board-ip> ./harness/api_contract.sh
//!   (or API_CONTRACT_REQUIRE_LIVE=1, which the script exports)
//! ```
//!
//! The live driver is deliberately env-gated rather than `#[ignore]`: a bare
//! `cargo test` with no board must still pass the in-process driver, while the
//! script must fail loudly when the URL is missing. [`live_gate`] is the one
//! decision both paths share, and [`live_gate_requires_the_board_url`] pins the
//! message.

#[path = "../../firmware/components/fileserver/src/http.rs"]
mod http;

#[path = "../../firmware/components/fileserver/src/router.rs"]
mod router;

#[path = "../../firmware/components/fileserver/src/json.rs"]
mod json;

#[path = "../../firmware/components/fileserver/src/body.rs"]
mod body;

#[path = "../../firmware/components/fileserver/src/path.rs"]
mod path;

#[path = "../../firmware/components/fileserver/src/range.rs"]
mod range;

use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use openapiv3::{OpenAPI, Operation, ReferenceOr, Response, StatusCode};
use serde_json::Value;

use body::FileSource;
use http::Status;
use router::Route;

/// The document the firmware embeds with `include_str!`; the in-process test
/// asserts the served copy is byte-identical to this one.
const OPENAPI_JSON: &str = include_str!("../../firmware/api/openapi.json");

/// The one variable the live driver reads.
const BOARD_URL_ENV: &str = "ESP32_BOARD_URL";

/// Set by `harness/api_contract.sh`: a missing board URL is then a failure
/// instead of a skip.
const REQUIRE_LIVE_ENV: &str = "API_CONTRACT_REQUIRE_LIVE";

const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const IO_TIMEOUT: Duration = Duration::from_secs(15);

const JSON: &str = "application/json";
const OCTET_STREAM: &str = "application/octet-stream";

/// The card directory the listing reports, as in `server.rs`.
const LIST_DIR: &str = "rec";
/// The mount point the download route maps onto, as in `server.rs`.
const MOUNT_POINT: &str = "/sdcard";
const RECORDING_NAME: &str = "rec/REC_0001.avi";
const RECORDING_TARGET: &str = "/api/v1/files/rec/REC_0001.avi";
const RECORDING_LEN: usize = 4096;

const fn recording_pattern() -> [u8; RECORDING_LEN] {
    let mut data = [0u8; RECORDING_LEN];
    let mut index = 0;
    while index < RECORDING_LEN {
        data[index] = (index % 251) as u8;
        index += 1;
    }
    data
}

/// A position-dependent fixture, so a wrong range window cannot pass by
/// comparing equal bytes.
static RECORDING: [u8; RECORDING_LEN] = recording_pattern();

// ---- one reply, parsed the same way for both drivers ----------------------

#[derive(Debug)]
struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Reply {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(field, _)| field.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    /// The media type without parameters, lower case.
    fn content_type(&self) -> Option<String> {
        self.header("content-type")
            .map(|value| value.split(';').next().unwrap_or(value).trim().to_ascii_lowercase())
    }
}

/// Split a raw HTTP/1.1 response into status, headers and body.
fn parse_reply(raw: &[u8]) -> Result<Reply, String> {
    let blank = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or_else(|| format!("no head terminator in {} bytes", raw.len()))?;
    let head = std::str::from_utf8(&raw[..blank]).map_err(|_| "head is not UTF-8")?;
    let mut lines = head.split("\r\n");
    let status_line = lines.next().ok_or("empty response")?;
    let mut parts = status_line.split(' ');
    let version = parts.next().unwrap_or_default();
    if !version.starts_with("HTTP/1.") {
        return Err(format!("not an HTTP/1.x status line: {status_line:?}"));
    }
    let status = parts
        .next()
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| format!("no status code in {status_line:?}"))?;
    let mut headers = Vec::new();
    for line in lines {
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| format!("header line without a colon: {line:?}"))?;
        headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
    }
    Ok(Reply {
        status,
        headers,
        body: raw[blank + 4..].to_vec(),
    })
}

// ---- one case, one driver, one assertion set ------------------------------

struct Case {
    method: &'static str,
    /// The path pattern exactly as the document spells it.
    pattern: &'static str,
    /// The concrete request path (the template filled in).
    target: String,
    range: Option<String>,
    expected: u16,
}

impl Case {
    fn get(pattern: &'static str, target: &str, expected: u16) -> Case {
        Case {
            method: "GET",
            pattern,
            target: target.to_string(),
            range: None,
            expected,
        }
    }

    fn head(pattern: &'static str, target: &str, expected: u16) -> Case {
        Case {
            method: "HEAD",
            pattern,
            target: target.to_string(),
            range: None,
            expected,
        }
    }

    fn ranged(mut self, range: &str) -> Case {
        self.range = Some(range.to_string());
        self
    }

    fn range_note(&self) -> String {
        match &self.range {
            Some(range) => format!(" [Range: {range}]"),
            None => String::new(),
        }
    }
}

trait Driver {
    fn name(&self) -> &'static str;
    fn cases(&mut self) -> Vec<Case>;
    fn request(&mut self, case: &Case) -> Result<Reply, String>;
}

/// Validate `driver` against `doc`; returns (responses checked, JSON bodies
/// validated) and prints one line per endpoint.
fn assert_contract(driver: &mut dyn Driver, doc: &OpenAPI) -> (usize, usize) {
    let name = driver.name();
    let raw: Value = serde_json::from_str(OPENAPI_JSON).expect("the shipped document is JSON");
    let documented = document_operations(doc);
    let cases = driver.cases();

    for case in &cases {
        let key = (case.method.to_string(), case.pattern.to_string());
        assert!(
            documented.contains(&key),
            "contract[{name}]: {} {} is not a documented operation",
            case.method,
            case.pattern
        );
    }
    for (method, pattern) in &documented {
        assert!(
            cases
                .iter()
                .any(|case| case.method.eq_ignore_ascii_case(method) && case.pattern == pattern),
            "contract[{name}]: documented operation {method} {pattern} has no case"
        );
    }

    let mut checked = 0;
    let mut json_validated = 0;
    for case in &cases {
        let label = format!(
            "contract[{name}]: {} {}{}",
            case.method,
            case.target,
            case.range_note()
        );
        let reply = driver
            .request(case)
            .unwrap_or_else(|error| panic!("{label}: {error}"));
        assert_eq!(
            reply.status, case.expected,
            "{label}: expected {}, got {} (body: {})",
            case.expected,
            reply.status,
            body_preview(&reply.body)
        );

        let operation = operation_for(doc, case.pattern, case.method)
            .unwrap_or_else(|| panic!("{label}: no operation in the document"));
        let documented_status = operation
            .responses
            .responses
            .keys()
            .any(|code| *code == StatusCode::Code(reply.status))
            || operation.responses.default.is_some();
        assert!(
            documented_status,
            "{label}: status {} is not documented for {} {}",
            reply.status, case.method, case.pattern
        );

        let declared = reply
            .header("content-length")
            .unwrap_or_else(|| panic!("{label}: no Content-Length"));
        let declared = declared
            .parse::<u64>()
            .unwrap_or_else(|_| panic!("{label}: Content-Length {declared:?} is not a number"));
        let is_json = reply.content_type().as_deref() == Some(JSON);
        if case.method == "GET" || is_json {
            assert_eq!(
                declared,
                reply.body.len() as u64,
                "{label}: Content-Length must equal the body length"
            );
        }

        if let Some(content_range) = reply.header("content-range") {
            check_content_range(&label, reply.status, content_range, reply.body.len());
        }

        if is_json && !reply.body.is_empty() {
            let schema = json_schema_for(doc, &raw, case.pattern, case.method, reply.status)
                .unwrap_or_else(|| {
                    panic!(
                        "{label}: the document has no application/json schema for status {}",
                        reply.status
                    )
                });
            let instance: Value = serde_json::from_slice(&reply.body)
                .unwrap_or_else(|error| panic!("{label}: body is not JSON: {error}"));
            validate_against(&schema, &instance)
                .unwrap_or_else(|error| panic!("{label}: body does not match the document: {error}"));
            json_validated += 1;
        }

        println!(
            "{label} -> {} ({} bytes, Content-Length {declared}{})",
            reply.status,
            reply.body.len(),
            if is_json && !reply.body.is_empty() {
                ", JSON validated"
            } else {
                ""
            }
        );
        checked += 1;
    }
    println!("contract[{name}]: {checked} responses checked, {json_validated} JSON bodies validated");
    (checked, json_validated)
}

fn body_preview(body: &[u8]) -> String {
    let text = String::from_utf8_lossy(body);
    let text: String = text.chars().take(200).collect();
    format!("{text:?}")
}

/// `Content-Range: bytes <start>-<end>/<total>` must announce exactly the body
/// that followed; `bytes */<total>` is the 416 form and carries no byte.
fn check_content_range(label: &str, status: u16, header: &str, body_len: usize) {
    let rest = header
        .strip_prefix("bytes ")
        .unwrap_or_else(|| panic!("{label}: Content-Range {header:?} has no bytes unit"));
    let (span, total) = rest
        .split_once('/')
        .unwrap_or_else(|| panic!("{label}: Content-Range {header:?} has no /total"));
    let total = total
        .parse::<u64>()
        .unwrap_or_else(|_| panic!("{label}: Content-Range total {total:?} is not a number"));
    if span == "*" {
        assert_eq!(status, 416, "{label}: bytes */total belongs to a 416");
        assert_eq!(body_len, 0, "{label}: a 416 carries no byte");
        return;
    }
    let (start, end) = span
        .split_once('-')
        .unwrap_or_else(|| panic!("{label}: Content-Range span {span:?} has no -"));
    let start: u64 = start
        .parse()
        .unwrap_or_else(|_| panic!("{label}: Content-Range start {start:?} is not a number"));
    let end: u64 = end
        .parse()
        .unwrap_or_else(|_| panic!("{label}: Content-Range end {end:?} is not a number"));
    assert!(end >= start, "{label}: Content-Range end {end} < start {start}");
    assert!(end < total, "{label}: Content-Range end {end} is not below total {total}");
    assert_eq!(
        body_len as u64,
        end - start + 1,
        "{label}: the body must be exactly the announced span"
    );
}

// ---- the shipped document ------------------------------------------------

fn parse_document() -> OpenAPI {
    serde_json::from_str(OPENAPI_JSON).expect("the shipped document parses as OpenAPI 3.0")
}

/// Every documented (METHOD, path pattern), upper-cased method first.
fn document_operations(doc: &OpenAPI) -> BTreeSet<(String, String)> {
    doc.operations()
        .map(|(path, method, _)| (method.to_ascii_uppercase(), path.to_string()))
        .collect()
}

fn operation_for<'a>(doc: &'a OpenAPI, pattern: &str, method: &str) -> Option<&'a Operation> {
    doc.operations()
        .find(|(path, candidate, _)| *path == pattern && candidate.eq_ignore_ascii_case(method))
        .map(|(_, _, operation)| operation)
}

fn resolve_response<'a>(doc: &'a OpenAPI, item: &'a ReferenceOr<Response>) -> Option<&'a Response> {
    match item {
        ReferenceOr::Item(response) => Some(response),
        ReferenceOr::Reference { reference } => {
            let name = reference.strip_prefix("#/components/responses/")?;
            match doc.components.as_ref()?.responses.get(name)? {
                ReferenceOr::Item(response) => Some(response),
                ReferenceOr::Reference { .. } => None,
            }
        }
    }
}

/// The JSON Schema a valid response to `method pattern -> status` must match,
/// taken from the parsed document, dereferenced and made JSON-Schema-valid.
fn json_schema_for(
    doc: &OpenAPI,
    raw: &Value,
    pattern: &str,
    method: &str,
    status: u16,
) -> Option<Value> {
    let operation = operation_for(doc, pattern, method)?;
    let response = operation.responses.responses.get(&StatusCode::Code(status))?;
    let response = resolve_response(doc, response)?;
    let schema = response.content.get(JSON)?.schema.as_ref()?;
    let schema = match schema {
        ReferenceOr::Item(schema) => schema,
        ReferenceOr::Reference { reference } => {
            let name = reference.strip_prefix("#/components/schemas/")?;
            match doc.components.as_ref()?.schemas.get(name)? {
                ReferenceOr::Item(schema) => schema,
                ReferenceOr::Reference { .. } => return None,
            }
        }
    };
    let value = serde_json::to_value(schema).expect("schema serialises");
    Some(normalise_nullable(deref_document(raw, &value)))
}

/// Replace every `{"$ref": "#/..."}` with the value that pointer names, so the
/// schema handed to `jsonschema` is self-contained.
fn deref_document(document: &Value, value: &Value) -> Value {
    match value {
        Value::Object(map) if map.len() == 1 && map.contains_key("$ref") => {
            let reference = map["$ref"].as_str().expect("$ref is a string");
            let target = document
                .pointer(reference)
                .unwrap_or_else(|| panic!("the document has no {reference}"));
            deref_document(document, target)
        }
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(name, child)| (name.clone(), deref_document(document, child)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|child| deref_document(document, child))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// Turn the OpenAPI 3.0 `nullable` keyword into JSON Schema's `"null"` type.
///
/// [`nullable_normaliser_is_not_vacuous`] proves this changes the outcome: a
/// document-marked-nullable field accepts `null` only after this pass, while a
/// `null` in a field without `nullable` still fails.
fn normalise_nullable(value: Value) -> Value {
    match value {
        Value::Object(mut map) => {
            for child in map.values_mut() {
                *child = normalise_nullable(std::mem::take(child));
            }
            let nullable = map
                .remove("nullable")
                .and_then(|flag| flag.as_bool())
                .unwrap_or(false);
            if nullable {
                match map.get_mut("type") {
                    Some(Value::String(name)) => {
                        let name = std::mem::take(name);
                        map.insert(
                            "type".to_string(),
                            Value::Array(vec![Value::String(name), Value::String("null".to_string())]),
                        );
                    }
                    Some(Value::Array(names)) => names.push(Value::String("null".to_string())),
                    _ => panic!("a nullable schema without a type cannot be normalised: {map:?}"),
                }
            }
            Value::Object(map)
        }
        Value::Array(items) => Value::Array(items.into_iter().map(normalise_nullable).collect()),
        other => other,
    }
}

/// Validate `instance`; on failure return every error with its JSON pointer,
/// e.g. `/uptime_s: "42" is not of type "integer"`.
fn validate_against(schema: &Value, instance: &Value) -> Result<(), String> {
    let validator = jsonschema::validator_for(schema)
        .map_err(|error| format!("the document's schema does not compile: {error}"))?;
    let mut errors: Vec<String> = validator
        .iter_errors(instance)
        .map(|error| {
            let pointer = error.instance_path().as_str().to_string();
            let pointer = if pointer.is_empty() { "/".to_string() } else { pointer };
            format!("{pointer}: {error}")
        })
        .collect();
    if errors.is_empty() {
        return Ok(());
    }
    errors.sort();
    Err(errors.join("; "))
}

// ---- driver 1: the crate's pure code, no socket --------------------------

struct InProcess;

/// What `response_for`/`serve_request` in `server.rs` produce, before bytes.
struct Prepared {
    status: Status,
    content_length: u64,
    content_range: Option<String>,
    download: bool,
    body: body::Body,
}

fn snapshot() -> json::Snapshot<'static> {
    json::Snapshot {
        version: json::VERSION,
        build: json::BUILD,
        uptime_s: 42,
        ip: "192.168.1.33",
        hostname: json::HOSTNAME,
        mounted: false,
        total_bytes: 0,
        free_bytes: 0,
        clock_khz: 0,
        rtsp_port: 554,
        rtsp_path: json::RTSP_PATH,
        rtsp_running: true,
        recording_enabled: true,
        recording_file: Some(RECORDING_NAME),
        temperature_c: None,
        heap_internal_free: 120_000,
        heap_spiram_free: 7_000_000,
    }
}

fn in_process_cases() -> Vec<Case> {
    vec![
        Case::get("/", "/", 200),
        Case::get("/api/v1/status", "/api/v1/status", 200),
        Case::get("/api/v1/files", "/api/v1/files", 200),
        Case::get("/api/v1/openapi.json", "/api/v1/openapi.json", 200),
        Case::get("/api/v1/files/{path}", RECORDING_TARGET, 200),
        Case::head("/api/v1/files/{path}", RECORDING_TARGET, 200),
        Case::get("/api/v1/files/{path}", RECORDING_TARGET, 206).ranged("bytes=1024-2047"),
        Case::get("/api/v1/files/{path}", RECORDING_TARGET, 416).ranged("bytes=9999-"),
    ]
}

fn prepare(method: http::Method, target: &str, range: Option<&str>) -> Result<Prepared, String> {
    let route =
        router::route(method, target).map_err(|status| format!("unrouted: {}", status.code()))?;
    let files = [(RECORDING_NAME, &RECORDING[..])];
    let source = body::MemorySource::new(&files);
    Ok(match route {
        Route::Index => {
            let mut text = String::new();
            json::index_body(&mut text);
            json_text(text)
        }
        Route::Status => {
            let mut text = String::new();
            json::status_body(&snapshot(), &mut text);
            json_text(text)
        }
        Route::Files => {
            let mut text = String::new();
            json::list_body(LIST_DIR, &source, &mut text, json::LIST_CAP);
            json_text(text)
        }
        Route::OpenApi => {
            let text = json::openapi_body();
            Prepared {
                status: Status::OK,
                content_length: text.len() as u64,
                content_range: None,
                download: false,
                body: body::Body::Bytes(text.as_bytes()),
            }
        }
        Route::Download => {
            let encoded = router::download_relative(route, target)
                .ok_or_else(|| "download route without a relative path".to_string())?;
            let mapped =
                path::map(encoded, MOUNT_POINT).map_err(|error| format!("path rejected: {error:?}"))?;
            let relative = mapped
                .strip_prefix(MOUNT_POINT)
                .and_then(|rest| rest.strip_prefix('/'))
                .ok_or_else(|| format!("{mapped} is not below {MOUNT_POINT}"))?;
            let total = source
                .len(relative)
                .map_err(|error| format!("length of {relative}: {error}"))?;
            let selected = range::parse(range, total);
            let download = body::download(relative, total, selected);
            Prepared {
                status: download.status,
                content_length: download.content_length,
                content_range: download.content_range,
                download: true,
                body: body::body_for(method, download.body),
            }
        }
    })
}

fn json_text(text: String) -> Prepared {
    Prepared {
        status: Status::OK,
        content_length: text.len() as u64,
        content_range: None,
        download: false,
        body: body::Body::Json(text),
    }
}

/// The download head from `server.rs`, which is hand-written there because
/// `ResponseHead::extra` is `&'static` and `Content-Range` is not.
fn download_head(status: Status, content_length: u64, content_range: Option<&str>) -> String {
    use std::fmt::Write as _;

    let mut head = String::with_capacity(192);
    let _ = write!(
        head,
        "HTTP/1.1 {} {}\r\nContent-Type: {OCTET_STREAM}\r\nContent-Length: {content_length}\r\n\
         Accept-Ranges: bytes\r\n",
        status.code(),
        status.reason()
    );
    if let Some(range) = content_range {
        let _ = write!(head, "Content-Range: {range}\r\n");
    }
    head.push_str("Connection: close\r\n\r\n");
    head
}

fn parse_method(method: &str) -> http::Method {
    match method {
        "GET" => http::Method::Get,
        "HEAD" => http::Method::Head,
        other => panic!("the contract test does not send {other}"),
    }
}

impl Driver for InProcess {
    fn name(&self) -> &'static str {
        "in-process"
    }

    fn cases(&mut self) -> Vec<Case> {
        in_process_cases()
    }

    fn request(&mut self, case: &Case) -> Result<Reply, String> {
        let method = parse_method(case.method);
        let prepared = prepare(method, &case.target, case.range.as_deref())?;
        let files = [(RECORDING_NAME, &RECORDING[..])];
        let source = body::MemorySource::new(&files);

        let mut wire = Vec::new();
        if prepared.download {
            wire.extend_from_slice(
                download_head(
                    prepared.status,
                    prepared.content_length,
                    prepared.content_range.as_deref(),
                )
                .as_bytes(),
            );
        } else {
            let head = http::ResponseHead::new(prepared.status, JSON, prepared.content_length);
            let mut buffer = [0u8; 512];
            let needed = http::head_len(&head);
            let written = http::write_head(&mut buffer, &head);
            if written != needed {
                return Err("response head does not fit its buffer".to_string());
            }
            wire.extend_from_slice(&buffer[..written]);
        }
        body::write_body(&prepared.body, &source, &mut wire)
            .map_err(|error| format!("body: {error}"))?;
        parse_reply(&wire)
    }
}

// ---- driver 2: the live board --------------------------------------------

struct BoardUrl {
    host: String,
    port: u16,
}

/// Whether the live driver should run, skip or fail when invoked.
enum LiveGate {
    Run(String),
    Skip(String),
    Fail(String),
}

fn live_gate(board_url: Option<&str>, required: bool) -> LiveGate {
    let value = board_url.unwrap_or_default().trim();
    if value.is_empty() {
        let message = format!(
            "{BOARD_URL_ENV} is required: set {BOARD_URL_ENV}=http://<board-ip> or run harness/api_contract.sh http://<board-ip>"
        );
        return if required {
            LiveGate::Fail(message)
        } else {
            LiveGate::Skip(format!("{message} (in-process driver only)"))
        };
    }
    LiveGate::Run(value.to_string())
}

fn parse_board_url(url: &str) -> Result<BoardUrl, String> {
    let authority = url
        .strip_prefix("http://")
        .ok_or_else(|| format!("{url:?} is not an http:// URL"))?;
    let (authority, path) = match authority.split_once('/') {
        Some((authority, path)) => (authority, path),
        None => (authority, ""),
    };
    if !path.is_empty() {
        return Err(format!(
            "{url:?} has the path /{path}: give the board root, e.g. http://<board-ip>"
        ));
    }
    if authority.is_empty() {
        return Err(format!("{url:?} has no host"));
    }
    match authority.rsplit_once(':') {
        Some((host, port)) => {
            if host.is_empty() {
                return Err(format!("{url:?} has no host"));
            }
            let port = port
                .parse::<u16>()
                .map_err(|_| format!("{url:?} has the port {port:?} that is not a u16"))?;
            Ok(BoardUrl {
                host: host.to_string(),
                port,
            })
        }
        None => Ok(BoardUrl {
            host: authority.to_string(),
            port: 80,
        }),
    }
}

struct LiveBoard(BoardUrl);

impl LiveBoard {
    fn exchange(&self, case: &Case) -> Result<Reply, String> {
        let address = format!("{}:{}", self.0.host, self.0.port);
        let socket = address
            .to_socket_addrs()
            .map_err(|error| format!("resolve {address}: {error}"))?
            .next()
            .ok_or_else(|| format!("{address} resolved to no socket"))?;
        let mut stream = TcpStream::connect_timeout(&socket, CONNECT_TIMEOUT)
            .map_err(|error| format!("connect {address}: {error}"))?;
        stream
            .set_read_timeout(Some(IO_TIMEOUT))
            .map_err(|error| format!("read timeout on {address}: {error}"))?;
        stream
            .set_write_timeout(Some(IO_TIMEOUT))
            .map_err(|error| format!("write timeout on {address}: {error}"))?;

        let mut request = format!(
            "{} {} HTTP/1.1\r\nHost: {}\r\nAccept: {JSON}, {OCTET_STREAM}\r\nConnection: close\r\n",
            case.method, case.target, self.0.host
        );
        if let Some(range) = &case.range {
            request.push_str(&format!("Range: {range}\r\n"));
        }
        request.push_str("\r\n");
        stream
            .write_all(request.as_bytes())
            .map_err(|error| format!("send {} {}: {error}", case.method, case.target))?;
        let mut raw = Vec::new();
        stream
            .read_to_end(&mut raw)
            .map_err(|error| format!("read {} {}: {error}", case.method, case.target))?;
        parse_reply(&raw)
    }

    /// The first listed recording, if the card has one.
    fn first_recording(&self) -> Option<String> {
        let reply = self
            .exchange(&Case::get("/api/v1/files", "/api/v1/files", 200))
            .ok()?;
        if reply.status != 200 {
            return None;
        }
        let listing: Value = serde_json::from_slice(&reply.body).ok()?;
        listing["files"]
            .as_array()?
            .first()?
            .get("path")?
            .as_str()
            .map(str::to_string)
    }
}

impl Driver for LiveBoard {
    fn name(&self) -> &'static str {
        "live"
    }

    fn cases(&mut self) -> Vec<Case> {
        let mut cases = vec![
            Case::get("/", "/", 200),
            Case::get("/api/v1/status", "/api/v1/status", 200),
            Case::get("/api/v1/files", "/api/v1/files", 200),
            Case::get("/api/v1/openapi.json", "/api/v1/openapi.json", 200),
        ];
        match self.first_recording() {
            Some(path) => {
                let target = format!("/api/v1/files/{}", path.trim_start_matches('/'));
                cases.push(Case::get("/api/v1/files/{path}", &target, 200));
                cases.push(Case::head("/api/v1/files/{path}", &target, 200));
            }
            None => {
                let target = "/api/v1/files/rec/absent.avi";
                cases.push(Case::get("/api/v1/files/{path}", target, 404));
                cases.push(Case::head("/api/v1/files/{path}", target, 404));
            }
        }
        cases
    }

    fn request(&mut self, case: &Case) -> Result<Reply, String> {
        self.exchange(case)
    }
}

// ---- tests ---------------------------------------------------------------

#[test]
fn in_process_driver_matches_the_shipped_document() {
    assert_eq!(
        json::openapi_body(),
        OPENAPI_JSON,
        "the firmware serves the checked-in document verbatim"
    );

    let doc = parse_document();
    let documented = document_operations(&doc);
    for (method, pattern) in &documented {
        assert!(
            router::ROUTES.contains(&(method.as_str(), pattern.as_str())),
            "{method} {pattern} is documented but absent from router::ROUTES"
        );
    }
    assert_eq!(
        router::ROUTES.len(),
        documented.len(),
        "router::ROUTES must not carry operations the document lacks: {documented:?}"
    );

    let mut driver = InProcess;
    let (checked, validated) = assert_contract(&mut driver, &doc);
    assert_eq!(checked, in_process_cases().len());
    assert_eq!(validated, 4, "the four documented JSON routes");

    let partial = driver
        .request(&Case::get("/api/v1/files/{path}", RECORDING_TARGET, 206).ranged("bytes=1024-2047"))
        .expect("the in-process driver serves the 206");
    assert_eq!(partial.body, RECORDING[1024..2048], "the 206 body is the requested window");
}

#[test]
fn live_driver_matches_the_shipped_document() {
    let board_url = std::env::var(BOARD_URL_ENV).ok();
    let required = std::env::var_os(REQUIRE_LIVE_ENV).is_some();
    let url = match live_gate(board_url.as_deref(), required) {
        LiveGate::Run(url) => url,
        LiveGate::Skip(reason) => {
            println!("SKIP: {reason}");
            return;
        }
        LiveGate::Fail(reason) => panic!("{reason}"),
    };
    let board = parse_board_url(&url).unwrap_or_else(|error| panic!("{BOARD_URL_ENV}: {error}"));
    println!("contract[live]: board {url}");
    let doc = parse_document();
    let mut driver = LiveBoard(board);
    let (checked, _validated) = assert_contract(&mut driver, &doc);
    assert!(checked >= 6, "every documented operation must be exercised");
}

#[test]
fn live_gate_requires_the_board_url() {
    match live_gate(None, true) {
        LiveGate::Fail(message) => assert!(
            message.contains("ESP32_BOARD_URL is required"),
            "the failure must name the variable: {message}"
        ),
        _ => panic!("an unset URL must fail when the live driver is required"),
    }
    assert!(
        matches!(live_gate(Some("   "), true), LiveGate::Fail(_)),
        "an empty URL is as missing as an unset one"
    );
    assert!(
        matches!(live_gate(None, false), LiveGate::Skip(_)),
        "a bare `cargo test` on a machine with no board must not fail"
    );
    match live_gate(Some("http://192.168.1.33"), false) {
        LiveGate::Run(url) => assert_eq!(url, "http://192.168.1.33"),
        _ => panic!("a set URL must run"),
    }
}

#[test]
fn nullable_normaliser_is_not_vacuous() {
    let raw: Value = serde_json::from_str(OPENAPI_JSON).expect("the shipped document is JSON");
    let doc = parse_document();
    let normalised = json_schema_for(&doc, &raw, "/api/v1/status", "get", 200)
        .expect("the document describes the status body");

    let mut body = String::new();
    json::status_body(&snapshot(), &mut body);
    let mut instance: Value = serde_json::from_str(&body).expect("the status body is JSON");
    validate_against(&normalised, &instance).expect("the documented fixture validates");

    instance["recording_file"] = Value::Null;
    validate_against(&normalised, &instance)
        .expect("recording_file is documented nullable, so null must pass after normalisation");

    let raw_schema = deref_document(
        &raw,
        raw.pointer("/components/schemas/Status")
            .expect("the document names the Status schema"),
    );
    assert!(
        validate_against(&raw_schema, &instance).is_err(),
        "without normalisation the nullable keyword is not JSON Schema, so null must fail - \
         otherwise the normaliser changes nothing"
    );

    instance["ip"] = Value::Null;
    let error = validate_against(&normalised, &instance)
        .expect_err("ip is documented non-nullable, so null must fail");
    assert!(
        error.contains("/ip"),
        "the failure must name the JSON pointer /ip: {error}"
    );
}

#[test]
fn wrong_field_type_names_the_json_pointer() {
    let raw: Value = serde_json::from_str(OPENAPI_JSON).expect("the shipped document is JSON");
    let doc = parse_document();
    let schema = json_schema_for(&doc, &raw, "/api/v1/status", "get", 200)
        .expect("the document describes the status body");
    let mut body = String::new();
    json::status_body(&snapshot(), &mut body);
    let mut instance: Value = serde_json::from_str(&body).expect("the status body is JSON");

    instance["uptime_s"] = Value::String("42".to_string());
    let error = validate_against(&schema, &instance)
        .expect_err("uptime_s is documented as an integer, so a string must fail");
    assert!(
        error.contains("/uptime_s"),
        "the failure must name the JSON pointer /uptime_s: {error}"
    );
}
