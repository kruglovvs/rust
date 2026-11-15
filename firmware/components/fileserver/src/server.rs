//! The connection model and the crate's only entry point.
//!
//! # Contract
//!
//! ```text
//! pub struct Config { pub port: u16, pub mount_point: &'static str,
//!                     pub rtsp_port: u16, pub rtsp_path: &'static str,
//!                     pub rtsp_running: bool, pub recording_enabled: bool }
//! pub enum Error {
//!     Netif(esp_idf_sys::EspError),          // LwIP could not be initialised
//!     Bind(std::io::Error),                  // the listen socket could not bind/listen
//!     Spawn(std::io::Error),                 // a thread could not be created
//!     Budget { free: usize, required: usize },// less internal RAM than the stacks need
//! }
//! pub struct Server;
//! pub fn start(config: Config) -> Result<Server, Error>;
//! impl Server {
//!     pub fn local_addr(&self) -> std::net::SocketAddr;
//!     pub fn stop(&self);
//! }
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
//! # Connection model
//!
//! One inline accept loop cannot both serve a multi-hundred-megabyte download
//! and answer a status request, so the model is fixed:
//!
//! - **One accept thread** (16 KiB stack) calls [`std::net::TcpListener::accept`]
//!   and nothing else. It never reads a request byte: it hands the accepted
//!   stream to a worker or answers the connection itself.
//! - **Two worker threads** (24 KiB stacks each) are created once by [`start`]
//!   and live for the life of the server. Each owns one job channel and blocks
//!   on it when idle; a connection is never served by a thread spawned for it,
//!   so the thread count cannot grow with the client count.
//! - **At most two workers run at once.** Admission is a two-bit slot set
//!   ([`Slots`]): the accept thread takes a free slot, sends the stream to that
//!   slot's worker and moves the guard along with the job, so the slot is held
//!   for exactly the connection and released when the worker drops it. When
//!   both slots are taken the accept thread writes `HTTP/1.1 503` with the JSON
//!   `ApiError` body on its own and closes, which is why a long download can
//!   never make the status endpoint wait.
//! - **Per connection:** a 10 s read timeout and a 10 s write timeout, at most
//!   [`MAX_HEAD`] (8192) bytes of request head, one request, one response, then
//!   close. The response always carries `Connection: close`, so a client that
//!   tries to pipeline a second request is not served twice.
//! - **Log lines:** one line per request,
//!   `filesrv: <method> <path> -> <status> <bytes>`, written by the worker that
//!   served it, and `filesrv: 503 busy` written by the accept thread for a
//!   rejected connection. A head that never completes - over 8192 bytes, at EOF,
//!   or still unfinished after the read timeout - is answered `400` and logged
//!   with `- -` in place of the method and path, because no request was parsed.
//!   A connection that disappears before a response can be written is logged as
//!   `filesrv: connection dropped: <reason>`; nothing is answered.
//!
//! # Request path
//!
//! The worker reads the head into an 8192-byte buffer, [`crate::http::parse`]s
//! it, [`crate::router::route`]s the method and path, builds the JSON body with
//! [`crate::json`] or the download decision with [`crate::body::download`], and
//! streams the body through [`crate::body::write_body`] with a
//! [`crate::body::CardSource`]. The status snapshot comes from
//! [`crate::status::gather`]. A download path is validated by
//! [`crate::path::map`] (traversal, NUL, bad escapes) and any rejection is a
//! `404`, so a malformed path never reveals whether a file exists.
//!
//! The head buffer is allocated on the heap rather than on the worker stack:
//! the worker already holds [`crate::body::CHUNK`] (4 KiB) while it streams and
//! builds a JSON body, and 8 KiB more on a 24 KiB stack would leave too little
//! for the standard-library call frames.
//!
//! # Response heads
//!
//! Every response except a download is serialised with
//! [`crate::http::write_head`] after sizing it with [`crate::http::head_len`].
//! A `206`/`416` download carries `Content-Range: bytes <start>-<end>/<total>`,
//! a value that only exists at run time; [`crate::http::ResponseHead::extra`]
//! is `&'static`, so a download head is built by [`download_head`] instead. It
//! emits the same lines in the same order (proven by
//! `download_head_matches_static_writer`), which keeps `Content-Length`,
//! `Connection: close` and the status line identical across both paths.
//!
//! # Stack budget
//!
//! 2 x 24 KiB of worker stack plus 16 KiB of accept stack is about 64 KiB of
//! internal SRAM spent before a single request is served. [`start`] therefore
//! reads `heap_caps_get_free_size(MALLOC_CAP_INTERNAL)` after the threads exist
//! and refuses to run with less than 64 KiB left. This is the crate's second
//! device call after [`crate::status`]; it exists because the stacks are the
//! server's own allocation and a boot with no headroom left would starve the
//! media pipeline instead of failing honestly.

