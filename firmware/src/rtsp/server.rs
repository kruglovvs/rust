//! RTSP session dispatch (plan Todo 11) — `OPTIONS`, `DESCRIBE`, `SETUP`,
//! `PLAY`, `TEARDOWN` (+ `GET_PARAMETER`), single-session admission, and the
//! per-track send interface Todo 12 consumes.
//!
//! Plan contract, quoted from `.omo/plans/esp32s3-av-stream-and-sd-recording.md`
//! Todo 11 "Do" / "Acceptance":
//!
//! > add `firmware/src/rtsp/` implementing `OPTIONS`, `DESCRIBE`, `SETUP`,
//! > `PLAY`, `TEARDOWN` (and `GET_PARAMETER` if needed), SDP generation, plus
//! > `Transport:` header parsing for both TCP-interleaved and UDP. Single
//! > session at a time; a second client is rejected cleanly rather than
//! > corrupting the first.
//! > [...] A second concurrent client is refused without disturbing the first.
//!
//! Status-code contract (asserted by the host matrix in this file's tests and
//! by `harness/src/task11_errors.rs` → `.omo/evidence/task-11/error-responses.txt`):
//!
//! | Trigger | Status |
//! |---|---|
//! | malformed request line / missing / non-numeric `CSeq` | 400 |
//! | `DESCRIBE` on a path other than [`STREAM_PATH`] | 404 |
//! | `SETUP` for an unknown track | 404 |
//! | `DESCRIBE` `Accept:` present but no `application/sdp` | 406 |
//! | unknown method | 405 (+ `Allow`) |
//! | no `Transport:` alternative the server offers | 461 |
//! | missing `Session:` on PLAY/TEARDOWN/GET_PARAMETER | 400 |
//! | unknown `Session:` id | 454 |
//! | second client while a session is active, or a non-owner connection | 453 |
//!
//! **453 Not Enough Bandwidth** is the RTSP-defined "cannot support another
//! session" refusal; the critical property (plan: "refused without disturbing
//! the first") is that the refusal path never mutates `self.active` nor the
//! lifecycle state — asserted by `second_client_refusal_leaves_first_session_untouched`.
//!
//! Lifecycle integration (plan Todo 14 follow-up, behaviour (d)): `PLAY` feeds
//! [`Event::ClientConnected`], `TEARDOWN` and [`RtspServer::on_disconnect`]
//! feed [`Event::ClientDisconnected`] into the caller's `&mut Lifecycle`
//! (normally `session::with_session(...)`), and the returned
//! [`SessionAction`]s — including [`SessionAction::TearDownServer`] — are
//! handed straight back for the caller to apply.

use std::io::{self, Write as _};
use std::net::{SocketAddr, TcpStream, UdpSocket};
use std::sync::{Arc, Mutex};

use crate::session::{Event, Session as Lifecycle, SessionAction};

use super::request::{self, Method, Request};
use super::sdp::{self, SdpParams};
use super::transport::{self, TransportReq};
use super::{interleaved_frame, PUBLIC_METHODS, SERVER_HEADER, STREAM_PATH};

/// One media track. Index order matches the SDP: video is track 0, audio is
/// track 1 (plan reference design: "MJPEG track 0 + audio track 1").
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Track {
    Video = 0,
    Audio = 1,
}

impl Track {
    pub fn index(self) -> usize {
        self as usize
    }

    /// `a=control:` value for this track (relative form, appended to the
    /// presentation URI by the client).
    pub fn control(self) -> &'static str {
        match self {
            Self::Video => "trackID=0",
            Self::Audio => "trackID=1",
        }
    }
}

/// Per-track negotiated transport.
#[derive(Clone, Debug)]
pub struct TrackState {
    pub set_up: bool,
    /// RTP-Info `seq` for the next PLAY response (Todo 12 contract: a track's
    /// first packet of a session carries `seq = 0, rtptime = 0`, matching the
    /// first PLAY's RTP-Info; [`RtspServer::note_rtp_sent`] keeps later PLAYs
    /// in step).
    pub seq: u16,
    pub rtptime: u32,
    pub transport: Option<TransportReq>,
}

impl Default for TrackState {
    fn default() -> Self {
        Self {
            set_up: false,
            seq: 0,
            rtptime: 0,
            transport: None,
        }
    }
}

/// The single active RTSP session. There is at most one; a second client is
/// refused with 453 without touching this value.
#[derive(Clone, Debug)]
pub struct ActiveSession {
    pub id: String,
    /// Connection id that owns the session (SETUP arrived on it).
    pub owner: u64,
    pub playing: bool,
    pub video: TrackState,
    pub audio: TrackState,
    video_sender: Option<RtpSender>,
    audio_sender: Option<RtpSender>,
}

impl ActiveSession {
    /// The Todo 12 send hook for a track, if its transport is wired.
    pub fn sender(&self, track: Track) -> Option<RtpSender> {
        match track {
            Track::Video => self.video_sender.clone(),
            Track::Audio => self.audio_sender.clone(),
        }
    }

    fn track_state(&self, track: Track) -> &TrackState {
        match track {
            Track::Video => &self.video,
            Track::Audio => &self.audio,
        }
    }

    fn track_state_mut(&mut self, track: Track) -> &mut TrackState {
        match track {
            Track::Video => &mut self.video,
            Track::Audio => &mut self.audio,
        }
    }
}

