//! Host-only end-to-end RTSP client harness (plan Todo 17).
//!
//! This target contains a small, dependency-free RTSP client (plain
//! `std::net`) that drives the firmware's real RTSP server over TCP
//! interleaved transport:
//!
//! `OPTIONS` → `DESCRIBE` (SDP) → `SETUP` (per track) → `PLAY` → read
//! `$`-interleaved RTP frames → `TEARDOWN`.
//!
//! # Running against a live board
//!
//! ```text
//! # From /workspaces/rust/harness:
//! ESP32_RTSP_URL=rtsp://<board-ip>:554/mjpeg/1 \
//!     cargo test --test e2e_rtsp -- --nocapture
//! ```
//!
//! When `ESP32_RTSP_URL` is unset or empty the live test prints a `SKIP`
//! message and returns successfully, so `cargo test` stays green on a machine
//! with no hardware. This env gate is deliberate: `#[ignore]` alone would let
//! a bare `cargo test` (or a CI that forgets `--ignored`) silently omit the
//! check without saying why.
//!
//! # Execution is currently BLOCKED on real hardware
//!
//! A live run cannot complete on the connected board today: the camera
//! returns no frames (`esp_camera_fb_get returned NULL` on every grab), so
//! the server has nothing to send, and the SD card mount fails
//! (`sdmmc_card_init` → `ESP_ERR_NOT_SUPPORTED`), so the recording half of
//! the acceptance run is unavailable too. The client, the thresholds, the
//! opt-in gate and the hardware-free negative paths below are complete and
//! exercised; only the on-board 10 s stream measurement is blocked.
//!
//! # What is asserted (when live)
//!
//! - SDP contains the plan-locked lines `a=rtpmap:0 PCMU/8000` and
//!   `a=rtpmap:26 JPEG/90000`.
//! - Over 10 s of TCP-interleaved RTP: ≥ 90 video frames (RTP marker-bit
//!   boundaries) and ≥ 450 audio frames (PCMU packets).
//! - A/V start delta ≤ 250 ms, measured from the arrival instant of each
//!   track's first RTP packet.
//!
//! # Negative paths (run WITHOUT hardware, no env var needed)
//!
//! - A closed port fails with a clear `connect` diagnostic in bounded time.
//! - An HTTP/1.1 server serving unrelated content is rejected with a
//!   non-RTSP diagnostic in bounded time.
//!
//! # Deviation from the plan sketch
//!
//! The plan suggested `retina` for depacketization. This harness implements
//! the wire flow with `std` only and does not touch `retina` at all, so the
//! tests remain buildable from the existing dependency set with no new
//! crates.

use std::fmt;
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

// ---- Acceptance thresholds (the plan's numeric contract) ------------------

/// Length of the live capture window.
const CAPTURE_SECS: u64 = 10;
/// Required video frames in [`CAPTURE_SECS`]: 9 fps × 10 s.
const MIN_VIDEO_FRAMES: u32 = 90;
/// Required audio frames in [`CAPTURE_SECS`]: 50 PCMU packets/s × 10 s.
const MIN_AUDIO_FRAMES: u32 = 450;
/// Allowed start skew between the first video and first audio RTP packet.
const MAX_AV_START_DELTA_MS: u128 = 250;
/// Bound on every handshake read and on the closed-port connect.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
/// Bound on a connect attempt (service is local/LAN).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
/// Hard ceiling for a negative-path test to finish.
const NEGATIVE_TEST_BOUND: Duration = Duration::from_secs(10);
/// Refuse to buffer an unbounded RTSP message.
const MAX_MESSAGE_BYTES: usize = 4 * 1024 * 1024;

/// The two SDP codec strings the plan locks (Todo 11 / Todo 15).
const SDP_AUDIO_RTPMAP: &str = "a=rtpmap:0 PCMU/8000";
const SDP_VIDEO_RTPMAP: &str = "a=rtpmap:26 JPEG/90000";

// ---- Errors ---------------------------------------------------------------

#[derive(Debug)]
enum ClientError {
    /// Could not resolve or open the TCP connection.
    Connect(String),
    /// Socket or stream I/O failed.
    Io(String),
    /// Peer spoke something that is not valid RTSP / the flow was rejected.
    Protocol(String),
    /// A bounded wait expired without the expected data.
    Timeout(String),
}

impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ClientError::Connect(m) => write!(f, "connection failed: {m}"),
            ClientError::Io(m) => write!(f, "I/O error: {m}"),
            ClientError::Protocol(m) => write!(f, "protocol error: {m}"),
            ClientError::Timeout(m) => write!(f, "timeout: {m}"),
        }
    }
}

impl std::error::Error for ClientError {}

// ---- URL ------------------------------------------------------------------

struct RtspUrl {
    host: String,
    port: u16,
    path: String,
}

impl RtspUrl {
    fn parse(url: &str) -> Result<Self, ClientError> {
        let rest = url
            .strip_prefix("rtsp://")
            .ok_or_else(|| ClientError::Protocol(format!("not an rtsp:// URL: {url:?}")))?;
        let (authority, path) = match rest.find('/') {
            Some(idx) => (&rest[..idx], &rest[idx..]),
            None => (rest, "/"),
        };
        if authority.is_empty() {
            return Err(ClientError::Protocol(format!(
                "rtsp:// URL has no host: {url:?}"
            )));
        }
        let (host, port) = match authority.rsplit_once(':') {
            Some((h, p)) => match p.parse::<u16>() {
                Ok(port) if !h.is_empty() => (h.to_string(), port),
                _ => {
                    return Err(ClientError::Protocol(format!(
                        "invalid port in rtsp:// URL: {url:?}"
                    )))
                }
            },
            None => (authority.to_string(), 554),
        };
        Ok(Self {
            host,
            port,
            path: path.to_string(),
        })
    }

    fn uri(&self) -> String {
        format!("rtsp://{}:{}{}", self.host, self.port, self.path)
    }
}

// ---- RTSP wire ------------------------------------------------------------

#[derive(Debug)]
struct Response {
    status: u16,
    reason: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Response {
    /// Case-insensitive header lookup; returns the first value.
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

enum Event {
    Response(Response),
    Interleaved { channel: u8, payload: Vec<u8> },
}

struct Conn {
    stream: TcpStream,
    buf: Vec<u8>,
}

impl Conn {
    /// Return the next complete event (RTSP response or interleaved RTP/RTCP
    /// frame), waiting no later than `deadline`. Handles the two stream
    /// grammars of RFC 2326 on one connection: text responses delimited by
    /// CRLFCRLF + `Content-Length`, and `$` frames delimited by a 16-bit
    /// length.
    fn next_event(&mut self, deadline: Instant) -> Result<Event, ClientError> {
        let mut tmp = [0u8; 16 * 1024];
        loop {
            if !self.buf.is_empty() {
                if self.buf[0] == b'$' {
                    if self.buf.len() >= 4 {
                        let channel = self.buf[1];
                        let len = u16::from_be_bytes([self.buf[2], self.buf[3]]) as usize;
                        if self.buf.len() >= 4 + len {
                            let payload = self.buf[4..4 + len].to_vec();
                            self.buf.drain(..4 + len);
                            return Ok(Event::Interleaved { channel, payload });
                        }
                    }
                } else if let Some(header_len) = find_subsequence(&self.buf, b"\r\n\r\n") {
                    let body_start = header_len + 4;
                    let content_length =
                        parse_content_length(&self.buf[..header_len]).unwrap_or(0);
                    if self.buf.len() >= body_start + content_length {
                        let head = self.buf[..header_len].to_vec();
                        let body = self.buf[body_start..body_start + content_length].to_vec();
                        self.buf.drain(..body_start + content_length);
                        return Ok(Event::Response(parse_response(&head, body)?));
                    }
                }

                if self.buf.len() > MAX_MESSAGE_BYTES {
                    return Err(ClientError::Protocol(format!(
                        "RTSP message exceeded {MAX_MESSAGE_BYTES} bytes without a complete frame"
                    )));
                }
            }

            let now = Instant::now();
            if now >= deadline {
                return Err(ClientError::Timeout(
                    "timed out waiting for the next RTSP response or interleaved frame".into(),
                ));
            }
            let remaining = deadline.saturating_duration_since(now);
            let read_timeout = remaining
                .min(Duration::from_millis(2_000))
                .max(Duration::from_millis(1));
            self.stream
                .set_read_timeout(Some(read_timeout))
                .map_err(|e| ClientError::Io(format!("set_read_timeout: {e}")))?;

            match self.stream.read(&mut tmp) {
                Ok(0) => {
                    return Err(ClientError::Io(
                        "connection closed by peer before a complete frame arrived".into(),
                    ))
                }
                Ok(n) => self.buf.extend_from_slice(&tmp[..n]),
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) =>
                {
                    // Loop re-checks the deadline; a short poll avoids blocking
                    // past the capture window on a quiet stream.
                    continue;
                }
                Err(e) => return Err(ClientError::Io(format!("read from RTSP socket: {e}"))),
            }
        }
    }
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn parse_content_length(head: &[u8]) -> Option<usize> {
    let text = String::from_utf8_lossy(head);
    for line in text.split("\r\n") {
        if let Some((name, value)) = line.split_once(':') {
            if name.trim().eq_ignore_ascii_case("content-length") {
                return value.trim().parse().ok();
            }
        }
    }
    None
}

fn parse_response(head: &[u8], body: Vec<u8>) -> Result<Response, ClientError> {
    let text = String::from_utf8_lossy(head);
    let mut lines = text.split("\r\n");
    let status_line = lines.next().unwrap_or("");
    let mut parts = status_line.splitn(3, ' ');
    let protocol = parts.next().unwrap_or("");
    if !protocol.starts_with("RTSP/") {
        return Err(ClientError::Protocol(format!(
            "expected an RTSP status line, got {status_line:?} (is this an RTSP server?)"
        )));
    }
    let status: u16 = parts
        .next()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| ClientError::Protocol(format!("malformed RTSP status line: {status_line:?}")))?;
    let reason = parts.next().unwrap_or("").to_string();

    let mut headers = Vec::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
        }
    }
    Ok(Response {
        status,
        reason,
        headers,
        body,
    })
}

