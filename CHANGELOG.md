# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Fixed

- `concat:` URIs whose **first segment starts with `//`** are now
  rejected (fuzz-found). `concat:////a|b` survived the scheme
  splitter's single authority-style `//` strip with a first segment of
  `//a` — which `format()` re-emits as `concat://a|b`, and the next
  parse strips two MORE slashes, so every `parse` → `format` → `parse`
  cycle silently ate two leading slashes and the documented fixpoint
  `parse(format(x)) == x` was violated. Both `parse_concat_uri` and
  `ConcatUri::new` now reject the ambiguous form with a precise error
  (mirroring the existing `|`-in-segment rule). One or two leading
  slashes are unaffected: `concat:///a|b` is the authority-style
  spelling of first segment `/a` and still normalises; segments after
  the first may carry any number of leading slashes.
- `BufferedSource` reader/worker **deadlock on a completely full
  ring** (fuzz-found): an in-window seek to the ring end performs no
  ring maintenance, so with the ring at capacity the worker was parked
  on `not_full`, the subsequent read missed the ring and parked on
  `not_empty`, and neither side could ever progress — surfacing after
  `prefetch_timeout` (default 30 s) as a bogus `TimedOut` on a
  perfectly healthy source. Two changes in the read-miss path: a known
  total length now yields an immediate EOF verdict (the worker may
  never get to read the final 0 that sets its `eof` flag while parked
  on a full ring), and a full ring that sits entirely behind the
  reader is drained down to the lookback allowance (the same retention
  rule the hit path applies) so the worker wakes and refills toward
  the reader's position.

- `BufferedSource` worker errors are now **sticky** and ordered after
  ring data. Previously the error was `take`n and surfaced exactly
  once; every subsequent read missed the ring, slept on the prefetch
  condvar, and after `prefetch_timeout` (default 30 s) reported a
  misleading `TimedOut` — and because the worker thread had exited, a
  seek-restart could never recover. Now the failure is stored as
  `(kind, message)` and re-surfaced on every read; prefetched bytes
  remain readable first (including lookback-window back-seeks) with
  the error surfacing only once the ring is exhausted; and a seek
  outside the ring window keeps the sticky error when the worker is
  dead (a live worker still gets its error cleared for the refill).
- `BufferedSource` worker retries `ErrorKind::Interrupted` from the
  inner source per the std `Read` convention instead of treating it as
  fatal and killing the prefetch stream.
- `slice:` URI parser enforces the canonical decimal grammar for
  `offset` and `length`: ASCII digits only, no sign, no leading zeros
  (`"0"` alone stays valid). Previously the tokens went through
  `str::parse::<u64>()`, which admits a leading `+` and zero-padding,
  so `slice:1++2!…` and `slice:007+010!…` were accepted — and then
  violated the documented invariant that `parse` → `format` reproduces
  a byte-identical URI for every accepted input (`format` always emits
  the canonical digit string). Non-canonical forms now fail with a
  precise error; overflow (`> u64::MAX`) and non-ASCII digits are
  rejected as before.

- Scheme matching in every bundled driver is now case-insensitive per
  RFC 3986 §3.1. `reg.open("MEM://id")` (and `FILE:`, `DATA:`, `SLICE:`,
  `CONCAT:` in any case mix) previously failed with "driver invoked on
  non-<scheme> URI": the registry normalises the scheme to lowercase
  for dispatch, but each driver re-split the URI and compared the
  scheme case-sensitively, rejecting URIs the registry had legitimately
  routed to it. The fix also covers `FileScope::resolve` and the
  inner-URI / segment dispatch inside `slice:` and `concat:`, so
  `SLICE:2+3!MEM://id` and `CONCAT:DATA:,a|MEM://b` now open. The
  wrong-driver guard is unchanged — `open_mem("FILE:///x")` still
  errors. New helper `uri::scheme_is` (crate-internal surface) keeps
  the driver-level re-check aligned with registry dispatch.

### Changed

- Error taxonomy aligned with the core `Error` contract (was:
  everything `InvalidData`). A well-formed `mem://` URI whose buffer
  isn't registered now returns `Io(NotFound)` — the same shape as a
  missing file; `open_bytes` on a scheme it has no driver for returns
  `Unsupported`, matching the registry's own miss variant;
  `FileScope` policy rejections (allow-list miss, deny-list hit)
  return `Io(PermissionDenied)`, and a canonicalisation failure keeps
  the underlying IO kind. Malformed URIs (bad grammar, escapes,
  base64, out-of-bounds slice windows) stay `InvalidData`. Composite
  drivers propagate the inner taxonomy unchanged
  (`tests/error_taxonomy.rs` pins all of this).

### Added