use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use esp_idf_sys as sys;

use crate::body::{self, Body, CardSource, FileSource};
use crate::http::{self, ResponseHead, Status};
use crate::json;
use crate::path;
use crate::range;
use crate::router::{self, Route};
use crate::status;

/// The content type of every JSON response.
const JSON: &str = "application/json";

/// The content type of a file download.
const OCTET_STREAM: &str = "application/octet-stream";

/// The card directory the `/api/v1/files` listing reports.
const LIST_DIR: &str = "rec";

/// Room for any response head this server writes; the largest is about 200
/// bytes, and a short buffer is reported instead of truncating one silently.
const HEAD_BUFFER: usize = 512;

/// How many connections can be served at once.
const WORKERS: usize = 2;

/// Stack of the accept thread, in bytes. It only accepts and writes a `503`.
const ACCEPT_STACK: usize = 16 * 1024;

/// Stack of one worker thread, in bytes. It parses, routes and streams.
const WORKER_STACK: usize = 24 * 1024;

/// Read and write timeout of one connection.
const IO_TIMEOUT: Duration = Duration::from_secs(10);

/// Largest request head the worker reads: the whole head including its final
/// blank line, exactly [`crate::http::MAX_HEAD`].
const MAX_HEAD: usize = crate::http::MAX_HEAD;

/// Internal SRAM that must remain free after the server's threads exist.
const MIN_INTERNAL_FREE: usize = 64 * 1024;

/// How long the accept loop backs off after an accept error that is neither a
/// stop signal nor an interrupted call.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(20);

/// The log line of a connection rejected because both worker slots were busy.
const BUSY_LOG: &str = "filesrv: 503 busy";

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
    /// The listen socket could not bind or listen (for example the port is taken).
    Bind(std::io::Error),
    /// A thread could not be created.
    Spawn(std::io::Error),
    /// Fewer than [`MIN_INTERNAL_FREE`] bytes of internal SRAM remained after the
    /// server's stacks were allocated.
    Budget { free: usize, required: usize },
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Netif(error) => write!(f, "network stack unavailable: {error}"),
            Error::Bind(error) => write!(f, "listen socket unavailable: {error}"),
            Error::Spawn(error) => write!(f, "server thread not created: {error}"),
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
            Error::Bind(error) | Error::Spawn(error) => Some(error),
            Error::Netif(_) | Error::Budget { .. } => None,
        }
    }
}

/// A running server: the listener address, the stop flag and the accept thread.
pub struct Server {
    local_addr: SocketAddr,
    stopped: Arc<AtomicBool>,
    accept: Mutex<Option<JoinHandle<()>>>,
}

