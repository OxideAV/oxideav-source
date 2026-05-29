//! `slice:` driver — round-trip through the public registry.

use std::io::Read;

use oxideav_source::{mem, BytesSource, SourceOutput, SourceRegistry};

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
