//! Response bodies, the [`FileSource`] seam, the chunk plan and the download
//! decision.
//!
//! # Contract
//!
//! ```text
//! pub enum Body {
//!     Bytes(&'static [u8]),
//!     Json(String),
//!     File { path: String, start: u64, len: u64 },
//! }
//! pub enum Error {
//!     NotFound,
//!     OutOfBounds { start: u64, len: u64, total: u64 },
//!     ShortRead { offset: u64, want: usize, got: usize },
//!     Io(std::io::Error),
//! }
//! pub struct DirEntry { pub name: String, pub is_dir: bool }
//! pub trait FileSource {
//!     fn len(&self, path: &str) -> Result<u64, Error>;
//!     fn read_at(&self, path: &str, offset: u64, buf: &mut [u8]) -> Result<usize, Error>;
//!     fn list(&self, dir: &str) -> Result<Vec<DirEntry>, Error>;
//! }
//! pub struct CardSource { /* the mount it was built for */ }
//! impl CardSource { pub fn new(mount: &str) -> CardSource }
//! pub const CHUNK: usize = 4096;
//! pub const ACCEPT_RANGES: (&str, &str) = ("Accept-Ranges", "bytes");
//! pub struct Chunks;  // Iterator<Item = (u64, usize)>
//! pub fn chunk_plan(total: u64, chunk: usize) -> Chunks;
//! pub struct Download {
//!     pub status: Status,
//!     pub content_length: u64,
//!     pub content_range: Option<String>,
//!     pub body: Body,
//! }
//! pub fn download(path: &str, total: u64, range: Result<RangeSpec, RangeError>) -> Download;
//! pub fn body_for(method: Method, body: Body) -> Body;
//! pub fn write_body<W: Write, S: FileSource + ?Sized>(
//!     body: &Body,
//!     source: &S,
//!     out: &mut W,
//! ) -> Result<u64, Error>;
//! ```
//!
//! ## Paths
//!
//! Every path in this module is **mount-relative without a leading slash**
//! (`rec/REC_0001.avi`), the form [`crate::path::map`] validates before it
//! prepends the mount. [`CardSource`] holds the mount and rejects an absolute
//! path or a `..` segment, so no body can name a VFS path outside the card and
//! the same body works against a test source with no card at all.
//!
//! ## The chunk buffer
//!
//! [`CHUNK`] is **4 KiB** and that is a design constraint, not a preference.
//! The card is one FatFs volume that the muxer is writing while a download is
//! read; FatFs serialises access per volume, so every chunk is a window in
//! which the recorder's write waits. 4 KiB bounds that window at a few
//! milliseconds while still keeping the per-request call count modest, and it
//! fits the worker's 16 KiB stack with room to spare. Raising it lengthens the
//! recorder's stall; lowering it costs open/seek work without helping the
//! muxer. Do not change it without a measurement.
//!
//! ## Streaming and byte accounting
//!
//! [`write_body`] streams a [`Body`] into any [`Write`] and returns the exact
//! number of body bytes written. `Bytes` and `Json` are written in one
//! `write_all`. `File` is streamed through [`chunk_plan`] with a 4 KiB buffer:
//! the source is asked for at most one chunk at a time, never the whole file,
//! and the source is consulted once for the file length before the first byte
//! so a file that shrank since the request was routed is refused instead of
//! announced with a `Content-Length` that cannot be met.
//!
//! The length is checked, not trusted: `start > total` or `len > total - start`
//! is [`Error::OutOfBounds`], and a source that returns fewer bytes than the
//! chunk asked for is [`Error::ShortRead`] - never a silently truncated `200`.
//! A directory or a path that does not exist is [`Error::NotFound`], which the
//! router answers with `404`. A file is opened for one [`FileSource::read_at`]
//! call and dropped before the next chunk, so no handle outlives the response
//! and a stalled client cannot pin a file on the card.
//!
//! ## The download decision
//!
//! [`download`] turns the file length plus the [`crate::range::parse`] result
//! into the whole response decision without touching the card:
//!
//! - `RangeSpec::Full` -> `200 OK`, `Content-Length: total`, no `Content-Range`,
//!   body `File { start: 0, len: total }`;
//! - `RangeSpec::Partial { start, end }` -> `206 Partial Content`,
//!   `Content-Length: end - start + 1`, `Content-Range: bytes start-end/total`,
//!   body `File { start, len: end - start + 1 }`;
//! - `RangeError::Unsatisfiable` -> `416 Range Not Satisfiable`,
//!   `Content-Length: 0`, `Content-Range: bytes */total` and an empty body.
//!
//! [`ACCEPT_RANGES`] is the header every download carries. [`body_for`] applies
//! the one method rule: a `HEAD` response carries the same head and **no**
//! body, so the connection writes the head and zero body bytes, while a `GET`
//! streams the body `download` selected.
//!
//! ## The two sources
//!
//! [`CardSource`] is the device implementation over `std::fs`: it never shares
//! the `Storage` handle, because the mount is process-global in the VFS and the
//! muxer already owns it, and it reads one chunk per open so it cannot hold a
//! handle past the response. The `#[cfg(test)]` `MemorySource` serves the same
//! trait over a slice of `(path, bytes)` pairs, which is what lets the device
//! test binary exercise listing and streaming without a card.

