//! Shared `Read + Seek` conformance suite.
//!
//! Every bytes-shaped source this crate can produce — plain drivers
//! (`file://`, `mem://`, `data:`), composites (`slice:`, `concat:`,
//! nested combinations), the programmatic `SubSource` window, and the
//! threaded `BufferedSource` wrapper — must expose identical stream
//! semantics:
//!
//! * `read` at EOF returns `Ok(0)`, repeatedly (idempotent).
//! * A zero-sized output buffer reads `Ok(0)` at any position.
//! * `seek(Start(n))` past the end is permitted; a subsequent read
//!   returns 0 until the position is reduced (mirrors `io::Cursor`).
//! * A seek resolving before byte 0 errors AND leaves the position
//!   unchanged (the failed call must not corrupt the cursor).
//! * `SeekFrom::End` anchors at the stream length; `SeekFrom::Current`
//!   is exact signed arithmetic.
//! * `stream_position` starts at 0 and tracks reads exactly.
//! * Rewinding to 0 re-reads the identical byte sequence.
//!
//! The suite is a single battery run against a fresh source per
//! section, so a state leak in one section cannot mask a bug in the
//! next. Zero-length sources run a reduced battery (no tail sections).

use std::io::{Cursor, Read, Seek, SeekFrom, Write};

use oxideav_source::{mem, open_bytes, BufferedSource, BytesSource, SubSource};

fn temp_file(bytes: &[u8]) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let mut path = std::env::temp_dir();
    let pid = std::process::id();
    let n = N.fetch_add(1, Ordering::Relaxed);
    path.push(format!("oxideav-conformance-{pid}-{n}.bin"));
    let mut f = std::fs::File::create(&path).unwrap();
    f.write_all(bytes).unwrap();
    path
}

fn ramp(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i % 251) as u8).collect() // 251 prime: no period-256 aliasing
}

/// Percent-encode every byte (worst-case but always valid `data:` form).
fn percent_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 3);
    for b in bytes {
        s.push_str(&format!("%{b:02X}"));
    }
    s
}

