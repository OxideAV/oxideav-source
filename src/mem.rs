//! Built-in `mem://` driver — in-memory synthetic byte buffers.
//!
//! A process-global registry maps a string identifier to an `Arc<Vec<u8>>`;
//! `mem://<id>` opens the buffer as a [`BytesSource`]. Useful for tests
//! and for pipelines that want to feed pre-baked bytes through the same
//! `open(uri)` shape they use for files.
//!
//! The scheme has **no on-wire spec** — it is internal-to-OxideAV.
//! Grammar (informal):
//!
//! ```text
//! mem://<id>
//! ```
//!
//! where `<id>` is any byte sequence excluding `/`. Empty `<id>` is
//! rejected (we want a non-ambiguous "default buffer" form to remain
//! available for future use).

use std::collections::HashMap;
use std::io::{self, Read, Seek, SeekFrom};
use std::sync::{Arc, OnceLock, RwLock};

use oxideav_core::{BytesSource, Error, Result};

use crate::uri;

/// Process-global table of registered `mem://` buffers.
fn table() -> &'static RwLock<HashMap<String, Arc<Vec<u8>>>> {
    static T: OnceLock<RwLock<HashMap<String, Arc<Vec<u8>>>>> = OnceLock::new();
    T.get_or_init(|| RwLock::new(HashMap::new()))
}

/// Install a buffer at `mem://<id>`. Replaces any prior entry under the
/// same id. The buffer is held by reference; concurrent opens share it
/// without copying.
pub fn put<I: Into<String>>(id: I, data: Vec<u8>) {
    table()
        .write()
        .expect("mem:// table poisoned")
        .insert(id.into(), Arc::new(data));
}

/// Remove an entry. Returns `true` if a buffer was present.
pub fn remove(id: &str) -> bool {
    table()
        .write()
        .expect("mem:// table poisoned")
        .remove(id)
        .is_some()
}

/// Drop every registered `mem://` entry. Intended for test teardown.
pub fn clear() {
    table().write().expect("mem:// table poisoned").clear();
}

/// Open a `mem://<id>` URI as a [`BytesSource`]. Each open returns an
/// independent reader over the **same** shared buffer — no per-open
/// copy. Readers and seekers do not interfere with each other because
/// each `MemReader` owns its own position, while the bytes themselves
/// are reference-counted via [`Arc`]. Large `mem://` buffers (e.g.
/// pre-loaded test fixtures or in-memory transcode roundtrip targets)
/// therefore cost a single `Arc` clone per `open` instead of a full
/// `Vec<u8>` copy.
pub fn open_mem(uri_str: &str) -> Result<Box<dyn BytesSource>> {
    let (scheme, rest) = uri::split(uri_str);
    if !uri::scheme_is(scheme, "mem") {
        return Err(Error::invalid(format!(
            "mem driver invoked on non-mem URI: {uri_str}"
        )));
    }
    let id = rest;
    if id.is_empty() {
        return Err(Error::invalid("mem:// URI requires a non-empty id"));
    }
    if id.contains('/') {
        return Err(Error::invalid(format!(
            "mem:// id must not contain '/': {id}"
        )));
    }
    let guard = table().read().expect("mem:// table poisoned");
    // Taxonomy: the URI is well-formed; the buffer just isn't there.
    // That is a NotFound lookup miss (like a missing file), not
    // malformed input — callers branching on the error kind get the
    // same shape as a failing `file://` open.
    let buf = guard.get(id).ok_or_else(|| {
        Error::Io(io::Error::new(
            io::ErrorKind::NotFound,
            format!("mem:// id '{id}' is not registered"),
        ))
    })?;
    Ok(Box::new(MemReader::new(Arc::clone(buf))))
}

/// `Read + Seek` view onto an `Arc<Vec<u8>>` buffer. One reader per
/// `open_mem` call; each carries its own position so reads on multiple
/// handles to the same buffer are independent.
struct MemReader {
    buf: Arc<Vec<u8>>,
    pos: u64,
}

impl MemReader {
    fn new(buf: Arc<Vec<u8>>) -> Self {
        Self { buf, pos: 0 }
    }
}

