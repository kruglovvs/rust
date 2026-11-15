//! `Range` header parsing and the 206/416 arithmetic.
//!
//! # Contract
//!
//! ESP-IDF's httpd has no `Range` support, so this module is the only range
//! implementation in the stack. [`parse`] takes the request's `Range` header
//! value (if any) and the selected representation's length and returns:
//!
//! * [`RangeSpec::Full`] - serve the whole representation with `200 OK`;
//! * [`RangeSpec::Partial { start, end }`] - serve `end - start + 1` bytes
//!   starting at `start` with `206 Partial Content`; both bounds are
//!   inclusive and the invariant `start <= end < total` holds;
//! * [`RangeError::Unsatisfiable`] - answer `416 Range Not Satisfiable` with
//!   `Content-Range: bytes */<total>` and a zero-length body.
//!
//! ## Rules
//!
//! * No header (`None`) is [`RangeSpec::Full`].
//! * `total == 0` is always [`RangeSpec::Full`] (the body is empty and no
//!   byte can be selected); the header is never consulted.
//! * Only a single `bytes=` range is parsed. The unit is matched
//!   case-insensitively and ASCII whitespace around `=`, `-` and the value is
//!   ignored.
//! * `bytes=<first>-<last>` selects `first ..= min(last, total - 1)`.
//! * `bytes=<first>-` selects `first ..= total - 1`.
//! * `bytes=-<suffix>` selects the last `suffix` bytes,
//!   `total.saturating_sub(suffix) ..= total - 1`; a suffix of at least
//!   `total` selects the whole representation.
//! * `start >= total` or `start > end` is [`RangeError::Unsatisfiable`], as is
//!   a zero-length suffix (`bytes=-0`), which selects no byte at all.
//! * A multi-range value (`bytes=0-1,3-4`) is [`RangeSpec::Full`]: the server
//!   ignores it and serves the whole file, which RFC 9110 section 14.2
//!   permits. Multipart/byteranges is deliberately not implemented.
//! * Anything unparsable - missing or another unit, empty set, non-numeric or
//!   overflowing numbers, a sign, stray separators - is [`RangeSpec::Full`]:
//!   an unusable range is ignored rather than rejected.
//!
//! The signature takes `u64`, so a negative length cannot be expressed, and
//! no arithmetic here can overflow: `total - 1` only runs for `total > 0`,
//! suffix starts use `saturating_sub`, and digit strings that do not fit a
//! `u64` fall into the unparsable branch.

/// What the server should send for a representation requested with `Range`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RangeSpec {
    /// Send the whole representation as `200 OK`.
    Full,
    /// Send bytes `start ..= end` (inclusive) as `206 Partial Content`.
    Partial { start: u64, end: u64 },
}

/// Why a syntactically valid range cannot be served.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RangeError {
    /// The range selects no byte of the representation; answer `416` with
    /// `Content-Range: bytes */<total>` and a zero-length body.
    Unsatisfiable,
}

#[cfg(test)]
mod tests {
    use super::{parse, RangeError, RangeSpec};

    fn partial(start: u64, end: u64) -> RangeSpec {
        RangeSpec::Partial { start, end }
    }

    #[test]
    fn no_header_is_full() {
        assert_eq!(parse(None, 1000), Ok(RangeSpec::Full));
        assert_eq!(parse(None, 0), Ok(RangeSpec::Full));
    }

    #[test]
    fn total_zero_is_full_even_with_a_range() {
        assert_eq!(parse(Some("bytes=0-0"), 0), Ok(RangeSpec::Full));
        assert_eq!(parse(Some("bytes=0-"), 0), Ok(RangeSpec::Full));
        assert_eq!(parse(Some("bytes=-1"), 0), Ok(RangeSpec::Full));
        assert_eq!(parse(Some("bytes=0-1,3-4"), 0), Ok(RangeSpec::Full));
    }

    #[test]
    fn explicit_range_is_inclusive() {
        assert_eq!(parse(Some("bytes=0-1023"), 4096), Ok(partial(0, 1023)));
        assert_eq!(parse(Some("bytes=500-999"), 1000), Ok(partial(500, 999)));
    }

    #[test]
    fn single_byte_range() {
        assert_eq!(parse(Some("bytes=0-0"), 1000), Ok(partial(0, 0)));
    }

    #[test]
    fn open_ended_range_runs_to_the_last_byte() {
        assert_eq!(parse(Some("bytes=500-"), 1000), Ok(partial(500, 999)));
    }

