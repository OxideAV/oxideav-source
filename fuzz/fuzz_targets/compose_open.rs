#![no_main]

//! Composition-surface fuzz target: nested `slice:` / `concat:` /
//! `data:` URIs, opened and driven differentially against a
//! `Cursor<Vec<u8>>` model.
//!
//! The fuzzer bytes are interpreted as a little build script:
//!
//! 1. Start from a `data:` leaf (percent- or base64-encoded payload
//!    taken from the input).
//! 2. Apply up to 6 composition steps: wrap in an in-range `slice:`
//!    window (model := window of model), append/prepend another
//!    `data:` leaf via `concat:` (only while the current URI is
//!    `|`-free, matching the grammar's own constraint), or probe an
//!    out-of-range `slice:` wrap, which must fail to open without
//!    disturbing anything.
//! 3. Open the final URI via `open_bytes` — which must succeed — and
//!    drive it with a random read/seek op stream in lockstep with the
//!    model. Every observable must agree: bytes read, seek ok-ness,
//!    and the reported stream position after every op.
//!
//! Everything stays in memory (`data:` leaves only — no filesystem, no
//! `mem://` global registry), so the target is deterministic and
//! thread-free.

use std::io::{Cursor, Read, Seek, SeekFrom};

use libfuzzer_sys::fuzz_target;
use oxideav_source::open_bytes;

struct Script<'a> {
    d: &'a [u8],
    i: usize,
}

impl<'a> Script<'a> {
    fn new(d: &'a [u8]) -> Self {
        Self { d, i: 0 }
    }
    fn u8(&mut self) -> u8 {
        let b = self.d.get(self.i).copied().unwrap_or(0);
        self.i += 1;
        b
    }
    fn u16(&mut self) -> u16 {
        u16::from_le_bytes([self.u8(), self.u8()])
    }
    fn exhausted(&self) -> bool {
        self.i >= self.d.len()
    }
    /// Uniform-ish pick in `0..n` (n > 0).
    fn below(&mut self, n: u64) -> u64 {
        if n <= 1 {
            return 0;
        }
        u64::from(self.u16()) % n
    }
    fn bytes(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| self.u8()).collect()
    }
}

/// Percent-encode every byte (canonical uppercase hex) — never emits
/// `|`, `!`, or any other separator, so the leaf composes freely.
fn data_uri_percent(payload: &[u8]) -> String {
    let mut s = String::with_capacity(6 + payload.len() * 3);
    s.push_str("data:,");
    for b in payload {
        s.push_str(&format!("%{b:02X}"));
    }
    s
}

/// RFC 4648 §4 base64 (standard alphabet, padded) — alphabet contains
/// no grammar separators either.
fn data_uri_base64(payload: &[u8]) -> String {
    const ALPHA: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::from("data:;base64,");
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
    out
}

fn data_leaf(sc: &mut Script<'_>, max_len: usize) -> (String, Vec<u8>) {
    let n = (sc.u16() as usize) % (max_len + 1);
    let payload = sc.bytes(n);
    let uri = if sc.u8() & 1 == 0 {
        data_uri_percent(&payload)
    } else {
        data_uri_base64(&payload)
    };
    (uri, payload)
}

/// Read up to `n` bytes, looping over short reads (composites may
/// return partial data at internal segment boundaries).
fn read_upto(r: &mut dyn Read, n: usize) -> std::io::Result<Vec<u8>> {
    let mut out = vec![0u8; n];
    let mut filled = 0;
    while filled < n {
        let k = r.read(&mut out[filled..])?;
        if k == 0 {
            break;
        }
        filled += k;
    }
    out.truncate(filled);
    Ok(out)
}

