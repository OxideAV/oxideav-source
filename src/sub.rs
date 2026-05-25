//! Windowed view over an existing [`BytesSource`].
//!
//! [`SubSource`] re-projects a slice `[base, base + len)` of an inner
//! source onto the virtual address space `[0, len)`. Containers commonly
//! hand a windowed view of the payload to a codec: e.g. an MP4 `mdat`
//! sample at file offset `4_321_000` with length `34_112` should look
//! like an independent `Read + Seek` stream `[0, 34_112)` to the codec
//! that decodes it.
//!
//! This is the seekable analogue of `std::io::Read::take`: `take` only
//! caps forward reads, but a codec that needs to seek backwards within
//! its sample (e.g. to re-read a header after probing it) needs a real
//! windowed seek too. `SubSource` provides both.
//!
//! ## Semantics
//!
//! * `Read`: returns up to `len - pos` bytes per call, then `Ok(0)` (EOF).
//! * `Seek::Start(n)`: maps to inner offset `base + n`. `n > len` is
//!   permitted (mirrors `std::io::Cursor`), but a subsequent `read`
//!   returns 0 until the position is reduced.
//! * `Seek::End(d)`: anchors at `base + len`; `d > 0` permitted, `d`
//!   such that the result is negative errors `InvalidInput`.
//! * `Seek::Current(d)`: relative to the current position; underflow
//!   errors `InvalidInput`.
//!
//! ## Sharing the inner source
//!
//! A `SubSource` takes ownership of the inner reader. To share one
//! underlying file across multiple windows, open the source once per
//! window — that is the cheap path (file descriptors are tiny; the
//! kernel page cache shares the actual bytes between readers). A
//! shared-`Arc`-with-locking design was rejected because it forces
//! every read to serialise on a mutex, which defeats the parallel-read
//! pattern a multi-stream demuxer needs.
//!
//! ## Bound checks
//!
//! [`SubSource::new`] requires `base + len <= inner.stream_len()`. The
//! inner length is captured at construction via `seek(SeekFrom::End(0))`
//! and the source is left positioned at `base`. If the inner source's
//! length changes under the window after construction, reads near the
//! tail may surface short reads from the underlying source like any
//! other reader.

use std::io::{self, Read, Seek, SeekFrom};

use oxideav_core::{BytesSource, Error, Result};

/// Windowed view over an inner [`BytesSource`].
///
/// See the [module docs](self) for full semantics.
pub struct SubSource {
    inner: Box<dyn BytesSource>,
    base: u64,
    len: u64,
    /// Current position in the *window* coordinate space (`0..len`).
    /// May exceed `len` after a seek-past-end (mirrors `Cursor`).
    pos: u64,
}

impl SubSource {
    /// Build a `SubSource` exposing `[base, base + len)` of `inner` as
    /// `[0, len)`.
    ///
    /// Errors when `base + len` overflows or exceeds the inner source's
    /// length, or when the underlying seek to `base` fails. The inner
    /// source is consumed; recover it via [`SubSource::into_inner`].
    pub fn new(mut inner: Box<dyn BytesSource>, base: u64, len: u64) -> Result<Self> {
        let end = base
            .checked_add(len)
            .ok_or_else(|| Error::invalid("SubSource: base + len overflows u64"))?;
        let inner_len = stream_len(&mut inner)
            .map_err(|e| Error::invalid(format!("SubSource: cannot probe inner length: {e}")))?;
        if end > inner_len {
            return Err(Error::invalid(format!(
                "SubSource: window [{base}, {end}) extends past inner length {inner_len}"
            )));
        }
        inner
            .seek(SeekFrom::Start(base))
            .map_err(|e| Error::invalid(format!("SubSource: cannot seek inner to {base}: {e}")))?;
        Ok(Self {
            inner,
            base,
            len,
            pos: 0,
        })
    }

    /// Window length (bytes accessible via this view).
    pub fn len(&self) -> u64 {
        self.len
    }

    /// `true` iff the window is zero-length.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Inner source's offset at which this window starts.
    pub fn base(&self) -> u64 {
        self.base
    }

    /// Consume the window and return the inner source. Its position is
    /// wherever the last `Read`/`Seek` left it; callers that want a
    /// known position should `seek` it themselves.
    pub fn into_inner(self) -> Box<dyn BytesSource> {
        self.inner
    }
}

