//! Randomised model-based differential tests.
//!
//! Every bytes-shaped source documents `std::io::Cursor` semantics
//! (seek-past-end tolerated, read 0 at EOF, underflow errors). So a
//! `Cursor<Vec<u8>>` over the same payload is a perfect executable
//! model: drive both with an identical pseudo-random operation sequence
//! and require identical observable behaviour at every step — seek
//! success/failure, reported positions, and every byte read.
//!
//! The PRNG is a fixed-seed xorshift so failures reproduce exactly;
//! iteration counts shrink under Miri to keep the interpreted run sane.

use std::io::{Cursor, Read, Seek, SeekFrom};

use oxideav_source::{mem, open_bytes, BufferedSource, BytesSource, SubSource};

// ───────────────────────── deterministic PRNG ─────────────────────────

struct XorShift(u64);

impl XorShift {
    fn new(seed: u64) -> Self {
        // Avoid the all-zero fixed point.
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

// ───────────────────────── harness ─────────────────────────

#[cfg(miri)]
const SEEDS: &[u64] = &[7, 40499];
#[cfg(not(miri))]
const SEEDS: &[u64] = &[7, 40499, 987_654_321, 0xDEAD_BEEF];

#[cfg(miri)]
const OPS: usize = 40;
#[cfg(not(miri))]
const OPS: usize = 400;

/// Read up to `n` bytes, looping over short reads (composites may
/// legitimately return partial data at internal segment boundaries).
/// Returns the bytes actually obtained before EOF.
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

/// Drive `src` and a `Cursor` model over `payload` with one op stream.
fn differential_run(label: &str, payload: &[u8], src: &mut dyn BytesSource, seed: u64) {
    let mut model = Cursor::new(payload.to_vec());
    let mut rng = XorShift::new(seed);
    let len = payload.len() as u64;

    for step in 0..OPS {
        let ctx = || format!("{label}: seed {seed} step {step}");
        match rng.below(8) {
            // Reads are the most common op.
            0..=3 => {
                let n = rng.below(18) as usize;
                let got = read_upto(src, n).unwrap_or_else(|e| panic!("{}: read: {e}", ctx()));
                let want = read_upto(&mut model, n).unwrap();
                assert_eq!(got, want, "{}: read({n}) bytes diverge", ctx());
            }
            4 => {
                // Start: usually near the payload, occasionally extreme.
                let target = if rng.below(10) == 0 {
                    u64::MAX - rng.below(4)
                } else {
                    rng.below(len * 2 + 16)
                };
                let got = src.seek(SeekFrom::Start(target));
                let want = model.seek(SeekFrom::Start(target));
                assert_eq!(
                    got.is_ok(),
                    want.is_ok(),
                    "{}: seek(Start({target})) ok-ness diverges",
                    ctx()
                );
                if let (Ok(a), Ok(b)) = (got, want) {
                    assert_eq!(a, b, "{}: seek(Start({target})) position", ctx());
                }
            }
            5 => {
                let mag = rng.below(len + 24) as i64;
                let delta = if rng.below(2) == 0 { mag } else { -mag };
                let delta = if rng.below(12) == 0 { i64::MIN } else { delta };
                let got = src.seek(SeekFrom::Current(delta));
                let want = model.seek(SeekFrom::Current(delta));
                assert_eq!(
                    got.is_ok(),
                    want.is_ok(),
                    "{}: seek(Current({delta})) ok-ness diverges",
                    ctx()
                );
                match (got, want) {
                    (Ok(a), Ok(b)) => {
                        assert_eq!(a, b, "{}: seek(Current({delta})) position", ctx())
                    }
                    (Err(_), Err(_)) => {
                        // Both refused: both must also have preserved
                        // their position (checked below via
                        // stream_position on every step).
                    }
                    _ => unreachable!(),
                }
            }
            6 => {
                let mag = rng.below(len + 24) as i64;
                let delta = if rng.below(2) == 0 { mag } else { -mag };
                let got = src.seek(SeekFrom::End(delta));
                let want = model.seek(SeekFrom::End(delta));
                assert_eq!(
                    got.is_ok(),
                    want.is_ok(),
                    "{}: seek(End({delta})) ok-ness diverges",
                    ctx()
                );
                if let (Ok(a), Ok(b)) = (got, want) {
                    assert_eq!(a, b, "{}: seek(End({delta})) position", ctx());
                }
            }
            _ => {
                // Zero-sized read buffer: both must return 0 bytes.
                let got = read_upto(src, 0).unwrap();
                assert!(got.is_empty(), "{}: zero-sized read", ctx());
            }
        }
        // Positions must agree after every operation.
        let sp = src
            .stream_position()
            .unwrap_or_else(|e| panic!("{}: stream_position: {e}", ctx()));
        let mp = model.stream_position().unwrap();
        assert_eq!(sp, mp, "{}: positions diverge after op", ctx());
    }
}

fn payload() -> Vec<u8> {
    (0..777).map(|i| (i % 251) as u8).collect()
}

// ───────────────────────── shapes ─────────────────────────

#[test]
fn mem_matches_model() {
    let p = payload();
    mem::put("diff-mem", p.clone());
    for &seed in SEEDS {
        let mut src = open_bytes("mem://diff-mem").unwrap();
        differential_run("mem", &p, &mut *src, seed);
    }
    mem::remove("diff-mem");
}

#[test]
fn data_matches_model() {
    let p = payload();
    let mut uri = String::from("data:,");
    for b in &p {
        uri.push_str(&format!("%{b:02X}"));
    }
    for &seed in SEEDS {
        let mut src = open_bytes(&uri).unwrap();
        differential_run("data", &p, &mut *src, seed);
    }
}

#[test]
fn slice_matches_model() {
    // Window strictly inside a larger buffer.
    let outer: Vec<u8> = (0..1000).map(|i| (i % 249) as u8).collect();
    let p = outer[111..888].to_vec();
    mem::put("diff-slice", outer);
    for &seed in SEEDS {
        let mut src = open_bytes("slice:111+777!mem://diff-slice").unwrap();
        differential_run("slice", &p, &mut *src, seed);
    }
    mem::remove("diff-slice");
}

#[test]
fn concat_matches_model() {
    let p = payload();
    mem::put("diff-cc-a", p[..259].to_vec());
    mem::put("diff-cc-b", p[259..518].to_vec());
    mem::put("diff-cc-c", p[518..].to_vec());
    for &seed in SEEDS {
        let mut src = open_bytes("concat:mem://diff-cc-a|mem://diff-cc-b|mem://diff-cc-c").unwrap();
        differential_run("concat", &p, &mut *src, seed);
    }
    for id in ["diff-cc-a", "diff-cc-b", "diff-cc-c"] {
        mem::remove(id);
    }
}

#[test]
fn slice_over_concat_matches_model() {
    let full = payload();
    let p = full[100..600].to_vec();
    mem::put("diff-soc-a", full[..300].to_vec());
    mem::put("diff-soc-b", full[300..].to_vec());
    for &seed in SEEDS {
        let mut src = open_bytes("slice:100+500!concat:mem://diff-soc-a|mem://diff-soc-b").unwrap();
        differential_run("slice-over-concat", &p, &mut *src, seed);
    }
    mem::remove("diff-soc-a");
    mem::remove("diff-soc-b");
}

#[test]
fn sub_source_matches_model() {
    let outer: Vec<u8> = (0..900).map(|i| (i % 247) as u8).collect();
    let p = outer[50..850].to_vec();
    for &seed in SEEDS {
        let inner: Box<dyn BytesSource> = Box::new(Cursor::new(outer.clone()));
        let mut src = SubSource::new(inner, 50, 800).unwrap();
        differential_run("sub-source", &p, &mut src, seed);
    }
}

#[test]
fn buffered_matches_model() {
    let p = payload();
    mem::put("diff-buf", p.clone());
    for &seed in SEEDS {
        let inner = open_bytes("mem://diff-buf").unwrap();
        let mut src = BufferedSource::new(Box::new(inner), 64 * 1024).unwrap();
        differential_run("buffered", &p, &mut src, seed);
    }
    mem::remove("diff-buf");
}

#[cfg(not(miri))] // threaded + 512 KiB payload: too slow interpreted
#[test]
fn buffered_streaming_matches_model() {
    // Payload much larger than the ring: seeks constantly restart the
    // prefetch worker and reads race its refills — the differential
    // stream must still be byte-exact.
    let p: Vec<u8> = (0..512 * 1024).map(|i| (i % 251) as u8).collect();
    mem::put("diff-buf-big", p.clone());
    for &seed in SEEDS {
        let inner = open_bytes("mem://diff-buf-big").unwrap();
        let mut src = BufferedSource::builder()
            .capacity(1) // clamped to 4 × 4 KiB
            .block_size(1) // clamped to 4 KiB
            .build(Box::new(inner))
            .unwrap();
        differential_run("buffered-streaming", &p, &mut src, seed);
    }
    mem::remove("diff-buf-big");
}
