//! Percent decoding and mapping of a request path onto the mounted card.
//!
//! # Contract
//!
//! ```text
//! pub struct CardPath<'a> { pub rel: &'a str }
//!
//! pub fn percent_decode(input: &str, out: &mut [u8]) -> Result<usize, PathError>
//! pub fn map(decoded_uri_path: &str, mount: &str) -> Result<String, PathError>
//! pub enum PathError { Empty, Absolute, Traversal, DotSegment, TrailingSeparator,
//!                      EmptySegment, Backslash, Nul, Control, BadEscape,
//!                      TooLong, NotUtf8, BufferTooSmall, Mount }
//! ```
//!
//! `percent_decode` copies `input` into `out`, decoding every `%XX` escape
//! (hex digits in either case) into the single byte it denotes. It returns the
//! number of bytes written and rejects, in order of precedence:
//!
//! - an incomplete or non-hex escape (`%`, `%2`, `%zz`) as [`PathError::BadEscape`];
//! - a decoded NUL byte as [`PathError::Nul`];
//! - a raw or decoded backslash as [`PathError::Backslash`] (FAT32 forbids it);
//! - any other decoded byte below `0x20` as [`PathError::Control`];
//! - more than 255 decoded bytes as [`PathError::TooLong`];
//! - an `out` buffer that fills before the input is consumed as
//!   [`PathError::BufferTooSmall`].
//!
//! Input is borrowed, output is written in place: the function never
//! allocates. Bytes at or above `0x20` other than `\` pass through unchanged,
//! so a space or a UTF-8 sequence survives.
//!
//! `map` percent-decodes `decoded_uri_path` **before** validating it, so an
//! encoded traversal cannot slip past the checks. The decoded relative path
//! must then:
//!
//! - be non-empty ([`PathError::Empty`]);
//! - not begin with `/` ([`PathError::Absolute`]) - the request path is always
//!   relative to the mount, never absolute on the VFS;
//! - contain no `..` segment ([`PathError::Traversal`]) and no `.` segment
//!   ([`PathError::DotSegment`]) - checked on whole segments, so `..hidden`
//!   and `a..b` are ordinary names;
//! - not end with `/` ([`PathError::TrailingSeparator`]);
//! - contain no empty segment, including `a//b` ([`PathError::EmptySegment`]);
//! - be valid UTF-8 after decoding ([`PathError::NotUtf8`]).
//!
//! `mount` must be an absolute, separator-free VFS path such as `/sdcard`
//! (non-empty, starting with `/`, no trailing `/`, no backslash, no control
//! byte); otherwise `map` returns [`PathError::Mount`].
//!
//! On success `map` returns `<mount>/<rel>`, assembled by pushing the mount and
//! then each validated segment behind a single `/`. It never concatenates the
//! raw request string, never consults the filesystem (`canonicalize` on the
//! VFS is unreliable and FAT32 has no symlinks), and never returns a path with
//! a trailing separator or a doubled separator.
//!
//! [`CardPath`] is the borrowed form of a decoded path the router will carry
//! before mapping; it performs no validation itself.

/// Maximum decoded length of a relative card path, in bytes.
pub const MAX_DECODED_LEN: usize = 255;

/// A decoded request path, still relative to the card mount.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CardPath<'a> {
    /// The decoded path bytes as text, relative to the mount.
    pub rel: &'a str,
}

/// Why a request path could not be decoded or mapped onto the card.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathError {
    /// The path is empty.
    Empty,
    /// The decoded path starts with `/` instead of a segment.
    Absolute,
    /// A `..` segment was present.
    Traversal,
    /// A `.` segment was present.
    DotSegment,
    /// The path ends with `/`.
    TrailingSeparator,
    /// A segment between two separators is empty, as in `a//b`.
    EmptySegment,
    /// A backslash was present (raw or decoded).
    Backslash,
    /// A NUL byte was present (raw or decoded).
    Nul,
    /// A control byte below `0x20` was present (raw or decoded).
    Control,
    /// A percent escape was incomplete or not hexadecimal.
    BadEscape,
    /// The decoded path exceeds [`MAX_DECODED_LEN`].
    TooLong,
    /// The decoded bytes are not valid UTF-8.
    NotUtf8,
    /// The caller's output buffer is smaller than the decoded input.
    BufferTooSmall,
    /// The mount prefix is not an absolute, separator-free VFS path.
    Mount,
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Decode `%XX` escapes from `input` into `out`, returning the byte count.
///
/// See the module contract for the rejection rules.
pub fn percent_decode(input: &str, out: &mut [u8]) -> Result<usize, PathError> {
    let src = input.as_bytes();
    let mut written = 0;
    let mut index = 0;

    while index < src.len() {
        let byte = if src[index] == b'%' {
            if index + 2 >= src.len() {
                return Err(PathError::BadEscape);
            }
            let high = hex_value(src[index + 1]).ok_or(PathError::BadEscape)?;
            let low = hex_value(src[index + 2]).ok_or(PathError::BadEscape)?;
            index += 3;
            (high << 4) | low
        } else {
            let byte = src[index];
            index += 1;
            byte
        };

        match byte {
            0 => return Err(PathError::Nul),
            b'\\' => return Err(PathError::Backslash),
            0x01..=0x1f => return Err(PathError::Control),
            _ => {}
        }

        if written == MAX_DECODED_LEN {
            return Err(PathError::TooLong);
        }
        if written >= out.len() {
            return Err(PathError::BufferTooSmall);
        }
        out[written] = byte;
        written += 1;
    }

    Ok(written)
}