/// Probe the total length of a seekable source non-destructively:
/// remembers the current position, seeks to `End(0)`, and restores the
/// position before returning. Useful in any code path that wants the
/// inner length once and doesn't care about reading the bytes.
pub fn stream_len(src: &mut dyn BytesSource) -> io::Result<u64> {
    let saved = src.stream_position()?;
    let end = src.seek(SeekFrom::End(0))?;
    // Only seek back if we actually moved; minor optimisation for the
    // case where the caller just opened the source.
    if saved != end {
        src.seek(SeekFrom::Start(saved))?;
    }
    Ok(end)
}

impl Read for SubSource {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() || self.pos >= self.len {
            return Ok(0);
        }
        // Bytes remaining in the window from current position.
        let remaining = (self.len - self.pos) as usize;
        let want = buf.len().min(remaining);

        // Inner-source offset for our current window position. The inner
        // source's position may not match (if the caller mixed reads
        // and seeks on a different window over the same FD — but we
        // own the inner exclusively, so this is just defensive), so we
        // explicitly seek before each read. Two syscalls per read is
        // cheap relative to the actual IO and avoids a fragile "we know
        // where the inner is" invariant.
        let inner_off = self.base + self.pos;
        self.inner.seek(SeekFrom::Start(inner_off))?;
        let n = self.inner.read(&mut buf[..want])?;
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for SubSource {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let new_pos = match from {
            SeekFrom::Start(n) => n,
            SeekFrom::End(d) => add_signed(self.len, d)?,
            SeekFrom::Current(d) => add_signed(self.pos, d)?,
        };
        // Update *window* position; defer the inner-source seek to the
        // next read. A pure seek call with no follow-up read should not
        // pay for an inner syscall.
        self.pos = new_pos;
        Ok(self.pos)
    }
}