    #[test]
    fn open_ended_whole_file() {
        assert_eq!(parse(Some("bytes=0-"), 1000), Ok(partial(0, 999)));
    }

    #[test]
    fn suffix_is_the_last_n_bytes() {
        assert_eq!(parse(Some("bytes=-500"), 1000), Ok(partial(500, 999)));
    }

    #[test]
    fn suffix_of_one_is_the_last_byte() {
        assert_eq!(parse(Some("bytes=-1"), 1000), Ok(partial(999, 999)));
    }

    #[test]
    fn suffix_longer_than_file_is_the_whole_file() {
        assert_eq!(parse(Some("bytes=-5000"), 1000), Ok(partial(0, 999)));
        assert_eq!(
            parse(Some("bytes=-18446744073709551615"), 1000),
            Ok(partial(0, 999))
        );
    }

    #[test]
    fn suffix_of_zero_is_unsatisfiable() {
        assert_eq!(
            parse(Some("bytes=-0"), 1000),
            Err(RangeError::Unsatisfiable)
        );
    }

    #[test]
    fn end_beyond_eof_is_clamped() {
        assert_eq!(parse(Some("bytes=0-999999"), 1000), Ok(partial(0, 999)));
        assert_eq!(parse(Some("bytes=10-5000"), 1000), Ok(partial(10, 999)));
    }

    #[test]
    fn start_at_eof_is_unsatisfiable() {
        assert_eq!(
            parse(Some("bytes=999999-"), 1000),
            Err(RangeError::Unsatisfiable)
        );
        assert_eq!(
            parse(Some("bytes=1000-1000"), 1000),
            Err(RangeError::Unsatisfiable)
        );
        assert_eq!(
            parse(Some("bytes=1000-2000"), 1000),
            Err(RangeError::Unsatisfiable)
        );
    }

    #[test]
    fn start_after_end_is_unsatisfiable() {
        assert_eq!(
            parse(Some("bytes=5-2"), 1000),
            Err(RangeError::Unsatisfiable)
        );
    }

    #[test]
    fn start_equals_end_is_one_byte() {
        assert_eq!(parse(Some("bytes=5-5"), 1000), Ok(partial(5, 5)));
        assert_eq!(parse(Some("bytes=999-999"), 1000), Ok(partial(999, 999)));
    }

    #[test]
    fn multi_range_is_full() {
        assert_eq!(parse(Some("bytes=0-1,3-4"), 1000), Ok(RangeSpec::Full));
        assert_eq!(parse(Some("bytes=0-0,2-2,4-4"), 1000), Ok(RangeSpec::Full));
        assert_eq!(parse(Some("bytes=0-1,"), 1000), Ok(RangeSpec::Full));
    }

    #[test]
    fn empty_spec_is_full() {
        assert_eq!(parse(Some("bytes="), 1000), Ok(RangeSpec::Full));
        assert_eq!(parse(Some("bytes=  "), 1000), Ok(RangeSpec::Full));
        assert_eq!(parse(Some(""), 1000), Ok(RangeSpec::Full));
        assert_eq!(parse(Some("bytes=-"), 1000), Ok(RangeSpec::Full));
    }

    #[test]
    fn non_numeric_is_full() {
        assert_eq!(parse(Some("bytes=abc"), 1000), Ok(RangeSpec::Full));
        assert_eq!(parse(Some("bytes=abc-def"), 1000), Ok(RangeSpec::Full));
        assert_eq!(parse(Some("bytes=1-x"), 1000), Ok(RangeSpec::Full));
        assert_eq!(parse(Some("bytes=x-1"), 1000), Ok(RangeSpec::Full));
        assert_eq!(parse(Some("bytes=--5"), 1000), Ok(RangeSpec::Full));
    }

    #[test]
    fn other_unit_is_full() {
        assert_eq!(parse(Some("items=0-1"), 1000), Ok(RangeSpec::Full));
        assert_eq!(parse(Some("seconds=0-1"), 1000), Ok(RangeSpec::Full));
    }

    #[test]
    fn missing_unit_is_full() {
        assert_eq!(parse(Some("0-1"), 1000), Ok(RangeSpec::Full));
        assert_eq!(parse(Some("=0-1"), 1000), Ok(RangeSpec::Full));
    }

    #[test]
    fn overflowing_number_is_full() {
        assert_eq!(
            parse(Some("bytes=0-18446744073709551616"), 1000),
            Ok(RangeSpec::Full)
        );
        assert_eq!(
            parse(Some("bytes=99999999999999999999999-"), 1000),
            Ok(RangeSpec::Full)
        );
        assert_eq!(
            parse(Some("bytes=-99999999999999999999999999"), 1000),
            Ok(RangeSpec::Full)
        );
    }

