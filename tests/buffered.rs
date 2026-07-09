//! BufferedSource correctness — exercise the prefetch ring without
//! depending on an external source.

use std::io::{Cursor, Read, Seek, SeekFrom};

use oxideav_source::BufferedSource;

fn ramp(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i & 0xff) as u8).collect()
}

#[test]
fn sequential_read_matches_inner() {
    let data = ramp(2 * 1024 * 1024);
    let inner = Box::new(Cursor::new(data.clone()));
    let mut buf = BufferedSource::new(inner, 1024 * 1024).unwrap();
    let mut out = vec![0u8; data.len()];
    buf.read_exact(&mut out).unwrap();
    assert_eq!(out, data);
}

#[test]
fn read_at_eof_returns_zero() {
    let data = ramp(4096);
    let inner = Box::new(Cursor::new(data));
    let mut buf = BufferedSource::new(inner, 0).unwrap();
    let mut out = vec![0u8; 4096];
    buf.read_exact(&mut out).unwrap();
    let mut tail = [0u8; 16];
    assert_eq!(buf.read(&mut tail).unwrap(), 0);
}

#[test]
fn seek_within_window_does_not_block_or_lose_bytes() {
    let data = ramp(4 * 1024 * 1024);
    let inner = Box::new(Cursor::new(data.clone()));
    let mut buf = BufferedSource::new(inner, 2 * 1024 * 1024).unwrap();
    // Read 64 KiB, seek back 32 KiB, read again — should match.
    let mut a = vec![0u8; 64 * 1024];
    buf.read_exact(&mut a).unwrap();
    buf.seek(SeekFrom::Current(-32 * 1024)).unwrap();
    let mut b = vec![0u8; 32 * 1024];
    buf.read_exact(&mut b).unwrap();
    assert_eq!(b[..], data[32 * 1024..64 * 1024]);
}

#[test]
fn seek_outside_window_restarts_prefetch() {
    let data = ramp(4 * 1024 * 1024);
    let inner = Box::new(Cursor::new(data.clone()));
    let mut buf = BufferedSource::new(inner, 256 * 1024).unwrap();
    // Read 8 KiB, then jump to 3 MiB and read a chunk.
    let mut a = vec![0u8; 8 * 1024];
    buf.read_exact(&mut a).unwrap();
    let target: u64 = 3 * 1024 * 1024;
    buf.seek(SeekFrom::Start(target)).unwrap();
    let mut b = vec![0u8; 16 * 1024];
    buf.read_exact(&mut b).unwrap();
    assert_eq!(b[..], data[target as usize..(target as usize + 16 * 1024)]);
}

#[test]
fn seek_to_end_then_read_returns_zero() {
    use std::time::{Duration, Instant};
    let data = ramp(64 * 1024);
    let inner = Box::new(Cursor::new(data));
    let mut buf = BufferedSource::new(inner, 0).unwrap();
    let end = buf.seek(SeekFrom::End(0)).unwrap();
    assert_eq!(end, 64 * 1024);
    let mut out = [0u8; 8];
    // Must not block on the worker — at EOF the read should return
    // immediately, not wait the prefetch timeout.
    let t0 = Instant::now();
    assert_eq!(buf.read(&mut out).unwrap(), 0);
    assert!(t0.elapsed() < Duration::from_secs(2));
}

#[test]
fn drop_terminates_worker_promptly() {
    use std::time::{Duration, Instant};
    let data = ramp(8 * 1024 * 1024);
    let inner = Box::new(Cursor::new(data));
    let buf = BufferedSource::new(inner, 4 * 1024 * 1024).unwrap();
    // Drop the buffer; the worker should exit and join() should return
    // well under a second.
    let t0 = Instant::now();
    drop(buf);
    assert!(t0.elapsed() < Duration::from_secs(1));
}

#[test]
fn backward_seek_outside_window_then_read_serves_correct_bytes() {
    // Regression test: BufferedSource must never surface "reader behind
    // ring start" to the caller. Prior bug: Seek set `self.pos = new_pos`
    // while the worker still owned ring_start at the old (larger) offset,
    // so a Read racing the worker saw `self.pos < ring_start` and errored.
    let data = ramp(4 * 1024 * 1024);
    let inner = Box::new(Cursor::new(data.clone()));
    let mut buf = BufferedSource::new(inner, 256 * 1024).unwrap();
    // Read ahead far enough that the ring window moves past the start.
    let mut scratch = vec![0u8; 512 * 1024];
    buf.read_exact(&mut scratch).unwrap();
    // Now seek back to the beginning — well behind the current ring_start.
    buf.seek(SeekFrom::Start(0)).unwrap();
    // Immediately read. Must succeed and return the first bytes of data.
    let mut out = vec![0u8; 4096];
    buf.read_exact(&mut out).unwrap();
    assert_eq!(out, data[..4096]);
}

