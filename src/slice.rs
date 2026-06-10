//! Built-in `slice:` driver — URI-level windowed view over an inner source.
//!
//! `slice:<offset>+<length>!<inner-uri>` opens `<inner-uri>` with one of
//! the bundled in-process openers and then wraps it in a [`SubSource`]
//! that re-projects `[offset, offset + length)` onto `[0, length)`. The
//! returned reader satisfies `BytesSource`, so a codec hands the slice
//! to a demuxer that thinks it owns a complete file.
//!
//! This is the URI-level analogue of constructing [`SubSource`]
//! programmatically: a pipeline that takes a URI on the command line can
//! address a sub-range without first materialising the inner stream as a
//! file or a `mem://` blob.
//!
//! Grammar (no on-wire spec — internal to OxideAV):
//!
//! ```text
//! sliceurl  = "slice:" offset "+" length "!" inner-uri
//! offset    = 1*DIGIT          ; decimal u64
//! length    = 1*DIGIT          ; decimal u64
//! inner-uri = <a file path or any supported scheme except "slice:" itself
//!              when "!"-encoded inside the slice payload would re-enter>
//! ```
//!
//! The `!` separator was chosen because it is unreserved in RFC 3986
//! sub-delims, never appears in `file://` paths in practice, and is not
//! used by the other bundled schemes — so the split is unambiguous even
//! when the inner URI carries its own `:` and `://`. A literal `!` inside
//! an inner URI is not supported; the first `!` after the `length` token
//! is treated as the separator.
//!
//! Supported inner schemes:
//!
//! - `file://` and bare paths (delegates to [`open_file`]).
//! - `mem://<id>` (delegates to [`open_mem`]).
//! - `data:` (delegates to [`open_data`]).
//! - `slice:` (recursive — composition works as one would expect:
//!   `slice:5+3!slice:10+8!file:///x.bin` first windows the inner file
//!   to bytes `[10, 18)`, then slices that to its bytes `[5, 8)` which
//!   are file bytes `[15, 18)`).
//!
//! The driver does **not** dispatch through a [`SourceRegistry`] — the
//! registry's opener API takes a plain `fn` pointer with no captured
//! context. Drivers that need registry-mediated inner resolution (HTTP,
//! custom schemes) should compose [`SubSource`] programmatically after
//! resolving the inner URI themselves.
//!
//! Clean-room note: no external `slice:`-like URL implementation was
//! consulted. The grammar is a straightforward composition of two
//! decimal integers, a separator, and the existing `data:` / `mem://`
//! / `file://` openers.

use oxideav_core::{BytesSource, Error, Result};

use crate::data::open_data;
use crate::file::open_file;
use crate::mem::open_mem;
use crate::sub::SubSource;
use crate::uri;

/// Parsed components of a `slice:` URI.
///
/// Mirrors [`crate::DataUri`] for the `data:` scheme: a public typed view
/// over the parsed form, so callers that want to inspect a slice URI
/// without immediately opening it (CLI parsers, pipeline tooling,
/// fixture builders) can do so without re-implementing the grammar.
///
/// Round-trip: [`parse`] followed by [`SliceUri::format`] reproduces a
/// byte-identical URI string for every input the parser accepts (the
/// grammar has a single canonical form — no whitespace, no leading
/// zeros are introduced, and the `!` separator is unambiguous because
/// inner URIs containing literal `!` are rejected at construction
/// time).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SliceUri {
    /// Absolute byte offset within the inner source at which the window
    /// starts.
    pub offset: u64,
    /// Window length in bytes. A zero-length window is admitted (it
    /// produces an immediate-EOF reader, matching [`SubSource`]'s
    /// zero-length semantics).
    pub length: u64,
    /// Inner URI string. Any of the schemes [`open_slice`] accepts as
    /// an inner source (`file://` / bare path, `mem://`, `data:`, or a
    /// nested `slice:`); not validated by the parser beyond
    /// non-empty and `!`-free for round-trip safety.
    pub inner: String,
}

