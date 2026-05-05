//! Source registry shim — `oxideav_core::SourceRegistry` plus the
//! built-in `file://` driver and a prefetching `BufferedSource`
//! wrapper.
//!
//! `SourceRegistry`, the typed source traits ([`BytesSource`],
//! [`PacketSource`], [`FrameSource`]), and the [`SourceOutput`] enum
//! live in `oxideav-core`. This crate retains the concrete `file://`
//! driver and the `BufferedSource` helper that `oxideav-http` and the
//! player rely on; it also exposes a [`with_defaults`] free function
//! that pre-populates a registry with the file driver, matching the
//! historical surface.
//!
//! ```no_run
//! let reg = oxideav_source::with_defaults();
//! let _input = reg.open("/tmp/video.mp4").unwrap();
//! ```

pub use oxideav_core::{
    BytesSource, FrameSource, PacketSource, ReadSeek, SourceOutput, SourceRegistry,
};

mod buffered;
mod file;
mod uri;

pub use buffered::BufferedSource;
pub use file::open_file;

/// Build a [`SourceRegistry`] pre-populated with the built-in `file`
/// driver. Bare paths (without a scheme) also dispatch to it via the
/// registry's fall-back behaviour.
pub fn with_defaults() -> SourceRegistry {
    let mut r = SourceRegistry::new();
    r.register_bytes("file", open_file);
    r
}

/// Install the `file://` (and bare-path) source driver into the given
/// runtime context. Idempotent — replacing a prior registration of the
/// `file` scheme.
pub fn register(ctx: &mut oxideav_core::RuntimeContext) {
    ctx.sources.register_bytes("file", open_file);
}

oxideav_core::register!("source", register);