#[test]
fn len_reports_total() {
    let data = ramp(12345);
    let inner = Box::new(Cursor::new(data));
    let buf = BufferedSource::new(inner, 0).unwrap();
    assert_eq!(buf.len(), Some(12345));
}

#[test]
fn builder_default_matches_new() {
    // BufferedSource::new(inner, cap) is a thin wrapper around the
    // builder; they must produce equivalent readers.
    let data = ramp(64 * 1024);
    let inner_a = Box::new(Cursor::new(data.clone()));
    let inner_b = Box::new(Cursor::new(data.clone()));
    let mut a = BufferedSource::new(inner_a, 1024 * 1024).unwrap();
    let mut b = BufferedSource::builder()
        .capacity(1024 * 1024)
        .build(inner_b)
        .unwrap();
    let mut out_a = vec![0u8; data.len()];
    let mut out_b = vec![0u8; data.len()];
    a.read_exact(&mut out_a).unwrap();
    b.read_exact(&mut out_b).unwrap();
    assert_eq!(out_a, out_b);
    assert_eq!(out_a, data);
}

#[test]
fn builder_custom_capacity_block_size() {
    // Tiny ring with tiny block reads — still streams the file correctly.
    let data = ramp(128 * 1024);
    let inner = Box::new(Cursor::new(data.clone()));
    let mut buf = BufferedSource::builder()
        .capacity(8 * 1024) // will be clamped up to 4 * block
        .block_size(8 * 1024)
        .build(inner)
        .unwrap();
    let mut out = vec![0u8; data.len()];
    buf.read_exact(&mut out).unwrap();
    assert_eq!(out, data);
}

#[test]
fn builder_records_prefetch_timeout() {
    let data = ramp(1024);
    let inner = Box::new(Cursor::new(data));
    let buf = BufferedSource::builder()
        .capacity(0)
        .prefetch_timeout(std::time::Duration::from_secs(5))
        .build(inner)
        .unwrap();
    assert_eq!(buf.prefetch_timeout(), std::time::Duration::from_secs(5));
}

#[test]
fn builder_prefetch_timeout_clamps_zero_to_one_millisecond() {
    let data = ramp(1024);
    let inner = Box::new(Cursor::new(data));
    let buf = BufferedSource::builder()
        .prefetch_timeout(std::time::Duration::ZERO)
        .build(inner)
        .unwrap();
    // Zero would flap reads through TimedOut immediately; the builder
    // clamps up to 1 ms so the timeout still bounds a hung worker but
    // doesn't fire on every read.
    assert!(buf.prefetch_timeout() >= std::time::Duration::from_millis(1));
}

#[test]
fn builder_lookback_zero_means_no_back_seek_cache() {
    // With lookback 0/N, a backward seek outside the worker block is
    // expected to restart prefetch. We don't observe the restart
    // directly, but the back-seek must still produce correct bytes.
    let data = ramp(1024 * 1024);
    let inner = Box::new(Cursor::new(data.clone()));
    let mut buf = BufferedSource::builder()
        .capacity(64 * 1024)
        .block_size(8 * 1024)
        .lookback_fraction(0, 8)
        .build(inner)
        .unwrap();
    let mut a = vec![0u8; 128 * 1024];
    buf.read_exact(&mut a).unwrap();
    buf.seek(SeekFrom::Start(0)).unwrap();
    let mut b = vec![0u8; 4096];
    buf.read_exact(&mut b).unwrap();
    assert_eq!(b, data[..4096]);
}

#[test]
fn builder_lookback_clamps_full_fraction() {
    // num >= den would leave zero forward window. Builder clamps to
    // (den - 1) / den so the ring still serves forward reads.
    let data = ramp(64 * 1024);
    let inner = Box::new(Cursor::new(data.clone()));
    let mut buf = BufferedSource::builder()
        .capacity(16 * 1024)
        .block_size(4 * 1024)
        .lookback_fraction(8, 8) // would be 100% lookback; clamped to 7/8
        .build(inner)
        .unwrap();
    let mut out = vec![0u8; data.len()];
    buf.read_exact(&mut out).unwrap();
    assert_eq!(out, data);
}

