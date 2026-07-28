//! Prefetch-ring-buffer wrapper around any `ReadSeek`.
//!
//! A worker thread owns the inner source and continuously fills a ring
//! buffer ahead of the read cursor. Reads serve from the ring; seeks
//! either move the cursor inside the ring (no IO) or restart the worker
//! at the new offset.
//!
//! Designed for streaming playback over a slow source (HTTP).
//!
//! ## Tuning
//!
//! Defaults work for typical HTTP playback: 256 KiB block reads from the
//! inner source, a 30 s reader-side prefetch timeout, and a 1/8 lookback
//! retention (the ring keeps ~12.5 % of its capacity behind the reader to
//! satisfy short back-seeks without re-fetching). All four knobs —
//! capacity, block size, lookback fraction, and prefetch timeout — are
//! exposed via [`BufferedSource::builder`] for callers whose source has a
//! different latency or transfer profile.
//!
//! Constructing a `BufferedSource` via [`BufferedSource::new`] keeps the
//! historical signature (`capacity` only) and resolves the other knobs to
//! their defaults.

use std::collections::VecDeque;
use std::io::{self, Read, Seek, SeekFrom};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use oxideav_core::ReadSeek;

/// Default worker block size in bytes (`block_size`).
pub const DEFAULT_BLOCK: usize = 256 * 1024;

/// Default reader-side prefetch wait timeout. A read that has to wait
/// for the worker longer than this surfaces `io::ErrorKind::TimedOut`
/// instead of hanging forever; useful when the inner source has stalled
/// (e.g. a frozen HTTP connection).
pub const DEFAULT_PREFETCH_TIMEOUT: Duration = Duration::from_secs(30);

/// Default lookback fraction numerator. The ring keeps the most recent
/// `capacity * LOOKBACK_NUM / LOOKBACK_DEN` bytes behind the reader so a
/// short back-seek hits the ring instead of restarting prefetch. The
/// default 1/8 (~12.5 %) matches the historical hardcoded value.
pub const DEFAULT_LOOKBACK_NUM: u32 = 1;
/// Default lookback fraction denominator. See [`DEFAULT_LOOKBACK_NUM`].
pub const DEFAULT_LOOKBACK_DEN: u32 = 8;

/// Shared state between reader and worker.
struct RingState {
    /// Bytes prefetched, oldest first. `buf[0]` corresponds to `ring_start`.
    buf: VecDeque<u8>,
    /// Absolute offset of `buf[0]` in the inner source.
    ring_start: u64,
    /// Maximum number of bytes the ring may hold.
    capacity: usize,
    /// Worker block size: maximum bytes the worker reads from the inner
    /// source per `read` syscall. Stored on the state so the reader-side
    /// "free space" check can cap each worker fill at this value without
    /// the worker having to re-export it.
    block_size: usize,
    /// Lookback-fraction numerator: ring retains at least
    /// `capacity * lookback_num / lookback_den` bytes behind the reader.
    lookback_num: u32,
    /// Lookback-fraction denominator. See [`lookback_num`].
    lookback_den: u32,
    /// Total length of inner source, if known.
    total_len: Option<u64>,
    /// Worker has reached EOF at the current ring tail.
    eof: bool,
    /// Sticky error from the worker, stored as `(kind, message)` so it
    /// can be re-surfaced on EVERY subsequent reader call (`io::Error`
    /// is not `Clone`). Cleared by a reader seek only while the worker
    /// is still alive — a dead worker cannot refill the ring, so its
    /// error must keep surfacing instead of degenerating into a
    /// misleading `TimedOut` after `prefetch_timeout`.
    err: Option<(io::ErrorKind, String)>,
    /// The worker thread has exited (after surfacing an error). Reader
    /// seeks must not clear `err` in this state: nothing will ever
    /// refill the ring again.
    worker_dead: bool,
    /// Reader has set this to ask the worker to discard the ring and
    /// reposition the inner source. The worker clears it when it has acted.
    target_pos: Option<u64>,
    /// Reader is gone; worker should exit promptly.
    stop: bool,
}

impl RingState {
    /// Reconstruct the sticky worker error for surfacing to the reader.
    fn surface_err(&self) -> Option<io::Error> {
        self.err
            .as_ref()
            .map(|(kind, msg)| io::Error::new(*kind, msg.clone()))
    }

