//! RTSP over TCP service — blocking sockets on threads (plan R14: no
//! tokio), one thread per connection (plan MUST DO #6: zero new crates).
//!
//! The accept loop is the glue between wire bytes and
//! [`super::server::RtspServer`]:
//!
//! - **`$`-interleaved frames from the client** (RTCP receiver reports ride
//!   the same connection, RFC 2326 §10.12) are decoded and consumed by
//!   [`FrameDecoder`] so the RTSP stream never desyncs; routing them to
//!   Todo 13 is the one marked hook below.
//! - **Lifecycle actions** returned by every dispatch are applied here:
//!   `TearDownServer` closes the connection loop from inside the handler
//!   (plan Todo 14 behaviour (d)).
//! - **Lock order**: rtsp state (`RtspServer` mutex) → lifecycle
//!   (`session::SESSION`), never reversed.
//! - **Writes** to the control connection go through the same
//!   `Arc<Mutex<TcpStream>>` the [`super::server::RtpSender`] uses, so an
//!   RTSP response can never split an interleaved RTP frame.

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::session::{with_session, SessionAction};

use super::server::{response_status, ConnIo, Config, RtspServer};

/// Largest header block accepted before declaring the peer misbehaving.
const MAX_HEADER_BYTES: usize = 8 * 1024;
/// Largest request body accepted (`GET_PARAMETER` keepalives are bytes).
const MAX_BODY_BYTES: usize = 64 * 1024;
/// A silent connection is dropped after this long (session `timeout=` is 60 s;
/// well-behaved clients send keepalives inside it).
const READ_TIMEOUT: Duration = Duration::from_secs(180);
/// Non-blocking accept poll interval while waiting for shutdown.
const ACCEPT_POLL: Duration = Duration::from_millis(50);

// AtomicU32, not AtomicU64: xtensa has no 64-bit atomics (ESP32-S3 is a
// 32-bit target) — connection ids are widened to u64 at the API boundary.
static NEXT_CONN_ID: AtomicU32 = AtomicU32::new(1);

/// One decoded unit from the client side of the control connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decoded {
    /// A complete RTSP message (request line + headers + declared body).
    Rtsp(Vec<u8>),
    /// A complete `$`-interleaved frame.
    Interleaved { channel: u8, payload: Vec<u8> },
}

/// Wire-framing violation → the connection is dropped with a logged reason
/// (never a panic; plan QA — failure: 4xx / clean handling, not a crash).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeError {
    HeaderTooLarge,
    BodyTooLarge,
    MalformedHeader,
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::HeaderTooLarge => "header block exceeds limit",
            Self::BodyTooLarge => "request body exceeds limit",
            Self::MalformedHeader => "header block is not decodable",
        })
    }
}

impl std::error::Error for DecodeError {}

/// Incremental decoder over the client byte stream: `$`-interleaved frames
/// and RTSP messages (with `Content-Length` bodies), split or coales across
/// TCP segments arbitrarily. Pure over bytes — unit-tested without sockets.
#[derive(Default)]
pub struct FrameDecoder {
    buf: Vec<u8>,
}

impl FrameDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    pub fn next(&mut self) -> Result<Option<Decoded>, DecodeError> {
        if self.buf.is_empty() {
            return Ok(None);
        }
        if self.buf[0] == b'$' {
            if self.buf.len() < 4 {
                return Ok(None);
            }
            let len = u16::from_be_bytes([self.buf[2], self.buf[3]]) as usize;
            if self.buf.len() < 4 + len {
                return Ok(None);
            }
            let channel = self.buf[1];
            let payload = self.buf[4..4 + len].to_vec();
            self.buf.drain(..4 + len);
            return Ok(Some(Decoded::Interleaved { channel, payload }));
        }

