//! RTSP request parsing (plan Todo 11).
//!
//! Plan contract, quoted from `.omo/plans/esp32s3-av-stream-and-sd-recording.md`
//! Todo 11 "QA — failure":
//!
//! > assert that a malformed request line, a wrong `CSeq`, an unknown method
//! > and a `Transport:` header the server does not offer each produce a
//! > correct RTSP error response (4xx) rather than a dropped connection or a
//! > panic.
//!
//! Every [`RequestError`] variant maps to **400 Bad Request**; unknown
//! *methods* are not a parse error (they dispatch to 405 in
//! [`super::server::RtspServer`]) so the request line must parse them
//! successfully first. Pure `std`, no sockets — runs as host-executed tests
//! via the `#[path]` include in `harness/tests/task11_rtsp.rs`.

/// The RTSP methods this server implements (plan Todo 11 "Do"). Anything else
/// parses fine and is answered with 405 by the dispatcher.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Method {
    Options,
    Describe,
    Setup,
    Play,
    Teardown,
    GetParameter,
    /// Parses successfully, not implemented → 405 Method Not Allowed.
    Other(String),
}

impl Method {
    fn from_token(token: &str) -> Self {
        match token {
            "OPTIONS" => Self::Options,
            "DESCRIBE" => Self::Describe,
            "SETUP" => Self::Setup,
            "PLAY" => Self::Play,
            "TEARDOWN" => Self::Teardown,
            "GET_PARAMETER" => Self::GetParameter,
            other => Self::Other(other.to_string()),
        }
    }

    /// Token as it appeared on the request line (used in logs and for the
    /// 405 `Allow` interplay).
    pub fn as_str(&self) -> &str {
        match self {
            Self::Options => "OPTIONS",
            Self::Describe => "DESCRIBE",
            Self::Setup => "SETUP",
            Self::Play => "PLAY",
            Self::Teardown => "TEARDOWN",
            Self::GetParameter => "GET_PARAMETER",
            Self::Other(other) => other.as_str(),
        }
    }
}

/// A parsed RTSP request. Header names are lower-cased at parse time so
/// [`Request::header`] lookups are case-insensitive (RFC 2326 field names
/// are case-insensitive; clients differ in practice). `PartialEq` exists so
/// the parse tests can assert full `Err`/`Ok` equality.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub method: Method,
    /// Request URI exactly as received (absolute `rtsp://…` or a path).
    pub uri: String,
    /// `CSeq` value, validated as digits and echoed verbatim in the response.
    pub cseq: String,
    headers: Vec<(String, String)>,
}

impl Request {
    /// Case-insensitive header lookup; value is trimmed.
    pub fn header(&self, name: &str) -> Option<&str> {
        let want = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(k, _)| *k == want)
            .map(|(_, v)| v.as_str())
    }
}

/// Why a byte stream failed to parse. All three variants are answered with
/// **400 Bad Request** (plan QA — failure row).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestError {
    /// First non-empty line is not `<METHOD> <URI> RTSP/x.y`.
    MalformedRequestLine,
    /// No `CSeq` header at all.
    MissingCSeq,
    /// `CSeq` present but not a bare non-negative integer.
    InvalidCSeq,
}

impl std::fmt::Display for RequestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::MalformedRequestLine => "malformed request line",
            Self::MissingCSeq => "missing CSeq header",
            Self::InvalidCSeq => "CSeq is not an unsigned integer",
        })
    }
}

impl std::error::Error for RequestError {}

