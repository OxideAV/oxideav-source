#![no_main]

//! `BufferedSource` fuzz target: fuzzer-chosen prefetch tunables plus a
//! random read/seek op stream, driven differentially against a
//! `Cursor<Vec<u8>>` model.
//!
//! The builder clamps whatever capacity / block-size / lookback values
//! the fuzzer picks, so this doubles as a hardening test of the clamp
//! logic itself: any combination must yield a worker that makes
//! progress and a reader whose observable behaviour (bytes, seek
//! ok-ness, positions) is byte-exact against the model — including
//! seeks that leave the ring window and restart prefetch, back-seeks
//! into the lookback region, reads racing worker refills, EOF, and
//! extreme (`u64::MAX` / `i64::MIN`) seek targets.

use std::io::{Cursor, Read, Seek, SeekFrom};

use libfuzzer_sys::fuzz_target;
use oxideav_source::{BufferedSource, ReadSeek};

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
    fn below(&mut self, n: u64) -> u64 {
        if n <= 1 {
            return 0;
        }
        u64::from(self.u16()) % n
    }
}

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

    // Tunables — deliberately unclamped here; `build` must sanitise.
    let capacity = sc.u16() as usize;
    let block = sc.u16() as usize;
    let lb_num = u32::from(sc.u8());
    let lb_den = u32::from(sc.u8());

    // Payload: a seeded ramp (ring logic is content-agnostic; equality
    // checks only need model == source, so spending fuzz bytes on the
    // content would be waste). The bound deliberately exceeds the
    // largest clampable capacity (u16 max) so payloads bigger than the
    // ring — mid-stream full-ring states — are reachable.
    let n = (sc.u16() as usize) % 80_000;
    let seed = sc.u8();
    let payload: Vec<u8> = (0..n).map(|i| (i as u8).wrapping_add(seed)).collect();

    let inner: Box<dyn ReadSeek> = Box::new(Cursor::new(payload.clone()));
    let mut src = BufferedSource::builder()
        .capacity(capacity)
        .block_size(block)
        .lookback_fraction(lb_num, lb_den)
        .build(inner)
        .expect("in-memory build cannot fail");
    assert_eq!(src.len(), Some(n as u64), "probed total length");

    let mut cur = Cursor::new(payload);
    let len = n as u64;

    while !sc.exhausted() {
        match sc.u8() % 8 {
            0..=3 => {
                let k = (sc.u16() as usize) % 4000;
                let got = read_upto(&mut src, k).expect("buffered read");
                let want = read_upto(&mut cur, k).expect("model read");
                assert_eq!(got, want, "read({k}) bytes diverge");
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
                    x if x % 2 == 0 => mag,
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
                let got = read_upto(&mut src, 0).expect("zero-sized read");
                assert!(got.is_empty(), "zero-sized read must return no bytes");
            }
        }
        let sp = src.stream_position().expect("buffered stream_position");
        let mp = cur.stream_position().expect("model stream_position");
        assert_eq!(sp, mp, "positions diverge after op");
    }
});