/// Per-connection I/O resources, supplied by the accept loop
/// (`super::service`) and stored (via a [`RtpSender`]) on the session when a
/// track is set up. Tests construct it with [`ConnIo::for_test`].
#[derive(Clone)]
pub struct ConnIo {
    /// Client address — for UDP, `client_port` values are combined with this IP.
    pub peer: SocketAddr,
    /// Shared write handle for TCP-interleaved framing (the read half lives in
    /// the connection loop; both come from `TcpStream::try_clone`).
    pub tcp: Option<Arc<Mutex<TcpStream>>>,
    /// Shared server-side UDP sockets (bound once per service, optional).
    pub udp_rtp: Option<Arc<UdpSocket>>,
    pub udp_rtcp: Option<Arc<UdpSocket>>,
}

impl ConnIo {
    /// No sockets — enough for every dispatch-level test.
    pub fn for_test() -> Self {
        Self {
            peer: SocketAddr::from(([127, 0, 0, 1], 0)),
            tcp: None,
            udp_rtp: None,
            udp_rtcp: None,
        }
    }
}

#[derive(Clone, Debug)]
enum SenderKind {
    Tcp {
        stream: Arc<Mutex<TcpStream>>,
        rtp: u8,
        rtcp: u8,
    },
    Udp {
        rtp_sock: Arc<UdpSocket>,
        rtcp_sock: Arc<UdpSocket>,
        client_rtp: SocketAddr,
        client_rtcp: SocketAddr,
    },
}

/// **The Todo 12 consumption point.** One per negotiated track; obtained via
/// [`RtspServer::sender`] after `PLAY`. Writes are safe from any thread —
/// the TCP handle is behind the same mutex the RTSP response writer uses, so
/// interleaved RTP frames and RTSP responses can never interleave mid-frame.
///
/// TCP interleaving frames bytes exactly as RFC 2326 §10.12 requires:
/// `'$' (0x24) | channel (u8) | length (u16 big-endian) | payload` —
/// see [`super::interleaved_frame`].
#[derive(Clone, Debug)]
pub struct RtpSender {
    track: Track,
    kind: SenderKind,
}

impl RtpSender {
    pub fn track(&self) -> Track {
        self.track
    }

    /// Send one RTP packet for this track (payload already packetized by
    /// Todo 12's payloaders).
    pub fn send_rtp(&self, packet: &[u8]) -> io::Result<()> {
        self.send(packet, true)
    }

    /// Send one RTCP packet (Todo 13 sender reports) on the track's RTCP
    /// channel/port.
    pub fn send_rtcp(&self, packet: &[u8]) -> io::Result<()> {
        self.send(packet, false)
    }

    fn send(&self, packet: &[u8], is_rtp: bool) -> io::Result<()> {
        match &self.kind {
            SenderKind::Tcp { stream, rtp, rtcp } => {
                let channel = if is_rtp { *rtp } else { *rtcp };
                let frame = interleaved_frame(channel, packet);
                let mut w = stream.lock().unwrap_or_else(|p| p.into_inner());
                w.write_all(&frame)
            }
            SenderKind::Udp {
                rtp_sock,
                rtcp_sock,
                client_rtp,
                client_rtcp,
            } => {
                let (sock, addr) = if is_rtp {
                    (rtp_sock, client_rtp)
                } else {
                    (rtcp_sock, client_rtcp)
                };
                sock.send_to(packet, addr).map(|_| ())
            }
        }
    }
}

fn build_sender(track: Track, tt: TransportReq, io: &ConnIo) -> Option<RtpSender> {
    match tt {
        TransportReq::Tcp { rtp, rtcp } => io.tcp.clone().map(|stream| RtpSender {
            track,
            kind: SenderKind::Tcp { stream, rtp, rtcp },
        }),
        TransportReq::Udp {
            client_rtp,
            client_rtcp,
        } => {
            let rtp_sock = io.udp_rtp.clone()?;
            let rtcp_sock = io.udp_rtcp.clone()?;
            Some(RtpSender {
                track,
                kind: SenderKind::Udp {
                    rtp_sock,
                    rtcp_sock,
                    client_rtp: SocketAddr::new(io.peer.ip(), client_rtp),
                    client_rtcp: SocketAddr::new(io.peer.ip(), client_rtcp),
                },
            })
        }
    }
}

/// Server configuration. Defaults are what the pure layer and tests use;
/// `super::service::Service::spawn` replaces `udp_server_ports` with the
/// actually-bound ports (or `None`, which turns UDP `SETUP` into a truthful
/// 461 rather than an echo of a port nobody is listening on).
#[derive(Clone, Debug)]
pub struct Config {
    /// Presentation path — must match the mDNS `txt=path` from `net.rs`
    /// (`/mjpeg/1`).
    pub path: String,
    /// `server_port` pair echoed in UDP `Transport:` responses.
    pub udp_server_ports: Option<(u16, u16)>,
    /// `Session: id;timeout=N` value.
    pub session_timeout_secs: u32,
    /// `o=` session id in the SDP (the SDP origin, distinct from the
    /// per-connection RTSP session id issued at SETUP).
    pub sdp_session_id: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            path: STREAM_PATH.to_string(),
            // Placeholder for the pure layer; spawn() overwrites with the
            // real bound ports before any SETUP can be answered.
            udp_server_ports: Some((50_000, 50_001)),
            session_timeout_secs: 60,
            sdp_session_id: "1".to_string(),
        }
    }
}

/// A response about to be serialized. `Server:` and the `CSeq` echo are
/// added by [`Response::to_bytes`] (plan MUST DO: CSeq echoed in *every*
/// response, `Server:` present in every response).
#[derive(Clone, Debug)]
pub struct Response {
    pub status: u16,
    pub reason: &'static str,
    pub headers: Vec<(String, String)>,
    pub body: Option<String>,
}

fn reason_for(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        406 => "Not Acceptable",
        453 => "Not Enough Bandwidth",
        454 => "Session Not Found",
        461 => "Unsupported Transport",
        _ => "Error",
    }
}

