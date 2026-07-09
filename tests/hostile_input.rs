//! Hostile-input hardening sweep for the URI parsers.
//!
//! Deterministic pseudo-random fuzzing of `parse_slice_uri`,
//! `parse_data_uri`, and the `open_bytes` dispatcher with byte soup
//! biased toward the grammars' own separators (`!`, `+`, `|`, `,`,
//! `;`, `%`, `:`, digits) plus multi-byte UTF-8 — the classic byte-
//! index-slicing panic bait. Three properties:
//!
//! 1. **No panic**, ever, on any input (the parsers return `Result`).
//! 2. **Round-trip**: every canonical `slice:` URI the parser accepts
//!    formats back byte-identically; the equivalent `SLICE:` /
//!    `slice://` spellings normalise to the canonical form.
//! 3. **Parse-format-parse fixpoint**: re-parsing the formatted form
//!    yields the same typed value, for every accepted input.
//!
//! Fixed xorshift seeds keep failures reproducible; iteration counts
//! shrink under Miri.

use oxideav_source::{open_bytes, parse_data_uri, parse_slice_uri};

struct XorShift(u64);

impl XorShift {
    fn new(seed: u64) -> Self {
        Self(seed | 1)
    }
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
}

/// Characters biased toward grammar separators and edge bytes, plus
/// multi-byte UTF-8 to catch any byte-index slicing at a non-char
/// boundary.
const SOUP: &[char] = &[
    '0', '1', '9', '+', '!', '|', ',', ';', '%', ':', '/', '.', '-', '=', 'a', 'Z', ' ', '\t',
    '\u{0}', 'é', '中', '🎬', '\u{202e}', // RTL override
];

const PREFIXES: &[&str] = &[
    "slice:",
    "SLICE:",
    "data:",
    "concat:",
    "mem://",
    "file://",
    "file:",
    "",
    "slice:+",
    "slice:0",
    "slice:0+0!",
    "data:;base64,",
    "data:,",
    "concat:|",
];

fn random_uri(rng: &mut XorShift) -> String {
    let mut s = String::from(PREFIXES[rng.below(PREFIXES.len() as u64) as usize]);
    let n = rng.below(24);
    for _ in 0..n {
        s.push(SOUP[rng.below(SOUP.len() as u64) as usize]);
    }
    s
}

#[cfg(miri)]
const ITERS: usize = 300;
#[cfg(not(miri))]
const ITERS: usize = 30_000;

#[test]
fn slice_parser_never_panics_and_round_trips() {
    let mut rng = XorShift::new(0x5eed_511c);
    let mut accepted = 0u32;
    for _ in 0..ITERS {
        let uri = random_uri(&mut rng);
        if let Ok(parsed) = parse_slice_uri(&uri) {
            accepted += 1;
            // Documented invariant: canonical inputs (lowercase scheme,
            // no `//` after the colon) round-trip byte-identically;
            // equivalent spellings (SLICE:, slice://) normalise.
            let is_canonical = uri
                .strip_prefix("slice:")
                .is_some_and(|r| !r.starts_with("//"));
            if is_canonical {
                assert_eq!(
                    parsed.format(),
                    uri,
                    "slice round-trip must be byte-identical for canonical input"
                );
            }
            // Re-parsing the formatted form is a fixpoint for EVERY
            // accepted input.
            let again = parse_slice_uri(&parsed.format()).expect("format must re-parse");
            assert_eq!(again, parsed, "parse-format-parse must be a fixpoint");
        }
    }
    // The generator includes "slice:0+0!" prefixed soup, so some inputs
    // must actually exercise the accept path — otherwise the round-trip
    // assertions above are vacuous.
    assert!(accepted > 0, "sweep never hit the accept path");
}

#[test]
fn data_parser_never_panics() {
    let mut rng = XorShift::new(0xda7a_da7a);
    let mut accepted = 0u32;
    for _ in 0..ITERS {
        let uri = random_uri(&mut rng);
        if let Ok(parsed) = parse_data_uri(&uri) {
            accepted += 1;
            // The mediatype echoes the header text before the comma;
            // it must never contain the payload separator itself.
            assert!(
                !parsed.mediatype.contains(','),
                "mediatype must stop at the comma: {uri:?}"
            );
        }
    }
    assert!(accepted > 0, "sweep never hit the accept path");
}

#[test]
fn open_bytes_never_panics() {
    // Reduced iteration count: the file fallback path may touch the
    // filesystem (a failing File::open of byte soup), which is slower
    // than pure parsing and pointless to hammer.
    let mut rng = XorShift::new(0x000b_17e5);
    for _ in 0..ITERS / 10 {
        let uri = random_uri(&mut rng);
        let _ = open_bytes(&uri); // any Result is fine; panics are not
    }
}

#[test]
fn deeply_nested_slice_uri_is_safe() {
    // Recursion depth: open_slice recurses per nesting level. A parse
    // is flat (single split), but open walks the whole chain — make
    // sure a hostile depth neither panics nor overflows the stack for
    // a reasonable bound, and correctly errors (inner mem id absent).
    let mut uri = String::from("mem://hostile-absent");
    for _ in 0..500 {
        uri = format!("slice:0+0!{uri}");
    }
    let r = open_bytes(&uri);
    assert!(r.is_err(), "absent inner id must error, not panic");
}

#[test]
fn pathological_percent_soup_is_safe() {
    // Every alignment of truncated/invalid escapes near the end.
    for tail in ["%", "%2", "%2G", "%%%", "%FF%", "%f", "%0"] {
        let _ = parse_data_uri(&format!("data:,abc{tail}"));
        let _ = open_bytes(&format!("file:///tmp/x{tail}"));
    }
}
