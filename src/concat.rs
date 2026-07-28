//! Built-in `concat:` driver — concatenate multiple in-process sub-sources
//! into one seekable byte stream.
//!
//! The scheme has **no on-wire spec**; it follows the de-facto
//! `concat:a|b|c` shape (a `|`-separated list of segments after the
//! `concat:` prefix). The opened segments are presented as a single
//! logical stream whose length is the sum of the segment lengths, with
//! `Read` walking segment boundaries transparently and `Seek` resolving
//! an absolute offset to `(segment, intra-offset)`.
//!
//! Each segment is resolved through the shared in-process dispatcher
//! ([`crate::open_bytes`]), minus `concat:` itself:
//!
//! - `file://` and bare paths (delegates to [`crate::open_file`]).
//! - `mem://<id>` (delegates to [`crate::open_mem`]).
//! - `data:[<mediatype>][;base64],<bytes>` (delegates to [`crate::open_data`]).
//! - `slice:<offset>+<length>!<inner-uri>` (delegates to [`crate::open_slice`]).
//! - `concat:` itself is **not** allowed as a segment — a nested
//!   `concat:` would have to embed unescaped `|` separators, which the
//!   outer split would shred. Use a single flattened list. (The reverse
//!   nesting — `concat:` as a `slice:` inner — *is* supported, because
//!   the slice grammar splits on `!`, never on `|`.)
//!
//! Grammar (informal):
//!
//! ```text
//! concaturl = "concat:" segment *( "|" segment )
//! segment   = <bare path, file://, mem://, data:, or slice: URI with no embedded '|'>
//! ```
//!
//! At least one non-empty segment is required. An empty segment (e.g. a
//! trailing `|` or `a||b`) is rejected so a typo does not silently
//! collapse to fewer inputs. A literal `|` inside an inner URI is not
//! supported; segments are split on the first level of `|` only.
//!
//! Each segment's byte length is captured at open time via
//! `Seek::seek(SeekFrom::End(0))`, so the composite supports
//! `SeekFrom::End` and reports a stable length. Segments are assumed not
//! to change size while the composite is open; if one is truncated under
//! us a `Read` near its tail surfaces the short read from the underlying
//! source like any other reader.
//!
//! Clean-room note: only the public in-process openers and the standard
//! `Read`/`Seek` traits were used. No external `concat:` implementation
//! was consulted.

use std::io::{self, Read, Seek, SeekFrom};

use oxideav_core::{BytesSource, Error, Result};

use crate::uri;

/// A composite [`BytesSource`] that reads several sub-sources in order as
/// one contiguous stream.
///
/// Construction captures each segment's length (via a seek to its end),
/// builds the cumulative-offset table, and rewinds the first segment to
/// its start. `Read` and `Seek` then operate on the virtual address
/// space `[0, total_len)`.
struct ConcatSource {
    /// Open sub-sources, in concatenation order.
    parts: Vec<Box<dyn BytesSource>>,
    /// `starts[i]` is the absolute offset at which `parts[i]` begins;
    /// `starts[parts.len()]` is the total length. Monotonically
    /// non-decreasing (a zero-length part repeats the previous start).
    starts: Vec<u64>,
    /// Current absolute read position in `[0, total_len]`.
    pos: u64,
}

impl ConcatSource {
    /// Build a composite from already-opened, individually-seekable
    /// sub-sources. Each is seeked to its end to learn its length, then
    /// the first is rewound to offset 0 so reads start from the front.
    fn new(mut parts: Vec<Box<dyn BytesSource>>) -> Result<Self> {
        let mut starts = Vec::with_capacity(parts.len() + 1);
        let mut acc: u64 = 0;
        for part in parts.iter_mut() {
            starts.push(acc);
            let len = part.seek(SeekFrom::End(0))?;
            acc = acc
                .checked_add(len)
                .ok_or_else(|| Error::invalid("concat: total length overflows u64"))?;
        }
        starts.push(acc);
        // Rewind the first segment so a fresh composite reads from byte 0
        // without an explicit seek by the caller.
        if let Some(first) = parts.first_mut() {
            first.seek(SeekFrom::Start(0))?;
        }
        Ok(Self {
            parts,
            starts,
            pos: 0,
        })
    }