impl Response {
    pub fn new(status: u16) -> Self {
        Self {
            status,
            reason: reason_for(status),
            headers: Vec::new(),
            body: None,
        }
    }

    pub fn header(mut self, name: &str, value: impl Into<String>) -> Self {
        self.headers.push((name.to_string(), value.into()));
        self
    }

    pub fn body(mut self, body: String) -> Self {
        self.headers
            .push(("Content-Length".to_string(), body.len().to_string()));
        self.body = Some(body);
        self
    }

    pub fn to_bytes(&self, cseq: Option<&str>) -> Vec<u8> {
        let mut out = format!("RTSP/1.0 {} {}\r\n", self.status, self.reason);
        out += &format!("Server: {SERVER_HEADER}\r\n");
        if let Some(c) = cseq {
            out += &format!("CSeq: {c}\r\n");
        }
        for (name, value) in &self.headers {
            out += &format!("{name}: {value}\r\n");
        }
        out.push_str("\r\n");
        if let Some(body) = &self.body {
            out += body;
        }
        out.into_bytes()
    }
}

/// Status code of a serialized response (first-line token), for logging and
/// tests.
pub fn response_status(response: &[u8]) -> Option<u16> {
    let text = std::str::from_utf8(response).ok()?;
    let first = text.split("\r\n").next()?;
    let mut parts = first.split(' ');
    if parts.next()? != "RTSP/1.0" {
        return None;
    }
    parts.next()?.parse().ok()
}

/// Strip scheme/authority and query from a request URI, leaving the path.
fn extract_path(uri: &str) -> &str {
    let after_scheme = match uri.find("://") {
        Some(i) => {
            let rest = &uri[i + 3..];
            match rest.find('/') {
                Some(j) => &rest[j..],
                None => "/",
            }
        }
        None => uri,
    };
    after_scheme
        .split(|c| c == '?' || c == '#')
        .next()
        .unwrap_or(after_scheme)
}

/// Base URL for `a=control:` / `RTP-Info` URLs: the request URI truncated to
/// the presentation path, query dropped, trailing `/` ensured. The trailing
/// slash is load-bearing — RFC 3986 resolution of `trackID=0` against
/// `/mjpeg/1` would yield `/mjpeg/trackID=0`.
fn control_base(uri: &str) -> String {
    let no_query = uri.split(|c| c == '?' || c == '#').next().unwrap_or(uri);
    match no_query.find(STREAM_PATH) {
        Some(idx) => format!("{}/", &no_query[..idx + STREAM_PATH.len()]),
        None => format!("{STREAM_PATH}/"),
    }
}

fn track_from_uri(uri: &str) -> Option<Track> {
    let path = extract_path(uri);
    let last = path.rsplit('/').next()?;
    match last {
        t if t == Track::Video.control() => Some(Track::Video),
        t if t == Track::Audio.control() => Some(Track::Audio),
        _ => None,
    }
}

fn session_header_value(req: &Request) -> Option<String> {
    req.header("session")
        .map(|v| v.split(';').next().unwrap_or(v).trim().to_string())
}

fn session_header(id: &str, cfg: &Config) -> String {
    format!("{};timeout={}", id, cfg.session_timeout_secs)
}

fn transport_echo(tt: &TransportReq, cfg: &Config) -> String {
    match *tt {
        TransportReq::Tcp { rtp, rtcp } => {
            format!("RTP/AVP/TCP;unicast;interleaved={rtp}-{rtcp};mode=play")
        }
        TransportReq::Udp {
            client_rtp,
            client_rtcp,
        } => {
            let (sr, st) = cfg.udp_server_ports.unwrap_or((0, 1));
            format!(
                "RTP/AVP;unicast;client_port={client_rtp}-{client_rtcp};server_port={sr}-{st};mode=play"
            )
        }
    }
}

/// The RTSP server state machine: one optional session plus dispatch.
pub struct RtspServer {
    config: Config,
    active: Option<ActiveSession>,
    next_session_num: u32,
}

impl RtspServer {
    pub fn new(config: Config) -> Self {
        Self {
            config,
            active: None,
            next_session_num: 0,
        }
    }

    pub fn with_defaults() -> Self {
        Self::new(Config::default())
    }

    pub fn active_session(&self) -> Option<&ActiveSession> {
        self.active.as_ref()
    }

    /// Todo 12: the send hook for a track once the session is set up (present
    /// as soon as that track's SETUP succeeded — streaming should start after
    /// PLAY, gated on [`ActiveSession::playing`]).
    pub fn sender(&self, track: Track) -> Option<RtpSender> {
        self.active.as_ref()?.sender(track)
    }

    /// Todo 12: update the RTP-Info bookkeeping from the payloader's actual
    /// first/last sent packet so subsequent PLAY responses echo real values.
    pub fn note_rtp_sent(&mut self, track: Track, seq: u16, rtptime: u32) {
        if let Some(active) = self.active.as_mut() {
            let state = active.track_state_mut(track);
            state.seq = seq;
            state.rtptime = rtptime;
        }
    }

    /// Handle one complete RTSP message (bytes as read off the wire).
    /// Returns the serialized response and the lifecycle actions the caller
    /// must apply (e.g. `session::with_session` results).
    pub fn handle_bytes(
        &mut self,
        raw: &[u8],
        conn: u64,
        io: &ConnIo,
        life: &mut Lifecycle,
    ) -> (Vec<u8>, Vec<SessionAction>) {
        match request::parse(raw) {
            Ok(req) => {
                let cseq = req.cseq.clone();
                let (resp, actions) = self.dispatch(req, conn, io, life);
                (resp.to_bytes(Some(&cseq)), actions)
            }
            Err(_) => {
                let resp = Response::new(400);
                (
                    resp.to_bytes(request::best_effort_cseq(raw).as_deref()),
                    Vec::new(),
                )
            }
        }
    }