impl Server {
    /// The address the listen socket is bound to. The server binds every
    /// interface, so the IP is `0.0.0.0`; only the port is meaningful.
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Stop accepting connections and wait for the accept thread to exit.
    ///
    /// Idempotent: only the first call acts. Requests already being served
    /// finish on their workers, which exit as soon as the job channel closes.
    pub fn stop(&self) {
        if self.stopped.swap(true, Ordering::AcqRel) {
            return;
        }
        let handle = self.accept.lock().ok().and_then(|mut slot| slot.take());
        if let Some(handle) = handle {
            let _ = handle.join();
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Start the server on `config.port`.
///
/// Brings LwIP up (idempotent, so the boot path calling it first is fine), binds
/// every interface on `config.port` (`0` picks an ephemeral port), creates the
/// two workers and the accept thread, then checks that the internal heap still
/// has [`MIN_INTERNAL_FREE`] bytes. A server that started and then finds the
/// budget gone is stopped again before the typed error is returned.
pub fn start(config: Config) -> Result<Server, Error> {
    net::ensure_netif_initialised().map_err(Error::Netif)?;

    let listener =
        TcpListener::bind(SocketAddr::from(([0, 0, 0, 0], config.port))).map_err(Error::Bind)?;
    listener.set_nonblocking(true).map_err(Error::Bind)?;
    let local_addr = listener.local_addr().map_err(Error::Bind)?;

    let source = Arc::new(CardSource::new(config.mount_point));
    let slots = Slots::new();
    let stop = Arc::new(AtomicBool::new(false));

    let mut senders = Vec::with_capacity(WORKERS);
    for index in 0..WORKERS {
        let (sender, receiver) = mpsc::channel();
        let worker_source = Arc::clone(&source);
        thread::Builder::new()
            .name(format!("filesrv-worker-{index}"))
            .stack_size(WORKER_STACK)
            .spawn(move || worker_loop(receiver, config, worker_source))
            .map_err(Error::Spawn)?;
        senders.push(sender);
    }

    let accept_senders = senders.clone();
    let accept_slots = Arc::clone(&slots);
    let accept_stop = Arc::clone(&stop);
    let accept = thread::Builder::new()
        .name("filesrv-accept".into())
        .stack_size(ACCEPT_STACK)
        .spawn(move || accept_loop(listener, accept_slots, accept_senders, accept_stop))
        .map_err(Error::Spawn)?;

    let server = Server {
        local_addr,
        stopped: stop,
        accept: Mutex::new(Some(accept)),
    };

    let free = internal_free();
    if free < MIN_INTERNAL_FREE {
        server.stop();
        return Err(Error::Budget {
            free,
            required: MIN_INTERNAL_FREE,
        });
    }

    Ok(server)
}

/// Which worker owns which connection, as a two-bit set.
struct Slots {
    busy: AtomicUsize,
}

impl Slots {
    fn new() -> Arc<Slots> {
        Arc::new(Slots {
            busy: AtomicUsize::new(0),
        })
    }

    /// Take the lowest idle worker, or `None` when both are busy.
    fn try_acquire(self: &Arc<Self>) -> Option<Slot> {
        let mask = (1usize << WORKERS) - 1;
        let mut current = self.busy.load(Ordering::Acquire);
        loop {
            let idle = !current & mask;
            if idle == 0 {
                return None;
            }
            let index = idle.trailing_zeros() as usize;
            let taken = current | (1usize << index);
            match self.busy.compare_exchange_weak(
                current,
                taken,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    return Some(Slot {
                        slots: Arc::clone(self),
                        index,
                    })
                }
                Err(actual) => current = actual,
            }
        }
    }

    #[cfg(test)]
    fn busy_count(&self) -> usize {
        self.busy.load(Ordering::Acquire).count_ones() as usize
    }
}

/// One occupied worker slot. Dropping it hands the worker back to [`Slots`].
struct Slot {
    slots: Arc<Slots>,
    index: usize,
}

impl Slot {
    fn index(&self) -> usize {
        self.index
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.slots
            .busy
            .fetch_and(!(1usize << self.index), Ordering::Release);
    }
}

/// One connection on its way to its worker, with the slot it occupies.
struct Job {
    stream: TcpStream,
    _slot: Slot,
}

fn worker_loop(jobs: mpsc::Receiver<Job>, config: Config, source: Arc<CardSource>) {
    while let Ok(job) = jobs.recv() {
        let Job { stream, _slot } = job;
        handle_connection(stream, &config, &source);
    }
}

fn accept_loop(
    listener: TcpListener,
    slots: Arc<Slots>,
    senders: Vec<mpsc::Sender<Job>>,
    stop: Arc<AtomicBool>,
) {
    while !stop.load(Ordering::Acquire) {
        let stream = match listener.accept() {
            Ok((stream, _peer)) => stream,
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                // The listener is non-blocking so `stop` never depends on a
                // loopback route to wake the thread.
                thread::sleep(ACCEPT_BACKOFF);
                continue;
            }
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) => {
                logging::log!("filesrv: accept failed: {error}");
                thread::sleep(ACCEPT_BACKOFF);
                continue;
            }
        };
        if stop.load(Ordering::Acquire) {
            break;
        }
        match slots.try_acquire() {
            Some(slot) => {
                let index = slot.index();
                if senders[index]
                    .send(Job {
                        stream,
                        _slot: slot,
                    })
                    .is_err()
                {
                    logging::log!("filesrv: worker {index} is gone; connection dropped");
                }
            }
            None => busy_response(stream),
        }
    }
}