    /// Total length of the composite stream.
    fn total_len(&self) -> u64 {
        *self
            .starts
            .last()
            .expect("starts always has a trailing total")
    }

    /// Index of the segment containing absolute offset `pos`, or `None`
    /// if `pos` is at or past the end. Zero-length segments are skipped:
    /// the returned segment always has room for at least one byte.
    fn segment_for(&self, pos: u64) -> Option<usize> {
        if pos >= self.total_len() {
            return None;
        }
        // starts[i] <= pos < starts[i+1]; pick the segment whose half-open
        // range contains pos. Linear scan — segment counts are tiny.
        (0..self.parts.len()).find(|&i| pos >= self.starts[i] && pos < self.starts[i + 1])
    }
}

impl Read for ConcatSource {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let idx = match self.segment_for(self.pos) {
            Some(i) => i,
            None => return Ok(0), // at or past EOF
        };
        // Bytes remaining in this segment from the current position.
        let seg_start = self.starts[idx];
        let seg_end = self.starts[idx + 1];
        let intra = self.pos - seg_start;
        let remaining_in_seg = (seg_end - self.pos) as usize;
        let want = buf.len().min(remaining_in_seg);

        // Position the underlying segment, then read up to `want` bytes.
        let part = &mut self.parts[idx];
        part.seek(SeekFrom::Start(intra))?;
        let n = part.read(&mut buf[..want])?;
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for ConcatSource {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let total = self.total_len();
        let new_pos = match from {
            SeekFrom::Start(off) => off,
            SeekFrom::End(off) => add_signed(total, off)?,
            SeekFrom::Current(off) => add_signed(self.pos, off)?,
        };
        self.pos = new_pos;
        Ok(self.pos)
    }
}

/// Add a signed offset to an unsigned base, mapping under/overflow to an
/// `InvalidInput` error (matching `io::Cursor` seek semantics: seeking
/// before byte 0 is an error, seeking past the end is allowed).
fn add_signed(base: u64, off: i64) -> io::Result<u64> {
    let result = if off >= 0 {
        base.checked_add(off as u64)
    } else {
        base.checked_sub(off.unsigned_abs())
    };
    result.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "concat: seek resolves to a negative or overflowing position",
        )
    })
}

/// Split the `concat:` payload into its `|`-separated segment list.
/// Returns an error if any segment is empty (so `a||b` or a trailing
/// `|` is caught rather than silently dropped).
fn segments(rest: &str) -> Result<Vec<&str>> {
    let parts: Vec<&str> = rest.split('|').collect();
    if parts.iter().any(|s| s.is_empty()) {
        return Err(Error::invalid(format!(
            "concat: URI has an empty segment: {rest:?}"
        )));
    }
    Ok(parts)
}

/// Resolve a single `concat:` segment by dispatching to one of the
/// bundled in-process openers. Mirrors the dispatch surface of the
/// `slice:` driver: `file://` / bare paths, `mem://`, `data:`, and
/// `slice:`. A `concat:` segment is rejected — see the module docs for
/// the rationale (nested `concat:` would re-enter the outer `|` split).
fn open_segment(seg: &str) -> Result<Box<dyn BytesSource>> {
    let (seg_scheme, _) = uri::split(seg);
    // A nested concat: segment gets its own rejection up front — the
    // shared dispatcher would happily open it, but a nested concat URI
    // cannot survive the outer '|' split (its own separators were
    // already shredded), so any such segment reaching this point is a
    // caller error worth a precise message.
    if uri::scheme_is(seg_scheme, "concat") {
        return Err(Error::invalid(format!(
            "concat: segment {seg:?} is itself a concat: URI; nesting concat is not supported \
             because the outer '|' split would shred the inner segment list"
        )));
    }
    // Everything else goes through the shared in-process dispatcher
    // (file:// / bare paths, mem://, data:, slice:); unsupported
    // schemes surface its invalid-data error, other failures keep
    // their underlying taxonomy (missing file stays an IO error).
    crate::open_bytes(seg)
}