    /// The connection dropped (EOF/error). If — and only if — `conn` owned
    /// the session, clear it and raise [`Event::ClientDisconnected`] so the
    /// caller applies the returned actions (behaviour (d): teardown happens
    /// *from* the disconnect callback). A refused second connection is a
    /// no-op: it never owned the session, so the first one is undisturbed.
    pub fn on_disconnect(&mut self, conn: u64, life: &mut Lifecycle) -> Vec<SessionAction> {
        match &self.active {
            Some(active) if active.owner == conn => {
                self.active = None;
                life.handle(Event::ClientDisconnected)
            }
            _ => Vec::new(),
        }
    }

    fn dispatch(
        &mut self,
        req: Request,
        conn: u64,
        io: &ConnIo,
        life: &mut Lifecycle,
    ) -> (Response, Vec<SessionAction>) {
        let no_actions = Vec::new();
        match &req.method {
            Method::Options => (
                Response::new(200).header("Public", PUBLIC_METHODS),
                no_actions,
            ),
            Method::Describe => (self.describe(&req), no_actions),
            Method::Setup => self.setup(&req, conn, io),
            Method::Play => self.play(&req, conn, life),
            Method::Teardown => self.teardown(&req, conn, life),
            Method::GetParameter => (self.get_parameter(&req, conn), no_actions),
            Method::Other(_) => (
                Response::new(405).header("Allow", PUBLIC_METHODS),
                no_actions,
            ),
        }
    }

    fn describe(&self, req: &Request) -> Response {
        let path = extract_path(&req.uri);
        if path != self.config.path && path != format!("{}/", self.config.path) {
            return Response::new(404);
        }
        if let Some(accept) = req.header("accept") {
            let ok = accept.contains("application/sdp") || accept.contains("*/*");
            if !ok {
                return Response::new(406);
            }
        }
        let base = control_base(&req.uri);
        let body = sdp::generate(&SdpParams {
            session_id: &self.config.sdp_session_id,
            version: 1,
            control_base: &base,
        });
        Response::new(200)
            .header("Content-Base", base)
            .header("Content-Type", "application/sdp")
            .body(body)
    }

    fn setup(
        &mut self,
        req: &Request,
        conn: u64,
        io: &ConnIo,
    ) -> (Response, Vec<SessionAction>) {
        let none = Vec::new();
        let header = match req.header("transport") {
            Some(h) => h,
            None => return (Response::new(400), none),
        };
        let requested = match transport::parse(header) {
            Ok(t) => t,
            Err(_) => return (Response::new(461), none),
        };
        let track = match track_from_uri(&req.uri) {
            Some(t) => t,
            None => return (Response::new(404), none),
        };

        // Server-assigned channels: track i uses interleaved 2i/2i+1 (UDP
        // client ports are honoured as offered). Deterministic assignment
        // means ffmpeg re-offering interleaved=0-1 for the audio track gets
        // 2-3 back instead of colliding with video — clients follow the
        // response, per RFC 2326 §12.3.3.
        let negotiated = match requested {
            TransportReq::Tcp { .. } => TransportReq::Tcp {
                rtp: (2 * track.index()) as u8,
                rtcp: (2 * track.index() + 1) as u8,
            },
            TransportReq::Udp { .. } => {
                if self.config.udp_server_ports.is_none() {
                    return (Response::new(461), none);
                }
                requested
            }
        };

        let claimed = session_header_value(req);
        let id = match &self.active {
            // No session yet: a client claiming one has a stale id (server
            // restarted) — 454, and no session is created.
            None => {
                if claimed.is_some() {
                    return (Response::new(454), none);
                }
                self.next_session_num += 1;
                format!("esp-{:04x}", self.next_session_num)
            }
            // Single-session gate: a different connection is refused here and
            // NOTHING below runs — `self.active` and the lifecycle state are
            // not touched (plan: "refused without disturbing the first").
            Some(active) if active.owner != conn => return (Response::new(453), none),
            Some(active) => {
                if let Some(claimed) = &claimed {
                    if claimed != &active.id {
                        return (Response::new(454), none);
                    }
                }
                active.id.clone()
            }
        };

        let sender = build_sender(track, negotiated, io);
        let active = match self.active.as_mut() {
            Some(active) => active,
            None => self.active.insert(ActiveSession {
                id: id.clone(),
                owner: conn,
                playing: false,
                video: TrackState::default(),
                audio: TrackState::default(),
                video_sender: None,
                audio_sender: None,
            }),
        };
        {
            let state = active.track_state_mut(track);
            state.set_up = true;
            state.transport = Some(negotiated);
        }
        match track {
            Track::Video => active.video_sender = sender,
            Track::Audio => active.audio_sender = sender,
        }

        let resp = Response::new(200)
            .header("Transport", transport_echo(&negotiated, &self.config))
            .header("Session", session_header(&id, &self.config));
        (resp, none)
    }

