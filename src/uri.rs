//! Minimal URI scheme parsing.
//!
//! We only need to split off the leading `scheme://` (or `scheme:`) so the
//! registry can dispatch. Anything fancier (auth, query, fragment) is the
//! driver's problem.

use oxideav_core::{Error, Result};

/// Split a URI into `(scheme, rest)`. Bare paths (no scheme) report scheme
/// `"file"` and `rest = uri`. Path-like inputs that happen to start with
/// `c:` on Windows are treated as bare paths (the second char is `:`, but
/// no `//` follows and the part before `:` is a single ASCII letter).
pub fn split(uri: &str) -> (&str, &str) {
    if let Some(idx) = uri.find(':') {
        let (scheme, rest) = uri.split_at(idx);
        let rest = &rest[1..]; // skip ':'

        // Reject single-letter scheme that looks like a Windows drive letter.
        if scheme.len() == 1 && scheme.chars().next().unwrap().is_ascii_alphabetic() {
            return ("file", uri);
        }

        // Scheme must be ASCII alphanumeric / `+` / `-` / `.`, starting with a letter.
        let valid = !scheme.is_empty()
            && scheme.chars().next().unwrap().is_ascii_alphabetic()
            && scheme
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));

        if !valid {
            return ("file", uri);
        }

        // Strip leading `//` from rest if present.
        let rest = rest.strip_prefix("//").unwrap_or(rest);
        return (scheme, rest);
    }
    ("file", uri)
}

/// Returns `true` if `uri` starts with an explicit `file:` or `file://`
/// scheme prefix (case-insensitive on the scheme letters). Used to gate
/// percent-decoding to URI-form inputs only — bare paths are passed
/// verbatim so a real file whose name contains `%` is still openable.
pub fn has_file_scheme(uri: &str) -> bool {
    let bytes = uri.as_bytes();
    if bytes.len() < 5 {
        return false;
    }
    bytes[..4].eq_ignore_ascii_case(b"file") && bytes[4] == b':'
}

/// Percent-decode a URI path component per RFC 3986 §2.1: every `%HH`
/// triplet (two hexadecimal digits, either case) is replaced by the byte
/// `0xHH`; all other bytes pass through unchanged. Produces a `String`
/// because file paths on every supported host are byte strings whose
/// caller treats them as paths — invalid UTF-8 in the decoded output
/// surfaces here as an error (callers that need raw bytes can drop down
/// to [`percent_decode_bytes`]).
///
/// A `+` is **not** translated to space — that is a
/// `application/x-www-form-urlencoded` convention, not the RFC 3986
/// general-URI rule.
pub fn percent_decode_path(s: &str) -> Result<String> {
    let bytes = percent_decode_bytes(s)?;
    String::from_utf8(bytes).map_err(|e| {
        Error::invalid(format!(
            "percent-decoded path is not valid UTF-8: {}",
            e.utf8_error()
        ))
    })
}