/// Parsed components of a `concat:` URI.
///
/// Completes the typed-URI triad next to [`crate::DataUri`] and
/// [`crate::SliceUri`]: a public typed view over the parsed segment
/// list, so callers that want to inspect, filter, or re-order segments
/// (CLI parsers, playlist tooling, fixture builders) can do so without
/// re-implementing the `|` grammar.
///
/// Round-trip: [`parse`] followed by [`ConcatUri::format`] reproduces a
/// byte-identical URI for every canonical input the parser accepts
/// (lowercase `concat:` scheme, no `//` after the colon) — the split
/// on `|` and re-join are exact inverses because empty segments are
/// rejected and segments cannot contain `|`. The equivalent `CONCAT:`
/// / `concat://` spellings normalise to the canonical form.
/// `parse(format(x)) == x` holds for every accepted input.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConcatUri {
    /// Segment URIs in concatenation order. Each is any scheme
    /// [`open_concat`] accepts as a segment (`file://` / bare path,
    /// `mem://`, `data:`, `slice:`); the parser only guarantees
    /// non-empty and `|`-free (scheme validity, including the nested
    /// `concat:` rejection, is an open-time check).
    pub segments: Vec<String>,
}

impl ConcatUri {
    /// Build a `ConcatUri` from a segment list. Rejects an empty list
    /// (the URI form has no zero-segment spelling), any empty segment
    /// (the grammar reserves that as a typo guard), and any segment
    /// containing a literal `|` (it would be shredded by the split on
    /// re-parse, so the value could not round-trip).
    ///
    /// Mirrors [`crate::SliceUri::new`]'s philosophy: only grammar
    /// safety is validated here; whether each segment's scheme is
    /// openable (and the nested-`concat:` rejection) is [`open`](Self::open)'s
    /// job, exactly as for the string entry point.
    pub fn new<I, S>(segments: I) -> Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let segments: Vec<String> = segments.into_iter().map(Into::into).collect();
        if segments.is_empty() {
            return Err(Error::invalid("concat: URI requires at least one segment"));
        }
        for seg in &segments {
            if seg.is_empty() {
                return Err(Error::invalid("concat: URI segment cannot be empty"));
            }
            if seg.contains('|') {
                return Err(Error::invalid(format!(
                    "concat: URI segment {seg:?} contains a '|'; a literal '|' inside a \
                     segment cannot round-trip because the grammar splits on every '|'"
                )));
            }
        }
        if segments[0].starts_with("//") {
            // `format` would emit `concat://<...>`, whose leading `//` the
            // scheme splitter consumes as the authority-style spelling —
            // every parse of the formatted form would eat two more
            // slashes, so the value cannot round-trip.
            return Err(Error::invalid(format!(
                "concat: first segment {:?} starts with \"//\"; it cannot round-trip \
                 because the formatted URI's leading '//' reads as the authority-style \
                 `concat://` spelling",
                segments[0]
            )));
        }
        Ok(Self { segments })
    }

    /// Format this `ConcatUri` back into its canonical
    /// `concat:<a>|<b>|…` string form. [`parse`] followed by `format`
    /// reproduces a byte-identical URI for every canonical input; the
    /// equivalent `CONCAT:` / `concat://` spellings the parser also
    /// accepts normalise to this form.
    pub fn format(&self) -> String {
        format!("concat:{}", self.segments.join("|"))
    }

    /// Open the concatenation described by this typed value directly,
    /// without round-tripping through the URI string. Each segment is
    /// resolved through the shared in-process dispatcher (with the
    /// nested-`concat:` rejection), then presented as one seekable
    /// stream. The typed analogue of [`open_concat`], and byte-for-byte
    /// identical to `open_concat(&self.format())`.
    pub fn open(&self) -> Result<Box<dyn BytesSource>> {
        let mut parts: Vec<Box<dyn BytesSource>> = Vec::with_capacity(self.segments.len());
        for seg in &self.segments {
            parts.push(open_segment(seg)?);
        }
        Ok(Box::new(ConcatSource::new(parts)?))
    }
}

impl std::fmt::Display for ConcatUri {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "concat:{}", self.segments.join("|"))
    }
}

