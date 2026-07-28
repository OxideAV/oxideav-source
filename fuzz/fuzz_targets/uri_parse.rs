#![no_main]

//! Grammar-layer fuzz target: the URI parsers on raw attacker bytes.
//!
//! Feeds arbitrary bytes (as UTF-8, lossily converted when invalid) to
//! `parse_slice_uri`, `parse_concat_uri`, and `parse_data_uri`, and
//! checks the documented contracts on every accepted input:
//!
//! * **No panic** on any input — the parsers return `Result`.
//! * **Fixpoint**: `parse(format(x)) == x` for every accepted input.
//! * **Canonical round-trip**: when the input is already canonical
//!   (lowercase scheme, no `//` after the colon), `format` reproduces
//!   it byte-identically.
//! * **Constructor agreement**: rebuilding the typed value through
//!   `SliceUri::new` / `ConcatUri::new` from the parsed components
//!   succeeds and yields an equal value (whenever the components meet
//!   the constructors' stricter round-trip preconditions, which the
//!   parsers guarantee for `concat:` and guarantee for `slice:` unless
//!   the inner carries a nested `!`).
//! * **Open agreement** (`data:` only — purely in-memory): the bytes
//!   served by `open_data` equal the `data` field returned by `parse`.
//!
//! No filesystem access: `file://` / bare-path URIs are never opened
//! here (see `compose_open` for the in-memory composition surface).

use std::io::Read;

use libfuzzer_sys::fuzz_target;
use oxideav_source::{
    open_data, parse_concat_uri, parse_data_uri, parse_slice_uri, ConcatUri, DataUri, SliceUri,
};

fn check_slice(s: &str) {
    let Ok(parsed) = parse_slice_uri(s) else {
        return;
    };
    // Fixpoint for every accepted input.
    let formatted = parsed.format();
    let again = parse_slice_uri(&formatted).expect("slice: format must re-parse");
    assert_eq!(again, parsed, "slice: parse-format-parse fixpoint");
    // Canonical inputs round-trip byte-identically.
    if let Some(rest) = s.strip_prefix("slice:") {
        if !rest.starts_with("//") {
            assert_eq!(formatted, s, "slice: canonical round-trip");
        }
    }
    // Constructor agreement (the parser can hand back an inner with a
    // nested '!', which the strict constructor rejects by design).
    if !parsed.inner.contains('!') {
        let rebuilt = SliceUri::new(parsed.offset, parsed.length, parsed.inner.clone())
            .expect("parser-accepted '!'-free components must satisfy SliceUri::new");
        assert_eq!(rebuilt, parsed, "slice: constructor agreement");
    }
}

fn check_concat(s: &str) {
    let Ok(parsed) = parse_concat_uri(s) else {
        return;
    };
    let formatted = parsed.format();
    let again = parse_concat_uri(&formatted).expect("concat: format must re-parse");
    assert_eq!(again, parsed, "concat: parse-format-parse fixpoint");
    if let Some(rest) = s.strip_prefix("concat:") {
        if !rest.starts_with("//") {
            assert_eq!(formatted, s, "concat: canonical round-trip");
        }
    }
    // The parser guarantees non-empty, '|'-free segments — exactly the
    // constructor's preconditions.
    let rebuilt = ConcatUri::new(parsed.segments.clone())
        .expect("parser-accepted segments must satisfy ConcatUri::new");
    assert_eq!(rebuilt, parsed, "concat: constructor agreement");
}

fn check_data(s: &str) {
    let Ok(parsed) = parse_data_uri(s) else {
        return;
    };
    // The mediatype echoes the pre-comma header; it must never swallow
    // the separator.
    assert!(
        !parsed.mediatype.contains(','),
        "data: mediatype must stop at the comma"
    );
    // Value fixpoint: the payload is stored decoded, so byte-identity
    // with the input is out of scope, but re-parsing the canonical
    // formatted form must reproduce the same typed value.
    let formatted = parsed.format();
    let again = parse_data_uri(&formatted).expect("data: format must re-parse");
    assert_eq!(again, parsed, "data: parse-format-parse fixpoint");
    // Constructor agreement: the parser only emits component sets the
    // constructor's round-trip preconditions admit.
    let rebuilt = DataUri::new(parsed.mediatype.clone(), parsed.base64, parsed.data.clone())
        .expect("parser-accepted components must satisfy DataUri::new");
    assert_eq!(rebuilt, parsed, "data: constructor agreement");
    // open_data must serve exactly the bytes parse decoded.
    let mut r = open_data(s).expect("data: parse ok implies open ok");
    let mut served = Vec::new();
    r.read_to_end(&mut served).expect("cursor read cannot fail");
    assert_eq!(served, parsed.data, "data: open/parse byte agreement");
}

fuzz_target!(|data: &[u8]| {
    let s: std::borrow::Cow<'_, str> = String::from_utf8_lossy(data);
    check_slice(&s);
    check_concat(&s);
    check_data(&s);
});