/// Run the full conformance battery. `open` must yield a **fresh**
/// source positioned at 0 over exactly `payload`.
fn run_suite(label: &str, payload: &[u8], open: &dyn Fn() -> Box<dyn BytesSource>) {
    let len = payload.len() as u64;

    // ── 1. full read, then EOF is Ok(0) and idempotent ──
    let mut src = open();
    let mut all = Vec::new();
    src.read_to_end(&mut all).unwrap();
    assert_eq!(all, payload, "{label}: full read_to_end mismatch");
    let mut b4 = [0u8; 4];
    assert_eq!(src.read(&mut b4).unwrap(), 0, "{label}: read at EOF");
    assert_eq!(
        src.read(&mut b4).unwrap(),
        0,
        "{label}: read at EOF must stay 0 (idempotent)"
    );

    // ── 2. initial position is 0 ──
    let mut src = open();
    assert_eq!(
        src.stream_position().unwrap(),
        0,
        "{label}: fresh source must start at position 0"
    );

    // ── 3. zero-sized buffer reads Ok(0) at start, mid, and EOF ──
    let mut src = open();
    assert_eq!(src.read(&mut []).unwrap(), 0, "{label}: empty buf at start");
    if len >= 2 {
        src.seek(SeekFrom::Start(len / 2)).unwrap();
        assert_eq!(src.read(&mut []).unwrap(), 0, "{label}: empty buf mid");
    }
    src.seek(SeekFrom::End(0)).unwrap();
    assert_eq!(src.read(&mut []).unwrap(), 0, "{label}: empty buf at EOF");

    // ── 4. chunked reads reassemble the payload ──
    let mut src = open();
    let mut out = Vec::new();
    let mut chunk = [0u8; 3];
    loop {
        let n = src.read(&mut chunk).unwrap();
        if n == 0 {
            break;
        }
        out.extend_from_slice(&chunk[..n]);
    }
    assert_eq!(out, payload, "{label}: 3-byte chunked read mismatch");

    // ── 5. seek(Start(k)) + read_to_end == payload[k..] ──
    let mut anchors = vec![0, len / 3, len / 2];
    if len > 0 {
        anchors.push(len - 1);
    }
    anchors.push(len); // exact EOF
    for k in anchors {
        let mut src = open();
        let p = src.seek(SeekFrom::Start(k)).unwrap();
        assert_eq!(p, k, "{label}: seek(Start({k})) returned {p}");
        let mut tail = Vec::new();
        src.read_to_end(&mut tail).unwrap();
        assert_eq!(
            tail,
            &payload[k as usize..],
            "{label}: read_to_end after seek(Start({k}))"
        );
    }

    // ── 6. SeekFrom::End anchors at length ──
    let mut src = open();
    let end = src.seek(SeekFrom::End(0)).unwrap();
    assert_eq!(end, len, "{label}: seek(End(0)) must report length");
    let mut b1 = [0u8; 1];
    assert_eq!(src.read(&mut b1).unwrap(), 0, "{label}: read at End(0)");
    if len > 0 {
        let m = (len / 2).max(1);
        let mut src = open();
        let p = src.seek(SeekFrom::End(-(m as i64))).unwrap();
        assert_eq!(p, len - m, "{label}: seek(End(-{m}))");
        let mut tail = Vec::new();
        src.read_to_end(&mut tail).unwrap();
        assert_eq!(
            tail,
            &payload[(len - m) as usize..],
            "{label}: tail read after End(-{m})"
        );
    }

    // ── 7. SeekFrom::Current signed arithmetic ──
    if len >= 8 {
        let mut src = open();
        src.seek(SeekFrom::Start(4)).unwrap();
        let p = src.seek(SeekFrom::Current(3)).unwrap();
        assert_eq!(p, 7, "{label}: Current(+3) from 4");
        let p = src.seek(SeekFrom::Current(-5)).unwrap();
        assert_eq!(p, 2, "{label}: Current(-5) from 7");
        let p = src.seek(SeekFrom::Current(0)).unwrap();
        assert_eq!(p, 2, "{label}: Current(0) is a position query");
        let mut byte = [0u8; 1];
        src.read_exact(&mut byte).unwrap();
        assert_eq!(byte[0], payload[2], "{label}: read after Current chain");
    }

    // ── 8. seek past end tolerated; read 0; recoverable ──
    let mut src = open();
    let p = src.seek(SeekFrom::Start(len + 10)).unwrap();
    assert_eq!(p, len + 10, "{label}: seek past end must succeed");
    let mut b8 = [0u8; 8];
    assert_eq!(src.read(&mut b8).unwrap(), 0, "{label}: read past end");
    if len > 0 {
        src.seek(SeekFrom::Start(0)).unwrap();
        let mut byte = [0u8; 1];
        src.read_exact(&mut byte).unwrap();
        assert_eq!(byte[0], payload[0], "{label}: recover after past-end seek");
    }

    // ── 9. seek before zero errors and preserves the position ──
    if len >= 2 {
        let mut src = open();
        src.seek(SeekFrom::Start(1)).unwrap();
        assert!(
            src.seek(SeekFrom::Current(-2)).is_err(),
            "{label}: Current underflow must error"
        );
        // The failed seek must not have moved the cursor.
        let mut byte = [0u8; 1];
        src.read_exact(&mut byte).unwrap();
        assert_eq!(
            byte[0], payload[1],
            "{label}: failed seek must preserve position"
        );
    }
    let mut src = open();
    assert!(
        src.seek(SeekFrom::End(-(len as i64) - 1)).is_err(),
        "{label}: End underflow must error"
    );

    // ── 10. rewind and re-read the identical bytes ──
    let mut src = open();
    let mut first = Vec::new();
    src.read_to_end(&mut first).unwrap();
    src.seek(SeekFrom::Start(0)).unwrap();
    let mut second = Vec::new();
    src.read_to_end(&mut second).unwrap();
    assert_eq!(first, second, "{label}: rewind re-read mismatch");

    // ── 11. stream_position tracks reads exactly ──
    if len >= 5 {
        let mut src = open();
        let mut buf5 = [0u8; 5];
        src.read_exact(&mut buf5).unwrap();
        assert_eq!(
            src.stream_position().unwrap(),
            5,
            "{label}: position after 5-byte read"
        );
        assert_eq!(&buf5, &payload[..5], "{label}: first 5 bytes");
    }
}