#[test]
fn builder_seek_within_window_with_large_lookback() {
    // 7/8 lookback keeps most of the ring as back-cache, so a long
    // backward seek inside the ring window should still hit without
    // restarting prefetch.
    let data = ramp(4 * 1024 * 1024);
    let inner = Box::new(Cursor::new(data.clone()));
    let mut buf = BufferedSource::builder()
        .capacity(1024 * 1024)
        .lookback_fraction(7, 8)
        .build(inner)
        .unwrap();
    let mut a = vec![0u8; 256 * 1024];
    buf.read_exact(&mut a).unwrap();
    buf.seek(SeekFrom::Current(-(192 * 1024_i64))).unwrap();
    let mut b = vec![0u8; 64 * 1024];
    buf.read_exact(&mut b).unwrap();
    assert_eq!(b, data[64 * 1024..128 * 1024]);
}

#[test]
fn prefetch_timeout_surfaces_when_worker_stalls() {
    // A reader whose inner source blocks forever should hit the
    // configured prefetch timeout instead of hanging indefinitely.
    use std::io::Read as _;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    /// A reader whose `read` blocks until a stop flag flips. Used to
    /// drive BufferedSource into its prefetch-timeout path.
    struct Blocking {
        stop: Arc<AtomicBool>,
        len: u64,
    }
    impl std::io::Read for Blocking {
        fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
            while !self.stop.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(5));
            }
            // Inner has been "shut down" — surface EOF so the worker
            // returns cleanly when the test ends.
            Ok(0)
        }
    }
    impl std::io::Seek for Blocking {
        fn seek(&mut self, from: std::io::SeekFrom) -> std::io::Result<u64> {
            match from {
                std::io::SeekFrom::End(0) => Ok(self.len),
                std::io::SeekFrom::Start(n) => Ok(n),
                std::io::SeekFrom::Current(_) | std::io::SeekFrom::End(_) => Ok(0),
            }
        }
    }

    let stop = Arc::new(AtomicBool::new(false));
    let inner = Box::new(Blocking {
        stop: Arc::clone(&stop),
        len: 1024,
    });
    let mut buf = BufferedSource::builder()
        .capacity(0)
        .prefetch_timeout(Duration::from_millis(50))
        .build(inner)
        .unwrap();
    let mut out = [0u8; 16];
    let t0 = Instant::now();
    let res = buf.read(&mut out);
    let elapsed = t0.elapsed();
    // Must surface TimedOut, not hang.
    let err = res.expect_err("blocked inner must surface TimedOut");
    assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
    // Generous upper bound — we just want to be sure the 30 s default
    // wasn't applied. 5 s allows for slow CI.
    assert!(
        elapsed < Duration::from_secs(5),
        "timeout took too long: {elapsed:?}"
    );
    // Release the blocked worker so drop() joins promptly.
    stop.store(true, Ordering::SeqCst);
}

/// Inner source that serves `good` bytes, then fails every subsequent
/// read with the given error kind. Reports a total length larger than
/// the good region so the worker keeps trying (and hits the failure)
/// instead of declaring EOF.
struct FailAfter {
    good: Vec<u8>,
    pos: u64,
    reported_len: u64,
    kind: std::io::ErrorKind,
}

impl Read for FailAfter {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let good_len = self.good.len() as u64;
        if self.pos < good_len {
            let start = self.pos as usize;
            let n = buf.len().min(self.good.len() - start);
            buf[..n].copy_from_slice(&self.good[start..start + n]);
            self.pos += n as u64;
            return Ok(n);
        }
        Err(std::io::Error::new(self.kind, "synthetic inner failure"))
    }
}

impl Seek for FailAfter {
    fn seek(&mut self, from: SeekFrom) -> std::io::Result<u64> {
        let new = match from {
            SeekFrom::Start(n) => n,
            SeekFrom::End(d) => self.reported_len.checked_add_signed(d).ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::InvalidInput, "seek out of range")
            })?,
            SeekFrom::Current(d) => self.pos.checked_add_signed(d).ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::InvalidInput, "seek out of range")
            })?,
        };
        self.pos = new;
        Ok(new)
    }
}

