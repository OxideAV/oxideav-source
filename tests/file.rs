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

/// Helper: create a temp file with a percent-decodable name containing a
/// space, write `body`, and return the path.
fn temp_file_with_space(stem: &str, body: &[u8]) -> std::path::PathBuf {
    use std::io::Write;
    let pid = std::process::id();
    let p = std::env::temp_dir().join(format!("oxideav-source-r222 {stem}-{pid}.bin"));
    let mut f = std::fs::File::create(&p).unwrap();
    f.write_all(body).unwrap();
    f.flush().unwrap();
    p
}

/// Percent-encode the path string for embedding in a `file://` URI:
/// every byte outside `[A-Za-z0-9-._~/]` becomes `%HH`.
fn percent_encode_path(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        let unreserved = matches!(*b, b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9'
            | b'-' | b'_' | b'.' | b'~' | b'/');
        if unreserved {
            out.push(*b as char);
        } else {
            out.push_str(&format!("%{:02X}", b));
        }
    }
    out
}

#[test]
fn open_file_url_percent_decodes_space() {
    let reg = oxideav_source::with_defaults();
    let path = temp_file_with_space("space-test", b"PERCENT");
    let encoded = percent_encode_path(&path.display().to_string());
    let url = format!("file://{encoded}");
    let mut f = open_bytes(&reg, &url);
    let mut buf = Vec::new();
    std::io::Read::read_to_end(&mut f, &mut buf).unwrap();
    assert_eq!(buf, b"PERCENT");
    std::fs::remove_file(path).ok();
}

#[test]
fn open_file_url_percent_decodes_utf8_multibyte() {
    // A file whose name contains a non-ASCII Unicode character must
    // open via its percent-encoded URI form.
    use std::io::Write;
    let pid = std::process::id();
    // "Привет" — UTF-8: D0 9F D1 80 D0 B8 D0 B2 D0 B5 D1 82.
    let path = std::env::temp_dir().join(format!("oxideav-source-r222-Привет-{pid}.bin"));
    let mut f = std::fs::File::create(&path).unwrap();
    f.write_all(b"UTF8").unwrap();
    f.flush().unwrap();

    let reg = oxideav_source::with_defaults();
    let encoded = percent_encode_path(&path.display().to_string());
    let url = format!("file://{encoded}");
    let mut bs = open_bytes(&reg, &url);
    let mut buf = Vec::new();
    std::io::Read::read_to_end(&mut bs, &mut buf).unwrap();
    assert_eq!(buf, b"UTF8");
    std::fs::remove_file(path).ok();
}

#[test]
fn open_bare_path_does_not_percent_decode() {
    // A bare path is passed verbatim; a real file containing `%20` in
    // its name must open without surprise decoding.
    use std::io::Write;
    let pid = std::process::id();
    let path = std::env::temp_dir().join(format!("oxideav-source-r222-100%25-{pid}.bin"));
    let mut f = std::fs::File::create(&path).unwrap();
    f.write_all(b"LITERAL").unwrap();
    f.flush().unwrap();

    let reg = oxideav_source::with_defaults();
    // Bare path with literal `%` byte — must reach the FS unchanged.
    let mut bs = open_bytes(&reg, &path.display().to_string());
    let mut buf = Vec::new();
    std::io::Read::read_to_end(&mut bs, &mut buf).unwrap();
    assert_eq!(buf, b"LITERAL");
    std::fs::remove_file(path).ok();
}

#[test]
fn open_file_url_truncated_percent_rejected() {
    // `file:///tmp/%F` is malformed per RFC 3986 — the file driver must
    // surface the parse error rather than handing junk to the FS.
    let reg = oxideav_source::with_defaults();
    let r = reg.open("file:///tmp/%F");
    assert!(r.is_err());
}

#[test]
fn open_file_url_bad_hex_rejected() {
    let reg = oxideav_source::with_defaults();
    let r = reg.open("file:///tmp/%ZZ");
    assert!(r.is_err());
}