const PAYLOAD_LEN: usize = 300;

// ───────────────────────── plain drivers ─────────────────────────

#[test]
fn file_driver_conformance() {
    let payload = ramp(PAYLOAD_LEN);
    let path = temp_file(&payload);
    let uri = format!("file://{}", path.display());
    run_suite("file", &payload, &|| open_bytes(&uri).unwrap());
    std::fs::remove_file(&path).ok();
}

#[test]
fn file_driver_empty_conformance() {
    let path = temp_file(b"");
    let uri = format!("file://{}", path.display());
    run_suite("file-empty", b"", &|| open_bytes(&uri).unwrap());
    std::fs::remove_file(&path).ok();
}

#[test]
fn mem_driver_conformance() {
    let payload = ramp(PAYLOAD_LEN);
    mem::put("conf-mem", payload.clone());
    run_suite("mem", &payload, &|| open_bytes("mem://conf-mem").unwrap());
    mem::remove("conf-mem");
}

#[test]
fn mem_driver_empty_conformance() {
    mem::put("conf-mem-empty", Vec::new());
    run_suite("mem-empty", b"", &|| {
        open_bytes("mem://conf-mem-empty").unwrap()
    });
    mem::remove("conf-mem-empty");
}

#[test]
fn data_percent_conformance() {
    let payload = ramp(PAYLOAD_LEN);
    let uri = format!("data:,{}", percent_encode(&payload));
    run_suite("data-percent", &payload, &|| open_bytes(&uri).unwrap());
}

#[test]
fn data_base64_conformance() {
    // "Hello, world!" in base64 keeps the fixture independent of any
    // encoder in this crate (the driver only decodes).
    let payload = b"Hello, world!";
    let uri = "data:;base64,SGVsbG8sIHdvcmxkIQ==";
    run_suite("data-base64", payload, &|| open_bytes(uri).unwrap());
}

#[test]
fn data_empty_conformance() {
    run_suite("data-empty", b"", &|| open_bytes("data:,").unwrap());
}

// ───────────────────────── composites ─────────────────────────

#[test]
fn slice_over_file_conformance() {
    // The window sits strictly inside the file: prefix and suffix bytes
    // must never leak into the view.
    let payload = ramp(PAYLOAD_LEN);
    let mut fixture = vec![0xAAu8; 64];
    fixture.extend_from_slice(&payload);
    fixture.extend(vec![0xBBu8; 64]);
    let path = temp_file(&fixture);
    let uri = format!("slice:64+{PAYLOAD_LEN}!file://{}", path.display());
    run_suite("slice-file", &payload, &|| open_bytes(&uri).unwrap());
    std::fs::remove_file(&path).ok();
}

#[test]
fn slice_zero_length_conformance() {
    mem::put("conf-slice-zero", ramp(32));
    run_suite("slice-zero", b"", &|| {
        open_bytes("slice:16+0!mem://conf-slice-zero").unwrap()
    });
    mem::remove("conf-slice-zero");
}

#[test]
fn nested_slice_conformance() {
    // outer [8, 8+300) of inner [50, 50+400) of a 500-byte buffer
    // == buffer[58..358].
    let buf = ramp(500);
    let payload = buf[58..358].to_vec();
    mem::put("conf-slice-nest", buf);
    run_suite("slice-nested", &payload, &|| {
        open_bytes("slice:8+300!slice:50+400!mem://conf-slice-nest").unwrap()
    });
    mem::remove("conf-slice-nest");
}

