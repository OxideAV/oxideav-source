//! Built-in `data:` driver — inline byte literals embedded in the URI.
//!
//! Implements RFC 2397 (`dataurl := "data:" [ mediatype ] [ ";base64" ] ","
//! data`). The driver returns a [`BytesSource`] backed by an in-memory
//! `Cursor` — no IO, no allocation past the payload, no external state.
//!
//! Useful for:
//! - Fixture URIs embedded in tests or CLI flags without a temp file.
//! - Single-shot transports where the entire payload fits in the URI
//!   (small icons, calibration tones, RTP-payload trace dumps).
//! - Configuration knobs that take a URI and would otherwise need a
//!   sentinel like `--no-input` to mean "use these bytes literally".
//!
//! Grammar (RFC 2397 §3, abbreviated):
//!
//! ```text
//! dataurl    = "data:" [ mediatype ] [ ";base64" ] "," data
//! mediatype  = [ type "/" subtype ] *( ";" parameter )
//! parameter  = attribute "=" value
//! data       = *urlchar
//! ```
//!
//! When `mediatype` is absent the RFC defaults it to
//! `text/plain;charset=US-ASCII`. The driver does not interpret the
//! media type — it only carries the bytes — but [`parse`] surfaces the
//! parsed string so callers can route based on it.
//!
//! Encodings:
//! - **`;base64`** present: payload is base64-decoded per RFC 4648 §4
//!   ("standard" alphabet with `+` `/`). Whitespace in the payload is
//!   tolerated and skipped. Padding is required to make the input
//!   length a multiple of four.
//! - Otherwise: payload is percent-decoded (`%HH` → byte `0xHH`).
//!   Non-`%` bytes pass through unchanged.
//!
//! Clean-room note: RFC 2397 was read as the only reference. No
//! external `data:` URL implementation was consulted.

use std::io::Cursor;

use oxideav_core::{BytesSource, Error, Result};

use crate::uri;

/// Parsed components of a `data:` URI.
///
/// Completes the typed-URI triad alongside [`crate::SliceUri`] and
/// [`crate::ConcatUri`]: [`parse`] → `DataUri` → [`DataUri::format`] /
/// [`DataUri::open`], with [`DataUri::new`] for building a value from
/// components.
///
/// Round-trip contract: because the payload is stored **decoded**, the
/// byte-identity property of the other two schemes does not apply (the
/// original URI's encoding choices — which bytes were `%HH`-escaped,
/// base64 line breaks — are not retained). The guaranteed invariant is
/// the value fixpoint: `parse(format(x)) == x` for every `DataUri` the
/// parser or constructor produces.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DataUri {
    /// Media type, exactly as written between `data:` and the `,` (less
    /// the trailing `;base64` marker if present). Empty string means
    /// the URI used the RFC default of `text/plain;charset=US-ASCII`;
    /// callers that care can apply that default themselves.
    pub mediatype: String,
    /// True iff the URI included the `;base64` marker.
    pub base64: bool,
    /// Decoded payload bytes.
    pub data: Vec<u8>,
}

impl DataUri {
    /// Build a `DataUri` from its components. Rejects only what breaks
    /// the `parse(format(x)) == x` fixpoint, mirroring
    /// [`crate::SliceUri::new`] / [`crate::ConcatUri::new`]:
    ///
    /// * a `mediatype` containing `,` — the grammar splits the header
    ///   from the payload at the first comma, so the formatted URI
    ///   would re-parse with a truncated mediatype;
    /// * a non-base64 `mediatype` ending in `;base64` (any case) — the
    ///   formatted `data:<mediatype>,…` would re-parse with the marker
    ///   stripped and the `base64` flag flipped.
    ///
    /// The payload is arbitrary bytes; `format` chooses a spelling
    /// that decodes back to it exactly.
    pub fn new(mediatype: impl Into<String>, base64: bool, data: Vec<u8>) -> Result<Self> {
        let mediatype: String = mediatype.into();
        if mediatype.contains(',') {
            return Err(Error::invalid(format!(
                "data: mediatype {mediatype:?} contains a ','; the grammar splits \
                 header from payload at the first comma, so the value cannot round-trip"
            )));
        }
        if !base64 && ends_with_base64_marker(&mediatype) {
            return Err(Error::invalid(format!(
                "data: mediatype {mediatype:?} ends with the \";base64\" marker while \
                 the payload is percent-encoded; the formatted URI would re-parse as \
                 base64 and the value cannot round-trip"
            )));
        }
        if mediatype.starts_with("//") {
            return Err(Error::invalid(format!(
                "data: mediatype {mediatype:?} starts with \"//\"; it cannot \
                 round-trip because the formatted URI's leading '//' reads as an \
                 authority-style spelling and is stripped on re-parse"
            )));
        }
        Ok(Self {
            mediatype,
            base64,
            data,
        })
    }

