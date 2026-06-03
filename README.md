# oxideav-source

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
| `concat:<a>\|<b>\|…` | `open_concat` | `\|`-separated segments presented as one seekable byte stream; reads walk segment boundaries, `Seek` resolves an absolute offset into the right segment. Each segment may be a bare path, `file://`, `mem://`, `data:`, or `slice:` URI (same set the `slice:` driver accepts as inner). Nested `concat:` segments rejected (the outer `\|` split would shred them); empty segments rejected. |
| `slice:<offset>+<length>!<inner-uri>` | `open_slice` | URI-level windowed view: `[offset, offset + length)` of `<inner-uri>` mapped onto `[0, length)`. The inner URI may be a `file://` / bare path, `mem://`, `data:`, or another `slice:` (recursive composition). Equivalent to constructing a `SubSource` programmatically, but expressible as a single URI string for CLI flags and config files. |
| `http://`, `https://` | provided by [oxideav-http](https://github.com/OxideAV/oxideav-http) | registered separately by that crate |

`with_defaults()` pre-populates a registry with the `file`, `mem`,
`data`, `concat`, and `slice` drivers (the `file` opener in its unscoped
form).
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