    /// Lookback allowance in bytes: how much the ring retains behind
    /// the reader so short back-seeks hit without re-fetching.
    /// `lookback_den` is sanitised non-zero on build; the sanitised
    /// fraction is strictly below 1, so this is strictly below
    /// `capacity`. Integer math; no floats.
    fn rear(&self) -> usize {
        (self.capacity as u64)
            .saturating_mul(self.lookback_num as u64)
            .checked_div(self.lookback_den as u64)
            .unwrap_or(0) as usize
    }
}

struct Shared {
    state: Mutex<RingState>,
    not_full: Condvar,
    not_empty: Condvar,
}

/// Builder for [`BufferedSource`]. Exposes every prefetch tunable for
/// callers whose source has a non-default latency or transfer profile;
/// callers that just want a sensible buffer can stay on
/// [`BufferedSource::new`].
///
/// Defaults match `BufferedSource::new`: 1 MiB capacity, [`DEFAULT_BLOCK`]
/// block size, [`DEFAULT_PREFETCH_TIMEOUT`] reader timeout, and
/// `DEFAULT_LOOKBACK_NUM / DEFAULT_LOOKBACK_DEN` lookback fraction.
///
/// All tunables are clamped on `build` so the worker is always able to
/// make forward progress regardless of the values handed in:
///
/// * `capacity` is rounded up to at least `4 * block_size` bytes so each
///   block read fits in the ring with three more behind it.
/// * `block_size` is rounded up to `4 KiB` if smaller (an inner `read` of
///   a few bytes per syscall would dominate the worker's wall time).
/// * `lookback_num / lookback_den` is clamped to the range `[0, 1)` —
///   a denominator of zero is treated as "no lookback" and a numerator
///   matching the denominator is dropped to "(den - 1) / den" so the
///   reader always has at least one byte of forward window.
/// * `prefetch_timeout` is clamped to a minimum of 1 ms so a misconfigured
///   `Duration::ZERO` does not flap reads through `TimedOut` immediately.
#[derive(Clone, Debug)]
pub struct BufferedSourceBuilder {
    capacity: usize,
    block_size: usize,
    prefetch_timeout: Duration,
    lookback_num: u32,
    lookback_den: u32,
}

impl Default for BufferedSourceBuilder {
    fn default() -> Self {
        Self {
            capacity: 1024 * 1024,
            block_size: DEFAULT_BLOCK,
            prefetch_timeout: DEFAULT_PREFETCH_TIMEOUT,
            lookback_num: DEFAULT_LOOKBACK_NUM,
            lookback_den: DEFAULT_LOOKBACK_DEN,
        }
    }
}

impl BufferedSourceBuilder {
    /// New builder with all knobs at their defaults.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the ring capacity in bytes. Will be clamped up to at least
    /// `4 * block_size` on `build` so the ring always holds several
    /// worker blocks.
    pub fn capacity(mut self, bytes: usize) -> Self {
        self.capacity = bytes;
        self
    }

    /// Maximum bytes the worker reads from the inner source per syscall.
    /// Clamped up to 4 KiB on `build`. Larger values reduce per-syscall
    /// overhead at the cost of coarser-grained ring fills.
    pub fn block_size(mut self, bytes: usize) -> Self {
        self.block_size = bytes;
        self
    }

    /// Maximum time a `Read` will block waiting for the worker to push
    /// fresh bytes. `TimedOut` is surfaced on expiry. Clamped up to 1 ms
    /// on `build`.
    pub fn prefetch_timeout(mut self, dt: Duration) -> Self {
        self.prefetch_timeout = dt;
        self
    }

    /// Fraction of the ring kept behind the reader as lookback so short
    /// backward seeks hit the ring. Expressed as `num/den` to avoid a
    /// floating-point knob (the worker uses integer division internally).
    /// `0/N` disables lookback entirely. `N/N` is clamped to `(N-1)/N`
    /// so the ring keeps a forward window.
    pub fn lookback_fraction(mut self, num: u32, den: u32) -> Self {
        self.lookback_num = num;
        self.lookback_den = den;
        self
    }

