# ADR-T002: Per-tile frame resampling = hold-last-good + duplicate/drop (sample on output tick)

- **Status:** Proposed
- **Area:** Streaming/Timing
- **Date:** 2026-06-02
- **Source brief:** [streaming-gotchas.md](../research/streaming-gotchas.md)

## Decision

Each input decodes independently into a per-tile single-slot latest-frame cell (overwrite policy). The compositor samples it on each output tick: duplicate (hold) when the source is slow, drop (newest-wins) when fast. No blending by default. Offer linear-blend (framerate) and motion-compensated (minterpolate) only as explicit opt-in per-tile quality modes. Detect real fps from a rolling median of decoded PTS deltas; never trust r_frame_rate/avg_frame_rate/SDP.

## Rationale

Mathematically equivalent to nearest/previous-PTS resampling with implicit dup-on-stall and drop-on-overrun, at zero motion-interpolation cost (the fps/videorate model, confirmed dup/drop-only on FFmpeg 8.1.1). Bounded memory via single-slot overwrite. minterpolate does per-frame block motion estimation (tens of ms/HD) and cannot sustain N tiles at 50/60 fps live; framerate ghosts on motion.

## Alternatives considered

Global minterpolate (stalls under load); per-input fps/videorate filter chains feeding one xstack (couples all inputs to one graph clock).

## Consequences

A 30 fps source on 60 fps output shows each frame twice (judder); 60->50 drops irregularly. Accepted unless the spec secretly requires smooth low-fps motion (OPEN QUESTION) - if so, this flips to minterpolate and limits tile count; benchmark first. Real input fps still needed for jitter-buffer sizing and drop-only-vs-duplicate policy.

## Addendum (2026-10-24, #133): latest-frame slot storage is a pointer-swap `RwLock`, not arc-swap