use crate::plan::{Method, Status};
use crate::range::{RangeError, RangeSpec};
use std::io::Write;

/// Size of the streaming buffer, in bytes. See the [module contract](self):
/// this bounds how long a download can delay the muxer's write.
pub const CHUNK: usize = 4096;

/// The header every download response carries: this server answers `Range`.
pub const ACCEPT_RANGES: (&str, &str) = ("Accept-Ranges", "bytes");

/// Why a body could not be produced.
#[derive(Debug)]
pub enum Error {
    /// The path does not name a readable file (missing, or a directory).
    NotFound,
    /// The selected range does not fit the source's current length.
    OutOfBounds { start: u64, len: u64, total: u64 },
    /// The source returned fewer bytes than the chunk asked for.
    ShortRead {
        offset: u64,
        want: usize,
        got: usize,
    },
    /// An I/O failure opening, reading or writing.
    Io(std::io::Error),
}

impl PartialEq for Error {
    /// Compare by shape; two [`Error::Io`] values are equal when their
    /// [`std::io::ErrorKind`] matches, since the message is platform text.
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Error::NotFound, Error::NotFound) => true,
            (
                Error::OutOfBounds {
                    start: a_start,
                    len: a_len,
                    total: a_total,
                },
                Error::OutOfBounds {
                    start: b_start,
                    len: b_len,
                    total: b_total,
                },
            ) => a_start == b_start && a_len == b_len && a_total == b_total,
            (
                Error::ShortRead {
                    offset: a_offset,
                    want: a_want,
                    got: a_got,
                },
                Error::ShortRead {
                    offset: b_offset,
                    want: b_want,
                    got: b_got,
                },
            ) => a_offset == b_offset && a_want == b_want && a_got == b_got,
            (Error::Io(a), Error::Io(b)) => a.kind() == b.kind(),
            _ => false,
        }
    }
}

impl Eq for Error {}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::NotFound => f.write_str("not found"),
            Error::OutOfBounds { start, len, total } => {
                write!(f, "range {start}+{len} exceeds {total} bytes")
            }
            Error::ShortRead { offset, want, got } => {
                write!(f, "short read at {offset}: wanted {want} bytes, got {got}")
            }
            Error::Io(error) => write!(f, "i/o error: {error}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Self {
        io_error(error)
    }
}

/// One entry of a directory as a [`FileSource`] reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    /// The name within the listed directory: no separator, never `.` or `..`.
    pub name: String,
    /// Whether the entry is a directory, which a listing skips.
    pub is_dir: bool,
}