fuzz_target!(|data: &[u8]| {
    let mut sc = Script::new(data);

    // 1. Leaf.
    let (mut uri, mut model) = data_leaf(&mut sc, 1024);

    // 2. Composition steps.
    let steps = (sc.u8() % 7) as usize;
    for _ in 0..steps {
        match sc.u8() % 4 {
            // In-range slice window.
            0 | 1 => {
                let len = model.len() as u64;
                let off = sc.below(len + 1);
                let l = sc.below(len - off + 1);
                uri = format!("slice:{off}+{l}!{uri}");
                model = model[off as usize..(off + l) as usize].to_vec();
            }
            // Concat with a fresh data leaf (grammar allows it only
            // while the current URI carries no '|').
            2 => {
                if uri.contains('|') {
                    continue;
                }
                let (leaf, extra) = data_leaf(&mut sc, 512);
                if sc.u8() & 1 == 0 {
                    uri = format!("concat:{uri}|{leaf}");
                    model.extend_from_slice(&extra);
                } else {
                    uri = format!("concat:{leaf}|{uri}");
                    let mut m = extra;
                    m.extend_from_slice(&model);
                    model = m;
                }
            }
            // Out-of-range slice probe: must fail to open, and must not
            // disturb the in-flight composition.
            _ => {
                let off = model.len() as u64 + 1 + u64::from(sc.u8());
                let bad = format!("slice:{off}+1!{uri}");
                assert!(
                    open_bytes(&bad).is_err(),
                    "window past the composite end must be rejected at open"
                );
            }
        }
    }

    // 3. Open and drive differentially.
    let mut src = open_bytes(&uri).expect("in-range composition must open");
    let mut cur = Cursor::new(model.clone());
    let len = model.len() as u64;

    while !sc.exhausted() {
        match sc.u8() % 8 {
            0..=3 => {
                let n = (sc.u16() as usize) % 300;
                let got = read_upto(&mut *src, n).expect("composite read");
                let want = read_upto(&mut cur, n).expect("model read");
                assert_eq!(got, want, "read({n}) bytes diverge on {uri}");
            }
            4 => {
                let target = if sc.u8() % 16 == 0 {
                    u64::MAX - u64::from(sc.u8() % 4)
                } else {
                    sc.below(len * 2 + 16)
                };
                let got = src.seek(SeekFrom::Start(target));
                let want = cur.seek(SeekFrom::Start(target));
                assert_eq!(got.is_ok(), want.is_ok(), "seek Start({target}) ok-ness");
                if let (Ok(a), Ok(b)) = (got, want) {
                    assert_eq!(a, b, "seek Start({target}) position");
                }
            }
            5 => {
                let mag = sc.below(len + 24) as i64;
                let delta = match sc.u8() % 12 {
                    0 => i64::MIN,
                    n if n % 2 == 0 => mag,
                    _ => -mag,
                };
                let got = src.seek(SeekFrom::Current(delta));
                let want = cur.seek(SeekFrom::Current(delta));
                assert_eq!(got.is_ok(), want.is_ok(), "seek Current({delta}) ok-ness");
                if let (Ok(a), Ok(b)) = (got, want) {
                    assert_eq!(a, b, "seek Current({delta}) position");
                }
            }
            6 => {
                let mag = sc.below(len + 24) as i64;
                let delta = if sc.u8() & 1 == 0 { mag } else { -mag };
                let got = src.seek(SeekFrom::End(delta));
                let want = cur.seek(SeekFrom::End(delta));
                assert_eq!(got.is_ok(), want.is_ok(), "seek End({delta}) ok-ness");
                if let (Ok(a), Ok(b)) = (got, want) {
                    assert_eq!(a, b, "seek End({delta}) position");
                }
            }
            _ => {
                let got = read_upto(&mut *src, 0).expect("zero-sized read");
                assert!(got.is_empty(), "zero-sized read must return no bytes");
            }
        }
        let sp = src.stream_position().expect("composite stream_position");
        let mp = cur.stream_position().expect("model stream_position");
        assert_eq!(sp, mp, "positions diverge after op on {uri}");
    }
});
