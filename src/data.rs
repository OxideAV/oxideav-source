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

/// Parse a `data:` URI into its [`DataUri`] components.
///
/// Accepts both `data:,hello` and `data:image/png;base64,iVBORw...`
/// shapes. Rejects URIs that lack the mandatory `,` separator.
pub fn parse(uri_str: &str) -> Result<DataUri> {
    let (scheme, rest) = uri::split(uri_str);
    if scheme != "data" {
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

    let data = if base64 {
        decode_base64(payload)?
    } else {
        percent_decode(payload)?
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

/// Decode RFC 3986 percent-encoded bytes. `%HH` → byte 0xHH; other
/// bytes pass through. A `+` is **not** translated to space — that is a
/// `application/x-www-form-urlencoded` convention, not the RFC 2397
/// data-URI rule.
fn percent_decode(s: &str) -> Result<Vec<u8>> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'%' {
            if i + 2 >= bytes.len() {
                return Err(Error::invalid(format!(
                    "data:// percent-encoding truncated at offset {i}"
                )));
            }
            let hi = hex_nibble(bytes[i + 1]).ok_or_else(|| {
                Error::invalid(format!(
                    "data:// percent-encoding: non-hex digit {:?}",
                    bytes[i + 1] as char
                ))
            })?;
            let lo = hex_nibble(bytes[i + 2]).ok_or_else(|| {
                Error::invalid(format!(
                    "data:// percent-encoding: non-hex digit {:?}",
                    bytes[i + 2] as char
                ))
            })?;
            out.push((hi << 4) | lo);
            i += 3;
        } else {
            out.push(b);
            i += 1;
        }
    }
    Ok(out)
}

fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
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
        // Hand-encode with our own encoder to keep this self-contained.
        let encoded = encode_b64(&payload);
        let uri = format!("data:application/octet-stream;base64,{encoded}");
        let parsed = parse(&uri).unwrap();
        assert_eq!(parsed.data, payload);
    }

    /// Test-helper encoder. Not exposed publicly — the driver decodes
    /// only. Kept inside `tests` so production code does not grow an
    /// encoder it does not need.
    fn encode_b64(input: &[u8]) -> String {
        const ALPHA: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
        let mut i = 0;
        while i + 3 <= input.len() {
            let b0 = input[i];
            let b1 = input[i + 1];
            let b2 = input[i + 2];
            out.push(ALPHA[(b0 >> 2) as usize] as char);
            out.push(ALPHA[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
            out.push(ALPHA[(((b1 & 0x0f) << 2) | (b2 >> 6)) as usize] as char);
            out.push(ALPHA[(b2 & 0x3f) as usize] as char);
            i += 3;
        }
        match input.len() - i {
            0 => {}
            1 => {
                let b0 = input[i];
                out.push(ALPHA[(b0 >> 2) as usize] as char);
                out.push(ALPHA[((b0 & 0x03) << 4) as usize] as char);
                out.push('=');
                out.push('=');
            }
            2 => {
                let b0 = input[i];
                let b1 = input[i + 1];
                out.push(ALPHA[(b0 >> 2) as usize] as char);
                out.push(ALPHA[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
                out.push(ALPHA[((b1 & 0x0f) << 2) as usize] as char);
                out.push('=');
            }
            _ => unreachable!(),
        }
        out
    }
}
