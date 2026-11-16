//! HTTP/1.1 request parsing and response head serialisation.
//!
//! # Contract
//!
//! `parse(buf) -> Result<Request<'_>, ParseError>` reads one request head from
//! the start of `buf` and borrows every string from it: no allocation, no copy,
//! no I/O. It never looks past the terminating blank line, so body bytes that
//! share `buf` are ignored and never inspected: no chunked framing is parsed
//! and `Content-Length` is neither used nor trusted.
//!
//! Accepted:
//! - `GET`, `HEAD`, `OPTIONS` map to [`Method::Get`], [`Method::Head`],
//!   [`Method::Options`]; any other syntactically valid method token maps to
//!   [`Method::Other`], which the router answers with 405. A lowercase `get`
//!   is therefore `Other`, never a `GET`.
//! - origin-form targets (`/x?y`) and absolute-form targets
//!   (`http://host/x?y`, `https://host/x?y`). [`Request::target`] is the raw
//!   target; [`Request::path`] and [`Request::query`] are its split, with the
//!   `?` removed and an empty query reported as `Some("")`.
//! - `HTTP/1.0` and `HTTP/1.1` only.
//! - header names compared case-insensitively; with duplicate names the first
//!   line wins.
//! - header values trimmed of leading and trailing space and tab, and allowed
//!   to contain `:`.
//!
//! Rejected with [`ParseError::TooLarge`] when the head - request line,
//! headers and the final CRLFCRLF - would exceed [`MAX_HEAD`] (8192) bytes, and
//! with [`ParseError::Malformed`] for everything else: a bare CR or LF line
//! ending (so LF-only requests fail), a missing or unsupported version, an
//! empty or extra-space request line, a fragment (`#`), a non-ASCII or control
//! byte in the target, and `*`/authority-form targets.
//!
//! `header(buf, name)` finds a header in the same head region with the same
//! rules (case-insensitive name, first duplicate wins, OWS trimmed) and yields
//! `None` when the header is absent, the name is empty, or the value is not
//! UTF-8. It never scans past the blank line, so a same-named line in a body is
//! not visible.
//!
//! `write_head(out, head)` serialises `HTTP/1.1 <status> <reason>`,
//! `Content-Type`, `Content-Length`, every `extra` field in order, then
//! `Connection: close`, each CRLF-terminated, and returns the number of bytes
//! written. It never panics and never writes past `out`; a short `out` yields a
//! truncated, invalid head, so the caller must compare the return value with
//! [`head_len`] before sending. Field text is filtered to printable ASCII, so a
//! `reason` or extra value cannot inject a header line. Responses are always
//! announced as `HTTP/1.1` and always close the connection, which is valid for
//! an `HTTP/1.0` request too.
//!
//! [`Status`] pairs a code with its canonical reason phrase; [`reason_for`]
//! maps a bare `u16` (the type of [`ResponseHead::status`]) to the same phrase.

/// Largest request head accepted: request line, headers and the final blank
/// line. Anything that needs more is [`ParseError::TooLarge`].
pub const MAX_HEAD: usize = 8192;

/// A request method. Only the three verbs this server answers keep their
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

/// The protocol version of a request.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Version {
    Http10,
    Http11,
}

impl Version {
    pub const fn as_str(self) -> &'static str {
        match self {
            Version::Http10 => "HTTP/1.0",
            Version::Http11 => "HTTP/1.1",
        }
    }
}

impl core::fmt::Display for Version {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why a request head could not be parsed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParseError {
    /// The head did not end within [`MAX_HEAD`] bytes.
    TooLarge,
    /// Anything else: line endings, request line, target or version.
    Malformed,
}

impl core::fmt::Display for ParseError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            ParseError::TooLarge => "request head exceeds 8192 bytes",
            ParseError::Malformed => "malformed request head",
        })
    }
}

impl core::error::Error for ParseError {}

/// A parsed request head. Every field borrows from the buffer passed to
/// `parse`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Request<'a> {
    pub method: Method,
    /// The raw request-target, exactly as received (still percent-encoded, and
    /// still absolute-form when the client sent one).
    pub target: &'a str,
    /// The path part of `target`: always starts with `/`, never contains `?`.
    pub path: &'a str,
    /// The query part of `target` without its leading `?`; `Some("")` for a
    /// trailing bare `?`.
    pub query: Option<&'a str>,
    pub version: Version,
}