fn busy_response(mut stream: TcpStream) {
    // The accept thread must never wait on a client, not even for this write.
    let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
    let mut body = String::new();
    json::error_body(
        Status::SERVICE_UNAVAILABLE,
        "both worker slots are busy; retry shortly",
        &mut body,
    );
    let head = ResponseHead::new(Status::SERVICE_UNAVAILABLE, JSON, body.len() as u64);
    let mut buffer = [0u8; HEAD_BUFFER];
    let needed = http::head_len(&head);
    let written = http::write_head(&mut buffer, &head);
    if written == needed {
        let _ = stream.write_all(&buffer[..written]);
        let _ = stream.write_all(body.as_bytes());
        let _ = stream.flush();
    }
    discard_pending(&mut stream);
    logging::log!("{BUSY_LOG}");
}

/// Read whatever the client already sent without waiting for more.
///
/// Closing a socket with unread request bytes makes the stack answer with RST,
/// which can discard a `503` the client has not read yet. This is not parsing:
/// the bytes are thrown away, the read is non-blocking, and the amount is
/// bounded.
fn discard_pending(stream: &mut TcpStream) {
    if stream.set_nonblocking(true).is_err() {
        return;
    }
    let mut buffer = [0u8; 256];
    let mut total = 0;
    while total < 4096 {
        match stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => total += read,
            Err(_) => break,
        }
    }
}

/// Why a request head was not read.
enum HeadError {
    /// Filled the whole buffer without seeing the blank line.
    TooLarge,
    /// The read timeout expired first.
    TimedOut,
    /// The client closed before the head was complete.
    Closed,
    /// Any other socket error, by kind so the log needs no allocation.
    Io(ErrorKind),
}

fn read_head(stream: &mut TcpStream, buffer: &mut [u8]) -> Result<usize, HeadError> {
    let mut filled = 0;
    loop {
        if filled == buffer.len() {
            return Err(HeadError::TooLarge);
        }
        match stream.read(&mut buffer[filled..]) {
            Ok(0) => return Err(HeadError::Closed),
            Ok(read) => {
                filled += read;
                if head_is_complete(&buffer[..filled]) {
                    return Ok(filled);
                }
            }
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                return Err(HeadError::TimedOut)
            }
            Err(error) => return Err(HeadError::Io(error.kind())),
        }
    }
}

/// Whether the head has reached its terminating blank line. A bare `\n\n` ends
/// the read too, so [`crate::http::parse`] can reject an LF-only request as
/// malformed instead of the worker waiting for the timeout.
fn head_is_complete(bytes: &[u8]) -> bool {
    bytes.windows(4).any(|window| window == b"\r\n\r\n")
        || bytes.windows(2).any(|window| window == b"\n\n")
}

fn handle_connection(mut stream: TcpStream, config: &Config, source: &CardSource) {
    let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
    let _ = stream.set_write_timeout(Some(IO_TIMEOUT));

    // Heap, not stack: the worker already holds a 4 KiB chunk buffer and the
    // parse/build frames on a 24 KiB stack.
    let mut head = vec![0u8; MAX_HEAD];
    let filled = match read_head(&mut stream, &mut head) {
        Ok(filled) => filled,
        Err(HeadError::Closed) => {
            logging::log!("filesrv: connection dropped: closed before a request head");
            return;
        }
        Err(HeadError::Io(kind)) => {
            logging::log!("filesrv: connection dropped: {kind:?}");
            return;
        }
        Err(HeadError::TimedOut) => {
            let bytes = send_json_error(
                &mut stream,
                Status::BAD_REQUEST,
                "no complete request head within 10 s",
            );
            logging::log!("filesrv: - - -> {} {bytes}", Status::BAD_REQUEST.code());
            return;
        }
        Err(HeadError::TooLarge) => {
            let bytes = send_json_error(
                &mut stream,
                Status::BAD_REQUEST,
                "request head exceeds 8192 bytes",
            );
            logging::log!("filesrv: - - -> {} {bytes}", Status::BAD_REQUEST.code());
            return;
        }
    };

    let request = match http::parse(&head[..filled]) {
        Ok(request) => request,
        Err(error) => {
            let bytes = send_json_error(&mut stream, Status::BAD_REQUEST, &error.to_string());
            logging::log!("filesrv: - - -> {} {bytes}", Status::BAD_REQUEST.code());
            return;
        }
    };

    let (status, bytes) = serve_request(&mut stream, &request, &head[..filled], config, source);
    let method = raw_method(&head[..filled]).unwrap_or_else(|| request.method.as_str());
    logging::log!("{}", log_line(method, request.path, status, bytes));
}

