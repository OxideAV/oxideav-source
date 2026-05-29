//! `concat:` driver — round-trip through the public registry.

use std::io::{Read, Seek, SeekFrom, Write};

use oxideav_source::{BytesSource, SourceOutput, SourceRegistry};

fn open_bytes(reg: &SourceRegistry, uri: &str) -> Box<dyn BytesSource> {
    match reg.open(uri).expect("open") {
        SourceOutput::Bytes(b) => b,
        _ => panic!("expected SourceOutput::Bytes from the concat driver"),
    }
}

/// Write `bytes` to a uniquely-named temp file and return its path.
fn temp_file(bytes: &[u8]) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let mut path = std::env::temp_dir();
    let pid = std::process::id();
    let n = N.fetch_add(1, Ordering::Relaxed);
    path.push(format!("oxideav-concat-it-{pid}-{n}.bin"));
    let mut f = std::fs::File::create(&path).unwrap();
    f.write_all(bytes).unwrap();
    f.flush().unwrap();
    path
}

#[test]
fn with_defaults_registers_concat_scheme() {
    let reg = oxideav_source::with_defaults();
    let schemes: Vec<&str> = reg.schemes().collect();
    assert!(schemes.contains(&"concat"));
    assert!(schemes.contains(&"data"));
    assert!(schemes.contains(&"file"));
    assert!(schemes.contains(&"mem"));
}

#[test]
fn concat_two_files_via_registry() {
    let a = temp_file(b"first-");
    let b = temp_file(b"second");
    let uri = format!("concat:{}|{}", a.display(), b.display());
    let reg = oxideav_source::with_defaults();
    let mut r = open_bytes(&reg, &uri);
    let mut buf = Vec::new();
    r.read_to_end(&mut buf).unwrap();
    assert_eq!(buf, b"first-second");
    std::fs::remove_file(a).ok();
    std::fs::remove_file(b).ok();
}

#[test]
fn concat_is_seekable_across_boundary() {
    let a = temp_file(b"ABCDE"); // 0..5
    let b = temp_file(b"FGHIJ"); // 5..10
    let uri = format!("concat:{}|{}", a.display(), b.display());
    let reg = oxideav_source::with_defaults();
    let mut r = open_bytes(&reg, &uri);
    let end = r.seek(SeekFrom::End(0)).unwrap();
    assert_eq!(end, 10);
    r.seek(SeekFrom::Start(4)).unwrap();
    let mut buf = [0u8; 3]; // last of a + first 2 of b
    r.read_exact(&mut buf).unwrap();
    assert_eq!(&buf, b"EFG");
    std::fs::remove_file(a).ok();
    std::fs::remove_file(b).ok();
}

#[test]
fn concat_malformed_errors_through_registry() {
    let reg = oxideav_source::with_defaults();
    assert!(reg.open("concat:").is_err());
    assert!(reg.open("concat:a||b").is_err());
}

#[test]
fn concat_mixed_inner_schemes_via_registry() {
    // file + mem://<id> + data:,literal — same composability the `slice:`
    // driver already offers as its inner scheme.
    oxideav_source::mem::put("concat-it-r184", b"MEM".to_vec());
    let f = temp_file(b"FILE");
    let uri = format!("concat:{}|mem://concat-it-r184|data:,TAIL", f.display());
    let reg = oxideav_source::with_defaults();
    let mut r = open_bytes(&reg, &uri);
    let mut buf = Vec::new();
    r.read_to_end(&mut buf).unwrap();
    assert_eq!(buf, b"FILEMEMTAIL");
    oxideav_source::mem::remove("concat-it-r184");
    std::fs::remove_file(f).ok();
}

#[test]
fn concat_slice_segment_via_registry() {
    // A `slice:` segment composes with a literal `data:` segment.
    oxideav_source::mem::put("concat-it-r184-slc", b"abcdefgh".to_vec());
    let reg = oxideav_source::with_defaults();
    let mut r = open_bytes(
        &reg,
        "concat:slice:2+3!mem://concat-it-r184-slc|data:,_TAIL",
    );
    let mut buf = Vec::new();
    r.read_to_end(&mut buf).unwrap();
    assert_eq!(buf, b"cde_TAIL");
    oxideav_source::mem::remove("concat-it-r184-slc");
}