fn validate_mount(mount: &str) -> Result<(), PathError> {
    if mount.is_empty() || !mount.starts_with('/') || mount.ends_with('/') {
        return Err(PathError::Mount);
    }
    if mount
        .bytes()
        .any(|byte| byte == b'\\' || byte < 0x20 || byte == 0x7f)
    {
        return Err(PathError::Mount);
    }
    Ok(())
}

fn validate_relative(rel: &str) -> Result<(), PathError> {
    if rel.is_empty() {
        return Err(PathError::Empty);
    }
    if rel.starts_with('/') {
        return Err(PathError::Absolute);
    }

    for segment in rel.split('/') {
        match segment {
            ".." => return Err(PathError::Traversal),
            "." => return Err(PathError::DotSegment),
            _ => {}
        }
    }

    if rel.ends_with('/') {
        return Err(PathError::TrailingSeparator);
    }
    if rel.split('/').any(str::is_empty) {
        return Err(PathError::EmptySegment);
    }

    Ok(())
}

/// Map a decoded URI path onto `mount`, rejecting every unsafe form.
///
/// See the module contract for the rejection rules and the returned shape.
pub fn map(decoded_uri_path: &str, mount: &str) -> Result<String, PathError> {
    validate_mount(mount)?;

    let mut buffer = [0u8; MAX_DECODED_LEN];
    let len = percent_decode(decoded_uri_path, &mut buffer)?;
    let rel = core::str::from_utf8(&buffer[..len]).map_err(|_| PathError::NotUtf8)?;

    validate_relative(rel)?;

    let mut path = String::with_capacity(mount.len() + 1 + rel.len());
    path.push_str(mount);
    for segment in rel.split('/') {
        path.push('/');
        path.push_str(segment);
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(input: &str) -> Result<Vec<u8>, PathError> {
        let mut out = [0u8; MAX_DECODED_LEN];
        let len = percent_decode(input, &mut out)?;
        Ok(out[..len].to_vec())
    }

    // --- percent_decode ---------------------------------------------------

    #[test]
    fn percent_decode_passes_plain_text_through() {
        assert_eq!(decode("rec/REC_0001.avi").unwrap(), b"rec/REC_0001.avi");
    }

    #[test]
    fn percent_decode_decodes_mixed_case_escapes() {
        assert_eq!(decode("%2E%2e%2F").unwrap(), b"../");
        assert_eq!(decode("rec%2FREC_0001.avi").unwrap(), b"rec/REC_0001.avi");
    }

    #[test]
    fn percent_decode_keeps_space_and_utf8() {
        assert_eq!(decode("rec/a%20b.avi").unwrap(), b"rec/a b.avi");
        assert_eq!(decode("rec/%C3%A9.avi").unwrap(), "rec/\u{e9}.avi".as_bytes());
    }

    #[test]
    fn percent_decode_rejects_bad_escapes() {
        assert_eq!(decode("%zz"), Err(PathError::BadEscape));
        assert_eq!(decode("%2"), Err(PathError::BadEscape));
        assert_eq!(decode("abc%"), Err(PathError::BadEscape));
        assert_eq!(decode("%g0"), Err(PathError::BadEscape));
    }

    #[test]
    fn percent_decode_rejects_nul_and_control_bytes() {
        assert_eq!(decode("a%00b"), Err(PathError::Nul));
        assert_eq!(decode("a\tb"), Err(PathError::Control));
        assert_eq!(decode("a%01b"), Err(PathError::Control));
    }

    #[test]
    fn percent_decode_rejects_backslash_raw_and_encoded() {
        assert_eq!(decode("rec\\a"), Err(PathError::Backslash));
        assert_eq!(decode("rec%5Ca"), Err(PathError::Backslash));
    }

    #[test]
    fn percent_decode_reports_a_small_buffer() {
        let mut out = [0u8; 2];
        assert_eq!(percent_decode("abc", &mut out), Err(PathError::BufferTooSmall));
    }

    #[test]
    fn percent_decode_accepts_255_bytes_and_rejects_256() {
        let max = "a".repeat(MAX_DECODED_LEN);
        assert_eq!(decode(&max).unwrap().len(), MAX_DECODED_LEN);

        let over = "a".repeat(MAX_DECODED_LEN + 1);
        assert_eq!(decode(&over), Err(PathError::TooLong));

        // Escapes count after decoding, not before.
        let escaped = "%61".repeat(MAX_DECODED_LEN + 1);
        assert_eq!(decode(&escaped), Err(PathError::TooLong));
    }

    // --- map: the accepted shape -----------------------------------------

    #[test]
    fn map_joins_mount_and_relative_path() {
        assert_eq!(
            map("rec/REC_0001.avi", "/sdcard").unwrap(),
            "/sdcard/rec/REC_0001.avi"
        );
        assert_eq!(map("a.avi", "/sdcard").unwrap(), "/sdcard/a.avi");
    }

    #[test]
    fn map_never_doubles_a_separator() {
        let path = map("rec/REC_0001.avi", "/sdcard").unwrap();
        assert!(!path.contains("//"));
    }

    #[test]
    fn map_decodes_before_joining() {
        assert_eq!(
            map("rec/REC%5F0001.avi", "/sdcard").unwrap(),
            "/sdcard/rec/REC_0001.avi"
        );
        assert_eq!(map("rec/a%20b.avi", "/sdcard").unwrap(), "/sdcard/rec/a b.avi");
    }

    #[test]
    fn map_allows_dots_inside_a_segment() {
        assert_eq!(map("..hidden", "/sdcard").unwrap(), "/sdcard/..hidden");
        assert_eq!(map("a..b", "/sdcard").unwrap(), "/sdcard/a..b");
    }

    #[test]
    fn map_allows_exactly_255_bytes() {
        let rel = "a".repeat(MAX_DECODED_LEN);
        let expected = format!("/sdcard/{rel}");
        assert_eq!(map(&rel, "/sdcard").unwrap(), expected);
    }

    // --- map: traversal and ambiguity ------------------------------------

    #[test]
    fn map_rejects_parent_segments() {
        assert_eq!(map("../etc/passwd", "/sdcard"), Err(PathError::Traversal));
        assert_eq!(map("rec/../../x", "/sdcard"), Err(PathError::Traversal));
        assert_eq!(map("rec/..", "/sdcard"), Err(PathError::Traversal));
        assert_eq!(map("..", "/sdcard"), Err(PathError::Traversal));
    }

    #[test]
    fn map_rejects_encoded_parent_segments() {
        assert_eq!(map("%2e%2e%2f", "/sdcard"), Err(PathError::Traversal));
        assert_eq!(map("..%2f..%2fx", "/sdcard"), Err(PathError::Traversal));
        assert_eq!(
            map("%2e%2e%2f%2e%2e%2fetc%2fpasswd", "/sdcard"),
            Err(PathError::Traversal)
        );
    }

    #[test]
    fn map_rejects_encoded_absolute_path() {
        assert_eq!(map("%2fetc%2fpasswd", "/sdcard"), Err(PathError::Absolute));
        assert_eq!(map("/etc/passwd", "/sdcard"), Err(PathError::Absolute));
    }

    #[test]
    fn map_rejects_dot_segments() {
        assert_eq!(map(".", "/sdcard"), Err(PathError::DotSegment));
        assert_eq!(map("a/./b", "/sdcard"), Err(PathError::DotSegment));
        assert_eq!(map("rec/%2e/x", "/sdcard"), Err(PathError::DotSegment));
    }

    #[test]
    fn map_rejects_empty_segments_and_trailing_separator() {
        assert_eq!(map("rec//a", "/sdcard"), Err(PathError::EmptySegment));
        assert_eq!(map("rec/", "/sdcard"), Err(PathError::TrailingSeparator));
        assert_eq!(map("rec//", "/sdcard"), Err(PathError::TrailingSeparator));
    }

    #[test]
    fn map_rejects_empty_path() {
        assert_eq!(map("", "/sdcard"), Err(PathError::Empty));
    }

    #[test]
    fn map_rejects_nul_and_backslash() {
        assert_eq!(map("rec/%00x", "/sdcard"), Err(PathError::Nul));
        assert_eq!(map("rec\\a", "/sdcard"), Err(PathError::Backslash));
    }

    #[test]
    fn map_rejects_bad_escapes() {
        assert_eq!(map("%zz", "/sdcard"), Err(PathError::BadEscape));
        assert_eq!(map("rec/%2", "/sdcard"), Err(PathError::BadEscape));
    }

    #[test]
    fn map_rejects_a_300_byte_name() {
        let rel = format!("rec/{}.avi", "a".repeat(300));
        assert_eq!(map(&rel, "/sdcard"), Err(PathError::TooLong));
    }

    #[test]
    fn map_rejects_a_256_segment_path() {
        let rel = vec!["a"; 256].join("/");
        assert_eq!(map(&rel, "/sdcard"), Err(PathError::TooLong));
    }

    #[test]
    fn map_rejects_non_utf8_decoding() {
        assert_eq!(map("rec/%ff.avi", "/sdcard"), Err(PathError::NotUtf8));
    }

    #[test]
    fn map_rejects_an_invalid_mount() {
        assert_eq!(map("a", ""), Err(PathError::Mount));
        assert_eq!(map("a", "sdcard"), Err(PathError::Mount));
        assert_eq!(map("a", "/sdcard/"), Err(PathError::Mount));
        assert_eq!(map("a", "/"), Err(PathError::Mount));
        assert_eq!(map("a", "/sd\\card"), Err(PathError::Mount));
    }

    #[test]
    fn card_path_carries_the_relative_path() {
        let card = CardPath { rel: "rec/REC_0001.avi" };
        assert_eq!(card.rel, "rec/REC_0001.avi");
    }
}
