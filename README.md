# oxideav-source

Generic source registry: opens URIs (file://, plus http:// via [oxideav-http](https://github.com/OxideAV/oxideav-http)) into Read+Seek; prefetch buffer wrapper.

The registry now returns `SourceOutput` — one of `Bytes` (the file/http
case here), `Packets`, or `Frames` — so transport-layer or generator
sources slot into the same opener API. The `file://` driver registers
as a `BytesSource` and the `with_defaults()` helper continues to
return a registry pre-populated with it.

Part of the [oxideav](https://github.com/OxideAV/oxideav-workspace) framework — a
100% pure Rust media transcoding and streaming stack. No C libraries, no FFI
wrappers, no `*-sys` crates.

## Usage

```toml
[dependencies]
oxideav-source = "0.0"
```

## License

MIT — see [LICENSE](LICENSE).