fn add_signed(base: u64, delta: i64) -> io::Result<u64> {
    let result = if delta >= 0 {
        base.checked_add(delta as u64)
    } else {
        base.checked_sub(delta.unsigned_abs())
    };
    result.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "SubSource: seek resolves to a negative or overflowing position",
        )
    })
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    fn ramp(n: usize) -> Vec<u8> {
        (0..n).map(|i| (i & 0xff) as u8).collect()
    }

    #[test]
    fn window_reads_the_correct_slice() {
        let data = ramp(256);
        let inner: Box<dyn BytesSource> = Box::new(Cursor::new(data.clone()));
        let mut sub = SubSource::new(inner, 50, 40).unwrap();
        assert_eq!(sub.len(), 40);
        assert_eq!(sub.base(), 50);
        let mut out = vec![0u8; 40];
        sub.read_exact(&mut out).unwrap();
        assert_eq!(out, &data[50..90]);
    }

    #[test]
    fn read_past_window_returns_eof() {
        let data = ramp(128);
        let inner: Box<dyn BytesSource> = Box::new(Cursor::new(data));
        let mut sub = SubSource::new(inner, 10, 20).unwrap();
        let mut out = vec![0u8; 50];
        let n = sub.read(&mut out).unwrap();
        assert_eq!(n, 20); // capped at window length, not buffer length
        let n2 = sub.read(&mut out).unwrap();
        assert_eq!(n2, 0); // window exhausted
    }

    #[test]
    fn seek_within_window_then_read() {
        let data = ramp(256);
        let inner: Box<dyn BytesSource> = Box::new(Cursor::new(data.clone()));
        let mut sub = SubSource::new(inner, 100, 100).unwrap();
        // Seek to window-relative 50 == inner offset 150.
        sub.seek(SeekFrom::Start(50)).unwrap();
        let mut byte = [0u8; 1];
        sub.read_exact(&mut byte).unwrap();
        assert_eq!(byte[0], data[150]);
    }

    #[test]
    fn seek_end_anchors_at_window_end() {
        let data = ramp(64);
        let inner: Box<dyn BytesSource> = Box::new(Cursor::new(data));
        let mut sub = SubSource::new(inner, 8, 16).unwrap();
        let pos = sub.seek(SeekFrom::End(0)).unwrap();
        assert_eq!(pos, 16);
        let mut byte = [0u8; 1];
        assert_eq!(sub.read(&mut byte).unwrap(), 0);
    }

    #[test]
    fn seek_current_relative() {
        let data = ramp(128);
        let inner: Box<dyn BytesSource> = Box::new(Cursor::new(data));
        let mut sub = SubSource::new(inner, 0, 64).unwrap();
        sub.seek(SeekFrom::Start(20)).unwrap();
        let p = sub.seek(SeekFrom::Current(5)).unwrap();
        assert_eq!(p, 25);
        let p = sub.seek(SeekFrom::Current(-10)).unwrap();
        assert_eq!(p, 15);
    }

    #[test]
    fn seek_before_zero_errors() {
        let data = ramp(32);
        let inner: Box<dyn BytesSource> = Box::new(Cursor::new(data));
        let mut sub = SubSource::new(inner, 0, 16).unwrap();
        let r = sub.seek(SeekFrom::Current(-1));
        assert!(r.is_err());
        let r = sub.seek(SeekFrom::End(-100));
        assert!(r.is_err());
    }

    #[test]
    fn seek_past_window_then_read_returns_zero() {
        // Cursor semantics: seeking past the end is OK, but reads return 0.
        let data = ramp(64);
        let inner: Box<dyn BytesSource> = Box::new(Cursor::new(data));
        let mut sub = SubSource::new(inner, 0, 32).unwrap();
        sub.seek(SeekFrom::Start(1000)).unwrap();
        let mut out = [0u8; 8];
        assert_eq!(sub.read(&mut out).unwrap(), 0);
        // Step back inside the window, the bytes should still be there.
        sub.seek(SeekFrom::Start(4)).unwrap();
        let mut byte = [0u8; 1];
        sub.read_exact(&mut byte).unwrap();
        assert_eq!(byte[0], 4);
    }

    #[test]
    fn window_extending_past_inner_rejected() {
        let data = ramp(64);
        let inner: Box<dyn BytesSource> = Box::new(Cursor::new(data));
        let r = SubSource::new(inner, 50, 50); // 50 + 50 = 100 > 64
        assert!(r.is_err());
    }

    #[test]
    fn window_at_exact_end_accepted() {
        let data = ramp(64);
        let inner: Box<dyn BytesSource> = Box::new(Cursor::new(data));
        // 30 + 34 = 64 == inner length, exact tail.
        let mut sub = SubSource::new(inner, 30, 34).unwrap();
        let mut out = Vec::new();
        sub.read_to_end(&mut out).unwrap();
        assert_eq!(out.len(), 34);
        assert_eq!(out[0], 30);
        assert_eq!(out[33], 63);
    }

    #[test]
    fn zero_length_window() {
        let data = ramp(64);
        let inner: Box<dyn BytesSource> = Box::new(Cursor::new(data));
        let mut sub = SubSource::new(inner, 16, 0).unwrap();
        assert!(sub.is_empty());
        let mut out = [0u8; 4];
        assert_eq!(sub.read(&mut out).unwrap(), 0);
    }

    #[test]
    fn overflowing_window_rejected() {
        let data = ramp(64);
        let inner: Box<dyn BytesSource> = Box::new(Cursor::new(data));
        let r = SubSource::new(inner, u64::MAX, 1);
        assert!(r.is_err());
    }

    #[test]
    fn into_inner_returns_inner_source() {
        let data = ramp(64);
        let inner: Box<dyn BytesSource> = Box::new(Cursor::new(data.clone()));
        let mut sub = SubSource::new(inner, 8, 16).unwrap();
        let mut byte = [0u8; 1];
        sub.read_exact(&mut byte).unwrap();
        let mut recovered = sub.into_inner();
        // Seek the recovered handle to a known offset and read.
        recovered.seek(SeekFrom::Start(0)).unwrap();
        let mut head = [0u8; 4];
        recovered.read_exact(&mut head).unwrap();
        assert_eq!(head, [0, 1, 2, 3]);
    }

    #[test]
    fn nested_windows_compose() {
        let data = ramp(256);
        let inner: Box<dyn BytesSource> = Box::new(Cursor::new(data.clone()));
        let outer = SubSource::new(inner, 64, 128).unwrap();
        // Window the outer into its own [16, 16+32) = inner [80, 112).
        let mut nested = SubSource::new(Box::new(outer), 16, 32).unwrap();
        let mut out = vec![0u8; 32];
        nested.read_exact(&mut out).unwrap();
        assert_eq!(out, &data[80..112]);
    }

    #[test]
    fn stream_len_helper_preserves_position() {
        let data = ramp(128);
        let mut src: Box<dyn BytesSource> = Box::new(Cursor::new(data));
        src.seek(SeekFrom::Start(42)).unwrap();
        let len = stream_len(&mut *src).unwrap();
        assert_eq!(len, 128);
        assert_eq!(src.stream_position().unwrap(), 42);
    }
}