/// A response body, selected by the router and streamed by [`write_body`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Body {
    /// A static byte string, e.g. an empty body.
    Bytes(&'static [u8]),
    /// An already serialised JSON document.
    Json(String),
    /// `len` bytes of `path` starting at `start`.
    File { path: String, start: u64, len: u64 },
}

impl Body {
    /// The empty body: what a `HEAD` response and a `416` carry.
    pub const fn empty() -> Body {
        Body::Bytes(&[])
    }
}

/// The response decision for one download, ready to be turned into a head and
/// a body by the connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Download {
    /// `200`, `206` or `416`.
    pub status: Status,
    /// The value of `Content-Length`.
    pub content_length: u64,
    /// The value of `Content-Range`, absent for a full `200`.
    pub content_range: Option<String>,
    /// The body to stream for a `GET`.
    pub body: Body,
}

/// An iterator over the chunks of `total` bytes.
#[derive(Debug, Clone)]
pub struct Chunks {
    offset: u64,
    remaining: u64,
    chunk: u64,
}

impl Iterator for Chunks {
    type Item = (u64, usize);

    fn next(&mut self) -> Option<(u64, usize)> {
        if self.remaining == 0 {
            return None;
        }
        let len = self.chunk.min(self.remaining);
        let offset = self.offset;
        self.offset += len;
        self.remaining -= len;
        Some((offset, len as usize))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let chunks = self.remaining.div_ceil(self.chunk);
        let chunks = usize::try_from(chunks).unwrap_or(usize::MAX);
        (chunks, Some(chunks))
    }
}

/// Plan the reads of `total` bytes in `chunk`-sized pieces.
///
/// Yields `(offset, len)` with `len > 0`, contiguous offsets from zero, and a
/// final short chunk when `total` is not a multiple of `chunk`. A `chunk` size
/// of 0 is treated as 1 so the plan always advances. A zero total yields
/// nothing.
pub fn chunk_plan(total: u64, chunk: usize) -> Chunks {
    Chunks {
        offset: 0,
        remaining: total,
        chunk: chunk.max(1) as u64,
    }
}

/// Read access to the files a response can name.
pub trait FileSource {
    /// The length of `path` in bytes, or [`Error::NotFound`] when it is not a
    /// readable file (a directory counts as not found).
    fn len(&self, path: &str) -> Result<u64, Error>;

    /// Fill `buf` with bytes starting at `offset` and return how many were
    /// read. Fewer than `buf.len()` means the file ended; [`write_body`]
    /// treats that as [`Error::ShortRead`] because the length was checked
    /// first.
    fn read_at(&self, path: &str, offset: u64, buf: &mut [u8]) -> Result<usize, Error>;

    /// The entries directly inside `dir`, unsorted. Errors when the directory
    /// cannot be read.
    fn list(&self, dir: &str) -> Result<Vec<DirEntry>, Error>;
}

/// The device [`FileSource`]: `std::fs` under one mount point.
#[derive(Debug, Clone)]
pub struct CardSource {
    mount: String,
}

impl CardSource {
    /// A source over `mount`, the VFS path of the card (`/sdcard`).
    pub fn new(mount: &str) -> CardSource {
        CardSource {
            mount: mount.to_string(),
        }
    }

    /// The VFS path for a mount-relative file path, refusing anything that
    /// could leave the mount.
    fn file_path(&self, rel: &str) -> Result<String, Error> {
        if rel.is_empty()
            || rel.starts_with('/')
            || rel
                .split('/')
                .any(|segment| segment.is_empty() || segment == "..")
        {
            return Err(Error::NotFound);
        }
        Ok(self.join(rel))
    }

    /// The VFS path for a mount-relative directory; the empty string is the
    /// mount itself.
    fn dir_path(&self, dir: &str) -> Result<String, Error> {
        if dir.is_empty() {
            Ok(self.mount.clone())
        } else {
            self.file_path(dir)
        }
    }

