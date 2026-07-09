//! `open_bytes` — registry-free dispatch over the bundled in-process
//! drivers. One free function resolves all five schemes; it is also the
//! dispatch surface `slice:` and `concat:` use for their inner/segment
//! URIs, so this suite doubles as coverage for that shared path.

use std::io::{Read, Write};

use oxideav_source::{mem, open_bytes};

fn temp_file(bytes: &[u8]) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let mut path = std::env::temp_dir();
    let pid = std::process::id();
    let n = N.fetch_add(1, Ordering::Relaxed);
    path.push(format!("oxideav-open-bytes-{pid}-{n}.bin"));
    let mut f = std::fs::File::create(&path).unwrap();
    f.write_all(bytes).unwrap();
    path
}

fn read_all(mut r: Box<dyn oxideav_source::BytesSource>) -> Vec<u8> {
    let mut buf = Vec::new();
    r.read_to_end(&mut buf).unwrap();
    buf
}

#[test]
fn dispatches_file_uri_and_bare_path() {
    let p = temp_file(b"file-bytes");
    // Explicit scheme.
    let r = open_bytes(&format!("file://{}", p.display())).unwrap();
    assert_eq!(read_all(r), b"file-bytes");
    // Bare path (scheme-less fallback).
    let r = open_bytes(&p.display().to_string()).unwrap();
    assert_eq!(read_all(r), b"file-bytes");
    std::fs::remove_file(p).ok();
}

#[test]
fn dispatches_mem() {
    mem::put("open-bytes-mem", b"mem-bytes".to_vec());
    let r = open_bytes("mem://open-bytes-mem").unwrap();
    assert_eq!(read_all(r), b"mem-bytes");
    mem::remove("open-bytes-mem");
}

#[test]
fn dispatches_data() {
    let r = open_bytes("data:,plain%20text").unwrap();
    assert_eq!(read_all(r), b"plain text");
    let r = open_bytes("data:;base64,SGVsbG8=").unwrap();
    assert_eq!(read_all(r), b"Hello");
}

#[test]
fn dispatches_slice() {
    let r = open_bytes("slice:3+2!data:,ABCDEFGH").unwrap();
    assert_eq!(read_all(r), b"DE");
}

#[test]
fn dispatches_concat() {
    let r = open_bytes("concat:data:,AA|data:,BB").unwrap();
    assert_eq!(read_all(r), b"AABB");
}

#[test]
fn slice_over_concat_composes() {
    // The shared dispatcher makes concat: a legal slice: inner — the
    // slice grammar splits on '!', never on '|', so the nesting is
    // unambiguous in this direction.
    let r = open_bytes("slice:2+4!concat:data:,AB|data:,CDEF").unwrap();
    assert_eq!(read_all(r), b"CDEF");
}

#[test]
fn unknown_scheme_rejected_with_scheme_name() {
    let err = match open_bytes("http://example.com/x") {
        Err(e) => e.to_string(),
        Ok(_) => panic!("http must not be dispatchable without a registry"),
    };
    assert!(err.contains("http"), "error should name the scheme: {err}");
}

#[test]
fn uppercase_schemes_dispatch() {
    mem::put("open-bytes-case", b"CASE".to_vec());
    let r = open_bytes("MEM://open-bytes-case").unwrap();
    assert_eq!(read_all(r), b"CASE");
    mem::remove("open-bytes-case");
}

#[test]
fn missing_file_keeps_io_error_taxonomy() {
    // A dispatchable scheme whose open fails must surface the driver's
    // own error, not a generic dispatcher error. For file:// that means
    // the IO error (NotFound) bubbles through untouched.
    let err = match open_bytes("/no/such/path/oxideav-open-bytes-xyzzy") {
        Err(e) => e,
        Ok(_) => panic!("missing file must error"),
    };
    let msg = err.to_string();
    assert!(
        !msg.contains("no bundled in-process driver"),
        "missing file must not be reported as an unknown scheme: {msg}"
    );
}