        let head_end = match self.buf.windows(4).position(|w| w == b"\r\n\r\n") {
            Some(i) => i,
            None => {
                return if self.buf.len() > MAX_HEADER_BYTES {
                    Err(DecodeError::HeaderTooLarge)
                } else {
                    Ok(None)
                };
            }
        };
        let head = String::from_utf8(self.buf[..head_end].to_vec())
            .map_err(|_| DecodeError::MalformedHeader)?;
        let mut content_length = 0usize;
        for line in head.split("\r\n") {
            if let Some((name, value)) = line.split_once(':') {
                if name.trim().eq_ignore_ascii_case("content-length") {
                    content_length = value
                        .trim()
                        .parse()
                        .map_err(|_| DecodeError::MalformedHeader)?;
                }
            }
        }
        if content_length > MAX_BODY_BYTES {
            return Err(DecodeError::BodyTooLarge);
        }
        let total = head_end + 4 + content_length;
        if self.buf.len() < total {
            return Ok(None);
        }
        let msg = self.buf[..total].to_vec();
        self.buf.drain(..total);
        Ok(Some(Decoded::Rtsp(msg)))
    }
}

/// Handle to a running RTSP service: the listener, the shared state machine
/// (Todo 12 reads senders through [`Service::server`]), and the shutdown flag.
pub struct Service {
    server: Arc<Mutex<RtspServer>>,
    local_addr: SocketAddr,
    shutdown: Arc<AtomicBool>,
    accept_thread: Option<JoinHandle<()>>,
}

impl Service {
    /// Bind `0.0.0.0:port` (pass 0 for an ephemeral port — tests), bind the
    /// optional UDP RTCP/RTP sockets, and start the accept loop. Returns
    /// immediately; the caller holds the handle for the process lifetime.
    pub fn spawn(port: u16) -> io::Result<Self> {
        let listener = TcpListener::bind(("0.0.0.0", port))?;
        let local_addr = listener.local_addr()?;
        listener.set_nonblocking(true)?;

        let udp_rtp = UdpSocket::bind(("0.0.0.0", 0)).ok().map(Arc::new);
        let udp_rtcp = UdpSocket::bind(("0.0.0.0", 0)).ok().map(Arc::new);
        let udp_ports = match (&udp_rtp, &udp_rtcp) {
            (Some(a), Some(b)) => Some((
                a.local_addr()?.port(),
                b.local_addr()?.port(),
            )),
            _ => None,
        };

        let mut config = Config::default();
        config.udp_server_ports = udp_ports;
        let server = Arc::new(Mutex::new(RtspServer::new(config)));

        let shutdown = Arc::new(AtomicBool::new(false));
        let thread_handle = {
            let server = server.clone();
            let shutdown = shutdown.clone();
            thread::Builder::new()
                .name("rtsp-accept".into())
                .spawn(move || accept_loop(listener, server, shutdown, udp_rtp, udp_rtcp))?
        };

        println!(
            "rtsp: listening on {} (udp={})",
            local_addr,
            udp_ports
                .map(|(a, b)| format!("{a}-{b}"))
                .unwrap_or_else(|| "disabled".to_string())
        );

        Ok(Self {
            server,
            local_addr,
            shutdown,
            accept_thread: Some(thread_handle),
        })
    }

    /// Shared state machine — Todo 12 obtains per-track [`super::server::RtpSender`]s
    /// here and calls [`RtspServer::note_rtp_sent`].
    pub fn server(&self) -> Arc<Mutex<RtspServer>> {
        self.server.clone()
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Stop accepting; existing connections finish their current exchange
    /// and exit on their next read.
    pub fn shutdown(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        if let Some(handle) = self.accept_thread.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        self.shutdown();
    }
}

type UdpShared = Option<Arc<UdpSocket>>;

fn accept_loop(
    listener: TcpListener,
    server: Arc<Mutex<RtspServer>>,
    shutdown: Arc<AtomicBool>,
    udp_rtp: UdpShared,
    udp_rtcp: UdpShared,
) {
    while !shutdown.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, peer)) => {
                let conn = NEXT_CONN_ID.fetch_add(1, Ordering::SeqCst) as u64;
                let write = match stream.try_clone() {
                    Ok(clone) => Arc::new(Mutex::new(clone)),
                    Err(e) => {
                        println!("rtsp: conn {conn} from {peer} dropped (try_clone: {e})");
                        continue;
                    }
                };
                let server = server.clone();
                let udp_rtp = udp_rtp.clone();
                let udp_rtcp = udp_rtcp.clone();
                let spawned = thread::Builder::new()
                    .name(format!("rtsp-c{conn}"))
                    .spawn(move || serve_conn(stream, write, peer, conn, server, udp_rtp, udp_rtcp));
                if let Err(e) = spawned {
                    println!("rtsp: conn {conn} from {peer} dropped (spawn: {e})");
                }
            }
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(ACCEPT_POLL);
            }
            Err(e) => {
                if !shutdown.load(Ordering::SeqCst) {
                    println!("rtsp: accept error: {e}");
                }
                break;
            }
        }
    }
}