/// Parse one complete RTSP message (headers plus optional body — the body is
/// not interpreted; it only has to have been consumed off the wire by the
/// caller so the stream stays in sync).
///
/// Robustness measures required for real-client interop:
/// - empty lines **before** the request line are skipped (RFC 2326 §9.1
///   explicitly allows a CRLF ahead of the Request-Line);
/// - header lines without a `:` are skipped rather than failing the whole
///   request (only the request line and `CSeq` are strict).
pub fn parse(raw: &[u8]) -> Result<Request, RequestError> {
    let text = std::str::from_utf8(raw).map_err(|_| RequestError::MalformedRequestLine)?;
    // Strip the RFC 2326 §9.1 robustness CRLFs BEFORE splitting off the
    // header section, otherwise leading empty lines would truncate the head
    // at the very first CRLF pair.
    let text = text.trim_start_matches("\r\n");
    let head = text.split("\r\n\r\n").next().unwrap_or(text);
    let mut lines = head.split("\r\n");

    // RFC 2326 §9.1 robustness: tolerate leading empty lines.
    let request_line = loop {
        let line = lines.next().ok_or(RequestError::MalformedRequestLine)?;
        if !line.trim().is_empty() {
            break line;
        }
    };

    let tokens: Vec<&str> = request_line.split_whitespace().collect();
    if tokens.len() != 3 {
        return Err(RequestError::MalformedRequestLine);
    }
    if !tokens[2].starts_with("RTSP/") {
        return Err(RequestError::MalformedRequestLine);
    }
    if tokens[1].is_empty() {
        return Err(RequestError::MalformedRequestLine);
    }

    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
        }
    }

    let cseq = headers
        .iter()
        .find(|(k, _)| k == "cseq")
        .map(|(_, v)| v.clone())
        .ok_or(RequestError::MissingCSeq)?;
    if cseq.is_empty() || !cseq.bytes().all(|b| b.is_ascii_digit()) {
        return Err(RequestError::InvalidCSeq);
    }

    Ok(Request {
        method: Method::from_token(tokens[0]),
        uri: tokens[1].to_string(),
        cseq,
        headers,
    })
}