#[test]
fn concat_conformance() {
    // file + mem + data segments; boundaries at 100 and 200 exercise
    // cross-segment reads and seeks in every battery section.
    let payload = ramp(PAYLOAD_LEN);
    let path = temp_file(&payload[..100]);
    mem::put("conf-concat-mid", payload[100..200].to_vec());
    let uri = format!(
        "concat:file://{}|mem://conf-concat-mid|data:,{}",
        path.display(),
        percent_encode(&payload[200..])
    );
    run_suite("concat", &payload, &|| open_bytes(&uri).unwrap());
    mem::remove("conf-concat-mid");
    std::fs::remove_file(&path).ok();
}

#[test]
fn concat_with_empty_segment_conformance() {
    // A zero-length middle segment must be invisible to the battery.
    let payload = ramp(64);
    let empty = temp_file(b"");
    mem::put("conf-concat-empty-a", payload[..32].to_vec());
    mem::put("conf-concat-empty-b", payload[32..].to_vec());
    let uri = format!(
        "concat:mem://conf-concat-empty-a|file://{}|mem://conf-concat-empty-b",
        empty.display()
    );
    run_suite("concat-empty-mid", &payload, &|| open_bytes(&uri).unwrap());
    mem::remove("conf-concat-empty-a");
    mem::remove("conf-concat-empty-b");
    std::fs::remove_file(&empty).ok();
}

#[test]
fn slice_over_concat_conformance() {
    // Window [30, 30+120) over a 3×60-byte concatenation: the window
    // spans both internal boundaries (60 and 120).
    let buf = ramp(180);
    let payload = buf[30..150].to_vec();
    mem::put("conf-soc-a", buf[..60].to_vec());
    mem::put("conf-soc-b", buf[60..120].to_vec());
    mem::put("conf-soc-c", buf[120..].to_vec());
    let uri = "slice:30+120!concat:mem://conf-soc-a|mem://conf-soc-b|mem://conf-soc-c";
    run_suite("slice-over-concat", &payload, &|| open_bytes(uri).unwrap());
    for id in ["conf-soc-a", "conf-soc-b", "conf-soc-c"] {
        mem::remove(id);
    }
}

// ───────────────────────── programmatic wrappers ─────────────────────────

#[test]
fn sub_source_conformance() {
    let buf = ramp(500);
    let payload = buf[100..400].to_vec();
    run_suite("sub-source", &payload, &|| {
        let inner: Box<dyn BytesSource> = Box::new(Cursor::new(ramp(500)));
        Box::new(SubSource::new(inner, 100, 300).unwrap())
    });
    let _ = buf;
}

#[test]
fn buffered_source_conformance() {
    let payload = ramp(PAYLOAD_LEN);
    mem::put("conf-buffered", payload.clone());
    run_suite("buffered", &payload, &|| {
        let inner = open_bytes("mem://conf-buffered").unwrap();
        Box::new(BufferedSource::new(Box::new(inner), 64 * 1024).unwrap())
    });
    mem::remove("conf-buffered");
}

#[test]
fn buffered_source_empty_conformance() {
    mem::put("conf-buffered-empty", Vec::new());
    run_suite("buffered-empty", b"", &|| {
        let inner = open_bytes("mem://conf-buffered-empty").unwrap();
        Box::new(BufferedSource::new(Box::new(inner), 64 * 1024).unwrap())
    });
    mem::remove("conf-buffered-empty");
}

#[test]
fn buffered_source_streaming_conformance() {
    // Payload much larger than the ring so the battery exercises the
    // worker-refill and lookback-drop paths, not just the "everything
    // fits" case. 512 KiB payload vs 16 KiB ring (builder clamps
    // capacity to 4 × 4 KiB block minimum).
    let payload = ramp(512 * 1024);
    mem::put("conf-buffered-big", payload.clone());
    run_suite("buffered-streaming", &payload, &|| {
        let inner = open_bytes("mem://conf-buffered-big").unwrap();
        Box::new(
            BufferedSource::builder()
                .capacity(1) // clamped up to 4 × block
                .block_size(1) // clamped up to 4 KiB
                .build(Box::new(inner))
                .unwrap(),
        )
    });
    mem::remove("conf-buffered-big");
}