/// A response status code with its canonical reason phrase.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Status(pub u16);

impl Status {
    pub const OK: Status = Status(200);
    pub const PARTIAL_CONTENT: Status = Status(206);
    pub const BAD_REQUEST: Status = Status(400);
    pub const NOT_FOUND: Status = Status(404);
    pub const METHOD_NOT_ALLOWED: Status = Status(405);
    pub const RANGE_NOT_SATISFIABLE: Status = Status(416);
    pub const INTERNAL_SERVER_ERROR: Status = Status(500);
    pub const SERVICE_UNAVAILABLE: Status = Status(503);

    pub const fn new(code: u16) -> Status {
        Status(code)
    }

    pub const fn code(self) -> u16 {
        self.0
    }

    /// The canonical reason phrase for this code.
    pub const fn reason(self) -> &'static str {
        reason_for(self.0)
    }
}

impl From<Status> for u16 {
    fn from(status: Status) -> u16 {
        status.0
    }
}

impl From<u16> for Status {
    fn from(code: u16) -> Status {
        Status(code)
    }
}

impl PartialEq<u16> for Status {
    fn eq(&self, other: &u16) -> bool {
        self.0 == *other
    }
}

/// The reason phrase for `code`, or `"Unknown"` for a code this server does
/// not use.
pub const fn reason_for(code: u16) -> &'static str {
    match code {
        200 => "OK",
        206 => "Partial Content",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        413 => "Payload Too Large",
        416 => "Range Not Satisfiable",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        503 => "Service Unavailable",
        _ => "Unknown",
    }
}

/// The head of a response, before its body. `extra` lines follow
/// `Content-Type`/`Content-Length` and must not repeat them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResponseHead {
    pub status: u16,
    pub reason: &'static str,
    pub content_type: &'static str,
    pub content_length: u64,
    pub extra: &'static [(&'static str, &'static str)],
}

impl ResponseHead {
    /// A head for `status` with no extra lines; the reason phrase is filled in
    /// from `reason_for`.
    pub const fn new(status: Status, content_type: &'static str, content_length: u64) -> Self {
        ResponseHead {
            status: status.0,
            reason: reason_for(status.0),
            content_type,
            content_length,
            extra: &[],
        }
    }

    /// The same head with the given extra header lines (for example
    /// `Accept-Ranges` or `Content-Range`).
    pub const fn with_extra(mut self, extra: &'static [(&'static str, &'static str)]) -> Self {
        self.extra = extra;
        self
    }
}

/// The exact number of bytes [`write_head`] writes for `head`, so the caller can
/// size its buffer and detect truncation.
pub fn head_len(head: &ResponseHead) -> usize {
    let mut writer = HeadWriter {
        out: None,
        written: 0,
    };
    write_head_into(&mut writer, head);
    writer.written
}

/// Serialise `head` into `out` and return the number of bytes written.
///
/// Never panics. A buffer shorter than [`head_len`] yields a truncated, invalid
/// head and the return value then equals `out.len()`, so callers detect
/// truncation by comparing the result with [`head_len`].
pub fn write_head(out: &mut [u8], head: &ResponseHead) -> usize {
    let capacity = out.len();
    let mut writer = HeadWriter {
        out: Some(out),
        written: 0,
    };
    write_head_into(&mut writer, head);
    writer.written.min(capacity)
}

struct HeadWriter<'a> {
    out: Option<&'a mut [u8]>,
    written: usize,
}

impl HeadWriter<'_> {
    /// Copy as much of `bytes` as fits, but always count the full length.
    fn bytes(&mut self, bytes: &[u8]) {
        if let Some(out) = self.out.as_deref_mut() {
            if self.written < out.len() {
                let take = bytes.len().min(out.len() - self.written);
                out[self.written..self.written + take].copy_from_slice(&bytes[..take]);
            }
        }
        self.written = self.written.saturating_add(bytes.len());
    }

    /// Field text only: drop control bytes so a field value can never start a
    /// new header line or smuggle in a DEL.
    fn field(&mut self, text: &str) {
        for &byte in text.as_bytes() {
            if byte >= 0x20 && byte != 0x7f {
                self.bytes(&[byte]);
            }
        }
    }

    /// Decimal digits, no allocation.
    fn decimal(&mut self, mut value: u64) {
        let mut digits = [0u8; 20];
        let mut index = digits.len();
        loop {
            index -= 1;
            digits[index] = b'0' + (value % 10) as u8;
            value /= 10;
            if value == 0 {
                break;
            }
        }
        self.bytes(&digits[index..]);
    }
}