/// One response, before its head is written.
struct Response {
    status: Status,
    content_length: u64,
    content_range: Option<String>,
    /// Whether this is a file body, which is the only thing that needs a
    /// dynamic `Content-Range` head.
    download: bool,
    body: Body,
}

impl Response {
    fn json(body: String) -> Response {
        Response {
            status: Status::OK,
            content_length: body.len() as u64,
            content_range: None,
            download: false,
            body: Body::Json(body),
        }
    }

    fn document(text: &'static str) -> Response {
        Response {
            status: Status::OK,
            content_length: text.len() as u64,
            content_range: None,
            download: false,
            body: Body::Bytes(text.as_bytes()),
        }
    }

    fn download(download: body::Download) -> Response {
        Response {
            status: download.status,
            content_length: download.content_length,
            content_range: download.content_range,
            download: true,
            body: download.body,
        }
    }
}

fn serve_request(
    stream: &mut TcpStream,
    request: &http::Request<'_>,
    head: &[u8],
    config: &Config,
    source: &CardSource,
) -> (u16, u64) {
    let response = match response_for(request, head, config, source) {
        Ok(response) => response,
        Err((status, message)) => {
            let bytes = send_json_error(stream, status, message);
            return (status.code(), bytes);
        }
    };
    let status = response.status.code();

    let head_written = if response.download {
        let text = download_head(
            response.status,
            response.content_length,
            response.content_range.as_deref(),
        );
        stream.write_all(text.as_bytes())
    } else {
        let descriptor = ResponseHead::new(response.status, JSON, response.content_length);
        write_static_head(stream, &descriptor)
    };
    if let Err(error) = head_written {
        logging::log!("filesrv: response head not written: {error}");
        return (status, 0);
    }

    let body = body::body_for(request.method, response.body);
    match body::write_body(&body, source, stream) {
        Ok(bytes) => (status, bytes),
        Err(error) => {
            logging::log!("filesrv: body aborted: {error}");
            (status, 0)
        }
    }
}

fn response_for(
    request: &http::Request<'_>,
    head: &[u8],
    config: &Config,
    source: &CardSource,
) -> Result<Response, (Status, &'static str)> {
    match router::route(request.method, request.path) {
        Err(status) => Err((status, error_message(status))),
        Ok(route) => match route {
            Route::Index => {
                let mut body = String::new();
                json::index_body(&mut body);
                Ok(Response::json(body))
            }
            Route::Status => {
                let mut body = String::new();
                json::status_body(&status::gather(config).snapshot, &mut body);
                Ok(Response::json(body))
            }
            Route::Files => {
                let mut body = String::new();
                json::list_body(LIST_DIR, source, &mut body, json::LIST_CAP);
                Ok(Response::json(body))
            }
            Route::OpenApi => Ok(Response::document(json::openapi_body())),
            Route::Download => download_response(request, head, config, source),
        },
    }
}

fn download_response(
    request: &http::Request<'_>,
    head: &[u8],
    config: &Config,
    source: &CardSource,
) -> Result<Response, (Status, &'static str)> {
    let encoded = router::download_relative(Route::Download, request.path)
        .ok_or((Status::NOT_FOUND, "no such file"))?;
    let mapped =
        path::map(encoded, config.mount_point).map_err(|_| (Status::NOT_FOUND, "no such file"))?;
    let relative = mapped
        .strip_prefix(config.mount_point)
        .and_then(|rest| rest.strip_prefix('/'))
        .ok_or((Status::NOT_FOUND, "no such file"))?;

    let total = match source.len(relative) {
        Ok(total) => total,
        Err(body::Error::NotFound) => return Err((Status::NOT_FOUND, "no such file")),
        Err(_) => return Err((Status::INTERNAL_SERVER_ERROR, "the card could not be read")),
    };
    let selected = range::parse(http::header(head, "Range"), total);
    Ok(Response::download(body::download(
        relative, total, selected,
    )))
}

fn error_message(status: Status) -> &'static str {
    match status {
        Status::METHOD_NOT_ALLOWED => "GET or HEAD only",
        Status::NOT_FOUND => "no such endpoint",
        _ => "request refused",
    }
}

