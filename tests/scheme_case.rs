//! Scheme names are case-insensitive per RFC 3986 §3.1 — `FILE:`,
//! `Mem:`, `DATA:` etc. must open exactly like their lowercase forms.
//!
//! Regression suite for a dispatch/validation split: the registry
//! normalises the scheme to lowercase before dispatching, but each
//! driver re-splits the URI and used to compare the scheme
//! case-sensitively, rejecting URIs the registry had legitimately
//! routed to it (`reg.open("MEM://x")` reached `open_mem`, which then
//! errored with "invoked on non-mem URI").

use std::io::{Read, Write};

use oxideav_source::{
    mem, open_concat, open_data, open_file, open_mem, open_slice, with_defaults, FileScope,
    SourceOutput,
};

fn temp_file(bytes: &[u8]) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let mut path = std::env::temp_dir();
    let pid = std::process::id();
    let n = N.fetch_add(1, Ordering::Relaxed);
    path.push(format!("oxideav-scheme-case-{pid}-{n}.bin"));
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
fn file_driver_accepts_uppercase_scheme() {
    let p = temp_file(b"case-file");
    let uri = format!("FILE://{}", p.display());
    let r = open_file(&uri).unwrap();
    assert_eq!(read_all(r), b"case-file");
    std::fs::remove_file(p).ok();
}

#[test]
fn file_driver_accepts_mixed_case_scheme() {
    let p = temp_file(b"case-file-mixed");
    let uri = format!("File://{}", p.display());
    let r = open_file(&uri).unwrap();
    assert_eq!(read_all(r), b"case-file-mixed");
    std::fs::remove_file(p).ok();
}

#[test]
fn uppercase_file_uri_still_percent_decodes() {
    // `has_file_scheme` was already case-insensitive; the scheme guard
    // was not. With both aligned, an uppercase file URI must keep the
    // percent-decoding contract of the lowercase form.
    let dir = std::env::temp_dir();
    let p = dir.join("oxideav scheme case.bin");
    std::fs::write(&p, b"decoded").unwrap();
    let encoded = p.display().to_string().replace(' ', "%20");
    let r = open_file(&format!("FILE://{encoded}")).unwrap();
    assert_eq!(read_all(r), b"decoded");
    std::fs::remove_file(p).ok();
}

#[test]
fn mem_driver_accepts_uppercase_scheme() {
    mem::put("scheme-case-mem", b"case-mem".to_vec());
    let r = open_mem("MEM://scheme-case-mem").unwrap();
    assert_eq!(read_all(r), b"case-mem");
    mem::remove("scheme-case-mem");
}

#[test]
fn data_driver_accepts_uppercase_scheme() {
    let r = open_data("DATA:,case-data").unwrap();
    assert_eq!(read_all(r), b"case-data");
    // Mixed case with base64 payload.
    let r = open_data("Data:;base64,SGVsbG8=").unwrap();
    assert_eq!(read_all(r), b"Hello");
}

#[test]
fn slice_driver_accepts_uppercase_scheme_and_inner() {
    mem::put("scheme-case-slice", b"abcdefgh".to_vec());
    // Both the outer `SLICE:` scheme and the inner `MEM://` scheme in
    // uppercase.
    let r = open_slice("SLICE:2+3!MEM://scheme-case-slice").unwrap();
    assert_eq!(read_all(r), b"cde");
    mem::remove("scheme-case-slice");
}

#[test]
fn concat_driver_accepts_uppercase_scheme_and_segments() {
    mem::put("scheme-case-concat", b"BB".to_vec());
    let r = open_concat("CONCAT:DATA:,AA|MEM://scheme-case-concat").unwrap();
    assert_eq!(read_all(r), b"AABB");
    mem::remove("scheme-case-concat");
}

#[test]
fn concat_rejects_uppercase_nested_concat_segment() {
    // The nested-concat rejection must fire regardless of case; if the
    // uppercase form slipped past it, the segment would fall into the
    // bare-path fallback and produce a confusing file-not-found instead.
    let err = match open_concat("concat:CONCAT:a|b") {
        Err(e) => e.to_string(),
        Ok(_) => panic!("uppercase nested concat must be rejected"),
    };
    assert!(
        err.contains("nesting concat"),
        "uppercase nested concat must hit the explicit rejection, got: {err}"
    );
}

#[test]
fn registry_open_dispatches_uppercase_schemes() {
    let reg = with_defaults();
    mem::put("scheme-case-reg", b"via-registry".to_vec());

    let out = reg.open("MEM://scheme-case-reg").unwrap();
    match out {
        SourceOutput::Bytes(r) => assert_eq!(read_all(r), b"via-registry"),
        _ => panic!("expected bytes"),
    }

    let out = reg.open("DATA:,reg-data").unwrap();
    match out {
        SourceOutput::Bytes(r) => assert_eq!(read_all(r), b"reg-data"),
        _ => panic!("expected bytes"),
    }

    let out = reg.open("SLICE:0+3!MEM://scheme-case-reg").unwrap();
    match out {
        SourceOutput::Bytes(r) => assert_eq!(read_all(r), b"via"),
        _ => panic!("expected bytes"),
    }

    mem::remove("scheme-case-reg");
}

#[test]
fn scope_resolve_accepts_uppercase_file_scheme() {
    let dir = std::env::temp_dir().join("oxideav-scheme-case-scope");
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("payload.bin");
    std::fs::write(&file, b"scoped").unwrap();
    let scope = FileScope::new().allow_dir(&dir);
    let canon = scope
        .resolve(&format!("FILE://{}", file.display()))
        .unwrap();
    assert_eq!(canon, std::fs::canonicalize(&file).unwrap());
}

#[test]
fn wrong_scheme_still_rejected_regardless_of_case() {
    // Case-insensitivity must not weaken the "wrong driver" guard.
    assert!(open_mem("FILE:///tmp/x").is_err());
    assert!(open_data("MEM://x").is_err());
    assert!(open_file("MEM://x").is_err());
    assert!(open_slice("DATA:,x").is_err());
    assert!(open_concat("SLICE:0+1!mem://x").is_err());
}
