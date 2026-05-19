//! `mem://` driver — round-trip through the public registry.

use std::io::{Read, Seek, SeekFrom};

use oxideav_source::{mem, BytesSource, SourceOutput, SourceRegistry};

fn open_bytes(reg: &SourceRegistry, uri: &str) -> Box<dyn BytesSource> {
    match reg.open(uri).expect("open") {
        SourceOutput::Bytes(b) => b,
        _ => panic!("expected SourceOutput::Bytes from mem driver"),
    }
}

#[test]
fn with_defaults_registers_mem_scheme() {
    let reg = oxideav_source::with_defaults();
    let schemes: Vec<&str> = reg.schemes().collect();
    assert!(schemes.contains(&"file"));
    assert!(schemes.contains(&"mem"));
}

#[test]
fn mem_round_trip_via_registry() {
    mem::put("test-mem-roundtrip", b"hello-from-mem".to_vec());
    let reg = oxideav_source::with_defaults();
    let mut r = open_bytes(&reg, "mem://test-mem-roundtrip");
    let mut buf = Vec::new();
    r.read_to_end(&mut buf).unwrap();
    assert_eq!(buf, b"hello-from-mem");
    assert!(mem::remove("test-mem-roundtrip"));
}

#[test]
fn mem_supports_seek_via_registry() {
    let body: Vec<u8> = (0..=255u8).collect();
    mem::put("test-mem-seek", body.clone());
    let reg = oxideav_source::with_defaults();
    let mut r = open_bytes(&reg, "mem://test-mem-seek");
    let end = r.seek(SeekFrom::End(0)).unwrap();
    assert_eq!(end, 256);
    r.seek(SeekFrom::Start(42)).unwrap();
    let mut byte = [0u8; 1];
    r.read_exact(&mut byte).unwrap();
    assert_eq!(byte[0], 42);
    assert!(mem::remove("test-mem-seek"));
}

#[test]
fn mem_missing_id_errors_through_registry() {
    let reg = oxideav_source::with_defaults();
    let r = reg.open("mem://this-id-was-never-registered");
    assert!(r.is_err());
}