    fn play(&mut self, req: &Request, conn: u64, life: &mut Lifecycle) -> (Response, Vec<SessionAction>) {
        let claimed = match session_header_value(req) {
            Some(c) => c,
            None => return (Response::new(400), Vec::new()),
        };
        let (id, rtp_info) = {
            let active = match self.active.as_mut() {
                Some(a) => a,
                None => return (Response::new(454), Vec::new()),
            };
            if claimed != active.id {
                return (Response::new(454), Vec::new());
            }
            if active.owner != conn {
                return (Response::new(453), Vec::new());
            }
            active.playing = true;
            let base = control_base(&req.uri);
            let info: Vec<String> = [Track::Video, Track::Audio]
                .iter()
                .filter(|t| active.track_state(**t).set_up)
                .map(|t| {
                    let s = active.track_state(*t);
                    format!(
                        "url={base}{};seq={};rtptime={}",
                        t.control(),
                        s.seq,
                        s.rtptime
                    )
                })
                .collect();
            (active.id.clone(), info.join(","))
        };

        // Lifecycle: capture may start (Todo 14 behaviour (c) turns on at
        // PLAY; `handle` is idempotent for a repeat PLAY).
        let actions = life.handle(Event::ClientConnected);
        let resp = Response::new(200)
            .header("Session", session_header(&id, &self.config))
            .header("RTP-Info", rtp_info);
        (resp, actions)
    }

    fn teardown(&mut self, req: &Request, conn: u64, life: &mut Lifecycle) -> (Response, Vec<SessionAction>) {
        let claimed = match session_header_value(req) {
            Some(c) => c,
            None => return (Response::new(400), Vec::new()),
        };
        let id = match &self.active {
            None => return (Response::new(454), Vec::new()),
            Some(a) if a.id != claimed => return (Response::new(454), Vec::new()),
            Some(a) if a.owner != conn => return (Response::new(453), Vec::new()),
            Some(a) => a.id.clone(),
        };
        self.active = None;
        // Behaviour (d): StopCapture + TearDownServer are produced right here.
        let actions = life.handle(Event::ClientDisconnected);
        let resp = Response::new(200).header("Session", session_header(&id, &self.config));
        (resp, actions)
    }