#[test]
fn worker_error_is_sticky_and_immediate() {
    use std::time::{Duration, Instant};

    let good = ramp(8 * 1024);
    let inner = Box::new(FailAfter {
        good: good.clone(),
        pos: 0,
        reported_len: 1024 * 1024,
        kind: std::io::ErrorKind::ConnectionReset,
    });
    let mut buf = BufferedSource::builder()
        .capacity(0)
        .prefetch_timeout(Duration::from_secs(5))
        .build(inner)
        .unwrap();

    // The good region streams fine.
    let mut head = vec![0u8; good.len()];
    buf.read_exact(&mut head).unwrap();
    assert_eq!(head, good);

    // First read past the good region: the worker's failure surfaces
    // with its original kind and message.
    let mut probe = [0u8; 16];
    let e1 = buf.read(&mut probe).expect_err("must surface worker error");
    assert_eq!(e1.kind(), std::io::ErrorKind::ConnectionReset);
    assert!(e1.to_string().contains("synthetic inner failure"));

    // Second read: the error must be STICKY — re-surfaced immediately,
    // not converted into a prefetch TimedOut after the 5 s wait (the
    // worker is dead; no data can ever arrive).
    let t0 = Instant::now();
    let e2 = buf.read(&mut probe).expect_err("error must stay sticky");
    assert_eq!(e2.kind(), std::io::ErrorKind::ConnectionReset);
    assert!(
        t0.elapsed() < Duration::from_secs(2),
        "sticky error must not wait out the prefetch timeout: {:?}",
        t0.elapsed()
    );
}

#[test]
fn seek_after_worker_death_keeps_failing_fast() {
    use std::time::{Duration, Instant};

    let good = ramp(8 * 1024);
    let inner = Box::new(FailAfter {
        good: good.clone(),
        pos: 0,
        reported_len: 1024 * 1024,
        kind: std::io::ErrorKind::BrokenPipe,
    });
    let mut buf = BufferedSource::builder()
        .capacity(0)
        .prefetch_timeout(Duration::from_secs(5))
        .build(inner)
        .unwrap();

    let mut head = vec![0u8; good.len()];
    buf.read_exact(&mut head).unwrap();
    let mut probe = [0u8; 16];
    let _ = buf.read(&mut probe).expect_err("worker failure expected");

    // Prefetched bytes stay valid after the failure: a seek back INTO
    // the ring's lookback window serves them from the ring.
    buf.seek(SeekFrom::Start(0)).unwrap();
    buf.read_exact(&mut probe).unwrap();
    assert_eq!(probe[..], good[..16]);

    // A seek OUTSIDE the ring window normally clears the sticky error
    // so a live worker can refill from the new position. The worker is
    // dead here — the error must survive the seek and the next read
    // must fail fast instead of waiting out the prefetch timeout for a
    // refill that cannot happen.
    buf.seek(SeekFrom::Start(512 * 1024)).unwrap();
    let t0 = Instant::now();
    let e = buf
        .read(&mut probe)
        .expect_err("dead worker cannot serve after out-of-window seek");
    assert_eq!(e.kind(), std::io::ErrorKind::BrokenPipe);
    assert!(
        t0.elapsed() < Duration::from_secs(2),
        "read after seek must fail fast, took {:?}",
        t0.elapsed()
    );
}

#[test]
fn interrupted_reads_are_retried_not_fatal() {
    // std Read convention: ErrorKind::Interrupted means "call read
    // again". The worker must retry instead of killing the stream.
    struct Interrupting {
        data: Vec<u8>,
        pos: u64,
        hiccups: u32,
    }
    impl Read for Interrupting {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.hiccups > 0 && self.pos % 3 == 0 && self.pos < self.data.len() as u64 {
                self.hiccups -= 1;
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Interrupted,
                    "interrupted",
                ));
            }
            let start = self.pos as usize;
            if start >= self.data.len() {
                return Ok(0);
            }
            // Deliberately small reads so the interrupt sites recur.
            let n = buf.len().min(3).min(self.data.len() - start);
            buf[..n].copy_from_slice(&self.data[start..start + n]);
            self.pos += n as u64;
            Ok(n)
        }
    }
    impl Seek for Interrupting {
        fn seek(&mut self, from: SeekFrom) -> std::io::Result<u64> {
            let len = self.data.len() as u64;
            let new = match from {
                SeekFrom::Start(n) => n,
                SeekFrom::End(d) => len.checked_add_signed(d).unwrap(),
                SeekFrom::Current(d) => self.pos.checked_add_signed(d).unwrap(),
            };
            self.pos = new;
            Ok(new)
        }
    }

    let data = ramp(1024);
    let inner = Box::new(Interrupting {
        data: data.clone(),
        pos: 0,
        hiccups: 8,
    });
    let mut buf = BufferedSource::new(inner, 0).unwrap();
    let mut out = vec![0u8; data.len()];
    buf.read_exact(&mut out).unwrap();
    assert_eq!(out, data);
    let mut tail = [0u8; 4];
    assert_eq!(buf.read(&mut tail).unwrap(), 0);
}