fn write_static_head(stream: &mut TcpStream, head: &ResponseHead) -> std::io::Result<()> {
    let mut buffer = [0u8; HEAD_BUFFER];
    let needed = http::head_len(head);
    let written = http::write_head(&mut buffer, head);
    if written != needed {
        return Err(std::io::Error::new(
            ErrorKind::Other,
            "response head does not fit",
        ));
    }
    stream.write_all(&buffer[..written])
}

/// The head of a download, which is the only response with a run-time header
/// value. See the [module contract](self).
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

fn send_json_error(stream: &mut TcpStream, status: Status, message: &str) -> u64 {
    let mut body = String::new();
    json::error_body(status, message, &mut body);
    let head = ResponseHead::new(status, JSON, body.len() as u64);
    if write_static_head(stream, &head).is_err() {
        return 0;
    }
    match stream.write_all(body.as_bytes()) {
        Ok(()) => body.len() as u64,
        Err(_) => 0,
    }
}

/// The request-line method for the log. [`crate::http::Method`] deliberately
/// collapses every verb it does not answer into `OTHER`; the log reports what
/// the client actually sent.
fn raw_method(head: &[u8]) -> Option<&str> {
    let line_end = head.windows(2).position(|window| window == b"\r\n")?;
    let line = &head[..line_end];
    let space = line.iter().position(|byte| *byte == b' ')?;
    core::str::from_utf8(&line[..space])
        .ok()
        .filter(|token| !token.is_empty())
}

fn log_line(method: &str, path: &str, status: u16, bytes: u64) -> String {
    format!("filesrv: {method} {path} -> {status} {bytes}")
}

