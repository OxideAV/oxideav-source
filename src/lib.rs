//! Source registry shim — `oxideav_core::SourceRegistry` plus the
//! built-in `file://`, `mem://`, `data:`, and `concat:` drivers and a
//! prefetching `BufferedSource` wrapper.
//!
//! `SourceRegistry`, the typed source traits ([`BytesSource`],
//! [`PacketSource`], [`FrameSource`]), and the [`SourceOutput`] enum
//! live in `oxideav-core`. This crate retains the concrete drivers and
//! the `BufferedSource` helper that `oxideav-http` and the player rely
//! on; it also exposes a [`with_defaults`] free function that
//! pre-populates a registry with the bundled drivers, matching the
//! historical surface.
//!
//! ```no_run
//! let reg = oxideav_source::with_defaults();
//! let _input = reg.open("/tmp/video.mp4").unwrap();
//! ```
//!
//! ## Schemes
//!
//! - **`file://<path>`** and bare paths — local filesystem. Default
//!   opener is unscoped; install a [`FileScope`] via
//!   [`FileScope::register_into`] to restrict by directory allow-list.
//! - **`mem://<id>`** — process-global in-memory buffer; register a
//!   payload with [`mem::put`].
//! - **`data:[<mediatype>][;base64],<bytes>`** — RFC 2397 inline byte
//!   literals; payload is decoded directly from the URI with no
//!   filesystem access.
//! - **`concat:<a>|<b>|…`** — concatenate several `file://` segments into
//!   one seekable byte stream (de-facto `concat:` shape; no on-wire
//!   spec).
//! - **`slice:<offset>+<length>!<inner-uri>`** — URI-level windowed view
//!   over an inner `file://` / `mem://` / `data:` / `slice:` / `concat:`
//!   source. Pipelines can address a byte-range sub-stream without first
//!   materialising the inner source.
//!
//! All five schemes are also reachable without a registry through the
//! [`open_bytes`] free function, which is the same dispatch surface the
//! `slice:` and `concat:` drivers use to resolve their inner / segment
//! URIs.

pub use oxideav_core::{
    BytesSource, FrameSource, PacketSource, ReadSeek, SourceOutput, SourceRegistry,
};

mod buffered;
mod concat;
pub mod data;
mod file;
pub mod mem;
mod scope;
mod slice;
mod sub;
mod uri;

pub use buffered::{
    BufferedSource, BufferedSourceBuilder, DEFAULT_BLOCK, DEFAULT_LOOKBACK_DEN,
    DEFAULT_LOOKBACK_NUM, DEFAULT_PREFETCH_TIMEOUT,
};
pub use concat::open_concat;
pub use data::{open_data, parse as parse_data_uri, DataUri};
pub use file::open_file;
pub use mem::open_mem;
pub use scope::{open_file_scoped, FileScope};
pub use slice::{open_slice, parse as parse_slice_uri, SliceUri};
pub use sub::{stream_len, SubSource};

/// Open a URI with the bundled in-process drivers, without constructing
/// a [`SourceRegistry`]. Dispatches on the URI scheme
/// (case-insensitively, per RFC 3986 §3.1):
///
/// - `file://` and bare paths → [`open_file`]
/// - `mem://<id>` → [`open_mem`]
/// - `data:…` → [`open_data`]
/// - `slice:…` → [`open_slice`]
/// - `concat:…` → [`open_concat`]
///
/// Unknown schemes error. This is the free-function analogue of
/// `with_defaults().open(uri)` for callers that only ever want a
/// byte-shaped source: it returns the `Box<dyn BytesSource>` directly
/// instead of a [`SourceOutput`] to match on, and it is the same
/// dispatch surface the `slice:` and `concat:` drivers use to resolve
/// their inner / segment URIs.
pub fn open_bytes(uri_str: &str) -> oxideav_core::Result<Box<dyn BytesSource>> {
    let (scheme, _) = uri::split(uri_str);
    if uri::scheme_is(scheme, "file") {
        open_file(uri_str)
    } else if uri::scheme_is(scheme, "mem") {
        open_mem(uri_str)
    } else if uri::scheme_is(scheme, "data") {
        open_data(uri_str)
    } else if uri::scheme_is(scheme, "slice") {
        open_slice(uri_str)
    } else if uri::scheme_is(scheme, "concat") {
        open_concat(uri_str)
    } else {
        Err(oxideav_core::Error::invalid(format!(
            "no bundled in-process driver for scheme {scheme:?} (URI: {uri_str}); \
             only file/mem/data/slice/concat are dispatchable without a registry"
        )))
    }
}

/// Build a [`SourceRegistry`] pre-populated with the built-in `file`,
/// `mem`, `data`, `concat`, and `slice` drivers. Bare paths (without a
/// scheme) dispatch to the `file` driver via the registry's fall-back
/// behaviour.
pub fn with_defaults() -> SourceRegistry {
    let mut r = SourceRegistry::new();
    r.register_bytes("file", open_file);
    r.register_bytes("mem", open_mem);
    r.register_bytes("data", open_data);
    r.register_bytes("concat", open_concat);
    r.register_bytes("slice", open_slice);
    r
}

/// Install the bundled source drivers (`file://`, bare paths, `mem://`,
/// `data:`, `concat:`, `slice:`) into the given runtime context.
/// Idempotent — replacing any prior registration of those schemes.
pub fn register(ctx: &mut oxideav_core::RuntimeContext) {
    ctx.sources.register_bytes("file", open_file);
    ctx.sources.register_bytes("mem", open_mem);
    ctx.sources.register_bytes("data", open_data);
    ctx.sources.register_bytes("concat", open_concat);
    ctx.sources.register_bytes("slice", open_slice);
}

oxideav_core::register!("source", register);