- **Status:** Accepted (R3 design note: invariants #1, #2 and #10)
- **Source:** incident, PR #132 CI on macOS-latest (ARM). `display_mailbox.rs::concurrent_reader_never_observes_older_than_seen` failed with `sequence regressed: 6613 < 7357`.

### Context

`LatestSlot` (the invariant-#2 cell above) and the `TileStore` media-time ring stored their `Arc`s in `arc_swap::ArcSwap{,Option}` 1.9.2, which uses the default hybrid strategy. `latest.rs` claimed a single reader could never regress. The argument was per-location coherence of the one atomic pointer.

### Root cause

Coherence holds; the slot's pointer never went backwards. The reader got a value that was **never stored into its slot**. Here is how the arc-swap 1.9.2 hybrid fast path (`strategy/hybrid.rs::attempt`) leads to it:

1. A reader of slot A loads pointer `P` and records a debt for `P` in its thread's fast slot. The debt list is process-global and keyed by **raw address** only, with no storage identity.
2. Before the reader re-confirms, A's writer stores a new value. `pay_all` pays the reader's debt on `P`, the last reference drops, and `P` is freed.
3. The allocator reuses `P`'s address for a value stored into a **different** slot B of the same size class. B's next store runs `pay_all(P)` and finds the reader's slot holding the bit pattern `P`. Depending on the interleaving, a reused debt on the same address can also be in play.
4. The reader's confirm load sees A changed. Its `debt.pay(P)` fails because someone else paid it, and arc-swap takes that to mean "already paid, safe to use". It returns `P`, which is now **B's** `Arc`.

The display test binary runs `writer_never_blocks_when_nothing_drains` (100 000 publishes of the same `Stamped<Payload>` type) in parallel with the failing test. So frame 6613 came from the other mailbox. The same bug is tracked upstream as [vorner/arc-swap#210](https://github.com/vorner/arc-swap/issues/210) (open). It has a public minimal reproducer on Apple aarch64. arc-swap 1.9.3 only adds an opt-in `rwlock-strategy` feature and leaves `DefaultStrategy` unchanged. We reproduced it on x86_64 Linux in debug builds: `concurrency.rs::load_never_returns_a_value_published_into_another_slot` failed 3 of 3 runs before this change.

### Decision

`multiview-framestore` stops using arc-swap. A crate-private `SwapCell<P>` (`src/swap.rs`) is a `std::sync::RwLock` around the pointer (`Option<Arc<T>>` for the slot, `Arc<Vec<RingEntry<T>>>` for the ring). Every critical section is pointer-sized:

| Operation | Under the lock | Outside the lock |
| --- | --- | --- |
| `load` (reader, every output tick) | one `Arc::clone` (one atomic increment) | everything the caller does with the frame |
| `store` / `publish` (writer) | one `mem::replace` of the pointer | `Arc::new`, the seq `fetch_add`, dropping the old value (destructor, free) |
| `rcu` (ring publish) | one `Arc::ptr_eq` + `mem::replace` | the O(capacity) ring copy, the new `Arc` |

No critical section allocates, frees, runs a destructor or user code, or does I/O, and none can panic, so the lock cannot be poisoned. Poisoning is tolerated anyway, by taking the inner value. A wedged, slow or dead consumer holds no lock, because it only holds an `Arc`. That keeps invariant #10: the publisher never waits on a consumer's progress. The publisher's worst-case wait is one reader's clone. If the OS preempts that reader inside the clone, the wait grows by one scheduler slice. Under contention the queued waits are still clone-sized.

Measured on the 56-core x86_64 dev container (release, 2 M ops, Linux pthread `RwLock`, the same `Option<Arc<u64>>` payload):

| | p50 | p99 | p99.99 | max |
| --- | --- | --- | --- | --- |
| publish, 1 hot reader, `RwLock` | 0.4–0.6 µs | 2.7–2.9 µs | 17 µs | 156–159 µs |
| publish, 1 hot reader, arc-swap 1.9.2 | 0.7–1.7 µs | 1.2–3.9 µs | 17–19 µs | 68–506 µs |
| publish, 4 hot readers, `RwLock` | 0.18 µs | 26 µs | 49 µs | 246 µs |
| publish, 4 hot readers, arc-swap 1.9.2 | 2.4 µs | 7 µs | 21 µs | 445 µs |
| load under a hammering writer, `RwLock` | 85 ns | 2.2 µs | 13 µs | 85 µs |
| load under a hammering writer, arc-swap 1.9.2 | 223 ns | 0.5 µs | 12 µs | 60 µs |

The 4-reader row is a synthetic worst case, with readers spinning on the slot nonstop. Production has one compositor or sink reader per slot, sampling once per tick. Every tail sits at least two orders of magnitude inside the shortest output tick (16.7 ms at 60 fps), and arc-swap's tails were the same order. The ring copy already ran on the input thread, and it still runs there, outside the lock.

The `LatestSlot` and `display::mailbox` public APIs are unchanged. `latest.rs`, the `lib.rs` invariants list, the `isolation.rs` and `mailbox.rs` docs, and the `registry.rs` teardown note now describe this bound instead of "wait-free / no lock".

### Alternatives considered

| Alternative | Rejected because |
| --- | --- |
| Reader-side monotonic seq guard (track the max seq, retry or return the cached value on older) | It only catches *older* foreign values. A foreign value with a higher seq passes. In production that is another tile's or output's frame, and with a different `T` it is type confusion. Unsound residual. |
| Owner-id + seq stamp in each entry, rejecting foreign entries | It reads the foreign allocation to check the id. If the foreign `T` differs, that read is already UB. Defence in depth at most, never a fix. Not kept, because the new storage cannot return a foreign value at all. |
| arc-swap 1.9.3 `rwlock-strategy` | It is the same lock idea, but it needs a version bump and a new feature flag on a crate whose default strategy has the open bug, and a strategy type parameter on every use. Plain `std::sync::RwLock` adds no dependency surface, and we own its bound. |
| A seqlock or triple buffer over `Arc` | It cannot be written in safe Rust: a pointer read racing a free needs hazard pointers or an epoch. `forbid(unsafe_code)` holds in this crate. |
| `crossbeam-epoch` (already in `Cargo.lock`) | Its API is `unsafe` (`Shared::deref`, `defer_destroy`), so it is unusable under `forbid(unsafe_code)`, and deferred reclamation would delay frame drops by an unbounded epoch. |

### Consequences

- The reader is no longer *wait-free* in the formal sense. It is non-blocking with a pointer-sized critical section, and so is the publisher. Both bounds are stated in `swap.rs`, and any change that puts more work under that lock is a regression against this ADR.
- New guarantee: a `LatestSlot` load can only return a value published into that slot. This is tested in `tests/concurrency.rs` (cross-instance foreign read, single-reader non-regression).
- **Not yet migrated** (still on arc-swap 1.9.2 `DefaultStrategy`, exposed to #210): `multiview-audio` `AudioStore` window, `multiview-engine` `migration.rs` `DeviceLoadBoard`, and `ArcSwap` uses in `multiview-input`, `multiview-overlay`, `multiview-compositor` (watermark), `multiview-hal` (load) and `multiview-cli`. They were left out to keep #133 scoped to the invariant-#2 primitive. Each should move to the same pattern, or to arc-swap's opt-in `rwlock-strategy`, until #210 is fixed upstream. They are tracked in the PR's `## Concerns`.