fn write_head_into(writer: &mut HeadWriter<'_>, head: &ResponseHead) {
    writer.bytes(b"HTTP/1.1 ");
    writer.decimal(head.status as u64);
    writer.bytes(b" ");
    writer.field(head.reason);
    writer.bytes(b"\r\nContent-Type: ");
    writer.field(head.content_type);
    writer.bytes(b"\r\nContent-Length: ");
    writer.decimal(head.content_length);
    for (name, value) in head.extra {
        writer.bytes(b"\r\n");
        writer.field(name);
        writer.bytes(b": ");
        writer.field(value);
    }
    writer.bytes(b"\r\nConnection: close\r\n\r\n");
}

/// Parse one request head from the start of `buf`.
pub fn parse(buf: &[u8]) -> Result<Request<'_>, ParseError> {
    let head = bounded_head(buf)?;
    let line_end = find_crlf(head, 0).ok_or(ParseError::Malformed)?;
    let line = &head[..line_end];
    let first_space = line
        .iter()
        .position(|b| *b == b' ')
        .ok_or(ParseError::Malformed)?;
    let rest = &line[first_space + 1..];
    let second_space = rest
        .iter()
        .position(|b| *b == b' ')
        .map(|offset| first_space + 1 + offset)
        .ok_or(ParseError::Malformed)?;

    let method_token = &line[..first_space];
    let target_token = &line[first_space + 1..second_space];
    let version_token = &line[second_space + 1..];

    // Exactly three tokens separated by single SP: an empty one or a space in
    // the version means the client sent something this parser does not read.
    if method_token.is_empty()
        || target_token.is_empty()
        || version_token.is_empty()
        || version_token.iter().any(|b| *b == b' ' || *b == b'\t')
    {
        return Err(ParseError::Malformed);
    }

    let method = method_from_token(method_token)?;
    let version = match version_token {
        token if token == b"HTTP/1.1" => Version::Http11,
        token if token == b"HTTP/1.0" => Version::Http10,
        _ => return Err(ParseError::Malformed),
    };
    let target = core::str::from_utf8(target_token).map_err(|_| ParseError::Malformed)?;
    let (path, query) = split_target(target)?;

    Ok(Request {
        method,
        target,
        path,
        query,
        version,
    })
}

/// Find the value of the header called `name` (case-insensitive) in the request
/// head at the start of `buf`.
pub fn header<'a>(buf: &'a [u8], name: &str) -> Option<&'a str> {
    if name.is_empty() {
        return None;
    }
    let bytes = &buf[..buf.len().min(MAX_HEAD)];
    let head_end = match bytes.windows(4).position(|window| window == b"\r\n\r\n") {
        Some(blank) => blank + 2,
        None => bytes.len(),
    };
    let head = &bytes[..head_end];

    let mut line_start = find_crlf(head, 0)? + 2;
    while line_start < head.len() {
        let line_end = find_crlf(head, line_start)?;
        let line = &head[line_start..line_end];
        if let Some(colon) = line.iter().position(|b| *b == b':') {
            if line[..colon].eq_ignore_ascii_case(name.as_bytes()) {
                return core::str::from_utf8(trim_ows(&line[colon + 1..])).ok();
            }
        }
        line_start = line_end + 2;
    }
    None
}

/// The request head through the blank line, with CRLF line endings enforced.
fn bounded_head(buf: &[u8]) -> Result<&[u8], ParseError> {
    let window = &buf[..buf.len().min(MAX_HEAD)];
    match window.windows(4).position(|w| w == b"\r\n\r\n") {
        Some(blank) => {
            let head = &window[..blank + 4];
            if head_has_invalid_byte(head) {
                Err(ParseError::Malformed)
            } else {
                Ok(head)
            }
        }
        // No blank line inside the window: either it is not there yet
        // (malformed) or it does not fit (too large).
        None if buf.len() >= MAX_HEAD => Err(ParseError::TooLarge),
        None => Err(ParseError::Malformed),
    }
}

