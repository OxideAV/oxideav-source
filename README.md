# oxideav-source

Generic source registry: opens URIs into `Read + Seek` byte streams,
with bundled drivers and a prefetching `BufferedSource` wrapper.

The registry returns `SourceOutput` — one of `Bytes` (file / mem / http
here), `Packets`, or `Frames` — so transport-layer or generator sources
slot into the same opener API.

## Bundled schemes

| Scheme | Driver | Notes |
| --- | --- | --- |
| `file://<path>` and bare paths | `open_file` | unscoped — every readable path resolves |
| `file://<path>` (scoped) | `FileScope` + `open_file_scoped` | restricts opens to a canonicalised directory allow-list; blocks `..` traversals through symlinks |
| `mem://<id>` | `open_mem` | in-memory buffer registered via `oxideav_source::mem::put(id, bytes)`; useful for tests and synthetic sources |
| `data:[<mediatype>][;base64],<bytes>` | `open_data` | RFC 2397 inline byte literals; payload decoded directly from the URI (no filesystem access). Percent-decoded by default; base64 when `;base64` is present. |
| `concat:<a>\|<b>\|…` | `open_concat` | `\|`-separated `file://` segments presented as one seekable byte stream; reads walk segment boundaries, `Seek` resolves an absolute offset into the right segment. Empty segments rejected. |
| `http://`, `https://` | provided by [oxideav-http](https://github.com/OxideAV/oxideav-http) | registered separately by that crate |

`with_defaults()` pre-populates a registry with the `file`, `mem`,
`data`, and `concat` drivers (the `file` opener in its unscoped form).
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

## BufferedSource

`BufferedSource` wraps any `Box<dyn ReadSeek>` (HTTP, file, mem) with a
worker-thread prefetch ring. Backwards seeks inside the ring window are
handled without re-reading the inner source; seeks past the ring restart
prefetch at the new position. Reader-side reads block on the worker for
at most 30 s before surfacing `TimedOut`.

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