    fn join(&self, rel: &str) -> String {
        let mut path = String::with_capacity(self.mount.len() + 1 + rel.len());
        path.push_str(&self.mount);
        path.push('/');
        path.push_str(rel);
        path
    }
}

impl FileSource for CardSource {
    fn len(&self, path: &str) -> Result<u64, Error> {
        let full = self.file_path(path)?;
        let metadata = std::fs::metadata(&full).map_err(io_error)?;
        if metadata.is_dir() {
            return Err(Error::NotFound);
        }
        Ok(metadata.len())
    }

    fn read_at(&self, path: &str, offset: u64, buf: &mut [u8]) -> Result<usize, Error> {
        use std::io::{Read, Seek, SeekFrom};

        if buf.is_empty() {
            return Ok(0);
        }
        let full = self.file_path(path)?;
        let mut file = std::fs::File::open(&full).map_err(io_error)?;
        file.seek(SeekFrom::Start(offset)).map_err(Error::Io)?;
        let mut filled = 0;
        while filled < buf.len() {
            match file.read(&mut buf[filled..]) {
                Ok(0) => break,
                Ok(read) => filled += read,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(Error::Io(error)),
            }
        }
        Ok(filled)
    }

    fn list(&self, dir: &str) -> Result<Vec<DirEntry>, Error> {
        let full = self.dir_path(dir)?;
        let reader = std::fs::read_dir(&full).map_err(io_error)?;
        let mut entries = Vec::new();
        for entry in reader {
            let entry = entry.map_err(Error::Io)?;
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            let is_dir = entry.file_type().map_err(Error::Io)?.is_dir();
            entries.push(DirEntry { name, is_dir });
        }
        Ok(entries)
    }
}

/// The test [`FileSource`]: a table of mount-relative paths to bytes.
#[cfg(test)]
pub struct MemorySource<'a> {
    files: &'a [(&'a str, &'a [u8])],
}

#[cfg(test)]
impl<'a> MemorySource<'a> {
    /// A source serving `files`, whose paths are mount-relative.
    pub fn new(files: &'a [(&'a str, &'a [u8])]) -> MemorySource<'a> {
        MemorySource { files }
    }
}

#[cfg(test)]
impl FileSource for MemorySource<'_> {
    fn len(&self, path: &str) -> Result<u64, Error> {
        self.files
            .iter()
            .find(|(name, _)| *name == path)
            .map(|(_, bytes)| bytes.len() as u64)
            .ok_or(Error::NotFound)
    }

    fn read_at(&self, path: &str, offset: u64, buf: &mut [u8]) -> Result<usize, Error> {
        let (_, bytes) = self
            .files
            .iter()
            .find(|(name, _)| *name == path)
            .ok_or(Error::NotFound)?;
        let start = usize::try_from(offset).unwrap_or(usize::MAX);
        let Some(available) = bytes.get(start..) else {
            return Ok(0);
        };
        let take = buf.len().min(available.len());
        buf[..take].copy_from_slice(&available[..take]);
        Ok(take)
    }

    fn list(&self, dir: &str) -> Result<Vec<DirEntry>, Error> {
        let mut entries: Vec<DirEntry> = Vec::new();
        for (path, _) in self.files {
            let child = if dir.is_empty() {
                *path
            } else {
                match path
                    .strip_prefix(dir)
                    .and_then(|rest| rest.strip_prefix('/'))
                {
                    Some(rest) => rest,
                    None => continue,
                }
            };
            match child.split_once('/') {
                Some((name, _)) => {
                    if !entries
                        .iter()
                        .any(|entry| entry.is_dir && entry.name == name)
                    {
                        entries.push(DirEntry {
                            name: name.to_string(),
                            is_dir: true,
                        });
                    }
                }
                None if !child.is_empty() => entries.push(DirEntry {
                    name: child.to_string(),
                    is_dir: false,
                }),
                None => {}
            }
        }
        Ok(entries)
    }
}

