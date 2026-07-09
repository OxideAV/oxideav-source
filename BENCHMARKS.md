# oxideav-source benchmarks

`cargo bench --bench read_paths` — criterion 0.5, self-contained
synthesised payloads (1 MiB ramps for throughput rows, 64 KiB for the
decode rows). Reference numbers below from an Apple Silicon dev machine
(2026-07-09, round 399); treat them as relative guides, not absolutes.

## Sequential read throughput (1 MiB payload, 4 KiB read calls)

| shape | time | throughput |
| --- | --- | --- |
| `mem://` direct | 16.6 µs | ~59 GiB/s |
| `slice:` over mem | 17.3 µs | ~56 GiB/s |
| `concat:` 16 × 64 KiB segments | 17.7 µs | ~55 GiB/s |
| `BufferedSource` over mem (256 KiB ring) | 99 µs | ~9.8 GiB/s |

Takeaways: the `slice:` window and the 16-segment `concat:` walk cost
only ~4–6 % over a direct `mem://` read — the per-read re-seek of the
underlying segment is cheap against the copy itself. The prefetch ring
pays ~6× for the worker-thread handoff plus the `VecDeque` two-slice
copy; that is the intended trade (it exists to hide *slow* inner
sources such as HTTP, not to accelerate RAM).

## Seek patterns

| pattern | time |
| --- | --- |
| `BufferedSource` short back-seek inside the lookback region + 512 B read | ~21 ns |
| `concat:` alternating far seeks across 16 segments + 64 B read | ~10 ns |

The lookback hit confirms the ring serves recent history without a
worker restart; the concat far-seek shows the cumulative-offset table
resolution is negligible.

## URI decoding (64 KiB payload)

| decoder | time | throughput |
| --- | --- | --- |
| `data:` percent (`%HH` for every byte) | 87 µs | ~720 MiB/s |
| `data:` base64 | 146 µs | ~428 MiB/s |
| `parse_slice_uri` (grammar only) | 55 ns/op | — |