impl Read for MemReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        let len = self.buf.len() as u64;
        if self.pos >= len {
            return Ok(0);
        }
        let avail = (len - self.pos) as usize;
        let n = out.len().min(avail);
        let start = self.pos as usize;
        out[..n].copy_from_slice(&self.buf[start..start + n]);
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for MemReader {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let len = self.buf.len() as u64;
        let new_pos = match from {
            SeekFrom::Start(n) => n,
            SeekFrom::End(d) => add_signed(len, d)?,
            SeekFrom::Current(d) => add_signed(self.pos, d)?,
        };
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
            "mem:// reader: seek resolves to a negative or overflowing position",
        )
    })
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Seek, SeekFrom};

    use super::*;

    fn fresh_id() -> String {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        format!("test-{}", N.fetch_add(1, Ordering::Relaxed))
    }

    #[test]
    fn put_open_read_roundtrip() {
        let id = fresh_id();
        put(&id, b"hello, mem://".to_vec());
        let mut r = open_mem(&format!("mem://{id}")).unwrap();
        let mut buf = Vec::new();
        r.read_to_end(&mut buf).unwrap();
        assert_eq!(buf, b"hello, mem://");
        assert!(remove(&id));
    }

    #[test]
    fn open_supports_seek() {
        let id = fresh_id();
        put(&id, (0..=255u8).collect());
        let mut r = open_mem(&format!("mem://{id}")).unwrap();
        r.seek(SeekFrom::Start(100)).unwrap();
        let mut byte = [0u8; 1];
        r.read_exact(&mut byte).unwrap();
        assert_eq!(byte[0], 100);
        let end = r.seek(SeekFrom::End(0)).unwrap();
        assert_eq!(end, 256);
        assert!(remove(&id));
    }

    #[test]
    fn unknown_id_errors() {
        let r = open_mem("mem://does-not-exist-xyz");
        assert!(r.is_err());
    }

    #[test]
    fn empty_id_rejected() {
        let r = open_mem("mem://");
        assert!(r.is_err());
    }

    #[test]
    fn slash_in_id_rejected() {
        let r = open_mem("mem://foo/bar");
        assert!(r.is_err());
    }

    #[test]
    fn wrong_scheme_rejected() {
        let r = open_mem("file:///tmp/x");
        assert!(r.is_err());
    }

    #[test]
    fn seek_past_end_then_read_returns_zero() {
        let id = fresh_id();
        put(&id, b"abcdef".to_vec());
        let mut r = open_mem(&format!("mem://{id}")).unwrap();
        r.seek(SeekFrom::Start(100)).unwrap();
        let mut buf = [0u8; 8];
        assert_eq!(r.read(&mut buf).unwrap(), 0);
        // Step back inside the buffer, the bytes are still readable.
        r.seek(SeekFrom::Start(2)).unwrap();
        let mut chunk = [0u8; 3];
        r.read_exact(&mut chunk).unwrap();
        assert_eq!(&chunk, b"cde");
        assert!(remove(&id));
    }

    #[test]
    fn seek_before_zero_errors() {
        let id = fresh_id();
        put(&id, b"xy".to_vec());
        let mut r = open_mem(&format!("mem://{id}")).unwrap();
        let err = r.seek(SeekFrom::Current(-1));
        assert!(err.is_err());
        let err = r.seek(SeekFrom::End(-100));
        assert!(err.is_err());
        assert!(remove(&id));
    }

    #[test]
    fn large_buffer_open_does_not_copy() {
        // Sanity check that opening a multi-MB buffer is cheap. We
        // don't assert peak memory here (process-level RSS is noisy),
        // but the test exists so a future regression to a per-open
        // clone shows up as a noticeable slowdown.
        let id = fresh_id();
        let big: Vec<u8> = (0..(2 * 1024 * 1024)).map(|i| (i & 0xff) as u8).collect();
        put(&id, big.clone());
        // Open 16 readers; with the Arc-backed design this is 16 Arc
        // clones, not 16 × 2 MiB allocations.
        let mut readers = Vec::with_capacity(16);
        for _ in 0..16 {
            readers.push(open_mem(&format!("mem://{id}")).unwrap());
        }
        // Each reader should see the same bytes.
        for r in readers.iter_mut() {
            let mut head = [0u8; 8];
            r.read_exact(&mut head).unwrap();
            assert_eq!(head, [0, 1, 2, 3, 4, 5, 6, 7]);
        }
        // And independent positions.
        let pos = readers[0].stream_position().unwrap();
        assert_eq!(pos, 8);
        assert!(remove(&id));
    }

    #[test]
    fn multiple_opens_are_independent() {
        let id = fresh_id();
        put(&id, b"AAAAAAAA".to_vec());
        let mut r1 = open_mem(&format!("mem://{id}")).unwrap();
        let mut r2 = open_mem(&format!("mem://{id}")).unwrap();
        let mut a = [0u8; 4];
        r1.read_exact(&mut a).unwrap();
        // r2 is still at offset 0.
        let mut b = [0u8; 1];
        r2.read_exact(&mut b).unwrap();
        assert_eq!(b[0], b'A');
        // r1 has advanced by 4.
        let pos1 = r1.stream_position().unwrap();
        let pos2 = r2.stream_position().unwrap();
        assert_eq!(pos1, 4);
        assert_eq!(pos2, 1);
        assert!(remove(&id));
    }
}
