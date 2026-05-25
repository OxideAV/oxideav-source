//! `SubSource` end-to-end: open a real file through `with_defaults`,
//! window it, and confirm the windowed reader behaves like an
//! independent `[0, len)` `Read + Seek` stream.

use std::io::{Read, Seek, SeekFrom, Write};

use oxideav_source::{with_defaults, SourceOutput, SubSource};

fn temp_file(bytes: &[u8]) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let mut path = std::env::temp_dir();
    let pid = std::process::id();
    let n = N.fetch_add(1, Ordering::Relaxed);
    path.push(format!("oxideav-source-subtest-{pid}-{n}.bin"));
    let mut f = std::fs::File::create(&path).unwrap();
    f.write_all(bytes).unwrap();
    f.flush().unwrap();
    path
}

#[test]
fn windowed_view_of_a_file_reads_correct_slice() {
    // Make a 256-byte ramp file and window bytes [64, 96).
    let data: Vec<u8> = (0u8..=255).collect();
    let p = temp_file(&data);
    let reg = with_defaults();
    let inner = match reg.open(&p.display().to_string()).unwrap() {
        SourceOutput::Bytes(b) => b,
        _ => panic!("expected Bytes"),
    };
    let mut sub = SubSource::new(inner, 64, 32).unwrap();
    let mut out = vec![0u8; 32];
    sub.read_exact(&mut out).unwrap();
    let expected: Vec<u8> = (64u8..96).collect();
    assert_eq!(out, expected);
    std::fs::remove_file(p).ok();
}

#[test]
fn windowed_view_seek_back_then_re_read() {
    let data: Vec<u8> = (0u8..=255).collect();
    let p = temp_file(&data);
    let reg = with_defaults();
    let inner = match reg.open(&p.display().to_string()).unwrap() {
        SourceOutput::Bytes(b) => b,
        _ => panic!("expected Bytes"),
    };
    let mut sub = SubSource::new(inner, 100, 20).unwrap();
    // Read first 5 bytes.
    let mut first = [0u8; 5];
    sub.read_exact(&mut first).unwrap();
    assert_eq!(first, [100, 101, 102, 103, 104]);
    // Seek back to window-start, re-read.
    sub.seek(SeekFrom::Start(0)).unwrap();
    let mut again = [0u8; 5];
    sub.read_exact(&mut again).unwrap();
    assert_eq!(again, [100, 101, 102, 103, 104]);
    std::fs::remove_file(p).ok();
}

#[test]
fn window_extending_past_end_rejected_on_file() {
    let data: Vec<u8> = vec![0u8; 64];
    let p = temp_file(&data);
    let reg = with_defaults();
    let inner = match reg.open(&p.display().to_string()).unwrap() {
        SourceOutput::Bytes(b) => b,
        _ => panic!("expected Bytes"),
    };
    let r = SubSource::new(inner, 50, 50); // 100 > 64
    assert!(r.is_err());
    std::fs::remove_file(p).ok();
}
