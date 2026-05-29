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
//! Each segment may be one of the same scheme set the [`slice:`](crate::open_slice)
//! driver accepts as its inner URI:
//!
//! - `file://` and bare paths (delegates to [`open_file`]).
//! - `mem://<id>` (delegates to [`open_mem`]).
//! - `data:[<mediatype>][;base64],<bytes>` (delegates to [`open_data`]).
//! - `slice:<offset>+<length>!<inner-uri>` (delegates to [`open_slice`]).
//! - `concat:` itself is **not** allowed as a segment — a nested
//!   `concat:` would have to embed unescaped `|` separators, which the
//!   outer split would shred. Use a single flattened list.
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

use crate::data::open_data;
use crate::file::open_file;
use crate::mem::open_mem;
use crate::slice::open_slice;
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
    match seg_scheme {
        "file" => open_file(seg),
        "mem" => open_mem(seg),
        "data" => open_data(seg),
        "slice" => open_slice(seg),
        "concat" => Err(Error::invalid(format!(
            "concat: segment {seg:?} is itself a concat: URI; nesting concat is not supported \
             because the outer '|' split would shred the inner segment list"
        ))),
        other => Err(Error::invalid(format!(
            "concat: segment {seg:?} uses unsupported scheme {other:?}; \
             only file/mem/data/slice are accepted"
        ))),
    }
}

/// Open a `concat:<a>|<b>|…` URI as a single [`BytesSource`] that reads
/// the segments back-to-back. Each segment may be a bare path, a
/// `file://` URL, a `mem://<id>` reference, a `data:` literal, or a
/// `slice:` URI.
pub fn open_concat(uri_str: &str) -> Result<Box<dyn BytesSource>> {
    let (scheme, rest) = uri::split(uri_str);
    if scheme != "concat" {
        return Err(Error::invalid(format!(
            "concat driver invoked on non-concat URI: {uri_str}"
        )));
    }
    if rest.is_empty() {
        return Err(Error::invalid("concat: URI requires at least one segment"));
    }
    let segs = segments(rest)?;
    let mut parts: Vec<Box<dyn BytesSource>> = Vec::with_capacity(segs.len());
    for seg in segs {
        parts.push(open_segment(seg)?);
    }
    Ok(Box::new(ConcatSource::new(parts)?))
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
}