fn expect_ok(resp: &Response, what: &str) -> Result<(), ClientError> {
    if resp.status == 200 {
        return Ok(());
    }
    let body = String::from_utf8_lossy(&resp.body);
    let detail = if body.is_empty() {
        String::new()
    } else {
        format!(" — body: {body:?}")
    };
    Err(ClientError::Protocol(format!(
        "{what} rejected: {} {}{detail}",
        resp.status, resp.reason
    )))
}

struct Client {
    conn: Conn,
    url: RtspUrl,
    cseq: u32,
    io_timeout: Duration,
}

impl Client {
    fn connect(
        url: &str,
        connect_timeout: Duration,
        io_timeout: Duration,
    ) -> Result<Self, ClientError> {
        let url = RtspUrl::parse(url)?;
        let target = (url.host.as_str(), url.port);
        let addr = target
            .to_socket_addrs()
            .map_err(|e| ClientError::Connect(format!("resolve {}:{}: {e}", url.host, url.port)))?
            .next()
            .ok_or_else(|| {
                ClientError::Connect(format!("resolve {}:{}: no addresses", url.host, url.port))
            })?;
        let stream = TcpStream::connect_timeout(&addr, connect_timeout)
            .map_err(|e| ClientError::Connect(format!("connect to {addr}: {e}")))?;
        let _ = stream.set_nodelay(true);
        Ok(Self {
            conn: Conn {
                stream,
                buf: Vec::new(),
            },
            url,
            cseq: 0,
            io_timeout,
        })
    }

    fn request(
        &mut self,
        method: &str,
        uri: &str,
        headers: &[(&str, &str)],
    ) -> Result<Response, ClientError> {
        self.cseq += 1;
        let mut req = format!("{method} {uri} RTSP/1.0\r\nCSeq: {}\r\n", self.cseq);
        for (name, value) in headers {
            req.push_str(&format!("{name}: {value}\r\n"));
        }
        req.push_str("\r\n");
        self.conn
            .stream
            .write_all(req.as_bytes())
            .map_err(|e| ClientError::Io(format!("send {method}: {e}")))?;
        let _ = self.conn.stream.flush();

        let deadline = Instant::now() + self.io_timeout;
        loop {
            match self.conn.next_event(deadline)? {
                Event::Response(resp) => return Ok(resp),
                // Stray media before a response is unexpected during the
                // handshake, but harmless: keep reading for the response.
                Event::Interleaved { .. } => continue,
            }
        }
    }

    fn next_event(&mut self, deadline: Instant) -> Result<Event, ClientError> {
        self.conn.next_event(deadline)
    }
}

// ---- SDP / Transport parsing ----------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TrackKind {
    Video,
    Audio,
}

#[derive(Debug)]
struct Track {
    kind: TrackKind,
    control: String,
    payload_type: u8,
}

