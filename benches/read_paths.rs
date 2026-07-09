//! Hot-path benches: sequential read throughput per source shape,
//! composite boundary walking, prefetch-ring copy cost, back-seek ring
//! hits, and the URI decode paths (`%HH`, base64, `slice:` grammar).
//!
//! Self-contained — payloads are synthesised ramps; the only
//! filesystem touch is one tempfile for the file-backed rows.

use std::io::{Read, Seek, SeekFrom};

use criterion::{criterion_group, criterion_main, Criterion, Throughput};

use oxideav_source::{mem, open_bytes, parse_data_uri, parse_slice_uri, BufferedSource};

fn ramp(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i % 251) as u8).collect()
}

fn percent_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 3);
    for b in bytes {
        s.push_str(&format!("%{b:02X}"));
    }
    s
}

fn base64_encode(input: &[u8]) -> String {
    const ALPHA: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        out.push(ALPHA[(b0 >> 2) as usize] as char);
        out.push(ALPHA[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
        if chunk.len() > 1 {
            out.push(ALPHA[(((b1 & 0x0f) << 2) | (b2 >> 6)) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(ALPHA[(b2 & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

/// Drain a source to EOF through a fixed-size stack chunk; returns the
/// byte count so criterion can't optimise the loop away.
fn drain(src: &mut dyn Read) -> u64 {
    let mut chunk = [0u8; 4096];
    let mut total = 0u64;
    loop {
        let n = src.read(&mut chunk).unwrap();
        if n == 0 {
            return total;
        }
        total += n as u64;
    }
}

const SIZE: usize = 1024 * 1024; // 1 MiB payloads for the throughput rows

fn bench_sequential_reads(c: &mut Criterion) {
    let payload = ramp(SIZE);

    let mut g = c.benchmark_group("sequential_read");
    g.throughput(Throughput::Bytes(SIZE as u64));

    mem::put("bench-seq", payload.clone());
    g.bench_function("mem", |b| {
        b.iter(|| {
            let mut src = open_bytes("mem://bench-seq").unwrap();
            drain(&mut src)
        })
    });

    // Slice window covering the whole buffer minus the edges.
    g.bench_function("slice_over_mem", |b| {
        let uri = format!("slice:8+{}!mem://bench-seq", SIZE - 16);
        b.iter(|| {
            let mut src = open_bytes(&uri).unwrap();
            drain(&mut src)
        })
    });

    // 16 × 64 KiB segments: every read call near a boundary walks the
    // cumulative-offset table and re-seeks the segment.
    for (i, part) in payload.chunks(SIZE / 16).enumerate() {
        mem::put(format!("bench-cc-{i}"), part.to_vec());
    }
    let concat_uri = {
        let ids: Vec<String> = (0..16).map(|i| format!("mem://bench-cc-{i}")).collect();
        format!("concat:{}", ids.join("|"))
    };
    g.bench_function("concat_16_segments", |b| {
        b.iter(|| {
            let mut src = open_bytes(&concat_uri).unwrap();
            drain(&mut src)
        })
    });

    // Prefetch ring wrapper over the same bytes: measures the
    // worker-thread handoff + VecDeque two-slice copy against the
    // direct mem read above.
    g.bench_function("buffered_over_mem", |b| {
        b.iter(|| {
            let inner = open_bytes("mem://bench-seq").unwrap();
            let mut src = BufferedSource::new(Box::new(inner), 256 * 1024).unwrap();
            drain(&mut src)
        })
    });

    g.finish();
}

fn bench_seek_patterns(c: &mut Criterion) {
    let payload = ramp(SIZE);
    mem::put("bench-seek", payload);

    let mut g = c.benchmark_group("seek_patterns");

    // Short back-seek inside the ring's lookback region: must be
    // served from the ring (no worker restart).
    g.bench_function("buffered_lookback_hit", |b| {
        let inner = open_bytes("mem://bench-seek").unwrap();
        let mut src = BufferedSource::new(Box::new(inner), 256 * 1024).unwrap();
        // Warm the ring and move the cursor forward.
        let mut chunk = vec![0u8; 64 * 1024];
        src.read_exact(&mut chunk).unwrap();
        let mut probe = [0u8; 512];
        b.iter(|| {
            src.seek(SeekFrom::Current(-4096)).unwrap();
            src.read_exact(&mut probe).unwrap();
            src.seek(SeekFrom::Current(3584)).unwrap(); // net zero drift
            probe[0]
        })
    });

    // Alternating far seeks on the composite: every hop crosses
    // segments, exercising segment_for + the underlying re-seek.
    for (i, part) in ramp(SIZE).chunks(SIZE / 16).enumerate() {
        mem::put(format!("bench-skcc-{i}"), part.to_vec());
    }
    let concat_uri = {
        let ids: Vec<String> = (0..16).map(|i| format!("mem://bench-skcc-{i}")).collect();
        format!("concat:{}", ids.join("|"))
    };
    g.bench_function("concat_alternating_far_seeks", |b| {
        let mut src = open_bytes(&concat_uri).unwrap();
        let mut probe = [0u8; 64];
        let mut flip = false;
        b.iter(|| {
            let target = if flip { SIZE as u64 - 70_000 } else { 12_345 };
            flip = !flip;
            src.seek(SeekFrom::Start(target)).unwrap();
            src.read_exact(&mut probe).unwrap();
            probe[0]
        })
    });

    g.finish();
}

fn bench_uri_decoding(c: &mut Criterion) {
    let payload = ramp(64 * 1024);

    let mut g = c.benchmark_group("uri_decoding");
    g.throughput(Throughput::Bytes(payload.len() as u64));

    let percent_uri = format!("data:,{}", percent_encode(&payload));
    g.bench_function("data_percent_64k", |b| {
        b.iter(|| parse_data_uri(&percent_uri).unwrap().data.len())
    });

    let b64_uri = format!(
        "data:application/octet-stream;base64,{}",
        base64_encode(&payload)
    );
    g.bench_function("data_base64_64k", |b| {
        b.iter(|| parse_data_uri(&b64_uri).unwrap().data.len())
    });

    g.finish();

    // Grammar-only row: ops/sec on the slice header parse.
    c.bench_function("slice_uri_parse", |b| {
        b.iter(|| {
            parse_slice_uri("slice:1234567+89012345!file:///var/media/some/longish/path.mp4")
                .unwrap()
                .offset
        })
    });
}

criterion_group!(
    benches,
    bench_sequential_reads,
    bench_seek_patterns,
    bench_uri_decoding
);
criterion_main!(benches);
