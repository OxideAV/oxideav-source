# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- `data:[<mediatype>][;base64],<bytes>` scheme — RFC 2397 inline byte
  literals decoded directly from the URI (no filesystem access).
  `open_data` returns a `Cursor`-backed `BytesSource`; `parse_data_uri`
  surfaces the parsed `DataUri { mediatype, base64, data }` for callers
  that need to route on media type. `with_defaults()` and `register()`
  now install the `data` driver alongside `file` and `mem`. Percent
  decoding is default; the `;base64` marker (case-insensitive) switches
  to RFC 4648 §4 base64 with internal whitespace tolerated.
- `mem://<id>` scheme — in-memory buffer registry (`mem::put` / `mem::remove` / `mem::clear`) and `open_mem` opener. `with_defaults()` now installs both the `file` and `mem` drivers.
- `FileScope` — directory allow-list for the `file://` driver. Resolves
  requests through `std::fs::canonicalize` (defeats `../` traversal via
  symlinks), then rejects anything outside the canonicalised allow-list
  with component-aware prefix matching. Install with
  `FileScope::register_into(&mut SourceRegistry)`.

### Changed

- `BufferedSource::read` uses `VecDeque::as_slices` + `copy_from_slice`
  for the ring → out copy instead of an element-wise loop, so a
  million-byte hit no longer iterates byte-by-byte under the lock.
- `register()` now installs both `file` and `mem` drivers into the
  passed `RuntimeContext`.

## [0.1.4](https://github.com/OxideAV/oxideav-source/compare/v0.1.3...v0.1.4) - 2026-05-06

### Other

- reframe FFI claim — HW-engine crates use OS FFI by necessity
- drop dead `linkme` dep
- auto-register via oxideav_core::register! macro (linkme distributed slice)
- replace never-match regex with semver_check = false
- migrate to centralized OxideAV/.github reusable workflows

## [0.1.3](https://github.com/OxideAV/oxideav-source/compare/v0.1.2...v0.1.3) - 2026-05-02

### Other

- stay on 0.1.x during heavy dev (semver_check=false)
- Migrate file:// driver to SourceRegistry typed-bytes API
- pin release-plz to patch-only bumps

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