    /// Build a [`BufferedSource`] from this builder and an inner source.
    /// Spawns one worker thread that takes ownership of `inner`.
    pub fn build(self, mut inner: Box<dyn ReadSeek>) -> io::Result<BufferedSource> {
        // Clamp all knobs to safe ranges. See struct docs for rationale.
        let block_size = self.block_size.max(4 * 1024);
        let capacity = self.capacity.max(4 * block_size);
        let prefetch_timeout = self.prefetch_timeout.max(Duration::from_millis(1));
        let (lookback_num, lookback_den) = sanitise_lookback(self.lookback_num, self.lookback_den);

        // Determine total length up front (cheap for File / HttpSource).
        let pos = inner.stream_position()?;
        let end = inner.seek(SeekFrom::End(0))?;
        let total_len = Some(end);
        // Restore position.
        inner.seek(SeekFrom::Start(pos))?;

        let state = RingState {
            buf: VecDeque::with_capacity(capacity),
            ring_start: pos,
            capacity,
            block_size,
            lookback_num,
            lookback_den,
            total_len,
            eof: total_len == Some(pos),
            err: None,
            worker_dead: false,
            target_pos: None,
            stop: false,
        };
        let shared = Arc::new(Shared {
            state: Mutex::new(state),
            not_full: Condvar::new(),
            not_empty: Condvar::new(),
        });

        let worker_shared = Arc::clone(&shared);
        let worker_block = block_size;
        let worker = thread::spawn(move || worker_loop(worker_shared, inner, worker_block));

        Ok(BufferedSource {
            shared,
            pos,
            prefetch_timeout,
            worker: Some(worker),
        })
    }
}

/// Clamp `(num, den)` to a valid lookback fraction in `[0, 1)`. A
/// denominator of zero is treated as "no lookback". `num >= den` is
/// dropped to `(den.saturating_sub(1), den)` so the ring always keeps
/// at least one byte of forward window.
fn sanitise_lookback(num: u32, den: u32) -> (u32, u32) {
    if den == 0 {
        return (0, 1);
    }
    if num >= den {
        return (den.saturating_sub(1), den);
    }
    (num, den)
}

/// Buffered, prefetching wrapper around any `ReadSeek`.
pub struct BufferedSource {
    shared: Arc<Shared>,
    /// Reader's logical position in the inner source.
    pos: u64,
    /// Reader-side prefetch wait timeout. Reads waiting longer than this
    /// surface `io::ErrorKind::TimedOut`.
    prefetch_timeout: Duration,
    /// Worker handle. `None` only between drop signal and join.
    worker: Option<JoinHandle<()>>,
}

impl BufferedSource {
    /// Wrap `inner`, allocating up to `capacity` bytes for the prefetch
    /// ring. Spawns one worker thread that takes ownership of `inner`.
    /// `capacity` is rounded up to at least `4 * `[`DEFAULT_BLOCK`] bytes
    /// so the worker always has room to make forward progress.
    ///
    /// Other knobs (block size, prefetch timeout, lookback fraction)
    /// take their default values. Use [`BufferedSource::builder`] to
    /// tune them.
    pub fn new(inner: Box<dyn ReadSeek>, capacity: usize) -> io::Result<Self> {
        BufferedSourceBuilder::new().capacity(capacity).build(inner)
    }

    /// Open a builder for fine-grained control over capacity, block size,
    /// prefetch timeout, and lookback fraction. The builder consumes
    /// itself on each setter, returning a fresh value, then `build(inner)`
    /// yields the running [`BufferedSource`].
    pub fn builder() -> BufferedSourceBuilder {
        BufferedSourceBuilder::new()
    }

    /// Total length of the inner source, if known.
    pub fn len(&self) -> Option<u64> {
        self.shared.state.lock().unwrap().total_len
    }

    /// Whether the inner source is known to be empty. Returns `false` if
    /// the length couldn't be determined (treat as non-empty).
    pub fn is_empty(&self) -> bool {
        matches!(self.len(), Some(0))
    }

    /// Effective prefetch timeout in use by this `BufferedSource` (after
    /// builder clamping). Useful for diagnostics where the caller wants
    /// to confirm the value actually installed.
    pub fn prefetch_timeout(&self) -> Duration {
        self.prefetch_timeout
    }
}

