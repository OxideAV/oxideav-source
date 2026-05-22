//! `data:` driver — round-trip through the public registry.

use std::io::{Read, Seek, SeekFrom};

use oxideav_source::{BytesSource, SourceOutput, SourceRegistry};

fn open_bytes(reg: &SourceRegistry, uri: &str) -> Box<dyn BytesSource> {
    match reg.open(uri).expect("open") {
        SourceOutput::Bytes(b) => b,
        _ => panic!("expected SourceOutput::Bytes from data driver"),
    }
}

#[test]
fn with_defaults_registers_data_scheme() {
    let reg = oxideav_source::with_defaults();
    let schemes: Vec<&str> = reg.schemes().collect();
    assert!(schemes.contains(&"data"));
    assert!(schemes.contains(&"file"));
    assert!(schemes.contains(&"mem"));
}

#[test]
fn data_inline_text_via_registry() {
    let reg = oxideav_source::with_defaults();
    let mut r = open_bytes(&reg, "data:,Hello%2C%20world");
    let mut buf = Vec::new();
    r.read_to_end(&mut buf).unwrap();
    assert_eq!(buf, b"Hello, world");
}

#[test]
fn data_base64_via_registry() {
    // base64("OxideAV") = "T3hpZGVBVg=="
    let reg = oxideav_source::with_defaults();
    let mut r = open_bytes(&reg, "data:application/octet-stream;base64,T3hpZGVBVg==");
    let mut buf = Vec::new();
    r.read_to_end(&mut buf).unwrap();
    assert_eq!(buf, b"OxideAV");
}

#[test]
fn data_source_is_seekable() {
    let reg = oxideav_source::with_defaults();
    let mut r = open_bytes(&reg, "data:,abcdefghij");
    let end = r.seek(SeekFrom::End(0)).unwrap();
    assert_eq!(end, 10);
    r.seek(SeekFrom::Start(4)).unwrap();
    let mut byte = [0u8; 1];
    r.read_exact(&mut byte).unwrap();
    assert_eq!(byte[0], b'e');
}

#[test]
fn data_malformed_errors_through_registry() {
    let reg = oxideav_source::with_defaults();
    // Missing the mandatory ',' separator.
    let r = reg.open("data:text/plain;base64");
    assert!(r.is_err());
}

#[test]
fn data_parse_exposes_mediatype() {
    let p = oxideav_source::parse_data_uri("data:image/png;base64,SGVsbG8=").unwrap();
    assert_eq!(p.mediatype, "image/png");
    assert!(p.base64);
    assert_eq!(p.data, b"Hello");
}