- `fuzz/` — libFuzzer harness (cargo-fuzz layout) with three targets:
  `uri_parse` (the grammar layer on raw attacker bytes: no panic,
  `parse(format(x)) == x` fixpoint, canonical byte-identical
  round-trip, typed-constructor agreement, `data:` open/parse byte
  agreement), `compose_open` (fuzzer-built nested
  `slice:`/`concat:`/`data:` compositions — in-memory only — opened
  and driven with random read/seek op streams in lockstep with a
  `Cursor<Vec<u8>>` model, plus out-of-range-window rejection probes),
  and `buffered_model` (fuzzer-chosen capacity / block-size / lookback
  tunables, payloads larger than any clampable ring, random op stream
  against the model). First campaigns found the two fixes above
  (`concat:` leading-`//` round-trip breakage and the `BufferedSource`
  full-ring deadlock); post-fix campaigns are clean (30.8 M / 4.5 M /
  0.5 M runs respectively in 5-minute bounded runs).
- `FileScope` symlink-escape hardening suite
  (`tests/scope.rs::symlink_escapes`, Unix): pins the
  canonicalise-first contract with real symlinks — a link inside the
  allow root pointing at an outside file (or a whole symlinked
  directory) is rejected on its physical target; `..` applied *after*
  symlink resolution (`root/link/../bait`) follows the physical route
  out of the root and is rejected; a symlink aliasing a deny-listed
  file under an innocent name is rejected; a link that lives inside a
  denied subtree but points at a public file is admitted (deny
  verdicts are on the physical bytes, not the namespace — pinned as
  documented behaviour); percent-encoded traversals (`%2e%2e`,
  `%2F`-smuggled separators) decode per RFC 3986 §2.1 before
  canonicalisation and are rejected; plus an end-to-end
  registry-pipeline (`register_into` + `reg.open`) escape check.
- `ConcatUri` — public typed view of a parsed `concat:` URI,
  completing the typed-URI triad next to `DataUri` and `SliceUri`.
  `parse_concat_uri(uri)` returns
  `ConcatUri { segments: Vec<String> }` without opening any segment;
  `ConcatUri::new(segments)` validates grammar safety up-front (≥ 1
  segment, none empty, no literal `|` — the value could not
  round-trip); `format()` / `Display` emit the canonical
  `concat:<a>|<b>` form; `open()` is the typed analogue of
  `open_concat`, which is now defined as `parse(uri)?.open()` so the
  string and typed entry points share one open path. Segment scheme
  validity (including the nested-`concat:` rejection) stays an
  open-time concern, mirroring `SliceUri`. Round-trip contract matches
  the slice one: canonical inputs are byte-identical, `CONCAT:` /
  `concat://` spellings normalise, `parse(format(x)) == x` always.
- `open_bytes(uri)` — registry-free dispatch over the bundled
  in-process drivers (`file://` / bare paths, `mem://`, `data:`,
  `slice:`, `concat:`), returning `Box<dyn BytesSource>` directly.
  This is also now the single dispatch surface `slice:` and `concat:`
  use to resolve their inner / segment URIs (previously two parallel
  hand-rolled match blocks), so the three surfaces cannot drift.
  Driver errors pass through unchanged — a missing file stays an IO
  error rather than being rewrapped.
- `slice:` accepts a `concat:` inner URI:
  `slice:2+4!concat:data:,AB|data:,CDEF` opens the window `[2, 6)` of
  the concatenation. Unambiguous in this direction because the slice
  grammar splits on `!`, never on the concat segment separator `|`
  (the reverse — `concat:` nesting another `concat:` segment — remains
  rejected).
- `data:` percent-decoding now shares the RFC 3986 §2.1 decoder with
  the `file://` path decoding (one implementation instead of two
  byte-for-byte identical copies); behaviour is unchanged.
- Criterion benches on the hot read paths (`benches/read_paths.rs`,
  results in `BENCHMARKS.md`): sequential-read throughput per shape
  (mem ~59 GiB/s; slice / 16-segment concat within 4–6 % of it;
  BufferedSource ring ~9.8 GiB/s), BufferedSource lookback-region
  back-seek hit (~21 ns), concat cross-segment far seeks (~10 ns),
  and the URI decode paths (`%HH` ~720 MiB/s, base64 ~428 MiB/s,
  `parse_slice_uri` ~55 ns/op).
- Randomised model-based differential tests
  (`tests/model_differential.rs`): every source shape (mem, data,
  slice, concat, slice-over-concat, `SubSource`, `BufferedSource`
  incl. a streaming run through a 16 KiB ring) is driven with
  fixed-seed pseudo-random op sequences — reads of random sizes,
  `Start`/`Current`/`End` seeks including `u64::MAX` and `i64::MIN`
  extremes — in lockstep with a `Cursor<Vec<u8>>` model, requiring
  identical seek ok-ness, positions, bytes, and `stream_position`
  after every operation.
