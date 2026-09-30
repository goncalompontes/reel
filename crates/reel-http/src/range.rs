//! HTTP `Range: bytes=...` parsing.
//!
//! Kept separate from the handlers because it is the part most likely to have
//! off-by-one bugs, and the part most worth unit-testing.

use std::fmt;

/// A resolved byte range, inclusive on both ends (HTTP semantics).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteRange {
    pub start: u64,
    /// Inclusive.
    pub end: u64,
}

impl ByteRange {
    pub fn len(&self) -> u64 {
        self.end - self.start + 1
    }

    pub fn is_empty(&self) -> bool {
        false
    }
}

impl fmt::Display for ByteRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}-{}", self.start, self.end)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RangeError {
    /// The range is syntactically valid but lies outside the file.
    #[error("requested range is not satisfiable")]
    Unsatisfiable,
    /// The header is malformed; per RFC 9110 we ignore it and send the whole file.
    #[error("malformed range header")]
    Malformed,
}

/// Parse a `Range` header value against a known content length.
///
/// Returns:
/// * `Ok(None)` when the header is absent or malformed (serve the whole file),
/// * `Ok(Some(range))` for the first satisfiable range,
/// * `Err(Unsatisfiable)` when the client asked for bytes past the end.
///
/// Multi-range requests are not supported: the first range is used, which is
/// what every media player actually relies on.
pub fn parse_range(header: Option<&str>, content_length: u64) -> Result<Option<ByteRange>, RangeError> {
    let Some(raw) = header else {
        return Ok(None);
    };
    let raw = raw.trim();
    let Some(spec) = raw.strip_prefix("bytes=") else {
        return Ok(None);
    };
    // Only the first range of a multi-range request is honoured.
    let spec = spec.split(',').next().unwrap_or("").trim();
    let Some((start_s, end_s)) = spec.split_once('-') else {
        return Ok(None);
    };
    let (start_s, end_s) = (start_s.trim(), end_s.trim());

    if content_length == 0 {
        return Err(RangeError::Unsatisfiable);
    }

    let last = content_length - 1;

    let (start, end) = match (start_s.is_empty(), end_s.is_empty()) {
        // "-N": the last N bytes.
        (true, false) => {
            let Ok(suffix) = end_s.parse::<u64>() else {
                return Ok(None);
            };
            if suffix == 0 {
                return Err(RangeError::Unsatisfiable);
            }
            let start = content_length.saturating_sub(suffix);
            (start, last)
        }
        // "N-": from N to the end.
        (false, true) => {
            let Ok(start) = start_s.parse::<u64>() else {
                return Ok(None);
            };
            (start, last)
        }
        // "N-M".
        (false, false) => {
            let (Ok(start), Ok(end)) = (start_s.parse::<u64>(), end_s.parse::<u64>()) else {
                return Ok(None);
            };
            if end < start {
                return Err(RangeError::Unsatisfiable);
            }
            // Clamp rather than reject: clients commonly over-ask.
            (start, end.min(last))
        }
        (true, true) => return Ok(None),
    };

    if start > last {
        return Err(RangeError::Unsatisfiable);
    }

    Ok(Some(ByteRange { start, end }))
}

#[cfg(test)]
mod tests {
    use super::*;

    const LEN: u64 = 1000;

    fn r(start: u64, end: u64) -> Option<ByteRange> {
        Some(ByteRange { start, end })
    }

    #[test]
    fn absent_header_means_whole_file() {
        assert_eq!(parse_range(None, LEN).unwrap(), None);
    }

    #[test]
    fn closed_range() {
        assert_eq!(parse_range(Some("bytes=0-99"), LEN).unwrap(), r(0, 99));
        assert_eq!(parse_range(Some("bytes=500-999"), LEN).unwrap(), r(500, 999));
    }

    #[test]
    fn open_ended_range() {
        assert_eq!(parse_range(Some("bytes=200-"), LEN).unwrap(), r(200, 999));
    }

    #[test]
    fn suffix_range() {
        assert_eq!(parse_range(Some("bytes=-100"), LEN).unwrap(), r(900, 999));
        // Suffix longer than the file clamps to the whole file.
        assert_eq!(parse_range(Some("bytes=-5000"), LEN).unwrap(), r(0, 999));
    }

    #[test]
    fn end_is_clamped() {
        assert_eq!(parse_range(Some("bytes=0-100000"), LEN).unwrap(), r(0, 999));
    }

    #[test]
    fn multi_range_uses_first() {
        assert_eq!(
            parse_range(Some("bytes=0-9, 20-29"), LEN).unwrap(),
            r(0, 9)
        );
    }

    #[test]
    fn unsupported_unit_is_ignored() {
        assert_eq!(parse_range(Some("items=0-9"), LEN).unwrap(), None);
    }

    #[test]
    fn malformed_is_ignored() {
        assert_eq!(parse_range(Some("bytes=abc"), LEN).unwrap(), None);
        assert_eq!(parse_range(Some("bytes=-"), LEN).unwrap(), None);
        assert_eq!(parse_range(Some("garbage"), LEN).unwrap(), None);
    }

    #[test]
    fn unsatisfiable() {
        assert_eq!(
            parse_range(Some("bytes=1000-"), LEN),
            Err(RangeError::Unsatisfiable)
        );
        assert_eq!(
            parse_range(Some("bytes=500-100"), LEN),
            Err(RangeError::Unsatisfiable)
        );
        assert_eq!(
            parse_range(Some("bytes=-0"), LEN),
            Err(RangeError::Unsatisfiable)
        );
        assert_eq!(
            parse_range(Some("bytes=0-0"), 0),
            Err(RangeError::Unsatisfiable)
        );
    }

    #[test]
    fn single_byte_range() {
        let range = parse_range(Some("bytes=5-5"), LEN).unwrap().unwrap();
        assert_eq!(range.len(), 1);
    }

    #[test]
    fn range_len() {
        assert_eq!(ByteRange { start: 0, end: 0 }.len(), 1);
        assert_eq!(ByteRange { start: 0, end: 999 }.len(), 1000);
    }
}