fn serve_conn(
    mut stream: TcpStream,
    write: Arc<Mutex<TcpStream>>,
    peer: SocketAddr,
    conn: u64,
    server: Arc<Mutex<RtspServer>>,
    udp_rtp: UdpShared,
    udp_rtcp: UdpShared,
) {
    let _ = stream.set_read_timeout(Some(READ_TIMEOUT));
    let _ = stream.set_nodelay(true);
    let io = ConnIo {
        peer,
        tcp: Some(write.clone()),
        udp_rtp,
        udp_rtcp,
    };

    let mut decoder = FrameDecoder::new();
    let mut chunk = [0u8; 2048];
    'read: loop {
        match decoder.next() {
            Ok(Some(Decoded::Rtsp(raw))) => {
                let method = first_token(&raw);
                let (resp, actions) = {
                    let mut srv = server
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    with_session(|life| srv.handle_bytes(&raw, conn, &io, life))
                };
                let status = response_status(&resp);
                if let Err(e) = write
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .write_all(&resp)
                {
                    println!("rtsp: conn {conn} write failed ({method}): {e}");
                    break 'read;
                }
                println!("rtsp: conn {conn} {method} -> {}", status_str(status));
                if status == Some(453) {
                    // Plan: refused "without disturbing the first" — say so.
                    println!("rtsp: second client refused (453), active session undisturbed");
                }
                if actions.contains(&SessionAction::TearDownServer) {
                    // Behaviour (d): tear the connection down from here.
                    break 'read;
                }
            }
            Ok(Some(Decoded::Interleaved { channel, payload })) => {
                // TODO(Todo 13): route RTCP receiver reports (odd channels)
                // to the RTCP handler; RTP (even channels) from a client is
                // unexpected and dropped. Consuming them here is what keeps
                // the byte stream in sync.
                let _ = (channel, payload);
            }
            Ok(None) => match stream.read(&mut chunk) {
                Ok(0) => break 'read,
                Ok(n) => decoder.push(&chunk[..n]),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => {
                    let reason = if e.kind() == io::ErrorKind::TimedOut {
                        "read timeout (no keepalive within 180 s)"
                    } else {
                        "read error"
                    };
                    println!("rtsp: conn {conn} closing ({reason}: {e})");
                    break 'read;
                }
            },
            Err(e) => {
                println!("rtsp: conn {conn} protocol error, closing ({e})");
                break 'read;
            }
        }
    }

    let actions = {
        let mut srv = server
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        with_session(|life| srv.on_disconnect(conn, life))
    };
    if !actions.is_empty() {
        println!("rtsp: conn {conn} disconnect applied {actions:?}");
    }
}

fn status_str(status: Option<u16>) -> String {
    status
        .map(|s| s.to_string())
        .unwrap_or_else(|| "?".to_string())
}