    fn get_parameter(&self, req: &Request, conn: u64) -> Response {
        let claimed = match session_header_value(req) {
            Some(c) => c,
            None => return Response::new(400),
        };
        match &self.active {
            None => Response::new(454),
            Some(a) if a.id != claimed => Response::new(454),
            Some(a) if a.owner != conn => Response::new(453),
            Some(a) => Response::new(200).header("Session", session_header(&a.id, &self.config)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::SessionState;

    const HOST: &str = "rtsp://192.168.1.5:554";

    fn msg(method: &str, uri: &str, cseq: u32, extra: &[(&str, &str)]) -> Vec<u8> {
        let mut s = format!("{method} {uri} RTSP/1.0\r\nCSeq: {cseq}\r\n");
        for (k, v) in extra {
            s += &format!("{k}: {v}\r\n");
        }
        s.push_str("\r\n");
        s.into_bytes()
    }

    fn run(
        srv: &mut RtspServer,
        life: &mut Lifecycle,
        conn: u64,
        raw: &[u8],
    ) -> (Vec<u8>, Vec<SessionAction>) {
        let io = ConnIo::for_test();
        srv.handle_bytes(raw, conn, &io, life)
    }

    fn status(resp: &[u8]) -> u16 {
        response_status(resp).expect("response must start with RTSP/1.0")
    }

    fn resp_header(resp: &[u8], name: &str) -> Option<String> {
        let text = String::from_utf8_lossy(resp);
        text.lines().find_map(|line| {
            let (k, v) = line.split_once(':')?;
            if k.trim().eq_ignore_ascii_case(name) {
                Some(v.trim().to_string())
            } else {
                None
            }
        })
    }

    fn session_id(resp: &[u8]) -> String {
        resp_header(resp, "Session")
            .expect("Session header")
            .split(';')
            .next()
            .unwrap()
            .to_string()
    }

    fn setup(srv: &mut RtspServer, life: &mut Lifecycle, conn: u64, cseq: u32, track: &str) -> Vec<u8> {
        run(
            srv,
            life,
            conn,
            &msg(
                "SETUP",
                &format!("{HOST}/mjpeg/1/{track}"),
                cseq,
                &[("Transport", "RTP/AVP/TCP;unicast;interleaved=0-1")],
            ),
        )
        .0
    }

    #[test]
    fn options_advertises_the_implemented_methods_and_echoes_cseq() {
        let mut srv = RtspServer::with_defaults();
        let mut life = Lifecycle::with_defaults();
        let (resp, _) = run(&mut srv, &mut life, 1, &msg("OPTIONS", &format!("{HOST}/mjpeg/1"), 17, &[]));
        assert_eq!(status(&resp), 200);
        let text = String::from_utf8_lossy(&resp);
        assert!(text.contains("CSeq: 17"), "CSeq echo missing: {text}");
        assert!(text.contains("Server: "), "Server header missing: {text}");
        let public = resp_header(&resp, "Public").unwrap();
        for m in ["OPTIONS", "DESCRIBE", "SETUP", "PLAY", "TEARDOWN", "GET_PARAMETER"] {
            assert!(public.contains(m), "Public misses {m}: {public}");
        }
    }

    #[test]
    fn describe_returns_locked_sdp_with_two_media_lines() {
        let mut srv = RtspServer::with_defaults();
        let mut life = Lifecycle::with_defaults();
        let (resp, _) = run(
            &mut srv,
            &mut life,
            1,
            &msg("DESCRIBE", &format!("{HOST}/mjpeg/1"), 2, &[("Accept", "application/sdp")]),
        );
        assert_eq!(status(&resp), 200);
        assert_eq!(resp_header(&resp, "Content-Type").unwrap(), "application/sdp");
        let text = String::from_utf8_lossy(&resp);
        let body = text.split("\r\n\r\n").nth(1).unwrap();
        assert!(body.lines().any(|l| l == "a=rtpmap:0 PCMU/8000"));
        assert!(body.lines().any(|l| l == "a=rtpmap:26 JPEG/90000"));
        let mlines: Vec<&str> = body.lines().filter(|l| l.starts_with("m=")).collect();
        assert_eq!(mlines, vec!["m=video 0 RTP/AVP 26", "m=audio 0 RTP/AVP 0"]);
        assert!(body.lines().any(|l| l.starts_with("o=- ")));
        assert!(!body.contains("PCMA") && !body.contains("L16"));
        // Content-Length is exact (byte-counted, body is ASCII).
        let declared: usize = resp_header(&resp, "Content-Length").unwrap().parse().unwrap();
        assert_eq!(declared, body.len());
    }

    #[test]
    fn describe_rejects_wrong_path_and_unacceptable_accept_header() {
        let mut srv = RtspServer::with_defaults();
        let mut life = Lifecycle::with_defaults();
        let (resp, _) = run(&mut srv, &mut life, 1, &msg("DESCRIBE", &format!("{HOST}/other"), 1, &[]));
        assert_eq!(status(&resp), 404);
        let (resp, _) = run(
            &mut srv,
            &mut life,
            1,
            &msg(
                "DESCRIBE",
                &format!("{HOST}/mjpeg/1"),
                2,
                &[("Accept", "application/xml")],
            ),
        );
        assert_eq!(status(&resp), 406);
        // No Accept header at all is tolerated (interop).
        let (resp, _) = run(&mut srv, &mut life, 1, &msg("DESCRIBE", &format!("{HOST}/mjpeg/1"), 3, &[]));
        assert_eq!(status(&resp), 200);
    }

    #[test]
    fn setup_issues_a_session_and_echoes_server_assigned_interleaved_channels() {
        let mut srv = RtspServer::with_defaults();
        let mut life = Lifecycle::with_defaults();
        let resp = setup(&mut srv, &mut life, 1, 3, "trackID=0");
        assert_eq!(status(&resp), 200);
        let id = session_id(&resp);
        assert!(!id.is_empty());
        let transport = resp_header(&resp, "Transport").unwrap();
        assert!(transport.contains("interleaved=0-1"), "{transport}");

        // ffmpeg re-offers interleaved=0-1 for audio; the server assigns 2-3.
        let resp = setup(&mut srv, &mut life, 1, 4, "trackID=1");
        assert_eq!(status(&resp), 200);
        assert_eq!(session_id(&resp), id, "same session for the second track");
        let transport = resp_header(&resp, "Transport").unwrap();
        assert!(transport.contains("interleaved=2-3"), "{transport}");

        let active = srv.active_session().unwrap();
        assert_eq!(active.id, id);
        assert_eq!(active.owner, 1);
        assert!(active.video.set_up && active.audio.set_up);
        assert!(!active.playing);
        // Session has not started: no lifecycle event at SETUP time.
        assert_eq!(life.state(), SessionState::Idle);
    }

    #[test]
    fn setup_udp_echoes_client_and_server_ports() {
        let mut srv = RtspServer::with_defaults();
        let mut life = Lifecycle::with_defaults();
        let (resp, _) = run(
            &mut srv,
            &mut life,
            1,
            &msg(
                "SETUP",
                &format!("{HOST}/mjpeg/1/trackID=0"),
                1,
                &[("Transport", "RTP/AVP;unicast;client_port=6970-6971")],
            ),
        );
        assert_eq!(status(&resp), 200);
        let t = resp_header(&resp, "Transport").unwrap();
        assert!(t.contains("client_port=6970-6971"), "{t}");
        assert!(t.contains("server_port=50000-50001"), "{t}");
    }

    #[test]
    fn setup_with_unoffered_transport_is_461_and_creates_no_session() {
        let mut srv = RtspServer::with_defaults();
        let mut life = Lifecycle::with_defaults();
        for offered in [
            "raw/RAW;unicast",
            "RTP/AVP;multicast;port=0-1",
            "RTP/AVP;unicast",
        ] {
            let (resp, _) = run(
                &mut srv,
                &mut life,
                1,
                &msg(
                    "SETUP",
                    &format!("{HOST}/mjpeg/1/trackID=0"),
                    5,
                    &[("Transport", offered)],
                ),
            );
            assert_eq!(status(&resp), 461, "offered: {offered}");
        }
        assert!(srv.active_session().is_none());
        assert_eq!(life.state(), SessionState::Idle);
    }

    #[test]
    fn setup_missing_transport_or_unknown_track_is_400_or_404() {
        let mut srv = RtspServer::with_defaults();
        let mut life = Lifecycle::with_defaults();
        let (resp, _) = run(
            &mut srv,
            &mut life,
            1,
            &msg("SETUP", &format!("{HOST}/mjpeg/1/trackID=0"), 1, &[]),
        );
        assert_eq!(status(&resp), 400);
        let (resp, _) = run(
            &mut srv,
            &mut life,
            1,
            &msg(
                "SETUP",
                &format!("{HOST}/mjpeg/1/trackID=9"),
                2,
                &[("Transport", "RTP/AVP/TCP;unicast;interleaved=0-1")],
            ),
        );
        assert_eq!(status(&resp), 404);
        assert!(srv.active_session().is_none());
    }

    #[test]
    fn play_requires_matching_session_and_emits_rtp_info_then_starts_capture() {
        let mut srv = RtspServer::with_defaults();
        let mut life = Lifecycle::with_defaults();
        let resp = setup(&mut srv, &mut life, 1, 1, "trackID=0");
        let id = session_id(&resp);

        // Missing Session header → 400.
        let (resp, _) = run(&mut srv, &mut life, 1, &msg("PLAY", &format!("{HOST}/mjpeg/1"), 2, &[]));
        assert_eq!(status(&resp), 400);

        // Unknown Session header → 454.
        let (resp, _) = run(
            &mut srv,
            &mut life,
            1,
            &msg("PLAY", &format!("{HOST}/mjpeg/1"), 3, &[("Session", "not-the-id")]),
        );
        assert_eq!(status(&resp), 454);

        // Correct session → 200 + RTP-Info + StartCapture.
        let (resp, actions) = run(
            &mut srv,
            &mut life,
            1,
            &msg("PLAY", &format!("{HOST}/mjpeg/1"), 4, &[("Session", &id)]),
        );
        assert_eq!(status(&resp), 200);
        let info = resp_header(&resp, "RTP-Info").unwrap();
        assert!(info.contains(&format!("url={HOST}/mjpeg/1/trackID=0;seq=0;rtptime=0")), "{info}");
        assert!(!info.contains("trackID=1"), "unset track must not appear: {info}");
        assert_eq!(actions, vec![SessionAction::StartCapture]);
        assert_eq!(life.state(), SessionState::Active);
        assert!(srv.active_session().unwrap().playing);
    }

    #[test]
    fn rtp_info_reflects_payloader_updates_via_note_rtp_sent() {
        let mut srv = RtspServer::with_defaults();
        let mut life = Lifecycle::with_defaults();
        let id = session_id(&setup(&mut srv, &mut life, 1, 1, "trackID=0"));
        srv.note_rtp_sent(Track::Video, 0x1234, 90_000);
        let (resp, _) = run(
            &mut srv,
            &mut life,
            1,
            &msg("PLAY", &format!("{HOST}/mjpeg/1"), 2, &[("Session", &id)]),
        );
        let info = resp_header(&resp, "RTP-Info").unwrap();
        assert!(info.contains("seq=4660;rtptime=90000"), "{info}");
    }

    #[test]
    fn teardown_returns_stop_capture_and_teardown_server_actions() {
        let mut srv = RtspServer::with_defaults();
        let mut life = Lifecycle::with_defaults();
        let id = session_id(&setup(&mut srv, &mut life, 1, 1, "trackID=0"));
        run(&mut srv, &mut life, 1, &msg("PLAY", &format!("{HOST}/mjpeg/1"), 2, &[("Session", &id)]));

        let (resp, actions) = run(
            &mut srv,
            &mut life,
            1,
            &msg("TEARDOWN", &format!("{HOST}/mjpeg/1"), 3, &[("Session", &id)]),
        );
        assert_eq!(status(&resp), 200);
        assert_eq!(
            actions,
            vec![SessionAction::StopCapture, SessionAction::TearDownServer]
        );
        assert_eq!(life.state(), SessionState::TornDown);
        assert!(srv.active_session().is_none());

        // A repeat TEARDOWN (or a late one with no session) is 454, not a panic.
        let (resp, _) = run(
            &mut srv,
            &mut life,
            1,
            &msg("TEARDOWN", &format!("{HOST}/mjpeg/1"), 4, &[("Session", &id)]),
        );
        assert_eq!(status(&resp), 454);
    }

    #[test]
    fn second_client_refusal_leaves_first_session_untouched() {
        let mut srv = RtspServer::with_defaults();
        let mut life = Lifecycle::with_defaults();

        let id1 = session_id(&setup(&mut srv, &mut life, 1, 1, "trackID=0"));
        run(&mut srv, &mut life, 1, &msg("PLAY", &format!("{HOST}/mjpeg/1"), 2, &[("Session", &id1)]));
        assert_eq!(life.state(), SessionState::Active);

        // Second client, valid transport, valid track → 453.
        let (resp, actions) = run(
            &mut srv,
            &mut life,
            2,
            &msg(
                "SETUP",
                &format!("{HOST}/mjpeg/1/trackID=0"),
                1,
                &[("Transport", "RTP/AVP/TCP;unicast;interleaved=0-1")],
            ),
        );
        assert_eq!(status(&resp), 453);
        assert!(actions.is_empty(), "refusal must produce no lifecycle actions");

        // The first session is fully intact: same id, owner, lifecycle state.
        let active = srv.active_session().unwrap();
        assert_eq!(active.id, id1);
        assert_eq!(active.owner, 1);
        assert!(active.playing);
        assert_eq!(life.state(), SessionState::Active);

        // The refused connection dropping must not disturb the first session.
        let actions = srv.on_disconnect(2, &mut life);
        assert!(actions.is_empty());
        assert!(srv.active_session().is_some());
        assert_eq!(life.state(), SessionState::Active);

        // Second client's PLAY/TEARDOWN with a stolen id cannot touch it either.
        let (resp, _) = run(
            &mut srv,
            &mut life,
            2,
            &msg("TEARDOWN", &format!("{HOST}/mjpeg/1"), 2, &[("Session", &id1)]),
        );
        assert_eq!(status(&resp), 453);
        assert!(srv.active_session().is_some());

        // Owner still fully functional after all that.
        let (resp, _) = run(
            &mut srv,
            &mut life,
            1,
            &msg("PLAY", &format!("{HOST}/mjpeg/1"), 9, &[("Session", &id1)]),
        );
        assert_eq!(status(&resp), 200);
    }

    #[test]
    fn owner_disconnect_tears_down_from_the_callback() {
        let mut srv = RtspServer::with_defaults();
        let mut life = Lifecycle::with_defaults();
        let id = session_id(&setup(&mut srv, &mut life, 1, 1, "trackID=0"));
        run(&mut srv, &mut life, 1, &msg("PLAY", &format!("{HOST}/mjpeg/1"), 2, &[("Session", &id)]));

        let actions = srv.on_disconnect(1, &mut life);
        assert_eq!(
            actions,
            vec![SessionAction::StopCapture, SessionAction::TearDownServer]
        );
        assert!(srv.active_session().is_none());
        assert_eq!(life.state(), SessionState::TornDown);

        // Duplicate disconnect is a no-op (no double teardown).
        assert!(srv.on_disconnect(1, &mut life).is_empty());
    }

    #[test]
    fn malformed_request_lines_and_wrong_cseq_are_400_with_cseq_echo_when_possible() {
        let mut srv = RtspServer::with_defaults();
        let mut life = Lifecycle::with_defaults();

        let (resp, actions) = run(&mut srv, &mut life, 1, b"garbage\r\n\r\n");
        assert_eq!(status(&resp), 400);
        assert!(actions.is_empty());

        // Missing CSeq → 400.
        let (resp, _) = run(
            &mut srv,
            &mut life,
            1,
            b"OPTIONS rtsp://h/mjpeg/1 RTSP/1.0\r\n\r\n",
        );
        assert_eq!(status(&resp), 400);

        // Non-numeric CSeq → 400, but the raw value is still echoed.
        let (resp, _) = run(
            &mut srv,
            &mut life,
            1,
            b"OPTIONS rtsp://h/mjpeg/1 RTSP/1.0\r\nCSeq: not-a-number\r\n\r\n",
        );
        assert_eq!(status(&resp), 400);
        let text = String::from_utf8_lossy(&resp);
        assert!(text.contains("CSeq: not-a-number"), "{text}");
    }

    #[test]
    fn unknown_method_is_405_with_allow_header() {
        let mut srv = RtspServer::with_defaults();
        let mut life = Lifecycle::with_defaults();
        let (resp, _) = run(&mut srv, &mut life, 1, &msg("PAUSE", &format!("{HOST}/mjpeg/1"), 6, &[]));
        assert_eq!(status(&resp), 405);
        let allow = resp_header(&resp, "Allow").unwrap();
        assert!(allow.contains("DESCRIBE") && allow.contains("TEARDOWN"), "{allow}");
    }

    #[test]
    fn get_parameter_is_a_session_scoped_keepalive() {
        let mut srv = RtspServer::with_defaults();
        let mut life = Lifecycle::with_defaults();
        let id = session_id(&setup(&mut srv, &mut life, 1, 1, "trackID=0"));

        // No session at all → 400 (missing Session header).
        let (resp, _) = run(&mut srv, &mut life, 1, &msg("GET_PARAMETER", &format!("{HOST}/mjpeg/1"), 2, &[]));
        assert_eq!(status(&resp), 400);

        let (resp, _) = run(
            &mut srv,
            &mut life,
            1,
            &msg("GET_PARAMETER", &format!("{HOST}/mjpeg/1"), 3, &[("Session", &id)]),
        );
        assert_eq!(status(&resp), 200);
        assert_eq!(session_id(&resp), id);
    }

    #[test]
    fn stale_session_claim_before_any_setup_is_454() {
        let mut srv = RtspServer::with_defaults();
        let mut life = Lifecycle::with_defaults();
        let (resp, _) = run(
            &mut srv,
            &mut life,
            1,
            &msg(
                "SETUP",
                &format!("{HOST}/mjpeg/1/trackID=0"),
                1,
                &[
                    ("Transport", "RTP/AVP/TCP;unicast;interleaved=0-1"),
                    ("Session", "esp-dead"),
                ],
            ),
        );
        assert_eq!(status(&resp), 454);
        assert!(srv.active_session().is_none());
    }

    #[test]
    fn relative_describe_uri_still_yields_working_control_urls() {
        let mut srv = RtspServer::with_defaults();
        let mut life = Lifecycle::with_defaults();
        let (resp, _) = run(&mut srv, &mut life, 1, &msg("DESCRIBE", "/mjpeg/1", 1, &[]));
        assert_eq!(status(&resp), 200);
        let text = String::from_utf8_lossy(&resp);
        assert!(text.contains("a=control:/mjpeg/1/trackID=0"), "{text}");
    }

    #[test]
    fn a_full_client_happy_path_runs_cleanly() {
        let mut srv = RtspServer::with_defaults();
        let mut life = Lifecycle::with_defaults();
        let (resp, _) = run(&mut srv, &mut life, 1, &msg("OPTIONS", &format!("{HOST}/mjpeg/1"), 1, &[]));
        assert_eq!(status(&resp), 200);
        let (resp, _) = run(
            &mut srv,
            &mut life,
            1,
            &msg("DESCRIBE", &format!("{HOST}/mjpeg/1"), 2, &[("Accept", "application/sdp")]),
        );
        assert_eq!(status(&resp), 200);
        let id = session_id(&setup(&mut srv, &mut life, 1, 3, "trackID=0"));
        let id2 = session_id(&setup(&mut srv, &mut life, 1, 4, "trackID=1"));
        assert_eq!(id, id2);
        let (resp, _) = run(
            &mut srv,
            &mut life,
            1,
            &msg("PLAY", &format!("{HOST}/mjpeg/1"), 5, &[("Session", &id)]),
        );
        assert_eq!(status(&resp), 200);
        let info = resp_header(&resp, "RTP-Info").unwrap();
        assert!(info.contains("trackID=0") && info.contains("trackID=1"), "{info}");
        let (resp, _) = run(
            &mut srv,
            &mut life,
            1,
            &msg("TEARDOWN", &format!("{HOST}/mjpeg/1"), 6, &[("Session", &id)]),
        );
        assert_eq!(status(&resp), 200);
        assert_eq!(life.state(), SessionState::TornDown);
    }
}