    /// Format this `DataUri` back into a `data:` URI string:
    /// `data:<mediatype>[;base64],<payload>`.
    ///
    /// The payload spelling is canonical for the stored value: base64
    /// per RFC 4648 §4 (standard alphabet, padded, no line breaks)
    /// when `base64` is set, otherwise percent-encoding with uppercase
    /// hex escaping every byte outside the RFC 3986 unreserved set.
    /// `parse(format(x)) == x` always; byte-identity with whatever URI
    /// the value was originally parsed from is **not** guaranteed (see
    /// the type docs).
    pub fn format(&self) -> String {
        let mut s = String::with_capacity(6 + self.mediatype.len() + self.data.len() * 3);
        s.push_str("data:");
        s.push_str(&self.mediatype);
        if self.base64 {
            s.push_str(";base64,");
            encode_base64_into(&self.data, &mut s);
        } else {
            s.push(',');
            for &b in &self.data {
                // RFC 3986 §2.3 unreserved bytes pass through; everything
                // else is escaped (a conservative superset of what RFC
                // 2397 requires, always safe to emit).
                if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
                    s.push(b as char);
                } else {
                    s.push_str(&format!("%{b:02X}"));
                }
            }
        }
        s
    }

    /// Open the payload described by this typed value directly as a
    /// [`BytesSource`], without round-tripping through the URI string.
    /// The typed analogue of [`open_data`]; the reader serves exactly
    /// [`DataUri::data`].
    pub fn open(&self) -> Result<Box<dyn BytesSource>> {
        Ok(Box::new(Cursor::new(self.data.clone())))
    }
}

impl std::fmt::Display for DataUri {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.format())
    }
}

/// True iff `mediatype` ends with `;base64` compared case-insensitively
/// (the same match [`strip_base64_suffix`] applies during parsing).
fn ends_with_base64_marker(mediatype: &str) -> bool {
    strip_base64_suffix(mediatype).is_some()
}

/// Append RFC 4648 §4 base64 (standard alphabet, padded) to `out`.
fn encode_base64_into(payload: &[u8], out: &mut String) {
    const ALPHA: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut chunks = payload.chunks_exact(3);
    for c in &mut chunks {
        let t = (u32::from(c[0]) << 16) | (u32::from(c[1]) << 8) | u32::from(c[2]);
        for shift in [18u32, 12, 6, 0] {
            out.push(ALPHA[((t >> shift) & 0x3f) as usize] as char);
        }
    }
    match chunks.remainder() {
        [] => {}
        [b0] => {
            let t = u32::from(*b0) << 16;
            out.push(ALPHA[((t >> 18) & 0x3f) as usize] as char);
            out.push(ALPHA[((t >> 12) & 0x3f) as usize] as char);
            out.push_str("==");
        }
        [b0, b1] => {
            let t = (u32::from(*b0) << 16) | (u32::from(*b1) << 8);
            out.push(ALPHA[((t >> 18) & 0x3f) as usize] as char);
            out.push(ALPHA[((t >> 12) & 0x3f) as usize] as char);
            out.push(ALPHA[((t >> 6) & 0x3f) as usize] as char);
            out.push('=');
        }
        _ => unreachable!(),
    }
}

