# oxideav-source

Generic source registry: opens URIs (file://, plus http:// via [oxideav-http](https://github.com/OxideAV/oxideav-http)) into Read+Seek; prefetch buffer wrapper.

The registry now returns `SourceOutput` — one of `Bytes` (the file/http
case here), `Packets`, or `Frames` — so transport-layer or generator
sources slot into the same opener API. The `file://` driver registers
as a `BytesSource` and the `with_defaults()` helper continues to
return a registry pre-populated with it.

Part of the [oxideav](https://github.com/OxideAV/oxideav-workspace) framework — a pure-Rust media transcoding and streaming stack. Codec, container, and filter crates are implemented from the spec (no C codec libraries linked or wrapped, no `*-sys` crates). Optional hardware-engine crates (`oxideav-videotoolbox` / `-audiotoolbox` / `-vaapi` / `-vdpau` / `-nvidia` / `-vulkan-video`) bridge to OS APIs via runtime `libloading`; pass `--no-hwaccel` (or omit the `hwaccel` feature) to opt out.

## Usage

```toml
[dependencies]
oxideav-source = "0.0"
```

## License

MIT — see [LICENSE](LICENSE).