fn worker_loop(shared: Arc<Shared>, mut inner: Box<dyn ReadSeek>, block_size: usize) {
    let mut scratch = vec![0u8; block_size];
    loop {
        // Phase 1: handle stop / seek requests, wait if ring is full.
        let to_read: usize;
        {
            let mut st = shared.state.lock().unwrap();
            loop {
                if st.stop {
                    return;
                }
                if let Some(target) = st.target_pos.take() {
                    st.buf.clear();
                    st.ring_start = target;
                    st.eof = matches!(st.total_len, Some(end) if target >= end);
                    st.err = None;
                    // Reader may already be sleeping on not_empty waiting
                    // for data at the new position. Wake it so it sees the
                    // updated ring_start / eof state.
                    shared.not_empty.notify_all();
                    drop(st);
                    if let Err(e) = inner.seek(SeekFrom::Start(target)) {
                        let mut st = shared.state.lock().unwrap();
                        st.err = Some((e.kind(), e.to_string()));
                        st.worker_dead = true;
                        shared.not_empty.notify_all();
                        return;
                    }
                    st = shared.state.lock().unwrap();
                    continue;
                }
                if st.eof {
                    // No more data to fetch; sleep until reader seeks or drops.
                    st = shared.not_full.wait(st).unwrap();
                    continue;
                }
                let free = st.capacity - st.buf.len();
                if free == 0 {
                    // Wait for reader to drain.
                    st = shared.not_full.wait(st).unwrap();
                    continue;
                }
                to_read = free.min(st.block_size);
                break;
            }
        }

        // Phase 2: read into scratch outside the lock.
        let read_result = inner.read(&mut scratch[..to_read]);

        // Phase 3: deposit in ring or surface error / EOF.
        let mut st = shared.state.lock().unwrap();
        // Reader may have requested a seek while we were reading; if so,
        // discard what we just read and let phase 1 handle it next loop.
        if st.target_pos.is_some() || st.stop {
            continue;
        }
        match read_result {
            Ok(0) => {
                st.eof = true;
                shared.not_empty.notify_all();
            }
            Ok(n) => {
                st.buf.extend(scratch[..n].iter().copied());
                shared.not_empty.notify_all();
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {
                // Retry per the std `Read` convention: `Interrupted`
                // means "call read again", not "the stream is broken".
            }
            Err(e) => {
                // Fatal: record a sticky (kind, message) copy so EVERY
                // subsequent reader call re-surfaces it, and mark the
                // worker dead so reader seeks stop expecting a refill.
                st.err = Some((e.kind(), e.to_string()));
                st.worker_dead = true;
                shared.not_empty.notify_all();
                return;
            }
        }
    }
}

impl Read for BufferedSource {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        let mut st = self.shared.state.lock().unwrap();
        loop {
            // If reader is somehow before ring_start (shouldn't happen — Seek
            // bumps target_pos), surface as InvalidInput.
            if self.pos < st.ring_start {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "BufferedSource: reader behind ring start",
                ));
            }
            // Position relative to ring_start.
            let rel = (self.pos - st.ring_start) as usize;
            if rel < st.buf.len() {
                // Hit. Copy out using the VecDeque's two contiguous
                // slices — this is `copy_from_slice` per segment, vastly
                // faster than an element-wise loop on a million-byte ring.
                let avail = st.buf.len() - rel;
                let n = avail.min(out.len());
                let (front, back) = st.buf.as_slices();
                if rel < front.len() {
                    let f_off = rel;
                    let f_take = (front.len() - f_off).min(n);
                    out[..f_take].copy_from_slice(&front[f_off..f_off + f_take]);
                    if f_take < n {
                        let b_take = n - f_take;
                        out[f_take..n].copy_from_slice(&back[..b_take]);
                    }
                } else {
                    let b_off = rel - front.len();
                    out[..n].copy_from_slice(&back[b_off..b_off + n]);
                }
                self.pos += n as u64;
                // If we've consumed past the front of the ring, drop those
                // bytes so the worker can refill.
                let drop_n = rel + n;
                // But keep some slack so backward seeks within recent past
                // still hit. Use the builder-configured lookback fraction
                // (default 1/8 of capacity) as the "rear" the reader can
                // lookback into without re-fetching.
                let rear = st.rear();
                if drop_n > rear {
                    let to_drop = drop_n - rear;
                    st.buf.drain(..to_drop);
                    st.ring_start += to_drop as u64;
                    self.shared.not_full.notify_one();
                }
                return Ok(n);
            }
            // Miss: at or past the end of the ring. Prefetched bytes
            // (served above) are always valid even when the worker has
            // since failed — the error only surfaces once the ring is
            // exhausted, and it is sticky: NOT taken, so the same
            // failure keeps re-surfacing on every retry instead of
            // degrading into a `TimedOut` wait once the (dead) worker
            // can no longer refill the ring.
            if let Some(e) = st.surface_err() {
                return Err(e);
            }
            if st.eof {
                return Ok(0);
            }
            // The worker only learns EOF by reading a final 0 from the
            // inner source, which it may never get to do (e.g. it is
            // parked on a full ring). A known total length makes the
            // verdict immediate: reads at or past it are EOF.
            if matches!(st.total_len, Some(total) if self.pos >= total) {
                return Ok(0);
            }
            // A full ring that sits entirely behind the reader can never
            // satisfy this read: the worker is parked on `not_full` and
            // the reader is about to park on `not_empty` — a deadlock
            // that would eventually surface as a bogus `TimedOut`. This
            // state is reachable by seeking to the ring end (an
            // in-window seek performs no ring maintenance). Make room by
            // draining everything beyond the lookback allowance — the
            // same retention rule the hit path applies — and wake the
            // worker. The sanitised lookback fraction is strictly below
            // 1, so at least one byte is always freed.
            if st.buf.len() == st.capacity {
                let to_drop = st.buf.len() - st.rear();
                st.buf.drain(..to_drop);
                st.ring_start += to_drop as u64;
                self.shared.not_full.notify_one();
                continue; // ring is no longer full; fall through to the wait
            }
            // Wait for worker to push more bytes — bounded so a stuck
            // worker becomes visible rather than deadlocking forever.
            let timeout = self.prefetch_timeout;
            let (new_st, wait_result) = self.shared.not_empty.wait_timeout(st, timeout).unwrap();
            st = new_st;
            if wait_result.timed_out() && st.err.is_none() && !st.eof {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!(
                        "BufferedSource: prefetch timeout ({} ms)",
                        timeout.as_millis()
                    ),
                ));
            }
        }
    }
}