impl SliceUri {
    /// Build a `SliceUri` from its three components. Rejects an empty
    /// `inner` (the `!` separator would dangle) and an `inner`
    /// containing a literal `!` (the grammar splits on the first `!`
    /// after the length token, so an embedded `!` would re-enter the
    /// parser at the wrong position and round-trip would silently
    /// produce a different URI).
    pub fn new(offset: u64, length: u64, inner: impl Into<String>) -> Result<Self> {
        let inner: String = inner.into();
        if inner.is_empty() {
            return Err(Error::invalid("slice: URI inner reference cannot be empty"));
        }
        if inner.contains('!') {
            return Err(Error::invalid(format!(
                "slice: URI inner reference {inner:?} contains a '!'; \
                 a literal '!' inside the inner URI cannot round-trip \
                 because the grammar splits on the first '!' after the \
                 length token"
            )));
        }
        Ok(Self {
            offset,
            length,
            inner,
        })
    }

    /// Format this `SliceUri` back into its canonical
    /// `slice:<offset>+<length>!<inner>` string form. The grammar has a
    /// single canonical form so a [`parse`] followed by [`format`]
    /// reproduces a byte-identical URI for every input the parser
    /// accepts.
    pub fn format(&self) -> String {
        format!("slice:{}+{}!{}", self.offset, self.length, self.inner)
    }

    /// Open the window described by this typed value directly, without
    /// round-tripping through the URI string. Resolves `inner` with the
    /// matching bundled opener (`file://` / bare path, `mem://`, `data:`,
    /// or a nested `slice:`) and wraps the result in a [`SubSource`] that
    /// re-projects `[offset, offset + length)` onto `[0, length)`.
    ///
    /// This is the typed analogue of [`open_slice`] and the slice-scheme
    /// parallel to [`crate::DataUri`] → [`crate::open_data`]: a caller
    /// that built a `SliceUri` via [`SliceUri::new`] or inspected one via
    /// [`parse`] can open it straight away instead of calling
    /// [`SliceUri::format`] and re-parsing the string. The resulting
    /// reader is byte-for-byte identical to `open_slice(&self.format())`.
    pub fn open(&self) -> Result<Box<dyn BytesSource>> {
        let inner = open_inner(&self.inner)?;
        let sub = SubSource::new(inner, self.offset, self.length)?;
        Ok(Box::new(sub))
    }
}

impl std::fmt::Display for SliceUri {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "slice:{}+{}!{}", self.offset, self.length, self.inner)
    }
}

/// Parse a `slice:` URI into its [`SliceUri`] components without opening
/// any inner source. Useful for callers that want to inspect or
/// transform the parsed form before deciding whether (or how) to open
/// it — the reverse of [`SliceUri::format`].
///
/// Rejects URIs with a wrong scheme, a missing `!` separator, a missing
/// `+` between offset and length, a non-decimal offset or length, or an
/// empty inner reference.
pub fn parse(uri_str: &str) -> Result<SliceUri> {
    let (scheme, rest) = uri::split(uri_str);
    if scheme != "slice" {
        return Err(Error::invalid(format!(
            "slice driver invoked on non-slice URI: {uri_str}"
        )));
    }
    let h = parse_header(rest)?;
    Ok(SliceUri {
        offset: h.offset,
        length: h.length,
        inner: h.inner.to_string(),
    })
}

/// Parsed `slice:` URI header (internal, borrows from the input).
#[derive(Clone, Debug, PartialEq, Eq)]
struct SliceHeader<'a> {
    offset: u64,
    length: u64,
    inner: &'a str,
}

