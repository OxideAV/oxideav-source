//! File-driver round-trip tests.

use std::io::{Read, Seek, SeekFrom};

use oxideav_source::{SourceOutput, SourceRegistry};

/// Helper: open a URI and unwrap the expected `SourceOutput::Bytes`
/// variant (the file driver always produces bytes).
fn open_bytes(reg: &SourceRegistry, uri: &str) -> Box<dyn oxideav_source::BytesSource> {
    match reg.open(uri).expect("open") {
        SourceOutput::Bytes(b) => b,
        _ => panic!("expected SourceOutput::Bytes from the file driver"),
    }
}

#[test]
fn open_bare_path_reads_first_bytes() {
    let reg = oxideav_source::with_defaults();
    let mut f = open_bytes(&reg, "Cargo.toml");
    let mut head = [0u8; 4];
    f.read_exact(&mut head).expect("read");
    assert_eq!(&head, b"[pac"); // start of a Cargo.toml: "[package]"
}

#[test]
fn open_file_url_reads_first_bytes() {
    let reg = oxideav_source::with_defaults();
    let cwd = std::env::current_dir().unwrap();
    let url = format!("file://{}/Cargo.toml", cwd.display());
    let mut f = open_bytes(&reg, &url);
    let mut head = [0u8; 4];
    f.read_exact(&mut head).expect("read");
    assert_eq!(&head, b"[pac");
}

#[test]
fn open_supports_seek() {
    let reg = oxideav_source::with_defaults();
    let mut f = open_bytes(&reg, "Cargo.toml");
    let end = f.seek(SeekFrom::End(0)).unwrap();
    assert!(end > 0);
    f.seek(SeekFrom::Start(0)).unwrap();
    let pos = f.stream_position().unwrap();
    assert_eq!(pos, 0);
}

#[test]
fn unknown_scheme_with_no_driver_errors() {
    let mut reg = SourceRegistry::new();
    reg.register_bytes("file", oxideav_source::open_file);
    let r = reg.open("https://example.com/x");
    // No https driver registered — falls through to file driver, which
    // will then fail to open a file with that path.
    assert!(r.is_err());
}