- Hostile-input hardening sweep (`tests/hostile_input.rs`):
  deterministic fuzzing of `parse_slice_uri` / `parse_data_uri` /
  `open_bytes` with separator-biased byte soup plus multi-byte UTF-8
  and RTL-override characters — asserts no panics, byte-identical
  slice round-trip on every accepted input, parse-format-parse
  fixpoint, 500-deep nested-slice recursion safety, and truncated
  percent-escape alignment safety.
- Shared `Read + Seek` conformance suite (`tests/conformance.rs`):
  one behavioural battery — EOF idempotence, zero-sized-buffer reads,
  seek-past-end tolerance + recovery, seek-underflow error with
  position preservation, `SeekFrom::End`/`Current` arithmetic,
  rewind re-read, exact position tracking — run against 17 source
  shapes: file / mem / data (percent + base64) plain and empty,
  mid-file `slice:`, zero-length and nested slices, mixed-scheme
  `concat:` (with a zero-length middle segment), `slice:` over
  `concat:` spanning both internal boundaries, programmatic
  `SubSource`, and `BufferedSource` (fits-in-ring, empty, and a
  512 KiB payload streamed through a 16 KiB ring to exercise the
  worker-refill and lookback-drop paths).
- `SliceUri::open(&self)` — open the window described by a typed
  `SliceUri` directly, without round-tripping through the URI string.
  Resolves `inner` with the matching bundled opener (`file://` / bare
  path, `mem://`, `data:`, or a nested `slice:`) and wraps it in a
  `SubSource` over `[offset, offset + length)`. This is the typed
  analogue of `open_slice` and the slice-scheme parallel to
  `DataUri` → `open_data`: a caller that built a `SliceUri` via
  `SliceUri::new` or inspected one via `parse_slice_uri` can open it
  straight away instead of calling `.format()` and re-parsing the
  string. `open_slice(uri_str)` is now defined as
  `parse(uri_str)?.open()`, so the URI-string and typed-value entry
  points share a single open code path and cannot drift apart; the
  reader `parsed.open()` returns is byte-identical to
  `open_slice(&parsed.format())`.
- `SliceUri` — public typed view of a parsed `slice:` URI, parallel to
  the existing `DataUri` for `data:`. `parse_slice_uri(uri_str)` (also
  available as `slice::parse`) returns a `SliceUri { offset, length,
  inner: String }` without opening any inner source, letting CLI
  parsers, pipeline tooling, and fixture builders inspect or transform
  the parsed form before deciding whether (or how) to dispatch. The
  constructor `SliceUri::new(offset, length, inner)` validates `inner`
  (rejects empty + `!`-containing strings up-front so a non-round-trippable
  URI cannot be silently produced), and `SliceUri::format` / the
  `Display` impl emit the canonical `slice:<offset>+<length>!<inner>`
  form so `parse → format` is byte-identical for every URI the parser
  accepts. The existing string-only `open_slice(uri_str)` entry point
  is unchanged.
- `file://` driver percent-decodes the URI path per RFC 3986 §2.1. A URI
  of the form `file:///tmp/foo%20bar.txt` now opens `/tmp/foo bar.txt`
  as every spec-conformant URI handler does, and a UTF-8-encoded
  multibyte name (`file:///tmp/Привет.bin` written as
  `file:///tmp/%D0%9F%D1%80%D0%B8%D0%B2%D0%B5%D1%82.bin`) round-trips
  through `reg.open` end-to-end. Bare paths (no scheme) remain
  verbatim, so a real file whose name actually contains `%` continues
  to open. The same decode step is applied inside `FileScope::resolve`
  before the canonicalise / allow-list check, so a smuggled `%00`
  surfaces as a NUL-byte rejection before the path reaches the
  filesystem. New helpers in `oxideav_source::uri`:
  `percent_decode_path` (returns `String`, UTF-8-validated),
  `percent_decode_bytes` (returns `Vec<u8>` for paths that may not be
  UTF-8), and `has_file_scheme` (case-insensitive `file:` prefix test
  used to gate decoding to URI-form inputs).
- `FileScope::deny_dir(dir)` — deny-list carve-out for the `file://`
  driver scope. A path is rejected whenever its canonical form lies
  under any deny-listed root, even when the allow-list (or a
  [`FileScope::permissive`] scope) would otherwise admit it. This
  closes the "allow `/var/media` but never `/var/media/.snapshots`"
  gap that previously required redesigning the allow-list at
  registration time. Deny entries override permissive scopes too:
  `FileScope::permissive().deny_dir("/etc")` reads "everything except
  `/etc/**`". Component-aware prefix match (so `deny_dir("/foo")`
  does not affect `/foobar`); canonicalisation still feeds the check,
  so a `..` path that resolves into a deny-listed subtree is blocked.
  `FileScope::is_allowed_path(&Path)` exposes the combined verdict
  for callers that want to test a path without opening it.