/// Parse the payload that follows `slice:` (i.e. the value of `uri::split`'s
/// `rest`). Returns the offset, length, and the inner URI string.
fn parse_header(rest: &str) -> Result<SliceHeader<'_>> {
    let bang = rest
        .find('!')
        .ok_or_else(|| Error::invalid("slice: URI missing '!' separator before inner URI"))?;
    let (range, inner_with_bang) = rest.split_at(bang);
    let inner = &inner_with_bang[1..]; // skip '!'

    let plus = range
        .find('+')
        .ok_or_else(|| Error::invalid("slice: URI range missing '+' between offset and length"))?;
    let (off_s, len_with_plus) = range.split_at(plus);
    let len_s = &len_with_plus[1..]; // skip '+'

    let offset: u64 = off_s.parse().map_err(|e| {
        Error::invalid(format!(
            "slice: offset {off_s:?} is not a non-negative decimal u64: {e}"
        ))
    })?;
    let length: u64 = len_s.parse().map_err(|e| {
        Error::invalid(format!(
            "slice: length {len_s:?} is not a non-negative decimal u64: {e}"
        ))
    })?;

    if inner.is_empty() {
        return Err(Error::invalid(
            "slice: URI inner reference is empty after '!'",
        ));
    }

    Ok(SliceHeader {
        offset,
        length,
        inner,
    })
}

/// Resolve an inner URI by dispatching to one of the bundled openers.
/// Limited to schemes whose opener has no captured state: `file://`,
/// bare paths, `mem://`, `data:`, and recursive `slice:`.
fn open_inner(inner: &str) -> Result<Box<dyn BytesSource>> {
    let (scheme, _) = uri::split(inner);
    match scheme {
        "file" => open_file(inner),
        "mem" => open_mem(inner),
        "data" => open_data(inner),
        "slice" => open_slice(inner),
        other => Err(Error::invalid(format!(
            "slice: inner URI uses unsupported scheme {other:?}; \
             only file/mem/data/slice are accepted as inner sources"
        ))),
    }
}