fn validate_sdp(sdp: &str) -> Result<(), ClientError> {
    let has = |wanted: &str| sdp.lines().any(|line| line.trim_end() == wanted);
    if !has(SDP_VIDEO_RTPMAP) {
        return Err(ClientError::Protocol(format!(
            "SDP is missing the locked video line {SDP_VIDEO_RTPMAP:?}\n--- SDP ---\n{sdp}"
        )));
    }
    if !has(SDP_AUDIO_RTPMAP) {
        return Err(ClientError::Protocol(format!(
            "SDP is missing the locked audio line {SDP_AUDIO_RTPMAP:?}\n--- SDP ---\n{sdp}"
        )));
    }
    Ok(())
}

fn resolve_control(base: &RtspUrl, control: &str) -> String {
    if control.starts_with("rtsp://") {
        control.to_string()
    } else if control.starts_with('/') {
        format!("rtsp://{}:{}{}", base.host, base.port, control)
    } else {
        format!("{}/{}", base.uri().trim_end_matches('/'), control)
    }
}

fn parse_tracks(sdp: &str, base: &RtspUrl) -> Result<Vec<Track>, ClientError> {
    let mut tracks = Vec::new();
    let mut current: Option<(TrackKind, u8)> = None;

    for line in sdp.lines() {
        if let Some(rest) = line.strip_prefix("m=") {
            let fields: Vec<&str> = rest.split_whitespace().collect();
            let kind = match fields.first().copied() {
                Some("video") => TrackKind::Video,
                Some("audio") => TrackKind::Audio,
                _ => {
                    current = None;
                    continue;
                }
            };
            let payload_type = fields
                .last()
                .and_then(|s| s.parse::<u8>().ok())
                .ok_or_else(|| {
                    ClientError::Protocol(format!("cannot parse payload type from SDP line {line:?}"))
                })?;
            current = Some((kind, payload_type));
        } else if let Some(control) = line.strip_prefix("a=control:") {
            if let Some((kind, payload_type)) = current.take() {
                let control = control.trim();
                if control.is_empty() || control == "*" {
                    continue;
                }
                tracks.push(Track {
                    kind,
                    control: resolve_control(base, control),
                    payload_type,
                });
            }
        }
    }

    if !tracks.iter().any(|t| t.kind == TrackKind::Video) {
        return Err(ClientError::Protocol(
            "SDP has no video track with an a=control URL".into(),
        ));
    }
    if !tracks.iter().any(|t| t.kind == TrackKind::Audio) {
        return Err(ClientError::Protocol(
            "SDP has no audio track with an a=control URL".into(),
        ));
    }
    Ok(tracks)
}

/// Parse `interleaved=a-b` out of a `Transport:` response value.
fn parse_interleaved(transport: &str) -> Option<(u8, u8)> {
    for part in transport.split(';') {
        if let Some((name, value)) = part.split_once('=') {
            if name.trim().eq_ignore_ascii_case("interleaved") {
                let (rtp, rtcp) = value.split_once('-')?;
                return Some((rtp.trim().parse().ok()?, rtcp.trim().parse().ok()?));
            }
        }
    }
    None
}

// ---- Live capture ---------------------------------------------------------

#[derive(Debug, Default)]
struct StreamStats {
    video_frames: u32,
    audio_frames: u32,
    /// Arrival instant of the first valid RTP packet on each track.
    video_start: Option<Instant>,
    audio_start: Option<Instant>,
}