impl Seek for BufferedSource {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let mut st = self.shared.state.lock().unwrap();
        let total = st.total_len;
        let new_pos: u64 = match from {
            SeekFrom::Start(n) => n,
            SeekFrom::Current(d) => add_signed(self.pos, d)?,
            SeekFrom::End(d) => {
                let end = total.ok_or_else(|| {
                    io::Error::new(io::ErrorKind::Unsupported, "stream length unknown")
                })?;
                add_signed(end, d)?
            }
        };
        // If the new position is inside the current ring window, just
        // update the cursor — no IO needed.
        let ring_end = st.ring_start + st.buf.len() as u64;
        if new_pos >= st.ring_start && new_pos <= ring_end {
            self.pos = new_pos;
            return Ok(new_pos);
        }
        // Otherwise tell the worker to reposition the inner source and
        // restart prefetch from `new_pos`. Reset ring state here under the
        // lock so that `self.pos == ring_start` is invariant by the time
        // Seek returns — otherwise a Read call landing before the worker
        // acts on `target_pos` would see `self.pos < ring_start` (for
        // backward seeks) and wrongly return "reader behind ring start".
        st.target_pos = Some(new_pos);
        st.buf.clear();
        st.ring_start = new_pos;
        st.eof = matches!(total, Some(end) if new_pos >= end);
        // A live worker will reposition and refill, so its stale error
        // is cleared. A dead worker can never refill: keep the sticky
        // error so the next read fails immediately instead of waiting
        // out the prefetch timeout for data that cannot arrive.
        if !st.worker_dead {
            st.err = None;
        }
        self.pos = new_pos;
        self.shared.not_full.notify_all();
        self.shared.not_empty.notify_all();
        Ok(new_pos)
    }
}

fn add_signed(base: u64, delta: i64) -> io::Result<u64> {
    if delta >= 0 {
        base.checked_add(delta as u64)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "seek overflow"))
    } else {
        let mag = delta.unsigned_abs();
        base.checked_sub(mag)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "seek before start"))
    }
}

impl Drop for BufferedSource {
    fn drop(&mut self) {
        {
            let mut st = self.shared.state.lock().unwrap();
            st.stop = true;
        }
        self.shared.not_full.notify_all();
        self.shared.not_empty.notify_all();
        if let Some(h) = self.worker.take() {
            let _ = h.join();
        }
    }
}