/// A bare CR, a bare LF, or any control byte other than tab.
fn head_has_invalid_byte(bytes: &[u8]) -> bool {
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'\r' if bytes.get(index + 1) == Some(&b'\n') => index += 2,
            b'\r' | b'\n' => return true,
            byte if byte < 0x20 && byte != b'\t' => return true,
            _ => index += 1,
        }
    }
    false
}

fn find_crlf(bytes: &[u8], from: usize) -> Option<usize> {
    if from >= bytes.len() {
        return None;
    }
    bytes[from..]
        .windows(2)
        .position(|window| window == b"\r\n")
        .map(|offset| from + offset)
}

fn method_from_token(token: &[u8]) -> Result<Method, ParseError> {
    if !token.iter().all(|byte| is_token_byte(*byte)) {
        return Err(ParseError::Malformed);
    }
    Ok(match token {
        token if token == b"GET" => Method::Get,
        token if token == b"HEAD" => Method::Head,
        token if token == b"OPTIONS" => Method::Options,
        _ => Method::Other,
    })
}

/// RFC 9110 `tchar`: the bytes a method token may contain.
fn is_token_byte(byte: u8) -> bool {
    matches!(
        byte,
        b'!' | b'#'
            | b'$'
            | b'%'
            | b'&'
            | b'\''
            | b'*'
            | b'+'
            | b'-'
            | b'.'
            | b'^'
            | b'_'
            | b'`'
            | b'|'
            | b'~'
    ) || byte.is_ascii_alphanumeric()
}

/// Split a request-target into its path and query, accepting origin-form and
/// absolute-form only.
fn split_target(target: &str) -> Result<(&str, Option<&str>), ParseError> {
    let bytes = target.as_bytes();
    if bytes.is_empty()
        || bytes
            .iter()
            .any(|byte| !(0x21..0x7f).contains(byte) || *byte == b'#')
    {
        return Err(ParseError::Malformed);
    }

    for scheme in ["http://", "https://"] {
        if let Some(after_scheme) = strip_prefix_ascii_case(target, scheme) {
            let authority_end = after_scheme
                .find(|c| c == '/' || c == '?')
                .unwrap_or(after_scheme.len());
            if authority_end == 0 {
                return Err(ParseError::Malformed);
            }
            let remainder = &after_scheme[authority_end..];
            return match remainder.as_bytes().first() {
                None => Ok(("/", None)),
                Some(b'?') => Ok(("/", Some(&remainder[1..]))),
                _ => split_path_query(remainder),
            };
        }
    }

    if !target.starts_with('/') {
        return Err(ParseError::Malformed);
    }
    split_path_query(target)
}

fn split_path_query(target: &str) -> Result<(&str, Option<&str>), ParseError> {
    match target.find('?') {
        Some(0) => Err(ParseError::Malformed),
        Some(mark) => Ok((&target[..mark], Some(&target[mark + 1..]))),
        None => Ok((target, None)),
    }
}

fn strip_prefix_ascii_case<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    let head = text.as_bytes().get(..prefix.len())?;
    if head.eq_ignore_ascii_case(prefix.as_bytes()) {
        Some(&text[prefix.len()..])
    } else {
        None
    }
}