/// Best-effort `CSeq` recovery for a message that failed [`parse`] — error
/// responses still carry the client's `CSeq` whenever the header section
/// itself was readable (VLC/ffprobe retry logic expects the echo even on 400).
pub fn best_effort_cseq(raw: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(raw).ok()?;
    let text = text.trim_start_matches("\r\n");
    let head = text.split("\r\n\r\n").next().unwrap_or(text);
    for line in head.split("\r\n") {
        if let Some((name, value)) = line.split_once(':') {
            if name.trim().eq_ignore_ascii_case("cseq") {
                return Some(value.trim().to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(method: &str, uri: &str, cseq: Option<&str>) -> Vec<u8> {
        let mut s = format!("{method} {uri} RTSP/1.0\r\n");
        if let Some(c) = cseq {
            s.push_str(&format!("CSeq: {c}\r\n"));
        }
        s.push_str("\r\n");
        s.into_bytes()
    }

    #[test]
    fn parses_every_implemented_method() {
        for (token, want) in [
            ("OPTIONS", Method::Options),
            ("DESCRIBE", Method::Describe),
            ("SETUP", Method::Setup),
            ("PLAY", Method::Play),
            ("TEARDOWN", Method::Teardown),
            ("GET_PARAMETER", Method::GetParameter),
        ] {
            let req = parse(&raw(token, "rtsp://192.168.1.5:554/mjpeg/1", Some("1")))
                .unwrap_or_else(|e| panic!("{token} failed: {e}"));
            assert_eq!(req.method, want, "{token}");
            assert_eq!(req.uri, "rtsp://192.168.1.5:554/mjpeg/1");
            assert_eq!(req.cseq, "1");
        }
    }

    #[test]
    fn unknown_method_parses_so_dispatch_can_answer_405() {
        let req = parse(&raw("PAUSE", "rtsp://h/mjpeg/1", Some("3"))).unwrap();
        assert_eq!(req.method, Method::Other("PAUSE".to_string()));
        assert_eq!(req.method.as_str(), "PAUSE");
    }

    #[test]
    fn header_lookup_is_case_insensitive_and_trimmed() {
        let mut s = String::from("OPTIONS rtsp://h/mjpeg/1 RTSP/1.0\r\n");
        s.push_str("CSeq: 7\r\n");
        s.push_str("Transport:  RTP/AVP/TCP;unicast;interleaved=0-1 \r\n");
        s.push_str("\r\n");
        let req = parse(s.as_bytes()).unwrap();
        assert_eq!(req.header("cseq"), Some("7"));
        assert_eq!(req.header("CSeq"), Some("7"));
        assert_eq!(
            req.header("transport"),
            Some("RTP/AVP/TCP;unicast;interleaved=0-1")
        );
        assert_eq!(req.header("accept"), None);
    }

    #[test]
    fn malformed_request_lines_are_400_class_errors() {
        // Not three tokens.
        assert_eq!(
            parse(b"garbage\r\n\r\n"),
            Err(RequestError::MalformedRequestLine)
        );
        // Missing version.
        assert_eq!(
            parse(b"OPTIONS /mjpeg/1\r\n\r\n"),
            Err(RequestError::MalformedRequestLine)
        );
        // Wrong protocol family.
        assert_eq!(
            parse(b"OPTIONS /mjpeg/1 HTTP/1.1\r\nCSeq: 1\r\n\r\n"),
            Err(RequestError::MalformedRequestLine)
        );
        // Empty stream.
        assert_eq!(parse(b""), Err(RequestError::MalformedRequestLine));
        // Non-UTF-8 bytes in the header block.
        assert_eq!(
            parse(&[0xFF, 0xFE, b'\r', b'\n', b'\r', b'\n']),
            Err(RequestError::MalformedRequestLine)
        );
    }

    #[test]
    fn wrong_cseq_is_a_400_class_error() {
        assert_eq!(
            parse(&raw("OPTIONS", "rtsp://h/mjpeg/1", None)),
            Err(RequestError::MissingCSeq)
        );
        assert_eq!(
            parse(&raw("OPTIONS", "rtsp://h/mjpeg/1", Some("abc"))),
            Err(RequestError::InvalidCSeq)
        );
        assert_eq!(
            parse(&raw("OPTIONS", "rtsp://h/mjpeg/1", Some("5.0"))),
            Err(RequestError::InvalidCSeq)
        );
        assert_eq!(
            parse(&raw("OPTIONS", "rtsp://h/mjpeg/1", Some(""))),
            Err(RequestError::InvalidCSeq)
        );
        // Well-formed values still parse, including zero.
        assert_eq!(
            parse(&raw("OPTIONS", "rtsp://h/mjpeg/1", Some("0"))).unwrap().cseq,
            "0"
        );
    }

    #[test]
    fn leading_empty_lines_are_tolerated_per_rfc2326_9_1() {
        let mut s = String::from("\r\nOPTIONS rtsp://h/mjpeg/1 RTSP/1.0\r\nCSeq: 2\r\n\r\n");
        let req = parse(s.as_bytes()).unwrap();
        assert_eq!(req.method, Method::Options);
        s.clear();
        s.push_str("\r\n\r\nDESCRIBE rtsp://h/mjpeg/1 RTSP/1.0\r\nCSeq: 3\r\n\r\n");
        assert_eq!(parse(s.as_bytes()).unwrap().method, Method::Describe);
    }

    #[test]
    fn request_body_bytes_are_ignored_but_must_not_break_header_parsing() {
        // GET_PARAMETER keepalive with a text/parameters body (VLC shape).
        let s = "GET_PARAMETER rtsp://h/mjpeg/1 RTSP/1.0\r\n\
                 CSeq: 9\r\n\
                 Session: esp-0001\r\n\
                 Content-Length: 9\r\n\
                 \r\n\
                 \r\ny=0\r\n";
        let req = parse(s.as_bytes()).unwrap();
        assert_eq!(req.header("session"), Some("esp-0001"));
        assert_eq!(req.header("content-length"), Some("9"));
    }

    #[test]
    fn best_effort_cseq_recovers_from_a_broken_request_line() {
        assert_eq!(
            best_effort_cseq(b"totally broken\r\nCSeq: 42\r\n\r\n"),
            Some("42".to_string())
        );
        assert_eq!(best_effort_cseq(b"totally broken\r\n\r\n"), None);
        assert_eq!(
            best_effort_cseq(b"OPTIONS rtsp://h/x RTSP/1.0\r\nCSeq: 7\r\n\r\n"),
            Some("7".to_string())
        );
    }
}