fn run_live(url: &str) -> Result<StreamStats, ClientError> {
    let mut client = Client::connect(url, CONNECT_TIMEOUT, HANDSHAKE_TIMEOUT)?;
    let base_uri = client.url.uri();

    // OPTIONS
    let resp = client.request("OPTIONS", &base_uri, &[])?;
    expect_ok(&resp, "OPTIONS")?;
    println!(
        "OPTIONS ok (Public: {:?})",
        resp.header("public").unwrap_or("<none>")
    );

    // DESCRIBE
    let resp = client.request("DESCRIBE", &base_uri, &[("Accept", "application/sdp")])?;
    expect_ok(&resp, "DESCRIBE")?;
    let sdp = String::from_utf8_lossy(&resp.body).into_owned();
    validate_sdp(&sdp)?;
    println!("DESCRIBE SDP carries both locked rtpmap lines");
    let tracks = parse_tracks(&sdp, &client.url)?;

    // SETUP each track over TCP interleaved. The server reassigns channels per
    // track (video 0/1, audio 2/3), so read them back from the response.
    let mut session: Option<String> = None;
    let mut video_channel: Option<u8> = None;
    let mut audio_channel: Option<u8> = None;
    for track in &tracks {
        let resp = client.request(
            "SETUP",
            &track.control,
            &[("Transport", "RTP/AVP/TCP;unicast;interleaved=0-1")],
        )?;
        expect_ok(&resp, &format!("SETUP {}", track.control))?;

        let id = resp.header("session").ok_or_else(|| {
            ClientError::Protocol("SETUP response is missing the Session header".into())
        })?;
        session = Some(id.split(';').next().unwrap_or(id).trim().to_string());

        let transport = resp.header("transport").ok_or_else(|| {
            ClientError::Protocol("SETUP response is missing the Transport header".into())
        })?;
        let (rtp, _rtcp) = parse_interleaved(transport).ok_or_else(|| {
            ClientError::Protocol(format!(
                "SETUP Transport does not carry interleaved=a-b: {transport:?}"
            ))
        })?;
        match track.kind {
            TrackKind::Video => {
                assert_eq!(
                    track.payload_type, 26,
                    "video track payload type must be JPEG/26, SDP said {}",
                    track.payload_type
                );
                video_channel = Some(rtp);
            }
            TrackKind::Audio => {
                assert_eq!(
                    track.payload_type, 0,
                    "audio track payload type must be PCMU/0, SDP said {}",
                    track.payload_type
                );
                audio_channel = Some(rtp);
            }
        }
        println!(
            "SETUP {:?} ok (rtp channel {rtp}, session {})",
            track.kind,
            session.as_deref().unwrap_or("?")
        );
    }
    let session = session.ok_or_else(|| ClientError::Protocol("no SETUP session id".into()))?;
    let video_channel =
        video_channel.ok_or_else(|| ClientError::Protocol("video track was not set up".into()))?;
    let audio_channel =
        audio_channel.ok_or_else(|| ClientError::Protocol("audio track was not set up".into()))?;
    if video_channel == audio_channel {
        return Err(ClientError::Protocol(format!(
            "server assigned the same RTP channel ({video_channel}) to both tracks"
        )));
    }

    // PLAY
    let resp = client.request(
        "PLAY",
        &base_uri,
        &[("Session", session.as_str()), ("Range", "npt=0.000-")],
    )?;
    expect_ok(&resp, "PLAY")?;
    println!("PLAY ok — capturing for {CAPTURE_SECS} s");

    // Capture interleaved RTP for the window.
    let mut stats = StreamStats::default();
    let deadline = Instant::now() + Duration::from_secs(CAPTURE_SECS);
    loop {
        if Instant::now() >= deadline {
            break;
        }
        match client.next_event(deadline) {
            Ok(Event::Interleaved { channel, payload }) => {
                let now = Instant::now();
                if payload.len() >= 12 && payload[0] >> 6 == 2 {
                    if channel == video_channel {
                        if stats.video_start.is_none() {
                            stats.video_start = Some(now);
                        }
                        // RFC 3550 marker bit: end of a video frame.
                        if payload[1] & 0x80 != 0 {
                            stats.video_frames += 1;
                        }
                    } else if channel == audio_channel {
                        if stats.audio_start.is_none() {
                            stats.audio_start = Some(now);
                        }
                        stats.audio_frames += 1;
                    }
                    // Other channels are RTCP (SR/RR) — not frames.
                }
            }
            Ok(Event::Response(resp)) => {
                // A mid-session response is not expected here; log and keep
                // the capture going.
                println!(
                    "note: unsolicited in-session response {} {}",
                    resp.status, resp.reason
                );
            }
            Err(ClientError::Timeout(_)) => break,
            Err(e) => {
                let _ = client.request("TEARDOWN", &base_uri, &[("Session", session.as_str())]);
                return Err(e);
            }
        }
    }

    let _ = client.request("TEARDOWN", &base_uri, &[("Session", session.as_str())]);
    Ok(stats)
}

// ---- Tests ----------------------------------------------------------------

