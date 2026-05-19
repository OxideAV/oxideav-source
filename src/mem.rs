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
use std::io::Cursor;
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
/// independent `Cursor` view over the shared buffer; readers and seekers
/// do not interfere.
pub fn open_mem(uri_str: &str) -> Result<Box<dyn BytesSource>> {
    let (scheme, rest) = uri::split(uri_str);
    if scheme != "mem" {
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
    let buf = guard
        .get(id)
        .ok_or_else(|| Error::invalid(format!("mem:// id '{id}' is not registered")))?;
    // Clone the Arc, then take an owned copy of the bytes for the Cursor.
    // Buffers are typically small (test fixtures); we trade a copy for
    // simpler ownership than wrapping Arc<Vec<u8>> in a Read/Seek impl.
    let bytes: Vec<u8> = (**buf).clone();
    Ok(Box::new(Cursor::new(bytes)))
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