fn first_token(raw: &[u8]) -> String {
    std::str::from_utf8(raw)
        .ok()
        .and_then(|s| s.split_whitespace().next())
        .map(|s| s.chars().take(24).collect())
        .unwrap_or_else(|| "<binary>".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rtsp::server::Track;

    fn read_response(c: &mut TcpStream) -> String {
        let mut buf: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 1024];
        loop {
            if let Some(msg) = complete_message(&buf) {
                buf.truncate(msg);
                return String::from_utf8_lossy(&buf).to_string();
            }
            let n = c.read(&mut chunk).expect("response read");
            assert!(n > 0, "connection closed before a complete response");
            buf.extend_from_slice(&chunk[..n]);
        }
    }

    fn complete_message(buf: &[u8]) -> Option<usize> {
        let head_end = buf.windows(4).position(|w| w == b"\r\n\r\n")?;
        let head = std::str::from_utf8(&buf[..head_end]).ok()?;
        let mut len = 0usize;
        for line in head.split("\r\n") {
            if let Some((name, value)) = line.split_once(':') {
                if name.trim().eq_ignore_ascii_case("content-length") {
                    len = value.trim().parse().ok()?;
                }
            }
        }
        let total = head_end + 4 + len;
        (buf.len() >= total).then_some(total)
    }

    fn send(c: &mut TcpStream, text: &str) {
        c.write_all(text.as_bytes()).expect("request write");
    }

    fn status_of(response: &str) -> u16 {
        response_status(response.as_bytes()).expect("status line")
    }

    fn header_of<'a>(response: &'a str, name: &str) -> Option<&'a str> {
        response.lines().find_map(|line| {
            let (k, v) = line.split_once(':')?;
            if k.trim().eq_ignore_ascii_case(name) {
                Some(v.trim())
            } else {
                None
            }
        })
    }

    #[test]
    fn decoder_handles_split_and_coalesced_messages() {
        let mut d = FrameDecoder::new();
        d.push(b"OPT");
        assert_eq!(d.next().unwrap(), None);
        d.push(b"IONS rtsp://h/mjpeg/1 RTSP/1.0\r\nCSeq: 1\r\n\r\n");
        match d.next().unwrap() {
            Some(Decoded::Rtsp(raw)) => assert!(raw.starts_with(b"OPTIONS")),
            other => panic!("unexpected: {other:?}"),
        }
        assert_eq!(d.next().unwrap(), None);

        // Two messages coalesced into one segment.
        let mut d = FrameDecoder::new();
        d.push(b"A rtsp://h/x RTSP/1.0\r\nCSeq: 1\r\n\r\nB rtsp://h/x RTSP/1.0\r\nCSeq: 2\r\n\r\n");
        assert!(matches!(d.next().unwrap(), Some(Decoded::Rtsp(_))));
        assert!(matches!(d.next().unwrap(), Some(Decoded::Rtsp(_))));
        assert_eq!(d.next().unwrap(), None);
    }

    #[test]
    fn decoder_handles_bodies_and_interleaved_frames() {
        let mut d = FrameDecoder::new();
        d.push(
            b"GET_PARAMETER rtsp://h/x RTSP/1.0\r\nCSeq: 1\r\nContent-Length: 5\r\n\r\nabcde",
        );
        match d.next().unwrap() {
            Some(Decoded::Rtsp(raw)) => assert!(raw.ends_with(b"abcde")),
            other => panic!("unexpected: {other:?}"),
        }

        // Interleaved frame split across pushes.
        let mut d = FrameDecoder::new();
        d.push(&[0x24, 0x01, 0x00]);
        assert_eq!(d.next().unwrap(), None);
        d.push(&[0x03, 0xAA, 0xBB, 0xCC]);
        assert_eq!(
            d.next().unwrap(),
            Some(Decoded::Interleaved {
                channel: 1,
                payload: vec![0xAA, 0xBB, 0xCC]
            })
        );
    }

    #[test]
    fn decoder_rejects_oversized_headers_and_bodies() {
        let mut d = FrameDecoder::new();
        d.push(&vec![b'X'; MAX_HEADER_BYTES + 1]);
        assert_eq!(d.next(), Err(DecodeError::HeaderTooLarge));

        let mut d = FrameDecoder::new();
        d.push(
            format!(
                "SETUP rtsp://h/x RTSP/1.0\r\nCSeq: 1\r\nContent-Length: {}\r\n\r\n",
                MAX_BODY_BYTES + 1
            )
            .as_bytes(),
        );
        assert_eq!(d.next(), Err(DecodeError::BodyTooLarge));
    }

    #[test]
    fn loopback_service_serves_the_full_lifecycle_and_interleaved_rtp() {
        let mut service = Service::spawn(0).expect("bind ephemeral");
        let addr = service.local_addr();
        let mut client = TcpStream::connect(addr).expect("connect");
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();

        send(&mut client, "OPTIONS rtsp://x/mjpeg/1 RTSP/1.0\r\nCSeq: 1\r\n\r\n");
        let resp = read_response(&mut client);
        assert_eq!(status_of(&resp), 200);
        assert!(header_of(&resp, "Public").unwrap().contains("DESCRIBE"));

        send(
            &mut client,
            "DESCRIBE rtsp://x/mjpeg/1 RTSP/1.0\r\nCSeq: 2\r\nAccept: application/sdp\r\n\r\n",
        );
        let resp = read_response(&mut client);
        assert_eq!(status_of(&resp), 200);
        assert!(resp.contains("a=rtpmap:0 PCMU/8000"));
        assert!(resp.contains("a=rtpmap:26 JPEG/90000"));

        send(
            &mut client,
            "SETUP rtsp://x/mjpeg/1/trackID=0 RTSP/1.0\r\nCSeq: 3\r\nTransport: RTP/AVP/TCP;unicast;interleaved=0-1\r\n\r\n",
        );
        let resp = read_response(&mut client);
        assert_eq!(status_of(&resp), 200);
        let session = header_of(&resp, "Session")
            .expect("Session header")
            .split(';')
            .next()
            .unwrap()
            .to_string();

        send(
            &mut client,
            &format!("PLAY rtsp://x/mjpeg/1 RTSP/1.0\r\nCSeq: 4\r\nSession: {session}\r\n\r\n"),
        );
        let resp = read_response(&mut client);
        assert_eq!(status_of(&resp), 200);
        assert!(resp.contains("RTP-Info:"));

        // Todo 12's send hook over the REAL socket: RTP arrives pre-framed.
        let sender = {
            let srv = service.server();
            let srv = srv.lock().unwrap_or_else(|p| p.into_inner());
            srv.sender(Track::Video)
                .expect("video sender wired after SETUP")
        };
        sender.send_rtp(&[0xDE, 0xAD, 0xBE]).expect("interleaved send");
        let mut frame = [0u8; 7];
        client.read_exact(&mut frame).expect("interleaved frame");
        assert_eq!(frame, [0x24, 0x00, 0x00, 0x03, 0xDE, 0xAD, 0xBE]);

        // A second connection is refused over the wire; first stays intact.
        let mut second = TcpStream::connect(addr).expect("connect 2");
        second
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        send(
            &mut second,
            "SETUP rtsp://x/mjpeg/1/trackID=0 RTSP/1.0\r\nCSeq: 1\r\nTransport: RTP/AVP/TCP;unicast;interleaved=0-1\r\n\r\n",
        );
        let resp = read_response(&mut second);
        assert_eq!(status_of(&resp), 453);
        drop(second);
        {
            let srv = service.server();
            let srv = srv.lock().unwrap_or_else(|p| p.into_inner());
            let active = srv.active_session().expect("first session intact");
            assert_eq!(active.id, session);
        }

        send(
            &mut client,
            &format!(
                "TEARDOWN rtsp://x/mjpeg/1 RTSP/1.0\r\nCSeq: 5\r\nSession: {session}\r\n\r\n"
            ),
        );
        let resp = read_response(&mut client);
        assert_eq!(status_of(&resp), 200);
        {
            let srv = service.server();
            let srv = srv.lock().unwrap_or_else(|p| p.into_inner());
            assert!(srv.active_session().is_none());
        }

        drop(client);
        service.shutdown();
    }
}