/// Turn a file length and a [`crate::range::parse`] result into the response.
pub fn download(path: &str, total: u64, range: Result<RangeSpec, RangeError>) -> Download {
    match range {
        Ok(RangeSpec::Full) => Download {
            status: Status::OK,
            content_length: total,
            content_range: None,
            body: Body::File {
                path: path.to_string(),
                start: 0,
                len: total,
            },
        },
        Ok(RangeSpec::Partial { start, end }) => {
            let len = end - start + 1;
            Download {
                status: Status::PARTIAL_CONTENT,
                content_length: len,
                content_range: Some(format!("bytes {start}-{end}/{total}")),
                body: Body::File {
                    path: path.to_string(),
                    start,
                    len,
                },
            }
        }
        Err(RangeError::Unsatisfiable) => Download {
            status: Status::RANGE_NOT_SATISFIABLE,
            content_length: 0,
            content_range: Some(format!("bytes */{total}")),
            body: Body::empty(),
        },
    }
}

/// The body a response to `method` sends: `HEAD` has none.
pub fn body_for(method: Method, body: Body) -> Body {
    match method {
        Method::Get => body,
        Method::Head | Method::Options | Method::Other => Body::empty(),
    }
}

/// Stream `body` into `out` and return the number of body bytes written.
pub fn write_body<W: Write, S: FileSource + ?Sized>(
    body: &Body,
    source: &S,
    out: &mut W,
) -> Result<u64, Error> {
    match body {
        Body::Bytes(bytes) => {
            out.write_all(bytes).map_err(Error::Io)?;
            Ok(bytes.len() as u64)
        }
        Body::Json(json) => {
            out.write_all(json.as_bytes()).map_err(Error::Io)?;
            Ok(json.len() as u64)
        }
        Body::File { path, start, len } => {
            let total = source.len(path)?;
            if *start > total || *len > total - *start {
                return Err(Error::OutOfBounds {
                    start: *start,
                    len: *len,
                    total,
                });
            }
            let mut buffer = [0u8; CHUNK];
            let mut written = 0u64;
            for (offset, want) in chunk_plan(*len, CHUNK) {
                let at = *start + offset;
                let got = source.read_at(path, at, &mut buffer[..want])?;
                if got != want {
                    return Err(Error::ShortRead {
                        offset: at,
                        want,
                        got,
                    });
                }
                out.write_all(&buffer[..got]).map_err(Error::Io)?;
                written += got as u64;
            }
            Ok(written)
        }
    }
}