/// Parse a `data:` URI into its [`DataUri`] components.
///
/// Accepts both `data:,hello` and `data:image/png;base64,iVBORw...`
/// shapes. Rejects URIs that lack the mandatory `,` separator.
pub fn parse(uri_str: &str) -> Result<DataUri> {
    let (scheme, rest) = uri::split(uri_str);
    if !uri::scheme_is(scheme, "data") {
        return Err(Error::invalid(format!(
            "data driver invoked on non-data URI: {uri_str}"
        )));
    }
    let comma = rest
        .find(',')
        .ok_or_else(|| Error::invalid("data: URI missing comma separator"))?;
    let (header, payload) = rest.split_at(comma);
    let payload = &payload[1..]; // skip ','

    // Strip a trailing ";base64" marker (case-insensitive, like RFC 2397
    // examples in §4 mix "base64" and "BASE64").
    let (mediatype, base64) = if let Some(stripped) = strip_base64_suffix(header) {
        (stripped, true)
    } else {
        (header, false)
    };

    if mediatype.starts_with("//") {
        // The scheme splitter has already consumed one authority-style
        // `//`; a mediatype STILL starting with `//` (the URI carried
        // four-plus leading slashes) cannot round-trip — `format` would
        // emit `data://…`, whose re-parse strips two more slashes. Same
        // rule as the `concat:` first-segment guard.
        return Err(Error::invalid(format!(
            "data: mediatype {mediatype:?} starts with \"//\" after the scheme split; \
             it is ambiguous with an authority-style spelling and cannot round-trip"
        )));
    }

    let data = if base64 {
        decode_base64(payload)?
    } else {
        // RFC 3986 percent-decoding, shared with the file:// driver's
        // path decoding — `%HH` → byte 0xHH, everything else passes
        // through, `+` is NOT translated to space (that is a
        // `application/x-www-form-urlencoded` convention, not the
        // RFC 2397 data-URI rule).
        uri::percent_decode_bytes(payload)?
    };

    Ok(DataUri {
        mediatype: mediatype.to_string(),
        base64,
        data,
    })
}

/// Open a `data:` URI as a [`BytesSource`]. Equivalent to [`parse`]
/// followed by wrapping the decoded bytes in a `Cursor`.
pub fn open_data(uri_str: &str) -> Result<Box<dyn BytesSource>> {
    let parsed = parse(uri_str)?;
    Ok(Box::new(Cursor::new(parsed.data)))
}

/// If `header` ends with `;base64` (case-insensitive), return the prefix
/// without that marker. Otherwise `None`.
fn strip_base64_suffix(header: &str) -> Option<&str> {
    // RFC 2397 §3 places ";base64" after any other parameters and just
    // before the comma. We match the literal marker so we don't get
    // confused by a parameter named `base64=…`.
    let bytes = header.as_bytes();
    const MARKER: &[u8] = b";base64";
    if bytes.len() < MARKER.len() {
        return None;
    }
    let tail = &bytes[bytes.len() - MARKER.len()..];
    if tail.eq_ignore_ascii_case(MARKER) {
        // SAFETY: we only sliced at an ASCII byte boundary.
        Some(&header[..header.len() - MARKER.len()])
    } else {
        None
    }
}

/// Decode RFC 4648 §4 base64 (standard alphabet, with padding). Skips
/// ASCII whitespace inside the payload so multi-line embedding works.
fn decode_base64(s: &str) -> Result<Vec<u8>> {
    // Strip whitespace into a side buffer first; the input length after
    // stripping must be a multiple of 4 (with `=` padding accounted for).
    let mut clean: Vec<u8> = Vec::with_capacity(s.len());
    for &b in s.as_bytes() {
        if matches!(b, b' ' | b'\t' | b'\n' | b'\r') {
            continue;
        }
        clean.push(b);
    }
    if clean.len() % 4 != 0 {
        return Err(Error::invalid(format!(
            "data:// base64 payload length {} is not a multiple of 4",
            clean.len()
        )));
    }
    let mut out: Vec<u8> = Vec::with_capacity(clean.len() / 4 * 3);
    let mut chunk = [0u8; 4];
    let mut i = 0;
    while i < clean.len() {
        let mut pad = 0;
        for j in 0..4 {
            let b = clean[i + j];
            if b == b'=' {
                pad += 1;
                chunk[j] = 0;
            } else {
                if pad > 0 {
                    return Err(Error::invalid(
                        "data:// base64 padding character before end of payload",
                    ));
                }
                chunk[j] = b64_value(b).ok_or_else(|| {
                    Error::invalid(format!("data:// base64: invalid character {:?}", b as char))
                })?;
            }
        }
        if pad > 2 {
            return Err(Error::invalid(
                "data:// base64: more than two padding characters in a group",
            ));
        }
        // Padding is only legal in the final group.
        if pad > 0 && i + 4 < clean.len() {
            return Err(Error::invalid("data:// base64: padding before final group"));
        }
        let triple = (u32::from(chunk[0]) << 18)
            | (u32::from(chunk[1]) << 12)
            | (u32::from(chunk[2]) << 6)
            | u32::from(chunk[3]);
        out.push(((triple >> 16) & 0xff) as u8);
        if pad < 2 {
            out.push(((triple >> 8) & 0xff) as u8);
        }
        if pad < 1 {
            out.push((triple & 0xff) as u8);
        }
        i += 4;
    }
    Ok(out)
}