/// Open a `slice:<offset>+<length>!<inner-uri>` URI.
///
/// Equivalent to [`parse`] followed by [`SliceUri::open`]: the string is
/// parsed into a [`SliceUri`] and then opened through the single typed
/// open path, so the URI-string and typed-value entry points cannot
/// drift apart.
pub fn open_slice(uri_str: &str) -> Result<Box<dyn BytesSource>> {
    parse(uri_str)?.open()
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Seek, SeekFrom, Write};

    use crate::mem;

    use super::*;

    fn ramp(n: usize) -> Vec<u8> {
        (0..n).map(|i| (i & 0xff) as u8).collect()
    }

    fn temp_ramp(n: usize) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let mut path = std::env::temp_dir();
        let pid = std::process::id();
        let k = N.fetch_add(1, Ordering::Relaxed);
        path.push(format!("oxideav-slice-test-{pid}-{k}.bin"));
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(&ramp(n)).unwrap();
        f.flush().unwrap();
        path
    }

    #[test]
    fn parse_basic() {
        let h = parse_header("10+20!file:///tmp/x").unwrap();
        assert_eq!(h.offset, 10);
        assert_eq!(h.length, 20);
        assert_eq!(h.inner, "file:///tmp/x");
    }

    #[test]
    fn parse_zero_length_is_ok() {
        // SubSource itself accepts a zero-length window.
        let h = parse_header("0+0!mem://x").unwrap();
        assert_eq!(h.offset, 0);
        assert_eq!(h.length, 0);
    }

    #[test]
    fn parse_missing_bang_rejected() {
        assert!(parse_header("10+20").is_err());
    }

    #[test]
    fn parse_missing_plus_rejected() {
        assert!(parse_header("10!file:///x").is_err());
    }

    #[test]
    fn parse_non_numeric_offset_rejected() {
        assert!(parse_header("abc+20!file:///x").is_err());
    }

    #[test]
    fn parse_non_numeric_length_rejected() {
        assert!(parse_header("10+abc!file:///x").is_err());
    }

    #[test]
    fn parse_empty_inner_rejected() {
        assert!(parse_header("10+20!").is_err());
    }

    #[test]
    fn parse_negative_offset_rejected() {
        // u64 doesn't accept '-'; ensure the error path triggers.
        assert!(parse_header("-1+20!mem://x").is_err());
    }

    #[test]
    fn wrong_scheme_rejected() {
        assert!(open_slice("file:///tmp/x").is_err());
        assert!(open_slice("mem://x").is_err());
    }

    #[test]
    fn slices_a_file() {
        let p = temp_ramp(256);
        let uri = format!("slice:50+40!file://{}", p.display());
        let mut r = open_slice(&uri).unwrap();
        let mut out = vec![0u8; 40];
        r.read_exact(&mut out).unwrap();
        assert_eq!(out, ramp(256)[50..90]);
        std::fs::remove_file(p).ok();
    }

    #[test]
    fn slices_a_mem_buffer() {
        mem::put("slice-r178-mem-a", ramp(128));
        let mut r = open_slice("slice:32+16!mem://slice-r178-mem-a").unwrap();
        let mut out = vec![0u8; 16];
        r.read_exact(&mut out).unwrap();
        assert_eq!(out, ramp(128)[32..48]);
        mem::remove("slice-r178-mem-a");
    }

    #[test]
    fn slices_a_data_uri() {
        // data:,ABCDEFGHIJ → 10 bytes; slice [3, 5) → "DE".
        let mut r = open_slice("slice:3+2!data:,ABCDEFGHIJ").unwrap();
        let mut out = vec![0u8; 2];
        r.read_exact(&mut out).unwrap();
        assert_eq!(&out, b"DE");
    }

    #[test]
    fn slices_a_base64_data_uri() {
        // "Hello" base64 = "SGVsbG8=" → bytes [1, 4) = "ell".
        let mut r = open_slice("slice:1+3!data:;base64,SGVsbG8=").unwrap();
        let mut out = vec![0u8; 3];
        r.read_exact(&mut out).unwrap();
        assert_eq!(&out, b"ell");
    }

    #[test]
    fn slice_seek_within_window() {
        let p = temp_ramp(256);
        let uri = format!("slice:100+50!file://{}", p.display());
        let mut r = open_slice(&uri).unwrap();
        r.seek(SeekFrom::Start(20)).unwrap();
        let mut byte = [0u8; 1];
        r.read_exact(&mut byte).unwrap();
        assert_eq!(byte[0], 120); // ramp[100 + 20]
        let end = r.seek(SeekFrom::End(0)).unwrap();
        assert_eq!(end, 50);
        std::fs::remove_file(p).ok();
    }

    #[test]
    fn window_past_inner_rejected() {
        mem::put("slice-r178-past", ramp(64));
        let r = open_slice("slice:50+50!mem://slice-r178-past");
        assert!(r.is_err());
        mem::remove("slice-r178-past");
    }

    #[test]
    fn nested_slice_recursive() {
        // Outer slice maps mem[10..30] to window[0..20].
        // Inner slice further maps window[5..15] (== mem[15..25]) to [0..10].
        mem::put("slice-r178-nest", ramp(64));
        let uri = "slice:5+10!slice:10+20!mem://slice-r178-nest";
        let mut r = open_slice(uri).unwrap();
        let mut out = vec![0u8; 10];
        r.read_exact(&mut out).unwrap();
        assert_eq!(out, ramp(64)[15..25]);
        mem::remove("slice-r178-nest");
    }

    #[test]
    fn inner_unsupported_scheme_rejected() {
        // "http://" inner is not dispatchable without registry context.
        let r = open_slice("slice:0+10!http://example.com/x");
        assert!(r.is_err());
    }

    #[test]
    fn inner_bare_path_accepted() {
        // `file` is the bare-path fallback in uri::split.
        let p = temp_ramp(32);
        let uri = format!("slice:4+8!{}", p.display());
        let mut r = open_slice(&uri).unwrap();
        let mut out = vec![0u8; 8];
        r.read_exact(&mut out).unwrap();
        assert_eq!(out, ramp(32)[4..12]);
        std::fs::remove_file(p).ok();
    }

    #[test]
    fn zero_length_window_returns_eof_immediately() {
        let p = temp_ramp(16);
        let uri = format!("slice:4+0!file://{}", p.display());
        let mut r = open_slice(&uri).unwrap();
        let mut byte = [0u8; 1];
        assert_eq!(r.read(&mut byte).unwrap(), 0);
        std::fs::remove_file(p).ok();
    }

    #[test]
    fn missing_inner_file_errors() {
        let uri = "slice:0+10!/no/such/path/xyzzy-oxideav-slice-r178";
        assert!(open_slice(uri).is_err());
    }

    // ---- typed `SliceUri` parse / format / round-trip ----

    #[test]
    fn typed_parse_basic() {
        let s = parse("slice:10+20!file:///tmp/x").unwrap();
        assert_eq!(s.offset, 10);
        assert_eq!(s.length, 20);
        assert_eq!(s.inner, "file:///tmp/x");
    }

    #[test]
    fn typed_parse_zero_length_admitted() {
        let s = parse("slice:0+0!mem://x").unwrap();
        assert_eq!(s.offset, 0);
        assert_eq!(s.length, 0);
        assert_eq!(s.inner, "mem://x");
    }

    #[test]
    fn typed_parse_nested_inner_preserved() {
        // Nested slice: as inner — the outer parser does not recurse,
        // so the entire `slice:5+3!mem://x` becomes the parsed inner.
        let s = parse("slice:10+20!slice:5+3!mem://x").unwrap();
        assert_eq!(s.offset, 10);
        assert_eq!(s.length, 20);
        assert_eq!(s.inner, "slice:5+3!mem://x");
    }

    #[test]
    fn typed_parse_data_inner_with_colon_preserved() {
        // The inner is `data:image/gif;base64,SGVsbG8=` — it contains
        // a colon and a `;` but no `!`, so it round-trips unchanged.
        let s = parse("slice:1+3!data:image/gif;base64,SGVsbG8=").unwrap();
        assert_eq!(s.inner, "data:image/gif;base64,SGVsbG8=");
    }

    #[test]
    fn typed_parse_wrong_scheme_rejected() {
        assert!(parse("file:///tmp/x").is_err());
        assert!(parse("mem://x").is_err());
        assert!(parse("data:,x").is_err());
    }

    #[test]
    fn typed_parse_missing_bang_rejected() {
        assert!(parse("slice:10+20").is_err());
    }

    #[test]
    fn typed_parse_missing_plus_rejected() {
        assert!(parse("slice:10!file:///x").is_err());
    }

    #[test]
    fn typed_parse_empty_inner_rejected() {
        assert!(parse("slice:10+20!").is_err());
    }

    #[test]
    fn typed_parse_negative_offset_rejected() {
        // u64 does not accept `-`; bubble the parse error.
        assert!(parse("slice:-1+20!mem://x").is_err());
    }

    #[test]
    fn typed_format_matches_canonical_form() {
        let s = SliceUri::new(10, 20, "file:///tmp/x").unwrap();
        assert_eq!(s.format(), "slice:10+20!file:///tmp/x");
        // Display impl matches `format`.
        assert_eq!(s.to_string(), "slice:10+20!file:///tmp/x");
    }

    #[test]
    fn typed_round_trip_byte_identical() {
        // Every URI the parser accepts round-trips byte-identically
        // through `parse -> format` because the grammar has a single
        // canonical form.
        for uri in [
            "slice:0+0!mem://x",
            "slice:1+1!data:,A",
            "slice:42+128!file:///tmp/foo.bin",
            "slice:18446744073709551615+1!mem://max-offset", // u64::MAX
            "slice:1+18446744073709551615!mem://max-length", // u64::MAX length
            "slice:5+10!data:image/png;base64,iVBORw0KGgo=",
        ] {
            let parsed = parse(uri).expect(uri);
            assert_eq!(parsed.format(), uri, "round-trip mismatch on {uri}");
        }
    }

    #[test]
    fn typed_constructor_rejects_empty_inner() {
        assert!(SliceUri::new(0, 0, "").is_err());
    }

    #[test]
    fn typed_constructor_rejects_bang_in_inner() {
        // A literal `!` in the inner would re-enter the parser at the
        // wrong split, so the constructor rejects it up-front rather
        // than silently producing a non-round-trippable URI.
        let r = SliceUri::new(0, 1, "file:///tmp/a!b");
        assert!(r.is_err(), "inner with embedded '!' must be rejected");
        let msg = r.err().unwrap().to_string();
        assert!(
            msg.contains("'!'") || msg.contains("round-trip"),
            "expected '!'-in-inner rejection message, got {msg}"
        );
    }

    #[test]
    fn typed_constructor_accepts_then_opens() {
        // Build a SliceUri via the typed constructor, format it, hand
        // the string to the existing opener — end-to-end pipeline.
        mem::put("slice-r264-typed-open", ramp(64));
        let uri = SliceUri::new(8, 16, "mem://slice-r264-typed-open")
            .unwrap()
            .format();
        let mut r = open_slice(&uri).unwrap();
        let mut out = vec![0u8; 16];
        r.read_exact(&mut out).unwrap();
        assert_eq!(out, ramp(64)[8..24]);
        mem::remove("slice-r264-typed-open");
    }

    // ---- typed `SliceUri::open` (open straight from the typed value) ----

    #[test]
    fn typed_open_direct_from_constructor() {
        // Build via the constructor and open directly — no format/parse
        // round-trip through the string form.
        mem::put("slice-r271-direct", ramp(64));
        let mut r = SliceUri::new(8, 16, "mem://slice-r271-direct")
            .unwrap()
            .open()
            .unwrap();
        let mut out = vec![0u8; 16];
        r.read_exact(&mut out).unwrap();
        assert_eq!(out, ramp(64)[8..24]);
        mem::remove("slice-r271-direct");
    }

    #[test]
    fn typed_open_matches_open_slice_bytes() {
        // `parsed.open()` must produce a byte-identical reader to
        // `open_slice(&parsed.format())`.
        mem::put("slice-r271-equiv", ramp(200));
        let s = parse("slice:30+40!mem://slice-r271-equiv").unwrap();

        let mut via_typed = s.open().unwrap();
        let mut a = vec![0u8; 40];
        via_typed.read_exact(&mut a).unwrap();

        let mut via_string = open_slice(&s.format()).unwrap();
        let mut b = vec![0u8; 40];
        via_string.read_exact(&mut b).unwrap();

        assert_eq!(a, b);
        assert_eq!(a, ramp(200)[30..70]);
        mem::remove("slice-r271-equiv");
    }

    #[test]
    fn typed_open_data_inner() {
        // data: inner — open straight from the typed value.
        let mut r = parse("slice:3+2!data:,ABCDEFGHIJ").unwrap().open().unwrap();
        let mut out = vec![0u8; 2];
        r.read_exact(&mut out).unwrap();
        assert_eq!(&out, b"DE");
    }

    #[test]
    fn typed_open_nested_slice_inner() {
        // The inner is itself a slice: — `open()` resolves it recursively
        // through `open_inner`, matching `open_slice`'s recursion. A
        // nested inner carries its own `!`, so it cannot come from the
        // strict `SliceUri::new` constructor (which rejects `!` for
        // round-trip safety) — it is produced by `parse`, which splits
        // on the first `!` only and leaves the rest as the inner.
        mem::put("slice-r271-nest", ramp(64));
        let s = parse("slice:5+10!slice:10+20!mem://slice-r271-nest").unwrap();
        assert_eq!(s.inner, "slice:10+20!mem://slice-r271-nest");
        let mut r = s.open().unwrap();
        let mut out = vec![0u8; 10];
        r.read_exact(&mut out).unwrap();
        assert_eq!(out, ramp(64)[15..25]);
        mem::remove("slice-r271-nest");
    }

    #[test]
    fn typed_open_window_past_inner_rejected() {
        // Bounds are validated at open time, same as `open_slice`.
        mem::put("slice-r271-past", ramp(64));
        let s = SliceUri::new(50, 50, "mem://slice-r271-past").unwrap();
        assert!(s.open().is_err());
        mem::remove("slice-r271-past");
    }

    #[test]
    fn typed_open_unsupported_inner_scheme_rejected() {
        // http:// has no captured-state opener, so the typed open path
        // rejects it just like `open_slice`.
        let s = SliceUri::new(0, 10, "http://example.com/x").unwrap();
        assert!(s.open().is_err());
    }
}