#[test]
fn live_rtsp_stream_meets_frame_and_av_thresholds() {
    let url = match std::env::var("ESP32_RTSP_URL") {
        Ok(value) if !value.trim().is_empty() => value,
        _ => {
            println!(
                "SKIP e2e_rtsp: ESP32_RTSP_URL is not set (or empty) — this live test needs a \
                 running board.\n\
                 Run it with:\n  ESP32_RTSP_URL=rtsp://<board-ip>:554/mjpeg/1 \\\n      \
                 cargo test --test e2e_rtsp -- --nocapture\n\
                 Note: live execution is currently blocked because the board's camera returns \
                 no frames and its SD mount fails; see the file header."
            );
            return;
        }
    };

    let stats = run_live(&url)
        .unwrap_or_else(|e| panic!("live RTSP session against {url} failed: {e}"));

    let av_delta = match (stats.video_start, stats.audio_start) {
        (Some(v), Some(a)) => {
            if a >= v {
                a.duration_since(v)
            } else {
                v.duration_since(a)
            }
        }
        (None, None) => panic!(
            "no RTP media received in {CAPTURE_SECS} s: 0 video frames and 0 audio frames — \
             is the board actually producing a camera + audio stream?"
        ),
        (None, Some(_)) => panic!(
            "audio arrived but NO video RTP packets were received in {CAPTURE_SECS} s \
             (camera stream missing)"
        ),
        (Some(_), None) => panic!(
            "video arrived but NO audio RTP packets were received in {CAPTURE_SECS} s \
             (audio stream missing)"
        ),
    };

    println!(
        "e2e result: video_frames={} (min {MIN_VIDEO_FRAMES}), audio_frames={} (min \
         {MIN_AUDIO_FRAMES}), A/V start delta={} ms (max {MAX_AV_START_DELTA_MS} ms)",
        stats.video_frames,
        stats.audio_frames,
        av_delta.as_millis()
    );

    assert!(
        stats.video_frames >= MIN_VIDEO_FRAMES,
        "received only {} video frames in {CAPTURE_SECS} s; IS-10 requires >= {MIN_VIDEO_FRAMES}",
        stats.video_frames
    );
    assert!(
        stats.audio_frames >= MIN_AUDIO_FRAMES,
        "received only {} audio frames in {CAPTURE_SECS} s; IS-10 requires >= {MIN_AUDIO_FRAMES}",
        stats.audio_frames
    );
    assert!(
        av_delta.as_millis() <= MAX_AV_START_DELTA_MS,
        "A/V start delta was {} ms; the contract allows <= {MAX_AV_START_DELTA_MS} ms",
        av_delta.as_millis()
    );
}

#[test]
fn closed_port_fails_cleanly_and_promptly() {
    let started = Instant::now();
    let error = match Client::connect(
        "rtsp://127.0.0.1:1/mjpeg/1",
        Duration::from_secs(2),
        Duration::from_secs(2),
    ) {
        Ok(_) => panic!("connect to the closed port 127.0.0.1:1 unexpectedly succeeded"),
        Err(e) => e,
    };
    let elapsed = started.elapsed();
    let message = error.to_string();

    assert!(
        message.contains("connect"),
        "closed-port diagnostic should name the failed connect, got: {message}"
    );
    assert!(
        elapsed < NEGATIVE_TEST_BOUND,
        "closed-port failure must be bounded, took {elapsed:?}"
    );
    println!("closed-port diagnostic after {elapsed:?}: {message}");
}

#[test]
fn http_content_is_rejected_with_a_clear_diagnostic() {
    // A local HTTP-looking server serving unrelated content: the client must
    // recognize it is not RTSP and fail fast with a diagnostic, not hang.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake HTTP listener");
    let addr = listener.local_addr().expect("fake listener local_addr");
    let handle = std::thread::spawn(move || {
        if let Ok((mut socket, _)) = listener.accept() {
            let mut request = [0u8; 2048];
            let _ = socket.read(&mut request);
            let body = b"<html><body>not an RTSP server</body></html>";
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\n\r\n",
                body.len()
            );
            let _ = socket.write_all(response.as_bytes());
            let _ = socket.write_all(body);
        }
    });

    let url = format!("rtsp://{addr}/mjpeg/1");
    let started = Instant::now();
    let mut client = Client::connect(&url, Duration::from_secs(2), Duration::from_secs(2))
        .expect("connect to the fake HTTP listener");
    let error = client
        .request("OPTIONS", &url, &[])
        .expect_err("an HTTP/1.1 200 response must NOT be accepted as RTSP");
    let elapsed = started.elapsed();
    let message = error.to_string();
    handle.join().ok();

    assert!(
        message.contains("RTSP") || message.contains("HTTP"),
        "wrong-content diagnostic should identify the non-RTSP response, got: {message}"
    );
    assert!(
        elapsed < NEGATIVE_TEST_BOUND,
        "wrong-content failure must be bounded, took {elapsed:?}"
    );
    println!("wrong-content diagnostic after {elapsed:?}: {message}");
}