/// Parse a `concat:` URI into its [`ConcatUri`] components without
/// opening any segment. Rejects a wrong scheme, an empty segment list,
/// and empty segments; segment *schemes* are not validated (that is an
/// open-time concern), matching the `slice:` parser's philosophy
/// ([`crate::parse_slice_uri`]).
pub fn parse(uri_str: &str) -> Result<ConcatUri> {
    let (scheme, rest) = uri::split(uri_str);
    if !uri::scheme_is(scheme, "concat") {
        return Err(Error::invalid(format!(
            "concat driver invoked on non-concat URI: {uri_str}"
        )));
    }
    if rest.is_empty() {
        return Err(Error::invalid("concat: URI requires at least one segment"));
    }
    let segs = segments(rest)?;
    if segs[0].starts_with("//") {
        // The scheme splitter has already consumed one authority-style
        // `//`; a first segment STILL starting with `//` means the URI
        // carried four-plus leading slashes (`concat:////x|…`). Such a
        // value cannot round-trip — each `format` → `parse` cycle would
        // strip two more slashes — so reject it up front instead of
        // silently mutating the segment on every pass.
        return Err(Error::invalid(format!(
            "concat: first segment {:?} starts with \"//\" after the scheme split; \
             a first segment with leading '//' is ambiguous with the authority-style \
             `concat://` spelling and cannot round-trip",
            segs[0]
        )));
    }
    Ok(ConcatUri {
        segments: segs.into_iter().map(str::to_string).collect(),
    })
}