/// Lower-level percent-decoder returning raw bytes. Same grammar as
/// [`percent_decode_path`] but without the trailing UTF-8 validation
/// step. Currently used internally; exposed for future drivers that
/// need to handle paths that may not be UTF-8.
pub fn percent_decode_bytes(s: &str) -> Result<Vec<u8>> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'%' {
            if i + 2 >= bytes.len() {
                return Err(Error::invalid(format!(
                    "percent-encoding truncated at offset {i}"
                )));
            }
            let hi = hex_nibble(bytes[i + 1]).ok_or_else(|| {
                Error::invalid(format!(
                    "percent-encoding: non-hex digit {:?}",
                    bytes[i + 1] as char
                ))
            })?;
            let lo = hex_nibble(bytes[i + 2]).ok_or_else(|| {
                Error::invalid(format!(
                    "percent-encoding: non-hex digit {:?}",
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_path() {
        assert_eq!(split("/tmp/x.mp4"), ("file", "/tmp/x.mp4"));
        assert_eq!(split("relative/x.mp4"), ("file", "relative/x.mp4"));
        assert_eq!(split("Cargo.toml"), ("file", "Cargo.toml"));
    }

    #[test]
    fn file_scheme() {
        assert_eq!(split("file:///tmp/x.mp4"), ("file", "/tmp/x.mp4"));
        assert_eq!(split("file:relative"), ("file", "relative"));
    }

    #[test]
    fn http_scheme() {
        assert_eq!(
            split("https://example.com/a.mp4"),
            ("https", "example.com/a.mp4")
        );
        assert_eq!(
            split("http://example.com:8080/a"),
            ("http", "example.com:8080/a")
        );
    }

    #[test]
    fn windows_drive_letter_is_bare_path() {
        assert_eq!(
            split("C:\\Users\\file.mp4"),
            ("file", "C:\\Users\\file.mp4")
        );
    }

    #[test]
    fn has_file_scheme_basic() {
        assert!(has_file_scheme("file:///tmp/x"));
        assert!(has_file_scheme("file:rel"));
        // Case-insensitive on the scheme letters per RFC 3986 §3.1.
        assert!(has_file_scheme("FILE:///tmp/x"));
        assert!(has_file_scheme("File:///tmp/x"));
        // Bare path: no scheme prefix.
        assert!(!has_file_scheme("/tmp/x"));
        assert!(!has_file_scheme("Cargo.toml"));
        // Different scheme.
        assert!(!has_file_scheme("mem://x"));
        assert!(!has_file_scheme("data:,abc"));
        // Too short to start with `file:`.
        assert!(!has_file_scheme(""));
        assert!(!has_file_scheme("file"));
    }

    #[test]
    fn percent_decode_no_escapes_passthrough() {
        assert_eq!(percent_decode_path("/tmp/x.mp4").unwrap(), "/tmp/x.mp4");
        assert_eq!(percent_decode_path("").unwrap(), "");
    }

    #[test]
    fn percent_decode_space() {
        // RFC 3986 §2.1 — `%20` is the canonical encoding of a space.
        assert_eq!(
            percent_decode_path("/tmp/foo%20bar.txt").unwrap(),
            "/tmp/foo bar.txt"
        );
    }

    #[test]
    fn percent_decode_mixed_case_hex() {
        // The two hex digits may be in either case per RFC 3986 §2.1.
        // `%2f` and `%2F` both decode to `/`.
        assert_eq!(percent_decode_path("%2f%2F").unwrap(), "//");
        // `%41` (uppercase hex) and `%61` (lowercase hex) round-trip to
        // ASCII `A` and `a`.
        assert_eq!(percent_decode_path("%41%61").unwrap(), "Aa");
        // Lowercase `%6a` should equal uppercase `%6A` (both → `j`).
        assert_eq!(
            percent_decode_path("%6a").unwrap(),
            percent_decode_path("%6A").unwrap()
        );
    }

    #[test]
    fn percent_decode_utf8_multibyte() {
        // A UTF-8 multibyte sequence — "Привет" (Russian "hi") encodes as
        // %D0%9F%D1%80%D0%B8%D0%B2%D0%B5%D1%82.
        assert_eq!(
            percent_decode_path("%D0%9F%D1%80%D0%B8%D0%B2%D0%B5%D1%82").unwrap(),
            "Привет"
        );
    }

    #[test]
    fn percent_decode_plus_is_not_space() {
        // `+` is the urlencoded-form convention, not the URI rule.
        assert_eq!(percent_decode_path("/tmp/a+b.txt").unwrap(), "/tmp/a+b.txt");
    }

    #[test]
    fn percent_decode_truncated_rejected() {
        assert!(percent_decode_path("/tmp/%F").is_err());
        assert!(percent_decode_path("%").is_err());
    }

    #[test]
    fn percent_decode_bad_hex_rejected() {
        assert!(percent_decode_path("%ZZ").is_err());
        assert!(percent_decode_path("%2G").is_err());
    }

    #[test]
    fn percent_decode_invalid_utf8_rejected() {
        // 0xFF is not valid UTF-8.
        assert!(percent_decode_path("%FF").is_err());
    }

    #[test]
    fn percent_decode_bytes_accepts_invalid_utf8() {
        // The byte-level decoder doesn't validate UTF-8.
        let v = percent_decode_bytes("%FF%00%7E").unwrap();
        assert_eq!(v, [0xff, 0x00, 0x7e]);
    }
}