    #[test]
    fn unit_is_case_insensitive() {
        assert_eq!(parse(Some("Bytes=0-1"), 1000), Ok(partial(0, 1)));
        assert_eq!(parse(Some("BYTES=0-1"), 1000), Ok(partial(0, 1)));
    }

    #[test]
    fn whitespace_around_the_spec_is_allowed() {
        assert_eq!(parse(Some(" bytes = 5 - 10 "), 1000), Ok(partial(5, 10)));
        assert_eq!(parse(Some("bytes= -500"), 1000), Ok(partial(500, 999)));
        assert_eq!(parse(Some("bytes= 500 - "), 1000), Ok(partial(500, 999)));
    }

    #[test]
    fn leading_zeros_are_accepted() {
        assert_eq!(parse(Some("bytes=000-0002"), 1000), Ok(partial(0, 2)));
        assert_eq!(parse(Some("bytes=-0001"), 1000), Ok(partial(999, 999)));
    }

    #[test]
    fn sign_is_not_a_digit() {
        assert_eq!(parse(Some("bytes=+1-2"), 1000), Ok(RangeSpec::Full));
    }

    #[test]
    fn u64_max_total_open_ended() {
        assert_eq!(
            parse(Some("bytes=0-"), u64::MAX),
            Ok(partial(0, u64::MAX - 1))
        );
    }

    #[test]
    fn u64_max_total_suffix() {
        assert_eq!(
            parse(Some("bytes=-500"), u64::MAX),
            Ok(partial(u64::MAX - 500, u64::MAX - 1))
        );
    }

    #[test]
    fn u64_max_total_start_is_unsatisfiable() {
        assert_eq!(
            parse(Some("bytes=18446744073709551615-"), u64::MAX),
            Err(RangeError::Unsatisfiable)
        );
    }

    #[test]
    fn u64_max_end_is_clamped() {
        assert_eq!(
            parse(Some("bytes=0-18446744073709551615"), u64::MAX),
            Ok(partial(0, u64::MAX - 1))
        );
        assert_eq!(
            parse(Some("bytes=10-18446744073709551615"), u64::MAX),
            Ok(partial(10, u64::MAX - 1))
        );
    }
}

/// Map a `Range` header value and the representation length to the response
/// arithmetic. See the [module contract](self) for the exact rule set.
pub fn parse(header: Option<&str>, total: u64) -> Result<RangeSpec, RangeError> {
    // An empty representation selects no byte, so no range applies; the
    // header cannot make the answer anything but a zero-length 200.
    if total == 0 {
        return Ok(RangeSpec::Full);
    }

    let Some(header) = header else {
        return Ok(RangeSpec::Full);
    };

    let Some((unit, spec)) = header.split_once('=') else {
        return Ok(RangeSpec::Full);
    };
    if !unit.trim().eq_ignore_ascii_case("bytes") {
        return Ok(RangeSpec::Full);
    }

    let spec = spec.trim();
    // Multi-range (or a malformed set with empty elements): ignore, serve all.
    if spec.contains(',') {
        return Ok(RangeSpec::Full);
    }

    let Some((first, last)) = spec.split_once('-') else {
        return Ok(RangeSpec::Full);
    };
    let (first, last) = (first.trim(), last.trim());

    if first.is_empty() {
        // Suffix range: the last `last` bytes.
        let Some(suffix) = digits(last) else {
            return Ok(RangeSpec::Full);
        };
        if suffix == 0 {
            return Err(RangeError::Unsatisfiable);
        }
        return Ok(RangeSpec::Partial {
            start: total.saturating_sub(suffix),
            end: total - 1,
        });
    }

    let Some(start) = digits(first) else {
        return Ok(RangeSpec::Full);
    };
    let end = if last.is_empty() {
        total - 1
    } else {
        let Some(end) = digits(last) else {
            return Ok(RangeSpec::Full);
        };
        if start > end {
            return Err(RangeError::Unsatisfiable);
        }
        end
    };

    if start >= total {
        return Err(RangeError::Unsatisfiable);
    }

    Ok(RangeSpec::Partial {
        start,
        end: end.min(total - 1),
    })
}

/// Parse an unsigned decimal number that consists only of ASCII digits, so a
/// sign, whitespace, or an empty string is not a number. `None` on overflow.
fn digits(text: &str) -> Option<u64> {
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    text.parse::<u64>().ok()
}