fn trim_ows(value: &[u8]) -> &[u8] {
    let mut start = 0;
    let mut end = value.len();
    while start < end && (value[start] == b' ' || value[start] == b'\t') {
        start += 1;
    }
    while end > start && (value[end - 1] == b' ' || value[end - 1] == b'\t') {
        end -= 1;
    }
    &value[start..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIMPLE: &[u8] =
        b"GET /api/v1/status HTTP/1.1\r\nHost: 192.168.1.50\r\nAccept: */*\r\n\r\n";

    /// A head whose total length is exactly `total` bytes, including the
    /// final CRLFCRLF.
    fn head_of_len(total: usize) -> std::vec::Vec<u8> {
        let prefix = b"GET / HTTP/1.1\r\nX-Pad: ";
        let mut buf = std::vec::Vec::with_capacity(total);
        buf.extend_from_slice(prefix);
        buf.resize(total - 4, b'p');
        buf.extend_from_slice(b"\r\n\r\n");
        buf
    }

    // --- request line -------------------------------------------------------

    #[test]
    fn parse_origin_form_with_query() {
        let request =
            parse(b"GET /api/v1/files/rec/REC_0001.avi?token=abc&x=1 HTTP/1.1\r\nHost: b\r\n\r\n")
                .expect("origin-form request parses");
        assert_eq!(request.method, Method::Get);
        assert_eq!(
            request.target,
            "/api/v1/files/rec/REC_0001.avi?token=abc&x=1"
        );
        assert_eq!(request.path, "/api/v1/files/rec/REC_0001.avi");
        assert_eq!(request.query, Some("token=abc&x=1"));
        assert_eq!(request.version, Version::Http11);
    }

    #[test]
    fn parse_origin_form_without_query() {
        let request = parse(b"GET / HTTP/1.0\r\n\r\n").expect("origin-form root parses");
        assert_eq!(request.path, "/");
        assert_eq!(request.query, None);
        assert_eq!(request.version, Version::Http10);
    }

    #[test]
    fn parse_borrows_target_and_path_from_the_buffer() {
        let request = parse(SIMPLE).expect("request parses");
        let base = SIMPLE.as_ptr() as usize;
        let end = base + SIMPLE.len();
        for part in [request.target, request.path] {
            let start = part.as_ptr() as usize;
            assert!(
                start >= base && start + part.len() <= end,
                "target and path must be subslices of the input, not copies"
            );
        }
        assert_eq!(request.path, "/api/v1/status");
        assert_eq!(request.query, None);
    }

    #[test]
    fn parse_absolute_form() {
        let request =
            parse(b"GET http://192.168.1.50:8080/api/v1/status HTTP/1.1\r\nHost: h\r\n\r\n")
                .expect("absolute-form request parses");
        assert_eq!(request.method, Method::Get);
        assert_eq!(request.target, "http://192.168.1.50:8080/api/v1/status");
        assert_eq!(request.path, "/api/v1/status");
        assert_eq!(request.query, None);
    }

    #[test]
    fn parse_absolute_form_with_query() {
        let request = parse(b"GET https://board.local/api/v1/files?dir=rec HTTP/1.1\r\n\r\n")
            .expect("absolute-form request with query parses");
        assert_eq!(request.path, "/api/v1/files");
        assert_eq!(request.query, Some("dir=rec"));
    }

    #[test]
    fn parse_absolute_form_without_path() {
        let request =
            parse(b"GET http://board.local HTTP/1.1\r\n\r\n").expect("absolute-form root parses");
        assert_eq!(request.path, "/");
        assert_eq!(request.query, None);
    }

    #[test]
    fn parse_head_method() {
        let request = parse(b"HEAD /api/v1/files/rec/REC_0001.avi HTTP/1.1\r\nHost: b\r\n\r\n")
            .expect("HEAD parses");
        assert_eq!(request.method, Method::Head);
        assert_eq!(request.method.as_str(), "HEAD");
    }

    #[test]
    fn parse_options_method() {
        let request = parse(b"OPTIONS /api/v1/status HTTP/1.1\r\n\r\n").expect("OPTIONS parses");
        assert_eq!(request.method, Method::Options);
    }

    #[test]
    fn parse_lowercase_method_is_other() {
        let request = parse(b"get / HTTP/1.1\r\n\r\n").expect("a lowercase method is a token");
        assert_eq!(request.method, Method::Other);
        assert_ne!(request.method, Method::Get);
        assert_eq!(request.method.as_str(), "OTHER");
    }

    #[test]
    fn parse_rejects_missing_version() {
        assert_eq!(parse(b"GET /a\r\n\r\n"), Err(ParseError::Malformed));
    }

    #[test]
    fn parse_rejects_unsupported_version() {
        assert_eq!(parse(b"GET / HTTP/2.0\r\n\r\n"), Err(ParseError::Malformed));
        assert_eq!(
            parse(b"GET / HTTP/1.1extra\r\n\r\n"),
            Err(ParseError::Malformed)
        );
    }

    #[test]
    fn parse_rejects_lf_only_line_endings() {
        assert_eq!(
            parse(b"GET / HTTP/1.1\nHost: h\nAccept: */*\n\n"),
            Err(ParseError::Malformed),
            "LF-only requests are not accepted"
        );
    }

    #[test]
    fn parse_rejects_bare_lf_inside_a_crlf_head() {
        assert_eq!(
            parse(b"GET / HTTP/1.1\r\nHost: h\nX-Y: z\r\n\r\n"),
            Err(ParseError::Malformed)
        );
    }

    #[test]
    fn parse_accepts_head_at_exactly_the_limit() {
        let buf = head_of_len(MAX_HEAD);
        assert_eq!(buf.len(), MAX_HEAD);
        let request = parse(&buf).expect("a head of exactly MAX_HEAD bytes is accepted");
        assert_eq!(request.path, "/");
    }

    #[test]
    fn parse_rejects_one_byte_over_the_limit() {
        let buf = head_of_len(MAX_HEAD + 1);
        assert_eq!(parse(&buf), Err(ParseError::TooLarge));
    }

    #[test]
    fn parse_rejects_nine_thousand_byte_head() {
        let buf = head_of_len(9000);
        assert_eq!(parse(&buf), Err(ParseError::TooLarge));
        // No terminator at all, still bigger than the cap.
        let unterminated = std::vec![b'G'; 9000];
        assert_eq!(parse(&unterminated), Err(ParseError::TooLarge));
    }

    #[test]
    fn parse_rejects_asterisk_form() {
        assert_eq!(
            parse(b"OPTIONS * HTTP/1.1\r\n\r\n"),
            Err(ParseError::Malformed)
        );
    }

    #[test]
    fn parse_rejects_fragment() {
        assert_eq!(
            parse(b"GET /a#frag HTTP/1.1\r\n\r\n"),
            Err(ParseError::Malformed)
        );
    }

    #[test]
    fn parse_rejects_extra_space_in_request_line() {
        assert_eq!(
            parse(b"GET  / HTTP/1.1\r\n\r\n"),
            Err(ParseError::Malformed)
        );
        assert_eq!(
            parse(b"GET /  HTTP/1.1\r\n\r\n"),
            Err(ParseError::Malformed)
        );
    }

    #[test]
    fn parse_rejects_non_method_token() {
        // `/` is not a tchar, so `GE/T` is not a method at all.
        assert_eq!(
            parse(b"GE/T / HTTP/1.1\r\n\r\n"),
            Err(ParseError::Malformed)
        );
        assert_eq!(parse(b" / HTTP/1.1\r\n\r\n"), Err(ParseError::Malformed));
    }

    #[test]
    fn parse_rejects_empty_buffer() {
        assert_eq!(parse(b""), Err(ParseError::Malformed));
        assert_eq!(parse(b"\r\n\r\n"), Err(ParseError::Malformed));
    }

    #[test]
    fn parse_never_panics_on_a_prefix_of_a_real_request() {
        for end in 0..SIMPLE.len() {
            let result = parse(&SIMPLE[..end]);
            assert!(
                result.is_err(),
                "a truncated head ({end} bytes) must not parse"
            );
        }
        assert!(parse(SIMPLE).is_ok());
    }

    #[test]
    fn parse_ten_thousand_times_without_panicking() {
        // The 60 s device idle watchdog bounds this loop; parse allocates
        // nothing, so 10 000 iterations stay cheap.
        for _ in 0..10_000 {
            let request = parse(SIMPLE).expect("repeated parse");
            assert_eq!(request.path, "/api/v1/status");
            assert_eq!(request.method, Method::Get);
            assert_eq!(header(SIMPLE, "Host"), Some("192.168.1.50"));
        }
    }

    // --- real client bytes --------------------------------------------------

    #[test]
    fn parse_real_curl_get_bytes() {
        // Captured from `curl -v http://192.168.1.50/api/v1/files/rec/REC_0001.avi`.
        let bytes: &[u8] = b"GET /api/v1/files/rec/REC_0001.avi HTTP/1.1\r\n\
Host: 192.168.1.50\r\n\
User-Agent: curl/8.10.1\r\n\
Accept: */*\r\n\
\r\n";
        let request = parse(bytes).expect("curl GET parses");
        assert_eq!(request.method, Method::Get);
        assert_eq!(request.path, "/api/v1/files/rec/REC_0001.avi");
        assert_eq!(request.query, None);
        assert_eq!(request.version, Version::Http11);
        assert_eq!(header(bytes, "User-Agent"), Some("curl/8.10.1"));
        assert_eq!(header(bytes, "Content-Length"), None);
    }

    #[test]
    fn parse_real_curl_post_bytes_and_content_length() {
        // Captured from `curl -v -d x=1 http://192.168.1.50/api/v1/status`.
        let bytes: &[u8] = b"POST /api/v1/status HTTP/1.1\r\n\
Host: 192.168.1.50\r\n\
User-Agent: curl/8.10.1\r\n\
Accept: */*\r\n\
Content-Length: 3\r\n\
Content-Type: application/x-www-form-urlencoded\r\n\
\r\n\
x=1";
        let request = parse(bytes).expect("curl POST head parses; the body is not its business");
        assert_eq!(request.method, Method::Other);
        assert_eq!(request.path, "/api/v1/status");
        // Content-Length is retrievable but never interpreted as a body length.
        assert_eq!(header(bytes, "Content-Length"), Some("3"));
    }

    #[test]
    fn parse_ignores_body_bytes_after_the_head() {
        let bytes: &[u8] =
            b"GET /a HTTP/1.1\r\nContent-Length: 999999\r\n\r\nGET /b HTTP/1.1\r\n\r\n";
        let request = parse(bytes).expect("only the first head is read");
        assert_eq!(request.path, "/a");
        assert_eq!(header(bytes, "Content-Length"), Some("999999"));
    }

    // --- headers ------------------------------------------------------------

    #[test]
    fn header_first_duplicate_wins() {
        let bytes: &[u8] = b"GET / HTTP/1.1\r\nX-Dup: first\r\nX-Dup: second\r\n\r\n";
        assert_eq!(header(bytes, "X-Dup"), Some("first"));
    }

    #[test]
    fn header_name_is_case_insensitive() {
        let bytes: &[u8] = b"GET / HTTP/1.1\r\ncontent-length: 0\r\nHost: b\r\n\r\n";
        assert_eq!(header(bytes, "Content-Length"), Some("0"));
        assert_eq!(header(bytes, "CONTENT-LENGTH"), Some("0"));
        assert_eq!(header(bytes, "HOST"), Some("b"));
    }

    #[test]
    fn header_value_may_contain_colons() {
        let bytes: &[u8] = b"GET / HTTP/1.1\r\nX-Time: 12:34:56\r\n\r\n";
        assert_eq!(header(bytes, "X-Time"), Some("12:34:56"));
        let url: &[u8] = b"GET / HTTP/1.1\r\nReferer: http://board.local:80/a?b=c\r\n\r\n";
        assert_eq!(header(url, "Referer"), Some("http://board.local:80/a?b=c"));
    }

    #[test]
    fn header_trims_optional_whitespace() {
        let bytes: &[u8] = b"GET / HTTP/1.1\r\nX-Pad:\t  value \t\r\n\r\n";
        assert_eq!(header(bytes, "X-Pad"), Some("value"));
        let empty: &[u8] = b"GET / HTTP/1.1\r\nX-Empty:   \r\n\r\n";
        assert_eq!(header(empty, "X-Empty"), Some(""));
    }

    #[test]
    fn header_absent_is_none() {
        assert_eq!(header(SIMPLE, "Range"), None);
        assert_eq!(header(SIMPLE, ""), None);
        assert_eq!(header(b"GET / HTTP/1.1\r\n\r\n", "Host"), None);
    }

    #[test]
    fn header_does_not_scan_into_the_body() {
        let bytes: &[u8] = b"GET / HTTP/1.1\r\nHost: h\r\n\r\nX-Body: 1\r\n\r\n";
        assert_eq!(header(bytes, "Host"), Some("h"));
        assert_eq!(
            header(bytes, "X-Body"),
            None,
            "bytes after the blank line are a body, not headers"
        );
    }

    // --- response head ------------------------------------------------------

    #[test]
    fn write_head_ok_body() {
        let head = ResponseHead::new(Status::OK, "application/json", 42);
        let mut out = [0u8; 256];
        let written = write_head(&mut out, &head);
        assert_eq!(written, head_len(&head));
        assert_eq!(
            core::str::from_utf8(&out[..written]).unwrap(),
            "HTTP/1.1 200 OK\r\n\
Content-Type: application/json\r\n\
Content-Length: 42\r\n\
Connection: close\r\n\
\r\n"
        );
    }

    #[test]
    fn write_head_extra_headers() {
        let head = ResponseHead::new(Status::OK, "application/octet-stream", 1048576)
            .with_extra(&[("Accept-Ranges", "bytes"), ("Cache-Control", "no-store")]);
        let mut out = [0u8; 256];
        let written = write_head(&mut out, &head);
        let text = core::str::from_utf8(&out[..written]).unwrap();
        assert!(text.contains("\r\nAccept-Ranges: bytes\r\nCache-Control: no-store\r\n"));
        assert!(text.ends_with("\r\nConnection: close\r\n\r\n"));
        assert!(text.contains("Content-Length: 1048576\r\n"));
    }

    #[test]
    fn write_head_416_carries_content_range() {
        let head = ResponseHead::new(Status::RANGE_NOT_SATISFIABLE, "text/plain", 0)
            .with_extra(&[("Content-Range", "bytes */1000")]);
        let mut out = [0u8; 256];
        let written = write_head(&mut out, &head);
        let text = core::str::from_utf8(&out[..written]).unwrap();
        assert!(text.starts_with("HTTP/1.1 416 Range Not Satisfiable\r\n"));
        assert!(text.contains("\r\nContent-Range: bytes */1000\r\n"));
        assert!(text.contains("\r\nContent-Length: 0\r\n"));
    }

    #[test]
    fn write_head_truncates_without_panicking() {
        let head = ResponseHead::new(Status::OK, "application/json", 7);
        let mut out = [0u8; 12];
        let written = write_head(&mut out, &head);
        assert_eq!(written, 12);
        assert_eq!(&out[..written], b"HTTP/1.1 200");
        // An empty buffer writes nothing but still reports nothing written.
        let mut none = [0u8; 0];
        assert_eq!(write_head(&mut none, &head), 0);
        assert!(head_len(&head) > 0);
    }

    #[test]
    fn write_head_never_writes_past_out_len() {
        let head = ResponseHead::new(Status::PARTIAL_CONTENT, "application/octet-stream", 1023)
            .with_extra(&[("Content-Range", "bytes 0-1022/1023")]);
        let needed = head_len(&head);
        let mut reference = std::vec![0u8; needed];
        assert_eq!(write_head(&mut reference, &head), needed);
        for size in 0..=needed {
            let mut out = std::vec![0xa5u8; size];
            let written = write_head(&mut out, &head);
            assert_eq!(written, size.min(needed), "reported length at {size}");
            assert_eq!(&out[..written], &reference[..written], "prefix at {size}");
        }
    }

    #[test]
    fn write_head_filters_control_bytes() {
        let head = ResponseHead {
            status: 200,
            reason: "OK\r\nX-Injected: 1",
            content_type: "text/plain",
            content_length: 0,
            extra: &[("X-Evil", "a\r\n\r\nGET / HTTP/1.1")],
        };
        let mut out = [0u8; 256];
        let written = write_head(&mut out, &head);
        let text = core::str::from_utf8(&out[..written]).unwrap();
        assert_eq!(text.matches("\r\n").count(), 6, "{text:?}");
        assert!(text.starts_with("HTTP/1.1 200 OKX-Injected: 1\r\n"));
        assert!(text.contains("X-Evil: aGET / HTTP/1.1\r\n"));
    }

    // --- status -------------------------------------------------------------

    #[test]
    fn status_reason_phrases_match_the_codes() {
        assert_eq!(Status::OK.reason(), "OK");
        assert_eq!(Status::METHOD_NOT_ALLOWED.reason(), "Method Not Allowed");
        assert_eq!(Status::NOT_FOUND.code(), 404);
        assert_eq!(Status::from(416u16).reason(), "Range Not Satisfiable");
        assert_eq!(u16::from(Status::SERVICE_UNAVAILABLE), 503);
        assert_eq!(Status(404), 404u16);
        assert_eq!(reason_for(999), "Unknown");
    }
}
