//! `slice:` driver — round-trip through the public registry.

use std::io::Read;

use oxideav_source::{mem, parse_slice_uri, BytesSource, SliceUri, SourceOutput, SourceRegistry};

fn open_bytes(reg: &SourceRegistry, uri: &str) -> Box<dyn BytesSource> {
    match reg.open(uri).expect("open") {
        SourceOutput::Bytes(b) => b,
        _ => panic!("expected SourceOutput::Bytes from slice driver"),
    }
}

#[test]
fn with_defaults_registers_slice_scheme() {
    let reg = oxideav_source::with_defaults();
    let schemes: Vec<&str> = reg.schemes().collect();
    assert!(schemes.contains(&"slice"));
}

#[test]
fn slice_over_mem_round_trip_via_registry() {
    let body: Vec<u8> = (0..=255u8).collect();
    mem::put("test-slice-via-reg", body.clone());
    let reg = oxideav_source::with_defaults();
    let mut r = open_bytes(&reg, "slice:64+32!mem://test-slice-via-reg");
    let mut buf = Vec::new();
    r.read_to_end(&mut buf).unwrap();
    assert_eq!(buf, &body[64..96]);
    assert!(mem::remove("test-slice-via-reg"));
}

#[test]
fn slice_over_data_round_trip_via_registry() {
    let reg = oxideav_source::with_defaults();
    // "The quick brown fox" → bytes [6, 11) = "ick b".
    let mut r = open_bytes(&reg, "slice:6+5!data:,The quick brown fox");
    let mut buf = Vec::new();
    r.read_to_end(&mut buf).unwrap();
    assert_eq!(&buf, b"ick b");
}

#[test]
fn slice_window_overflow_errors_through_registry() {
    mem::put("test-slice-overflow", vec![0u8; 16]);
    let reg = oxideav_source::with_defaults();
    let r = reg.open("slice:8+16!mem://test-slice-overflow");
    assert!(r.is_err(), "windowing past inner length must error");
    assert!(mem::remove("test-slice-overflow"));
}

#[test]
fn parse_slice_uri_typed_public_api() {
    // Public typed parser is reachable from a downstream crate via the
    // re-exported `parse_slice_uri` name (parallel to `parse_data_uri`).
    let s = parse_slice_uri("slice:100+250!mem://x").unwrap();
    assert_eq!(s.offset, 100);
    assert_eq!(s.length, 250);
    assert_eq!(s.inner, "mem://x");
}

#[test]
fn slice_uri_builder_round_trip_via_registry() {
    // Build a slice URI programmatically (no `format!()` gymnastics on
    // the caller side), open it through the registry, read it back.
    let body: Vec<u8> = (0..=255u8).collect();
    mem::put("test-slice-builder", body.clone());
    let uri = SliceUri::new(32, 8, "mem://test-slice-builder")
        .unwrap()
        .format();
    let reg = oxideav_source::with_defaults();
    let mut r = open_bytes(&reg, &uri);
    let mut buf = Vec::new();
    r.read_to_end(&mut buf).unwrap();
    assert_eq!(buf, &body[32..40]);
    assert!(mem::remove("test-slice-builder"));
}