/// Open a `concat:<a>|<b>|…` URI as a single [`BytesSource`] that reads
/// the segments back-to-back. Each segment may be a bare path, a
/// `file://` URL, a `mem://<id>` reference, a `data:` literal, or a
/// `slice:` URI.
///
/// Equivalent to [`parse`] followed by [`ConcatUri::open`]: the string
/// and typed-value entry points share a single open code path and
/// cannot drift apart.
pub fn open_concat(uri_str: &str) -> Result<Box<dyn BytesSource>> {
    parse(uri_str)?.open()
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Seek, SeekFrom, Write};

    use crate::mem;

    use super::*;

    /// Write `bytes` to a uniquely-named temp file and return its path.
    fn temp_file(bytes: &[u8]) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let mut path = std::env::temp_dir();
        let pid = std::process::id();
        let n = N.fetch_add(1, Ordering::Relaxed);
        path.push(format!("oxideav-concat-test-{pid}-{n}.bin"));
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(bytes).unwrap();
        f.flush().unwrap();
        path
    }

    fn uri_for(paths: &[&std::path::Path]) -> String {
        let joined: Vec<String> = paths.iter().map(|p| p.display().to_string()).collect();
        format!("concat:{}", joined.join("|"))
    }

    #[test]
    fn two_files_read_back_to_back() {
        let a = temp_file(b"Hello, ");
        let b = temp_file(b"world!");
        let mut r = open_concat(&uri_for(&[&a, &b])).unwrap();
        let mut buf = Vec::new();
        r.read_to_end(&mut buf).unwrap();
        assert_eq!(buf, b"Hello, world!");
        std::fs::remove_file(a).ok();
        std::fs::remove_file(b).ok();
    }

    #[test]
    fn three_segments() {
        let a = temp_file(b"AAA");
        let b = temp_file(b"BB");
        let c = temp_file(b"CCCC");
        let mut r = open_concat(&uri_for(&[&a, &b, &c])).unwrap();
        let mut buf = Vec::new();
        r.read_to_end(&mut buf).unwrap();
        assert_eq!(buf, b"AAABBCCCC");
        for p in [a, b, c] {
            std::fs::remove_file(p).ok();
        }
    }

    #[test]
    fn small_buffer_reads_cross_boundary() {
        // Force the boundary-walking path: read 1 byte at a time.
        let a = temp_file(b"XY");
        let b = temp_file(b"Z");
        let mut r = open_concat(&uri_for(&[&a, &b])).unwrap();
        let mut out = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            let n = r.read(&mut byte).unwrap();
            if n == 0 {
                break;
            }
            out.push(byte[0]);
        }
        assert_eq!(out, b"XYZ");
        std::fs::remove_file(a).ok();
        std::fs::remove_file(b).ok();
    }

    #[test]
    fn seek_end_reports_total_length() {
        let a = temp_file(b"12345");
        let b = temp_file(b"678");
        let mut r = open_concat(&uri_for(&[&a, &b])).unwrap();
        let end = r.seek(SeekFrom::End(0)).unwrap();
        assert_eq!(end, 8);
        std::fs::remove_file(a).ok();
        std::fs::remove_file(b).ok();
    }

    #[test]
    fn seek_into_second_segment() {
        let a = temp_file(b"abcd"); // offsets 0..4
        let b = temp_file(b"EFGH"); // offsets 4..8
        let mut r = open_concat(&uri_for(&[&a, &b])).unwrap();
        r.seek(SeekFrom::Start(5)).unwrap(); // second byte of segment b
        let mut byte = [0u8; 1];
        r.read_exact(&mut byte).unwrap();
        assert_eq!(byte[0], b'F');
        std::fs::remove_file(a).ok();
        std::fs::remove_file(b).ok();
    }

    #[test]
    fn seek_across_boundary_then_read() {
        let a = temp_file(b"abc"); // 0..3
        let b = temp_file(b"defg"); // 3..7
        let mut r = open_concat(&uri_for(&[&a, &b])).unwrap();
        r.seek(SeekFrom::Start(2)).unwrap();
        let mut buf = [0u8; 4]; // spans last byte of a + first 3 of b
        r.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"cdef");
        std::fs::remove_file(a).ok();
        std::fs::remove_file(b).ok();
    }

    #[test]
    fn seek_current_relative() {
        let a = temp_file(b"0123456789");
        let mut r = open_concat(&uri_for(&[&a])).unwrap();
        r.seek(SeekFrom::Start(3)).unwrap();
        let p = r.seek(SeekFrom::Current(2)).unwrap();
        assert_eq!(p, 5);
        let mut byte = [0u8; 1];
        r.read_exact(&mut byte).unwrap();
        assert_eq!(byte[0], b'5');
        std::fs::remove_file(a).ok();
    }

    #[test]
    fn seek_before_zero_errors() {
        let a = temp_file(b"abc");
        let mut r = open_concat(&uri_for(&[&a])).unwrap();
        let res = r.seek(SeekFrom::Current(-1));
        assert!(res.is_err());
        std::fs::remove_file(a).ok();
    }

    #[test]
    fn read_at_eof_returns_zero() {
        let a = temp_file(b"hi");
        let mut r = open_concat(&uri_for(&[&a])).unwrap();
        r.seek(SeekFrom::End(0)).unwrap();
        let mut buf = [0u8; 4];
        assert_eq!(r.read(&mut buf).unwrap(), 0);
        std::fs::remove_file(a).ok();
    }

    #[test]
    fn empty_segment_rejected() {
        assert!(open_concat("concat:a||b").is_err());
        assert!(open_concat("concat:a|").is_err());
        assert!(open_concat("concat:|a").is_err());
    }

    #[test]
    fn no_segments_rejected() {
        assert!(open_concat("concat:").is_err());
    }

    #[test]
    fn wrong_scheme_rejected() {
        assert!(open_concat("file:///tmp/x").is_err());
        assert!(open_concat("mem://x").is_err());
    }

    #[test]
    fn file_url_segment_accepted() {
        let a = temp_file(b"pre-");
        let b = temp_file(b"post");
        let uri = format!("concat:file://{}|file://{}", a.display(), b.display());
        let mut r = open_concat(&uri).unwrap();
        let mut buf = Vec::new();
        r.read_to_end(&mut buf).unwrap();
        assert_eq!(buf, b"pre-post");
        std::fs::remove_file(a).ok();
        std::fs::remove_file(b).ok();
    }

    #[test]
    fn missing_file_segment_errors() {
        let a = temp_file(b"ok");
        let uri = format!("concat:{}|/no/such/path/xyzzy-oxideav", a.display());
        assert!(open_concat(&uri).is_err());
        std::fs::remove_file(a).ok();
    }

    #[test]
    fn empty_file_segment_is_transparent() {
        // A zero-length middle segment must not break boundary math.
        let a = temp_file(b"AB");
        let empty = temp_file(b"");
        let c = temp_file(b"CD");
        let mut r = open_concat(&uri_for(&[&a, &empty, &c])).unwrap();
        let mut buf = Vec::new();
        r.read_to_end(&mut buf).unwrap();
        assert_eq!(buf, b"ABCD");
        for p in [a, empty, c] {
            std::fs::remove_file(p).ok();
        }
    }

    #[test]
    fn data_uri_segment_accepted() {
        // Two inline literals: "Hello, " and "world!".
        let mut r = open_concat("concat:data:,Hello%2C%20|data:,world%21").unwrap();
        let mut buf = Vec::new();
        r.read_to_end(&mut buf).unwrap();
        assert_eq!(buf, b"Hello, world!");
    }

    #[test]
    fn mem_segment_accepted() {
        mem::put("concat-r184-a", b"AAA".to_vec());
        mem::put("concat-r184-b", b"BB".to_vec());
        let mut r = open_concat("concat:mem://concat-r184-a|mem://concat-r184-b").unwrap();
        let mut buf = Vec::new();
        r.read_to_end(&mut buf).unwrap();
        assert_eq!(buf, b"AAABB");
        mem::remove("concat-r184-a");
        mem::remove("concat-r184-b");
    }

    #[test]
    fn slice_segment_accepted() {
        // Slice a mem buffer down to [2, 5) then concat with a file.
        mem::put("concat-r184-slc", b"abcdefgh".to_vec());
        let f = temp_file(b"XYZ");
        let uri = format!("concat:slice:2+3!mem://concat-r184-slc|{}", f.display());
        let mut r = open_concat(&uri).unwrap();
        let mut buf = Vec::new();
        r.read_to_end(&mut buf).unwrap();
        assert_eq!(buf, b"cdeXYZ");
        mem::remove("concat-r184-slc");
        std::fs::remove_file(f).ok();
    }

    #[test]
    fn mixed_schemes_concat_in_order() {
        // file + mem + data, with a cross-boundary seek to prove the
        // composite address space behaves regardless of underlying scheme.
        mem::put("concat-r184-mix", b"MID".to_vec());
        let head = temp_file(b"HEAD"); // 4 bytes, offsets 0..4
                                       // mem        : 3 bytes, offsets 4..7
                                       // data:,TAIL : 4 bytes, offsets 7..11
        let uri = format!("concat:{}|mem://concat-r184-mix|data:,TAIL", head.display());
        let mut r = open_concat(&uri).unwrap();
        let total = r.seek(SeekFrom::End(0)).unwrap();
        assert_eq!(total, 11);
        r.seek(SeekFrom::Start(3)).unwrap(); // last byte of HEAD
        let mut buf = vec![0u8; 6]; // "DMIDTA" — spans 3 segments
        r.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"DMIDTA");
        mem::remove("concat-r184-mix");
        std::fs::remove_file(head).ok();
    }

    #[test]
    fn nested_concat_segment_rejected() {
        // A concat:-as-segment cannot be expressed unambiguously inside
        // an outer concat: URI because the '|' split would shred the
        // inner segment list. Reject explicitly.
        let res = open_concat("concat:concat:a|b|c");
        let err = res.err().expect("nested concat: must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("nesting concat") || msg.contains("not supported"),
            "expected nesting-concat rejection, got {msg}"
        );
    }

    #[test]
    fn unsupported_inner_scheme_rejected() {
        // http:// segments aren't dispatchable without registry context;
        // mirror the slice: driver's same rejection.
        let res = open_concat("concat:http://example.com/a|http://example.com/b");
        assert!(res.is_err());
    }

    // ---- typed `ConcatUri` parse / format / open ----

    #[test]
    fn typed_parse_basic() {
        let c = parse("concat:file:///a|mem://b|data:,C").unwrap();
        assert_eq!(c.segments, ["file:///a", "mem://b", "data:,C"]);
    }

    #[test]
    fn typed_parse_single_segment() {
        let c = parse("concat:/bare/path.bin").unwrap();
        assert_eq!(c.segments, ["/bare/path.bin"]);
    }

    #[test]
    fn typed_parse_rejections() {
        assert!(parse("file:///a").is_err(), "wrong scheme");
        assert!(parse("concat:").is_err(), "no segments");
        assert!(parse("concat:a||b").is_err(), "empty middle segment");
        assert!(parse("concat:a|").is_err(), "empty trailing segment");
    }

    #[test]
    fn typed_round_trip_byte_identical_for_canonical() {
        for uri in [
            "concat:/a",
            "concat:file:///a|mem://b",
            "concat:data:,x|slice:0+1!mem://y|/bare",
        ] {
            let parsed = parse(uri).expect(uri);
            assert_eq!(parsed.format(), uri, "round-trip mismatch on {uri}");
            let again = parse(&parsed.format()).unwrap();
            assert_eq!(again, parsed, "fixpoint mismatch on {uri}");
        }
    }

    #[test]
    fn typed_round_trip_normalises_equivalent_spellings() {
        let canonical = parse("concat:mem://a|mem://b").unwrap();
        for spelling in ["CONCAT:mem://a|mem://b", "concat://mem://a|mem://b"] {
            let parsed = parse(spelling).expect(spelling);
            assert_eq!(parsed, canonical, "{spelling} must parse equal");
            assert_eq!(parsed.format(), "concat:mem://a|mem://b");
        }
    }

    #[test]
    fn typed_constructor_validates_grammar() {
        assert!(ConcatUri::new(Vec::<String>::new()).is_err(), "empty list");
        assert!(ConcatUri::new(["a", ""]).is_err(), "empty segment");
        assert!(
            ConcatUri::new(["a", "b|c"]).is_err(),
            "'|' in a segment cannot round-trip"
        );
        // Scheme validity is an open-time concern (mirrors SliceUri::new
        // accepting an http:// inner): the constructor admits it...
        let c = ConcatUri::new(["http://example.com/x"]).unwrap();
        // ...and open rejects it.
        assert!(c.open().is_err());
    }

    #[test]
    fn leading_double_slash_first_segment_rejected() {
        // Fuzz-found (uri_parse target): `concat:////a|b` survives the
        // scheme splitter's single `//` strip with a first segment of
        // `//a`, which `format` re-emits as `concat://a|b` — and the
        // NEXT parse strips two more slashes. The fixpoint
        // `parse(format(x)) == x` demands rejection of the ambiguous
        // form in both the parser and the constructor.
        let msg = match parse("concat:////a|b") {
            Err(e) => e,
            Ok(v) => panic!("4-slash first segment must be rejected, got {v:?}"),
        };
        assert!(
            msg.to_string().contains("round-trip"),
            "expected round-trip rationale, got {msg}"
        );
        assert!(ConcatUri::new(["//a", "b"]).is_err());
        // One or two leading slashes stay fine: `concat:///a|b` is the
        // authority-style spelling of first segment `/a`.
        let c = parse("concat:///a|b").unwrap();
        assert_eq!(c.segments, ["/a", "b"]);
        assert_eq!(c.format(), "concat:/a|b");
        let again = parse(&c.format()).unwrap();
        assert_eq!(again, c);
        // Non-first segments may carry leading slashes freely.
        let c = parse("concat:a|//b").unwrap();
        assert_eq!(c.segments, ["a", "//b"]);
        assert_eq!(parse(&c.format()).unwrap(), c);
    }

    #[test]
    fn typed_open_matches_open_concat_bytes() {
        mem::put("concat-typed-a", b"left-".to_vec());
        mem::put("concat-typed-b", b"right".to_vec());
        let c = ConcatUri::new(["mem://concat-typed-a", "mem://concat-typed-b"]).unwrap();

        let mut via_typed = c.open().unwrap();
        let mut a = Vec::new();
        via_typed.read_to_end(&mut a).unwrap();

        let mut via_string = open_concat(&c.format()).unwrap();
        let mut b = Vec::new();
        via_string.read_to_end(&mut b).unwrap();

        assert_eq!(a, b);
        assert_eq!(a, b"left-right");
        assert_eq!(
            c.to_string(),
            "concat:mem://concat-typed-a|mem://concat-typed-b"
        );
        mem::remove("concat-typed-a");
        mem::remove("concat-typed-b");
    }

    #[test]
    fn typed_open_rejects_nested_concat_segment() {
        // Constructor admits it (grammar-safe: no '|'), open rejects it
        // with the bespoke nesting message — same split of concerns as
        // the string path.
        let c = ConcatUri::new(["concat:a"]).unwrap();
        let err = match c.open() {
            Err(e) => e.to_string(),
            Ok(_) => panic!("nested concat segment must be rejected at open"),
        };
        assert!(err.contains("nesting concat"), "got: {err}");
    }
}