fn b64_value(b: u8) -> Option<u8> {
    // RFC 4648 §4 standard alphabet:
    //   A-Z → 0-25, a-z → 26-51, 0-9 → 52-61, '+' → 62, '/' → 63
    match b {
        b'A'..=b'Z' => Some(b - b'A'),
        b'a'..=b'z' => Some(b - b'a' + 26),
        b'0'..=b'9' => Some(b - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::io::Read;

    use super::*;

    #[test]
    fn rfc2397_example_inline_text() {
        // RFC 2397 §4 example: "data:,A%20brief%20note"
        let p = parse("data:,A%20brief%20note").unwrap();
        assert_eq!(p.mediatype, "");
        assert!(!p.base64);
        assert_eq!(p.data, b"A brief note");
    }

    #[test]
    fn rfc2397_example_base64_image_prefix() {
        // RFC 2397 §4 references an image/gif base64. We assert the
        // parser splits header from payload and decodes a known base64
        // prefix correctly. "Hello" → "SGVsbG8=".
        let p = parse("data:image/gif;base64,SGVsbG8=").unwrap();
        assert_eq!(p.mediatype, "image/gif");
        assert!(p.base64);
        assert_eq!(p.data, b"Hello");
    }

    #[test]
    fn empty_mediatype_no_payload() {
        let p = parse("data:,").unwrap();
        assert_eq!(p.mediatype, "");
        assert_eq!(p.data, b"");
    }

    #[test]
    fn mediatype_with_parameter() {
        // RFC 2397 example: data:text/plain;charset=US-ASCII,xyz
        let p = parse("data:text/plain;charset=US-ASCII,abc").unwrap();
        assert_eq!(p.mediatype, "text/plain;charset=US-ASCII");
        assert!(!p.base64);
        assert_eq!(p.data, b"abc");
    }

    #[test]
    fn base64_marker_case_insensitive() {
        let p = parse("data:application/octet-stream;BASE64,SGVsbG8=").unwrap();
        assert!(p.base64);
        assert_eq!(p.data, b"Hello");
    }

    #[test]
    fn base64_with_internal_whitespace() {
        // Multi-line embedding tolerance (data: URIs in source files).
        let p = parse("data:;base64,SG Vs\nbG8=").unwrap();
        assert_eq!(p.data, b"Hello");
    }

    #[test]
    fn percent_decode_high_byte() {
        let p = parse("data:,%FF%00%7E").unwrap();
        assert_eq!(p.data, [0xff, 0x00, 0x7e]);
    }

    #[test]
    fn missing_comma_rejected() {
        let r = parse("data:text/plain;base64");
        assert!(r.is_err());
    }

    #[test]
    fn truncated_percent_rejected() {
        let r = parse("data:,%F");
        assert!(r.is_err());
    }

    #[test]
    fn bad_hex_rejected() {
        let r = parse("data:,%ZZ");
        assert!(r.is_err());
    }

    #[test]
    fn base64_bad_length_rejected() {
        // 3 chars (after whitespace strip) is not a multiple of 4.
        let r = parse("data:;base64,SGV");
        assert!(r.is_err());
    }

    #[test]
    fn base64_padding_in_middle_rejected() {
        // Padding may only occur in the final 4-char group.
        let r = parse("data:;base64,SGVs=GVs");
        assert!(r.is_err());
    }

    #[test]
    fn base64_invalid_char_rejected() {
        let r = parse("data:;base64,SG!s");
        assert!(r.is_err());
    }

    #[test]
    fn wrong_scheme_rejected() {
        let r = parse("file:///tmp/x");
        assert!(r.is_err());
        let r = open_data("mem://x");
        assert!(r.is_err());
    }

    #[test]
    fn open_data_returns_readable_cursor() {
        let mut r = open_data("data:,hello").unwrap();
        let mut buf = Vec::new();
        r.read_to_end(&mut buf).unwrap();
        assert_eq!(buf, b"hello");
    }

    #[test]
    fn base64_full_alphabet_roundtrip() {
        // Encodes 0..255 in 6-bit groups: 256 bytes → 344 base64 chars
        // + 0 padding (256 % 3 == 1 → 2 pad). 256 bytes / 3 = 85 r 1 →
        // 86 groups → 344 chars; one group has two `=`.
        let payload: Vec<u8> = (0u8..=255).collect();
        // Encode with the typed formatter — decode must invert it.
        let uri = DataUri::new("application/octet-stream", true, payload.clone())
            .unwrap()
            .format();
        let parsed = parse(&uri).unwrap();
        assert_eq!(parsed.data, payload);
        assert!(parsed.base64);
        assert_eq!(parsed.mediatype, "application/octet-stream");
    }

    // ---- typed `DataUri` new / format / open (triad completion) ----

    #[test]
    fn typed_format_percent_form() {
        let d = DataUri::new("", false, b"A brief note!".to_vec()).unwrap();
        // Unreserved bytes pass through; space and '!' are escaped with
        // uppercase hex.
        assert_eq!(d.format(), "data:,A%20brief%20note%21");
        assert_eq!(d.to_string(), d.format());
        assert_eq!(parse(&d.format()).unwrap(), d);
    }

    #[test]
    fn typed_format_base64_form() {
        let d = DataUri::new("image/gif", true, b"Hello".to_vec()).unwrap();
        assert_eq!(d.format(), "data:image/gif;base64,SGVsbG8=");
        assert_eq!(parse(&d.format()).unwrap(), d);
    }

    #[test]
    fn typed_open_serves_payload() {
        let d = DataUri::new("", false, vec![0xff, 0x00, 0x7e]).unwrap();
        let mut r = d.open().unwrap();
        let mut got = Vec::new();
        r.read_to_end(&mut got).unwrap();
        assert_eq!(got, d.data);
        // Byte-for-byte what open_data(&format()) serves.
        let mut r2 = open_data(&d.format()).unwrap();
        let mut got2 = Vec::new();
        r2.read_to_end(&mut got2).unwrap();
        assert_eq!(got2, got);
    }

    #[test]
    fn typed_constructor_rejects_comma_in_mediatype() {
        assert!(DataUri::new("text/plain,evil", false, vec![]).is_err());
        assert!(DataUri::new("text/plain,evil", true, vec![]).is_err());
    }

    #[test]
    fn typed_constructor_rejects_marker_suffix_on_percent_form() {
        // `data:<mt>,…` with mt ending ";base64" would re-parse with the
        // marker stripped and the flag flipped — not a fixpoint.
        assert!(DataUri::new("text/plain;base64", false, vec![]).is_err());
        assert!(DataUri::new("text/plain;BASE64", false, vec![]).is_err());
        // With the flag SET the marker lands after the mediatype anyway
        // and the parse strips exactly one — fixpoint holds, admitted.
        let d = DataUri::new("text/plain;base64", true, b"x".to_vec()).unwrap();
        assert_eq!(parse(&d.format()).unwrap(), d);
    }

    #[test]
    fn leading_double_slash_mediatype_rejected() {
        // Same ambiguity class as the concat: first-segment rule: the
        // scheme splitter consumes one authority-style `//`, so a
        // mediatype still starting with `//` (a 4-slash URI) loses two
        // slashes on every parse of its formatted form.
        assert!(parse("data:////x,abc").is_err());
        assert!(DataUri::new("//x", false, vec![]).is_err());
        // One or two leading slashes still normalise / round-trip.
        let d = parse("data://x,abc").unwrap(); // authority spelling of "x"
        assert_eq!(d.mediatype, "x");
        let d = parse("data:///x,abc").unwrap();
        assert_eq!(d.mediatype, "/x");
        assert_eq!(parse(&d.format()).unwrap(), d);
    }

    #[test]
    fn typed_value_fixpoint_across_shapes() {
        for d in [
            DataUri::new("", false, vec![]).unwrap(),
            DataUri::new("", true, vec![]).unwrap(),
            DataUri::new("text/plain;charset=US-ASCII", false, b"abc".to_vec()).unwrap(),
            DataUri::new("application/octet-stream", true, (0u8..=255).collect()).unwrap(),
            DataUri::new("x", false, (0u8..=255).collect()).unwrap(),
        ] {
            assert_eq!(parse(&d.format()).unwrap(), d, "fixpoint on {d}");
        }
    }
}