- `BufferedSource::builder()` — fluent builder exposing every prefetch
  knob (`capacity`, `block_size`, `prefetch_timeout`,
  `lookback_fraction`) as a per-source setting. Previously the worker
  block size (256 KiB), prefetch timeout (30 s), and lookback fraction
  (1/8) were compile-time constants; a caller talking to a fast local
  source or a slow satellite link had no way to tune them. The new
  builder reads sensible defaults and clamps each knob on `build` so the
  worker is always able to make forward progress (capacity ≥ 4 × block,
  block ≥ 4 KiB, timeout ≥ 1 ms, lookback strictly less than 1).
  `BufferedSource::new(inner, capacity)` keeps its historical two-arg
  shape and now resolves all other knobs to their defaults via the
  builder. Public constants `DEFAULT_BLOCK`,
  `DEFAULT_PREFETCH_TIMEOUT`, `DEFAULT_LOOKBACK_NUM`, and
  `DEFAULT_LOOKBACK_DEN` surface the defaults for callers that want to
  re-derive them. `BufferedSource::prefetch_timeout()` returns the
  effective timeout post-clamping for diagnostics.

## [0.1.5](https://github.com/OxideAV/oxideav-source/compare/v0.1.4...v0.1.5) - 2026-05-29

### Other

- accept mem://, data:, slice: segments alongside file://
- URI-level windowed view — slice:<offset>+<length>!<inner-uri>
- SubSource — windowed view + Arc-backed mem:// reader
- driver — concatenate file:// segments into one seekable stream
- add RFC 2397 data:[...][;base64],<bytes> driver
- make permissive() cross-platform
- add mem:// driver + FileScope allow-list for file://

### Changed

- `concat:` driver now accepts the same inner-scheme set the `slice:`
  driver does — segments may be bare paths, `file://`, `mem://`, `data:`,
  or `slice:` URIs (previously only bare paths and `file://` URLs were
  accepted). Dispatch is done per segment via the matching bundled
  opener, so the mixed-scheme case `concat:<file>|mem://<id>|data:,TAIL`
  works end-to-end without first materialising the inputs as files.
  Nested `concat:` segments are rejected because the outer `|` split
  would shred the inner segment list; use a single flattened list
  instead. Previously-rejected `concat:mem://x|mem://y` and
  `concat:data:,a|data:,b` URIs now succeed.

### Added

- `slice:<offset>+<length>!<inner-uri>` scheme — URI-level windowed view
  over an inner source. `open_slice` parses the decimal range header,
  dispatches the inner URI to the matching bundled opener (`file://` /
  bare path, `mem://`, `data:`, or another `slice:` for recursive
  composition), and wraps the result in a `SubSource` that re-projects
  `[offset, offset + length)` onto `[0, length)`. The `!` separator was
  chosen because it never appears in `file://` paths and is not used by
  the other bundled schemes, so the split is unambiguous even when the
  inner URI carries its own `:` and `://`. Pipelines and CLI flags can
  now address a sub-range of any in-process source without first
  materialising it. `with_defaults()` and `register()` install the
  `slice` driver alongside `file`, `mem`, `data`, and `concat`.
- `SubSource` — windowed view (`[base, base + len)` → `[0, len)`) over
  any `Box<dyn BytesSource>`. The seekable analogue of
  `std::io::Read::take`: containers can hand a codec a stream that looks
  like the codec's own sample, including support for seeking back inside
  the window (e.g. re-reading a header after probing). Bounds are
  validated at construction via a non-destructive end-seek probe;
  zero-length windows, exact-tail windows, and nested windows all
  compose. Helper `stream_len(&mut dyn BytesSource) -> io::Result<u64>`
  probes the inner length without disturbing the cursor.
- `concat:<a>|<b>|…` scheme — concatenate several `file://` segments into
  one seekable `BytesSource`. `open_concat` opens each `|`-separated
  segment with the `file` driver (bare paths and `file://` URLs both
  accepted), captures each segment length at open time, and presents the
  composite over the virtual address space `[0, total_len)`: `Read`
  walks segment boundaries transparently and `Seek`
  (`Start`/`End`/`Current`) resolves an absolute offset into the right
  segment. Empty segments (`a||b`, trailing `|`) and an empty payload
  are rejected. `with_defaults()` and `register()` now install the
  `concat` driver alongside `file`, `mem`, and `data`.
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
- `open_mem` returns an `Arc<Vec<u8>>`-backed `Read + Seek` reader
  instead of a fresh `Cursor<Vec<u8>>` cloned from the buffer. Multiple
  concurrent opens of the same id now share the bytes by reference; the
  per-open cost drops from a full `Vec<u8>` copy to a single `Arc`
  clone. Reader semantics are unchanged: each handle owns its own
  position, so reads on different handles are still independent.

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