/// Map an `std::fs` failure onto the typed error.
fn io_error(error: std::io::Error) -> Error {
    if error.kind() == std::io::ErrorKind::NotFound {
        Error::NotFound
    } else {
        Error::Io(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::vec::Vec;

    const MIB: u64 = 1024 * 1024;

    /// A deterministic, position-dependent pattern, so an offset bug cannot
    /// pass by comparing equal bytes.
    fn pattern(len: usize) -> Vec<u8> {
        (0..len).map(|index| (index % 251) as u8).collect()
    }

    /// A source with no file behind it, for the bodies that need none.
    fn empty_source() -> MemorySource<'static> {
        MemorySource::new(&[])
    }

    /// A source that reports the true length but always returns one byte less
    /// than asked: the dishonest source of the short-read failure case.
    struct ShortSource {
        data: &'static [u8],
    }

    impl FileSource for ShortSource {
        fn len(&self, _path: &str) -> Result<u64, Error> {
            Ok(self.data.len() as u64)
        }

        fn read_at(&self, _path: &str, offset: u64, buf: &mut [u8]) -> Result<usize, Error> {
            let start = offset as usize;
            let available = self.data.len().saturating_sub(start);
            let want = buf.len().min(available);
            let got = want.saturating_sub(1);
            buf[..got].copy_from_slice(&self.data[start..start + got]);
            Ok(got)
        }

        fn list(&self, _dir: &str) -> Result<Vec<DirEntry>, Error> {
            Ok(Vec::new())
        }
    }

    // --- chunk_plan ---------------------------------------------------------

    #[test]
    fn chunk_plan_zero() {
        assert_eq!(chunk_plan(0, CHUNK).collect::<Vec<_>>(), Vec::new());
        assert_eq!(chunk_plan(0, 1).count(), 0);
        assert_eq!(chunk_plan(0, 0).count(), 0);
    }

    #[test]
    fn chunk_plan_one() {
        assert_eq!(chunk_plan(1, CHUNK).collect::<Vec<_>>(), std::vec![(0, 1)]);
    }

    #[test]
    fn chunk_plan_exactly_one_chunk() {
        assert_eq!(
            chunk_plan(CHUNK as u64, CHUNK).collect::<Vec<_>>(),
            std::vec![(0, CHUNK)]
        );
    }

    #[test]
    fn chunk_plan_remainder() {
        let total = (2 * CHUNK + 5) as u64;
        assert_eq!(
            chunk_plan(total, CHUNK).collect::<Vec<_>>(),
            std::vec![(0, CHUNK), (CHUNK as u64, CHUNK), (2 * CHUNK as u64, 5),]
        );
    }

    #[test]
    fn chunk_plan_large_file() {
        let total = 3 * MIB;
        let plan = chunk_plan(total, CHUNK).collect::<Vec<_>>();
        assert_eq!(plan.len(), (total / CHUNK as u64) as usize);
        assert_eq!(
            plan.iter().map(|(_, len)| *len as u64).sum::<u64>(),
            total,
            "the plan covers every byte exactly once"
        );
        for (index, (offset, len)) in plan.iter().enumerate() {
            assert_eq!(*offset, index as u64 * CHUNK as u64);
            assert_eq!(*len, CHUNK);
        }
    }

    #[test]
    fn chunk_plan_never_yields_zero() {
        for total in [0u64, 1, 3, 4095, 4096, 4097, 100_000] {
            for chunk in [0usize, 1, 2, 4096, 100_000] {
                let plan = chunk_plan(total, chunk).collect::<Vec<_>>();
                assert!(
                    plan.iter().all(|(_, len)| *len > 0),
                    "{total} bytes in {chunk}-byte chunks yielded a zero-length chunk: {plan:?}"
                );
                assert_eq!(
                    plan.iter().map(|(_, len)| *len as u64).sum::<u64>(),
                    total,
                    "{total} bytes in {chunk}-byte chunks"
                );
                let mut expected = 0;
                for (offset, len) in plan {
                    assert_eq!(offset, expected, "offsets are contiguous");
                    expected += len as u64;
                }
            }
        }
        // A chunk larger than the remaining bytes is clamped, not padded.
        assert_eq!(chunk_plan(10, 4096).collect::<Vec<_>>(), std::vec![(0, 10)]);
        // The first chunks of a huge file are full chunks, never zeros.
        let plan = chunk_plan(u64::MAX, CHUNK).take(1000).collect::<Vec<_>>();
        assert!(plan.iter().all(|(_, len)| *len == CHUNK));
    }

    // --- write_body ---------------------------------------------------------

    #[test]
    fn write_body_bytes() {
        let source = empty_source();
        let mut out = Vec::new();
        assert_eq!(write_body(&Body::Bytes(b"hello"), &source, &mut out), Ok(5));
        assert_eq!(out, b"hello");
    }

    #[test]
    fn write_body_json() {
        let source = empty_source();
        let mut out = Vec::new();
        let json = "{\"files\":[]}";
        assert_eq!(
            write_body(&Body::Json(json.to_string()), &source, &mut out),
            Ok(json.len() as u64)
        );
        assert_eq!(out, json.as_bytes());
    }

    #[test]
    fn write_body_file_full() {
        let data = pattern(64 * 1024);
        let files = [("rec/REC_0001.avi", &data[..])];
        let source = MemorySource::new(&files);
        let body = Body::File {
            path: "rec/REC_0001.avi".to_string(),
            start: 0,
            len: data.len() as u64,
        };
        let mut out = Vec::new();
        assert_eq!(write_body(&body, &source, &mut out), Ok(data.len() as u64));
        assert_eq!(out, data);
    }

    #[test]
    fn write_body_file_partial() {
        let data = pattern(3 * MIB as usize);
        let files = [("rec/REC_0001.avi", &data[..])];
        let source = MemorySource::new(&files);
        let body = Body::File {
            path: "rec/REC_0001.avi".to_string(),
            start: MIB,
            len: MIB,
        };
        let mut out = Vec::new();
        assert_eq!(write_body(&body, &source, &mut out), Ok(MIB));
        assert_eq!(out, data[MIB as usize..(2 * MIB) as usize]);
    }

    #[test]
    fn write_body_file_empty() {
        let files = [("rec/EMPTY.avi", &[][..])];
        let source = MemorySource::new(&files);
        let body = Body::File {
            path: "rec/EMPTY.avi".to_string(),
            start: 0,
            len: 0,
        };
        let mut out = Vec::new();
        assert_eq!(write_body(&body, &source, &mut out), Ok(0));
        assert!(out.is_empty());
    }

    #[test]
    fn write_body_short_source_is_an_error() {
        static DATA: [u8; 8192] = [7u8; 8192];
        let source = ShortSource { data: &DATA };
        let body = Body::File {
            path: "rec/REC_0001.avi".to_string(),
            start: 0,
            len: DATA.len() as u64,
        };
        let mut out = Vec::new();
        assert_eq!(
            write_body(&body, &source, &mut out),
            Err(Error::ShortRead {
                offset: 0,
                want: CHUNK,
                got: CHUNK - 1,
            }),
            "a source that under-delivers must not become a truncated 200"
        );
        assert_eq!(out.len(), 0, "no partial chunk reaches the socket");
    }

    #[test]
    fn write_body_reports_exact_byte_count() {
        let data = pattern(3 * CHUNK);
        let files = [("rec/REC_0001.avi", &data[..])];
        let source = MemorySource::new(&files);

        let body = Body::File {
            path: "rec/REC_0001.avi".to_string(),
            start: CHUNK as u64,
            len: CHUNK as u64,
        };
        let mut out = Vec::new();
        let written = write_body(&body, &source, &mut out).expect("the window streams");
        assert_eq!(written, CHUNK as u64);
        assert_eq!(out.len() as u64, written);
        assert_eq!(out, data[CHUNK..2 * CHUNK]);

        // The whole file reports its whole length, not the chunk count.
        let whole = Body::File {
            path: "rec/REC_0001.avi".to_string(),
            start: 0,
            len: data.len() as u64,
        };
        let mut all = Vec::new();
        assert_eq!(write_body(&whole, &source, &mut all), Ok(data.len() as u64));
        assert_eq!(all, data);
    }

    #[test]
    fn write_body_file_beyond_the_source_is_an_error() {
        let data = pattern(1024);
        let files = [("rec/REC_0001.avi", &data[..])];
        let source = MemorySource::new(&files);
        let past_end = Body::File {
            path: "rec/REC_0001.avi".to_string(),
            start: 512,
            len: 1024,
        };
        assert_eq!(
            write_body(&past_end, &source, &mut Vec::new()),
            Err(Error::OutOfBounds {
                start: 512,
                len: 1024,
                total: 1024,
            })
        );
        let no_file = Body::File {
            path: "rec/MISSING.avi".to_string(),
            start: 0,
            len: 1,
        };
        assert_eq!(
            write_body(&no_file, &source, &mut Vec::new()),
            Err(Error::NotFound)
        );
    }

    #[test]
    fn directory_is_not_found() {
        let data = pattern(16);
        let files = [("rec/sub/REC.avi", &data[..])];
        let source = MemorySource::new(&files);
        assert_eq!(
            source.len("rec/sub"),
            Err(Error::NotFound),
            "a directory passed as a file is a 404, never the directory bytes"
        );
        assert_eq!(source.len("rec"), Err(Error::NotFound));
    }

    // --- download -----------------------------------------------------------

    #[test]
    fn download_full_range_is_200() {
        let response = download("rec/REC_0001.avi", 1000, Ok(RangeSpec::Full));
        assert_eq!(response.status, Status::OK);
        assert_eq!(response.content_length, 1000);
        assert_eq!(response.content_range, None);
        assert_eq!(
            response.body,
            Body::File {
                path: "rec/REC_0001.avi".to_string(),
                start: 0,
                len: 1000,
            }
        );
    }

    #[test]
    fn download_partial_range_is_206() {
        let range = crate::range::parse(Some("bytes=100-199"), 1000);
        let response = download("rec/REC_0001.avi", 1000, range);
        assert_eq!(response.status, Status::PARTIAL_CONTENT);
        assert_eq!(response.content_length, 100);
        assert_eq!(
            response.content_range.as_deref(),
            Some("bytes 100-199/1000")
        );
        assert_eq!(
            response.body,
            Body::File {
                path: "rec/REC_0001.avi".to_string(),
                start: 100,
                len: 100,
            }
        );
        assert_eq!(ACCEPT_RANGES, ("Accept-Ranges", "bytes"));
    }

    #[test]
    fn download_rejects_unsatisfiable_range_with_416() {
        let range = crate::range::parse(Some("bytes=5000-"), 1000);
        assert_eq!(range, Err(RangeError::Unsatisfiable));

        let response = download("rec/REC_0001.avi", 1000, range);
        assert_eq!(response.status, Status::RANGE_NOT_SATISFIABLE);
        assert_eq!(response.status.code(), 416);
        assert_eq!(response.content_length, 0);
        assert_eq!(
            response.content_range.as_deref(),
            Some("bytes */1000"),
            "416 names the current length so the client can resynchronise"
        );
        let mut out = Vec::new();
        assert_eq!(write_body(&response.body, &empty_source(), &mut out), Ok(0));
        assert!(out.is_empty());
    }

    #[test]
    fn download_of_an_empty_file_is_200_with_no_body() {
        let response = download("rec/EMPTY.avi", 0, crate::range::parse(None, 0));
        assert_eq!(response.status, Status::OK);
        assert_eq!(response.content_length, 0);
        assert_eq!(response.content_range, None);
        let files = [("rec/EMPTY.avi", &[][..])];
        let source = MemorySource::new(&files);
        let mut out = Vec::new();
        assert_eq!(write_body(&response.body, &source, &mut out), Ok(0));
        assert!(out.is_empty());
    }

    #[test]
    fn head_has_no_body() {
        let data = pattern(CHUNK);
        let files = [("rec/REC_0001.avi", &data[..])];
        let source = MemorySource::new(&files);
        let response = download("rec/REC_0001.avi", data.len() as u64, Ok(RangeSpec::Full));
        assert_eq!(
            response.content_length,
            data.len() as u64,
            "the head still announces the length"
        );

        let head = body_for(Method::Head, response.body.clone());
        let mut head_out = Vec::new();
        assert_eq!(write_body(&head, &source, &mut head_out), Ok(0));
        assert!(head_out.is_empty(), "HEAD must not write body bytes");

        // The same response as GET does stream, so the empty body is HEAD's
        // doing and not a broken source.
        let get = body_for(Method::Get, response.body);
        let mut get_out = Vec::new();
        assert_eq!(
            write_body(&get, &source, &mut get_out),
            Ok(data.len() as u64)
        );
        assert_eq!(get_out, data);
    }
}
