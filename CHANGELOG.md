# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- **Breaking**: Migrated to the new typed `SourceRegistry` API in
  `oxideav-core`. The `file://` driver now registers via
  `register_bytes("file", open_file)` (was `register("file", …)`),
  and `open_file` returns `Box<dyn BytesSource>` (was `Box<dyn
  ReadSeek>`). `BytesSource` is blanket-implemented for every
  `Read + Seek + Send` type so the underlying `File` shape is unchanged.
  `with_defaults()` / `register(&mut RuntimeContext)` keep their
  signatures; callers of `reg.open(uri)` now match a `SourceOutput`
  enum and bind the `Bytes` variant to get the reader back.
- Re-exports updated: `BytesSource`, `PacketSource`, `FrameSource`,
  `SourceOutput` are now surfaced from this crate alongside
  `SourceRegistry`. The `OpenSourceFn` re-export is removed.

## [0.1.2](https://github.com/OxideAV/oxideav-source/compare/v0.1.1...v0.1.2) - 2026-04-25

### Other

- drop "future" qualifier on http://
- release v0.1.1

## [0.1.1](https://github.com/OxideAV/oxideav-source/compare/v0.1.0...v0.1.1) - 2026-04-25

### Other

- re-export SourceRegistry from oxideav-core; expose register fn
- release v0.0.4

## [0.1.0](https://github.com/OxideAV/oxideav-source/compare/v0.0.3...v0.1.0) - 2026-04-19

### Other

- bump version to 0.1.0
- bump oxideav-container dep to "0.1"
- drop Cargo.lock — this crate is a library