fn internal_free() -> usize {
    // SAFETY: `heap_caps_get_free_size` is a plain getter with no preconditions.
    unsafe { sys::heap_caps_get_free_size(sys::MALLOC_CAP_INTERNAL) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream};
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    /// One request for the status endpoint, the cheapest route that needs no card.
    const STATUS_REQUEST: &[u8] = b"GET /api/v1/status HTTP/1.1\r\nHost: board\r\n\r\n";

    /// The reference extra set [`http::write_head`] formats, used to prove the
    /// download writer emits the same head.
    static ACCEPT_RANGES_HEADER: [(&str, &str); 1] = [body::ACCEPT_RANGES];

    /// Socket tests share this binary and the board's sixteen-socket pool, so
    /// they run one at a time. A panicking test must not poison the lock for the
    /// rest of the suite.
    static SOCKETS: Mutex<()> = Mutex::new(());

    fn sockets() -> std::sync::MutexGuard<'static, ()> {
        SOCKETS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn test_config(port: u16) -> Config {
        Config {
            port,
            mount_point: storage::BASE_PATH,
            rtsp_port: 554,
            rtsp_path: "/live",
            rtsp_running: true,
            recording_enabled: true,
        }
    }

    fn start_on_port_zero() -> Server {
        net::ensure_netif_initialised().expect("LwIP is up before the first socket");
        start(test_config(0)).expect("the server starts on an ephemeral port")
    }

    /// The address a client on the board reaches the listener on. The listener
    /// binds every interface, so its own address is `0.0.0.0`; LwIP's loopback
    /// netif serves `127.0.0.1`.
    fn loopback(server: &Server) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), server.local_addr().port())
    }

    fn connect(server: &Server) -> TcpStream {
        let stream = TcpStream::connect(loopback(server)).expect("the client connects");
        stream
            .set_read_timeout(Some(Duration::from_secs(20)))
            .expect("the client read timeout outlasts the server's 10 s");
        stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .expect("the client write timeout");
        stream
    }

    /// Read until the server closes the one-request connection: the close is the
    /// event every response ends with.
    fn read_to_close(stream: &mut TcpStream) -> String {
        let mut all = std::vec::Vec::new();
        let mut buffer = [0u8; 512];
        loop {
            match stream.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => all.extend_from_slice(&buffer[..read]),
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => panic!("reading the response failed: {error}"),
            }
        }
        String::from_utf8(all).expect("the server writes UTF-8 heads and JSON bodies")
    }

    fn body_of(response: &str) -> &str {
        match response.split_once("\r\n\r\n") {
            Some((_, body)) => body,
            None => "",
        }
    }

    fn get_status(server: &Server) -> String {
        let mut stream = connect(server);
        stream
            .write_all(STATUS_REQUEST)
            .expect("the request is written");
        read_to_close(&mut stream)
    }

    /// Wait until both worker slots are taken: a request that still gets `200`
    /// proves a slot was free, so it is retried. Every retry closes its own
    /// connection, which releases the slot it used.
    fn probe_until_busy(server: &Server) -> String {
        for attempt in 0..8 {
            let response = get_status(server);
            if response.starts_with("HTTP/1.1 503") {
                return response;
            }
            assert!(
                response.starts_with("HTTP/1.1 200"),
                "attempt {attempt} answered neither 200 nor 503: {response}"
            );
        }
        panic!("eight requests found a free worker slot every time");
    }

    #[test]
    fn config_is_data_only() {
        fn plain<T: Copy + Send + Sync + 'static>() {}
        plain::<Config>();
        assert!(
            !std::mem::needs_drop::<Config>(),
            "Config owns nothing that needs dropping"
        );

        let config = test_config(8080);
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
    fn start_on_port_zero_succeeds() {
        let _guard = sockets();
        let server = start_on_port_zero();
        assert!(server.local_addr().port() != 0);
    }

    #[test]
    fn local_addr_is_not_zero() {
        let _guard = sockets();
        let server = start_on_port_zero();
        let address = server.local_addr();
        assert_ne!(address.port(), 0, "the kernel assigned an ephemeral port");
        assert!(
            address.port() >= 1024,
            "an ephemeral port is never a privileged one: {address}"
        );
    }

    #[test]
    fn stop_is_idempotent() {
        let _guard = sockets();
        let server = start_on_port_zero();
        let address = loopback(&server);
        server.stop();
        server.stop();
        assert!(
            TcpStream::connect(address).is_err(),
            "a stopped server no longer accepts connections"
        );
        // Dropping a stopped server runs stop() a third time, which must also be
        // a no-op rather than a panic.
        drop(server);
    }

    #[test]
    fn second_start_on_same_port_fails_typed() {
        let _guard = sockets();
        let first = start_on_port_zero();
        let port = first.local_addr().port();
        let second = start(Config {
            port,
            ..test_config(port)
        });
        match second {
            Err(Error::Bind(error)) => assert_eq!(
                error.kind(),
                std::io::ErrorKind::AddrInUse,
                "the second listener reports the address as taken: {error}"
            ),
            Err(other) => panic!("wrong error for a taken port: {other}"),
            Ok(_) => panic!("a second server bound port {port} while the first was listening"),
        }
        first.stop();
    }

    #[test]
    fn status_line_formatting() {
        assert_eq!(
            log_line("GET", "/api/v1/status", 200, 312),
            "filesrv: GET /api/v1/status -> 200 312"
        );
        assert_eq!(
            log_line("POST", "/api/v1/status", 405, 0),
            "filesrv: POST /api/v1/status -> 405 0"
        );
        assert_eq!(
            log_line("HEAD", "/api/v1/files/rec/REC_0001.avi", 206, 1024),
            "filesrv: HEAD /api/v1/files/rec/REC_0001.avi -> 206 1024"
        );
        assert_eq!(BUSY_LOG, "filesrv: 503 busy");
    }

    #[test]
    fn worker_slots_cap_at_two() {
        let slots = Slots::new();
        let first = slots.try_acquire().expect("the first slot is free");
        let second = slots.try_acquire().expect("the second slot is free");
        assert_ne!(
            first.index(),
            second.index(),
            "each slot names its own worker"
        );
        assert_eq!(slots.busy_count(), WORKERS);
        assert!(
            slots.try_acquire().is_none(),
            "a third connection must be rejected, not queued"
        );
        assert_eq!(
            slots.busy_count(),
            WORKERS,
            "a rejected take changes nothing"
        );
    }

    #[test]
    fn worker_slot_released_on_drop() {
        let slots = Slots::new();
        {
            let slot = slots.try_acquire().expect("a slot is free");
            assert_eq!(slot.index(), 0, "the first idle worker takes the job");
            assert_eq!(slots.busy_count(), 1);
        }
        assert_eq!(slots.busy_count(), 0, "dropping the guard frees the slot");

        let again = slots.try_acquire().expect("the released slot is reusable");
        assert_eq!(again.index(), 0);
        let other = slots.try_acquire().expect("the second slot is still free");
        assert_eq!(other.index(), 1);
    }

    #[test]
    fn third_connection_gets_503() {
        let _guard = sockets();
        let server = start_on_port_zero();
        let _first = connect(&server);
        let _second = connect(&server);

        let response = probe_until_busy(&server);
        assert!(
            response.starts_with("HTTP/1.1 503 Service Unavailable"),
            "{response}"
        );
        assert!(
            response.contains("Content-Type: application/json"),
            "the busy answer is JSON like every other error: {response}"
        );
        assert!(
            response.contains("\r\nConnection: close\r\n\r\n"),
            "the busy answer closes the connection: {response}"
        );
        let body: serde_json::Value =
            serde_json::from_str(body_of(&response)).expect("the busy body is JSON");
        assert_eq!(body["error"], "Service Unavailable");
        assert!(
            body["message"]
                .as_str()
                .is_some_and(|text| !text.is_empty()),
            "the busy body explains itself: {body}"
        );
    }

    #[test]
    fn accept_thread_never_parses() {
        let _guard = sockets();
        let server = start_on_port_zero();
        let _first = connect(&server);
        let _second = connect(&server);
        probe_until_busy(&server);

        // Nothing is written on this connection. An accept thread that parsed
        // would wait for a request head and answer 400 after the 10 s read
        // timeout; a 503 with no request byte sent proves it never parsed.
        let mut silent = connect(&server);
        let response = read_to_close(&mut silent);
        assert!(
            response.starts_with("HTTP/1.1 503"),
            "a silent third connection is rejected without a parse: {response}"
        );
    }

    #[test]
    fn oversized_head_is_400() {
        let _guard = sockets();
        let server = start_on_port_zero();
        let mut stream = connect(&server);

        let mut head = std::vec::Vec::with_capacity(MAX_HEAD);
        head.extend_from_slice(b"GET / HTTP/1.1\r\nX-Pad: ");
        head.resize(MAX_HEAD, b'p');
        assert_eq!(head.len(), MAX_HEAD);
        stream
            .write_all(&head)
            .expect("the oversized head is written");

        let response = read_to_close(&mut stream);
        assert!(
            response.starts_with("HTTP/1.1 400 Bad Request"),
            "a head with no blank line inside {MAX_HEAD} bytes is a 400: {response}"
        );
    }

    #[test]
    fn silent_connection_times_out_without_panic() {
        let _guard = sockets();
        let server = start_on_port_zero();
        let mut silent = connect(&server);

        let started = Instant::now();
        let response = read_to_close(&mut silent);
        // The worker answers once its 10 s read timeout expires and closes; the
        // 1 s floor only rules out a response to an empty request.
        assert!(
            started.elapsed() >= Duration::from_secs(1),
            "the server waited for its read timeout before answering"
        );
        assert!(
            response.starts_with("HTTP/1.1 400 Bad Request"),
            "an idle connection is answered and closed, not dropped: {response}"
        );

        let after = get_status(&server);
        assert!(
            after.starts_with("HTTP/1.1 200 OK"),
            "the worker survived the timeout and still serves: {after}"
        );
    }

    #[test]
    fn download_head_matches_static_writer() {
        // A full download's extra set is static, so the download writer must
        // produce byte-identical output to the crate's own head writer.
        let static_head = ResponseHead::new(Status::OK, OCTET_STREAM, 1_048_576)
            .with_extra(&ACCEPT_RANGES_HEADER);
        let mut expected = [0u8; 256];
        let written = http::write_head(&mut expected, &static_head);
        assert_eq!(written, http::head_len(&static_head));
        assert_eq!(
            download_head(Status::OK, 1_048_576, None).as_bytes(),
            &expected[..written],
            "both head writers emit the same lines in the same order"
        );

        let partial = download_head(Status::PARTIAL_CONTENT, 512, Some("bytes 0-511/1024"));
        assert!(partial.starts_with("HTTP/1.1 206 Partial Content\r\n"));
        assert!(partial.contains("Content-Type: application/octet-stream\r\n"));
        assert!(partial.contains("Content-Length: 512\r\n"));
        assert!(partial.contains("Accept-Ranges: bytes\r\n"));
        assert!(partial.contains("Content-Range: bytes 0-511/1024\r\n"));
        assert!(partial.ends_with("Connection: close\r\n\r\n"));
    }
}
