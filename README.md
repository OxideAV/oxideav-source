# oxideav-source

[![CI](https://github.com/OxideAV/oxideav-source/actions/workflows/ci.yml/badge.svg)](https://github.com/OxideAV/oxideav-source/actions/workflows/ci.yml) [![crates.io](https://img.shields.io/crates/v/oxideav-source.svg)](https://crates.io/crates/oxideav-source) [![docs.rs](https://docs.rs/oxideav-source/badge.svg)](https://docs.rs/oxideav-source) [![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

Generic source registry: opens URIs into `Read + Seek` byte streams,
with bundled drivers and a prefetching `BufferedSource` wrapper.

The registry returns `SourceOutput` — one of `Bytes` (file / mem / http
here), `Packets`, or `Frames` — so transport-layer or generator sources
slot into the same opener API.

## Bundled schemes

| Scheme | Driver | Notes |
| --- | --- | --- |
| `file://<path>` and bare paths | `open_file` | unscoped — every readable path resolves. `file:` / `file://` inputs are **percent-decoded** per RFC 3986 §2.1 (so `file:///tmp/foo%20bar.txt` opens `/tmp/foo bar.txt`); bare paths are passed verbatim so a real file with `%` in its name still opens |
| `file://<path>` (scoped) | `FileScope` + `open_file_scoped` | restricts opens to a canonicalised directory allow-list, with optional `deny_dir` carve-outs that override allow-list matches; blocks `..` traversals through symlinks; same RFC 3986 §2.1 percent-decoding for URI-form inputs |
| `mem://<id>` | `open_mem` | in-memory buffer registered via `oxideav_source::mem::put(id, bytes)`; useful for tests and synthetic sources |
| `data:[<mediatype>][;base64],<bytes>` | `open_data` | RFC 2397 inline byte literals; payload decoded directly from the URI (no filesystem access). Percent-decoded by default; base64 when `;base64` is present. |
| `concat:<a>\|<b>\|…` | `open_concat` | `\|`-separated segments presented as one seekable byte stream; reads walk segment boundaries, `Seek` resolves an absolute offset into the right segment. Each segment may be a bare path, `file://`, `mem://`, `data:`, or `slice:` URI. Nested `concat:` segments rejected (the outer `\|` split would shred them); empty segments rejected. |
| `slice:<offset>+<length>!<inner-uri>` | `open_slice` | URI-level windowed view: `[offset, offset + length)` of `<inner-uri>` mapped onto `[0, length)`. The inner URI may be a `file://` / bare path, `mem://`, `data:`, another `slice:` (recursive composition), or a `concat:` (a window over a concatenation — unambiguous because the slice grammar splits on `!`, never on `\|`). `offset`/`length` are canonical decimals (no sign, no leading zeros). Equivalent to constructing a `SubSource` programmatically, but expressible as a single URI string for CLI flags and config files. |
| `http://`, `https://` | provided by [oxideav-http](https://github.com/OxideAV/oxideav-http) | registered separately by that crate |

Scheme names are matched **case-insensitively** per RFC 3986 §3.1
(`FILE:///x`, `MEM://id`, `DATA:,…` all open); the typed formatters
emit the canonical lowercase form.

`with_defaults()` pre-populates a registry with the `file`, `mem`,
`data`, `concat`, and `slice` drivers (the `file` opener in its unscoped
form). The same five schemes are reachable without a registry through
the `open_bytes(uri)` free function, which returns the
`Box<dyn BytesSource>` directly and is the same dispatch surface the
`slice:` / `concat:` drivers use for their inner and segment URIs.
For server-side use, build an empty registry and install a `FileScope`
instead:

```rust,no_run
use oxideav_source::{FileScope, SourceRegistry};

let mut reg = SourceRegistry::new();
FileScope::new()
    .allow_dir("/var/media")
    .allow_dir("/srv/uploads")
    .register_into(&mut reg);
// reg.open("file:///etc/passwd") now errors instead of leaking.
```

`deny_dir` carves a hole out of an allow-listed root — useful when a
broad root is admitted but a subtree must stay private:

```rust,no_run
use oxideav_source::{FileScope, SourceRegistry};

let mut reg = SourceRegistry::new();
FileScope::new()
    .allow_dir("/var/media")
    .deny_dir("/var/media/.snapshots") // blocked even though under allow root
    .register_into(&mut reg);
```

Deny entries override allow matches and also override
`FileScope::permissive()`: `permissive().deny_dir("/etc")` reads
"everything except `/etc/**`". The canonicalisation step is shared, so
a `..` path that resolves into a deny-listed subtree is blocked.

## BufferedSource

`BufferedSource` wraps any `Box<dyn ReadSeek>` (HTTP, file, mem) with a
worker-thread prefetch ring. Backwards seeks inside the ring window are
handled without re-reading the inner source; seeks past the ring restart
prefetch at the new position.

`BufferedSource::new(inner, capacity)` keeps the historical
two-argument shape with default tunables. For finer control —
non-default prefetch timeout, custom worker block size, or a different
lookback fraction — use `BufferedSource::builder()`:

```rust,no_run
use std::time::Duration;
use oxideav_source::BufferedSource;
# fn make_inner() -> Box<dyn oxideav_core::ReadSeek> { unimplemented!() }
let inner = make_inner();
let buf = BufferedSource::builder()
    .capacity(4 * 1024 * 1024)            // 4 MiB ring
    .block_size(64 * 1024)                // 64 KiB worker syscalls
    .prefetch_timeout(Duration::from_secs(5))
    .lookback_fraction(1, 4)              // 25 % back-cache
    .build(inner)
    .unwrap();
# let _ = buf;
```

Defaults are 1 MiB capacity, 256 KiB block size, 30 s prefetch timeout,
1/8 lookback. Builder values are clamped on `build` so the worker is
always able to make forward progress (capacity ≥ 4 × block, block ≥
4 KiB, timeout ≥ 1 ms, lookback strictly less than 1).

Failure semantics: a fatal error from the inner source is **sticky** —
already-prefetched bytes (including lookback-window back-seeks) stay
readable, and once the ring is exhausted every read re-surfaces the
original error `(kind, message)` immediately instead of waiting out the
prefetch timeout. `ErrorKind::Interrupted` from the inner source is
retried per the std `Read` convention, not treated as fatal.

## Typed URI values — `DataUri`, `SliceUri`, `ConcatUri`

Each composable scheme has a public typed view so URIs can be built and
inspected without string-formatting. `slice:` example:

```rust,no_run
use oxideav_source::{parse_slice_uri, SliceUri};

// Build from components — validates inner up-front.
let s = SliceUri::new(4_321_000, 34_112, "file:///some/container.mp4").unwrap();
assert_eq!(s.format(), "slice:4321000+34112!file:///some/container.mp4");

// Parse without opening: useful for CLI flag inspection.
let parsed = parse_slice_uri("slice:0+1024!mem://probe").unwrap();
assert_eq!(parsed.offset, 0);
assert_eq!(parsed.length, 1024);
assert_eq!(parsed.inner, "mem://probe");
```

A typed `SliceUri` opens directly via `SliceUri::open`, the slice-scheme
analogue of `DataUri` → `open_data` — no need to format back to a string
and re-parse:

```rust,no_run
use oxideav_source::SliceUri;

let s = SliceUri::new(8, 16, "mem://probe").unwrap();
let _reader = s.open().unwrap(); // == open_slice(&s.format())
```

`open_slice(uri_str)` is itself `parse_slice_uri(uri_str)?.open()`, so the
string and typed entry points share one open path and stay in lock-step.
`concat:` has the same triad: `parse_concat_uri(uri)` →
`ConcatUri { segments }`, `ConcatUri::new(segments)` /
`format()` / `open()`, with `open_concat` defined as
`parse(uri)?.open()`.

`parse → format` is byte-identical for every canonical URI the parser
accepts (equivalent `SLICE:` / `slice://` spellings normalise to the
canonical form; `parse(format(x)) == x` always). The constructors
reject only what breaks the grammar round-trip: `SliceUri::new` an
empty inner or an inner containing a literal `!`; `ConcatUri::new` an
empty list, an empty segment, or a segment containing `|`. Scheme
validity stays an open-time concern.

## SubSource — windowed view

`SubSource` re-projects a slice `[base, base + len)` of an inner
`BytesSource` onto `[0, len)` so containers can hand a codec a stream
that looks like the codec's own sample. This is the seekable analogue
of `std::io::Read::take`: `take` only caps forward reads, but a codec
that needs to seek backwards within its window — e.g. to re-read a
header it just probed — needs a real windowed seek too. The
[`stream_len`] helper probes a source's total length non-destructively
(useful at `SubSource::new`-time and anywhere else a length probe is
needed without disturbing the cursor).

```rust,no_run
use oxideav_source::{with_defaults, SourceOutput, SubSource};

let reg = with_defaults();
let inner = match reg.open("/some/container.mp4").unwrap() {
    SourceOutput::Bytes(b) => b,
    _ => panic!("expected Bytes"),
};
// Hand the codec just the mdat sample at offset 4_321_000, length 34_112.
let mut sample = SubSource::new(inner, 4_321_000, 34_112).unwrap();
// `sample` now behaves like a `Read + Seek` source over [0, 34_112).
# let _ = &mut sample;
```

## Testing & benchmarks

Beyond per-driver unit/integration tests, three cross-cutting suites
pin the I/O contract:

- **Conformance battery** (`tests/conformance.rs`) — one behavioural
  suite (EOF idempotence, zero-sized-buffer reads, seek-past-end
  tolerance + recovery, seek-underflow errors that preserve the
  cursor, `SeekFrom` arithmetic, rewind re-read, exact position
  tracking) run against 17 source shapes, from plain drivers to
  slice-over-concat composites and the streaming `BufferedSource`.
- **Model differential** (`tests/model_differential.rs`) — fixed-seed
  random op streams driven in lockstep against a `Cursor<Vec<u8>>`
  model; positions, bytes, and seek ok-ness must agree after every op
  on 8 shapes.
- **Hostile input** (`tests/hostile_input.rs`) — separator-biased byte
  soup (NUL, multi-byte UTF-8, RTL override) against the parsers: no
  panics, canonical round-trip, parse-format-parse fixpoint,
  500-deep nested-slice recursion, truncated percent escapes.

`cargo bench --bench read_paths` measures the hot paths — numbers in
[BENCHMARKS.md](BENCHMARKS.md) (mem ~59 GiB/s sequential; slice /
16-segment concat within 4–6 %; prefetch ring ~9.8 GiB/s; `%HH` decode
~720 MiB/s; base64 ~428 MiB/s; slice grammar parse ~55 ns).

## Status

Part of the [oxideav](https://github.com/OxideAV/oxideav-workspace)
framework — a pure-Rust media transcoding and streaming stack. Codec,
container, and filter crates are implemented from the spec (no C codec
libraries linked or wrapped, no `*-sys` crates). Optional
hardware-engine crates (`oxideav-videotoolbox` / `-audiotoolbox` /
`-vaapi` / `-vdpau` / `-nvidia` / `-vulkan-video`) bridge to OS APIs via
runtime `libloading`; pass `--no-hwaccel` (or omit the `hwaccel`
feature) to opt out.

## Usage

```toml
[dependencies]
oxideav-source = "0.1"
```

## License

MIT — see [LICENSE](LICENSE).
