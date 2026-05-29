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
//!   over an inner `file://` / `mem://` / `data:` / `slice:` source.
//!   Pipelines can address a byte-range sub-stream without first
//!   materialising the inner source.

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

pub use buffered::BufferedSource;
pub use concat::open_concat;
pub use data::{open_data, parse as parse_data_uri, DataUri};
pub use file::open_file;
pub use mem::open_mem;
pub use scope::{open_file_scoped, FileScope};
pub use slice::open_slice;
pub use sub::{stream_len, SubSource};

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
