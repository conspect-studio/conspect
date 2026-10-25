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

## Addendum (2026-10-24, #133): latest-frame slot storage is a tagged slot ring, not arc-swap

- **Status:** Accepted (R3 design note: invariants #1, #2 and #10). Revised in round 2 of review: the first fix, a single `RwLock`, let a descheduled reader stall the publisher. Revised again in round 3: a publisher descheduled while holding the write lock of the slot that had just become the head stalled every reader (see *Round 3* below).
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

### Requirement

1. The publisher never waits on a reader, in any situation. That includes a reader the OS deschedules while it is in the middle of a read.
2. A reader never blocks, and never waits on a publisher, including one the OS deschedules at any point of a publish.
3. A load returns only a value stored into that same cell (no #210-class foreign value).
4. The newest value is never lost while no further publish follows.
5. No `unsafe` in `multiview-framestore`, and `cargo deny` stays clean.

Round 1 used one `std::sync::RwLock` around the pointer. It met 2 to 5 but not 1. Every critical section was pointer-sized, but a short critical section does not bound scheduling delay. A reader preempted while holding the read lock made `publish` (on the output-clock thread) wait for that reader to be scheduled again, which can cost a tick.

### Decision

`multiview-framestore` stops using arc-swap. A crate-private `SwapCell<P>` (`src/swap.rs`) stores the pointer (`Option<Arc<T>>` for the slot, `Arc<Vec<RingEntry<T>>>` for the ring) in a **ring of `RwLock` slots** named by one **generation-tagged atomic head** (`generation << 16 | slot index`):

| Operation | What it does | Can it wait? |
| --- | --- | --- |
| `store` / `publish` | Takes a fresh per-cell generation. Claims a slot round-robin after the head: CAS the slot's atomic tag from `t` to `CLAIMED` only after reading a head generation `> t`, then `try_write` (on failure, restore the tag and skip). Writes `(generation, value)`, unlocks, stores tag = `generation`. Then `head.fetch_max`. If every slot is busy, it appends an 8-slot segment (`OnceLock`) and claims there. | **Never on a reader.** `try_write` does not block. The only possible wait is `OnceLock::get_or_init` against another *publisher* appending at the same instant. Production slots have one publisher. |
| `load` (reader, every output tick) | Reads `head`, `try_read`s the slot it names, and clones the value only if the entry's generation equals the head's generation. Otherwise it re-reads `head`. | **Never blocks, never waits on a publisher.** The head slot is never write-locked (see *Round 3*), so an attempt fails only if `head` moved during it: at most `k + 1` attempts for `k` publishes completing meanwhile (lock-free). |
| `rcu` (ring publish) / `swap` (`take`) | Same claim, then `compare_exchange` on `head` from the generation the update was computed against. Retry on loss. | As `store`. The O(capacity) ring copy runs outside any lock. |

Why this design meets the requirement:

- **Never foreign.** The tag is a per-cell counter, written under the slot's lock together with the value. A tag match means this is the exact value `head` names in *this* cell. There is no shared reclamation state.
- **Never lost, newest-wins.** `head` only increases. A publisher may claim only a slot whose tag is strictly below a head generation it read, so the head's entry is never overwritten. The newest value is readable as soon as its publish returns, with no pending value to drain and no clock-thread retry.
- **Never regresses.** A thread's successive reads of the single `head` atomic cannot go backwards (coherence), and each load returns the value of the generation it read.
- **A descheduled reader costs one slot, never time.** The publisher skips that slot. Memory is about the current and previous values, plus one value per slot a reader is pinning. Segments are bounded by the peak number of simultaneously pinned slots, not by the publish count.
- **No `unsafe`.** It uses only `std::sync::{RwLock, OnceLock}` and atomics. `forbid(unsafe_code)` still holds, and no dependency was added.

Generations use 48 bits, which lasts 2^48 publishes per cell: 8 900 years at 1 000/s.

Measured on the 56-core x86_64 dev container (release, 2 M ops, `Option<Arc<u64>>`, a temporary bench not committed). Round 1 and arc-swap rows are from the round-1 measurement on the same host:

| | p50 | p99 | p99.99 | max |
| --- | --- | --- | --- | --- |
| publish, 1 hot reader, slot ring | 0.48–0.54 µs | 0.9–2.2 µs | 11–13 µs | 37–44 µs |
| publish, 1 hot reader, round-1 `RwLock` | 0.4–0.6 µs | 2.7–2.9 µs | 17 µs | 156–159 µs |
| publish, 1 hot reader, arc-swap 1.9.2 | 0.7–1.7 µs | 1.2–3.9 µs | 17–19 µs | 68–506 µs |
| publish, 4 hot readers, slot ring | 1.0–1.1 µs | 2.2–3.8 µs | 17–18 µs | 123–271 µs |
| publish, 4 hot readers, round-1 `RwLock` | 0.18 µs | 26 µs | 49 µs | 246 µs |
| load under a hammering writer, slot ring | 0.26 µs | 0.44–0.46 µs | 5–11 µs | 234–246 µs |
| load under a hammering writer, round-1 `RwLock` | 85 ns | 2.2 µs | 13 µs | 85 µs |

The single-sample maxima come from host scheduling noise on a shared container. A wall-clock bench cannot prove the never-waits property, so that property is tested structurally instead. The structural property the round-1 lock lacked is tested directly. In `swap.rs::chaos::publisher_never_waits_on_a_descheduled_reader`, a reader holds the head slot's read side and stays parked on a barrier until the publisher thread has finished 1000 publishes. All 1000 must return while the reader is still parked. A `#[cfg(test)]` hook must count zero blocking acquisitions (the segment-append `OnceLock::get_or_init`, the only call in a publish that can block); `try_write` misses on the reader's slot are expected and counted. The whole loop has only a 2 s sanity bound, with no per-publish wall-clock budget. After release the reader must see value 1000. Round 1 failed the original form of this test (0 of 1000 publishes completed in 500 ms). `publish_grows_past_pinned_slots_and_never_loses_the_newest` pins 40 slots at once. `racing_publishers_converge_on_the_newest` covers 4 publishers and 2 readers.

The `LatestSlot` and `display::mailbox` public APIs are unchanged.

### Round 3: readers must not wait on a paused publisher

Reviewer interleaving (round 2 code): publisher A snapshots head = slot 0 in `claim` and pauses; publisher B publishes into slot 1, which becomes the head; A resumes, `try_write`s slot 1 successfully, and is descheduled before its "strictly older than head" re-check. Slot 1 is now the head and write-locked, so every `load` spins until A runs again. The round-2 claim "with a single publisher the head slot is never write-locked" was true, but `SwapCell` allows several publishers (`racing_publishers_converge_on_the_newest`), and the lock-free reader guarantee was false for them.

Fix (option (b)'s second variant from the review: no lock is ever held on a slot that is, or can become, the head): each slot carries an atomic **tag** = its entry's generation, or `CLAIMED`. Ownership is taken by CAS on the tag *before* any lock:

1. A publisher CASes tag `t → CLAIMED` only after reading a head generation `> t`. `head` only grows and generations are unique, so from that instant the head is never generation `t`, and the slot is not the head.
2. Installers store the slot's tag *before* `head.fetch_max` moves to it, so while `head = (g, i)` slot `i`'s tag is `g`, never below the head, never claimable.
3. Only the owner of a `CLAIMED` slot writes it, and it unlocks and stores the new tag before it can move `head` there. So a claimed (and locked) slot is never the head.

Hence the head slot is never write-locked in any interleaving, however long any publisher is paused, including between lock acquisition and any later step. A stale reader inside a non-head slot still only makes a publisher skip that slot (`try_write` fails → restore tag), so the publisher still never waits on a reader. Old guarantees are unchanged: never foreign (the per-cell generation is checked under the read lock), never lost, never regresses.

Tested deterministically by `swap.rs::chaos::readers_never_wait_on_a_publisher_parked_in_a_slot_lock`. `#[cfg(test)]` hooks park A after its head snapshot, let B publish, then resume A and park it immediately after it acquires a slot lock. Two readers must each return within 2 s (round-2 code: 0 of 2, A parked in slot 1 = head slot 1), return B's newest value, never a foreign or older one, follow B through 999 further publishes while A stays parked, and every load must take under 10 ms. After release, A's older-generation value must not displace B's newest.

Round-3 measurement, same host and bench (two runs each): publish with 1 hot reader p50 0.52–0.54 µs, p99 0.98–1.03 µs, p99.99 12–13 µs, max 64–85 µs; publish with 4 hot readers p50 1.04–1.09 µs, p99 2.3 µs, p99.99 14–15 µs, max 68–329 µs; load under a hammering writer p50 0.22–0.23 µs, p99 0.49–0.50 µs, p99.99 1.3–1.8 µs, max 56–116 µs. That is within noise of the round-2 slot ring: the tag CAS adds one uncontended atomic per claimed slot, and the load path is unchanged.

Stress, release builds, 1000 iterations in 8 parallel shards (deliberately oversubscribed): `tests/concurrency.rs` (all 4 tests) 1000/1000, `display_mailbox.rs::concurrent_reader_never_observes_older_than_seen` 1000/1000, and the new deterministic test 1000/1000 (plus 900/900 at 6-way). The only failures were 20 in `publisher_never_waits_on_a_descheduled_reader`, where one publish sample exceeded its 1 ms wall-clock budget (1.0–6.3 ms) because the oversubscribed host preempted the publisher. The round-2 code fails that budget at the same rate under the same load (2 of 800 vs 1 of 800 here), and both pass 200 of 200 unloaded. So this is host preemption, not a wait on a reader. That per-publish budget was then replaced by the structural assertions above, and the rewritten test passed 1000 of 1000 runs of the `swap::` suite under the same 8-way oversubscribed load.

### Alternatives considered

| Alternative | Rejected because |
| --- | --- |
| Round 1: a single `std::sync::RwLock<P>` | A reader descheduled inside its read section blocks `publish` for a whole scheduler delay. That breaks requirement 1, and the chaos test proves it. |
| (A) `try_write` plus a side "pending" cell, drained by the next publish or reader, plus a clock-thread retry | The pending cell needs its own lock, which reintroduces the same descheduled-holder problem one level down. The newest value sits invisible until a drain happens, which breaks requirement 4 without an extra periodic retry. The slot ring is the generalisation of this idea with no pending state: the new value always goes into a free slot and is visible immediately. |
| (B) `crossbeam-epoch` (in `Cargo.lock`) | Its load and store API is `unsafe` (`Shared::deref`, `defer_destroy`), so it is unusable under `forbid(unsafe_code)`. A pinned, descheduled reader stalls epoch advance, so frame drops are delayed without bound. |
| (B) `haphazard` hazard pointers | It is not in `Cargo.lock`, and its last release (0.1.8) was in December 2023 (pre-1.0). Retiring a replaced pointer is an `unsafe` call, so it does not fit `forbid(unsafe_code)`. Its default domain is process-global reclamation state, the same shape we would have to audit for a #210-class bug. A std-only design needs no such audit. |
| (C) `left-right` / `triple_buffer` | `left-right`'s writer `publish` *waits* for readers of the old side to leave, which breaks requirement 1. `triple_buffer` (MPL-2.0, not in the lockfile) is single-reader by type and needs `&mut` on read, but `LatestSlot` and `TileStore` are shared by `&self` across clones. |
| Reader-side monotonic seq guard | It only catches *older* foreign values. A foreign value with a higher seq passes, and with a different `T` it is type confusion. |
| arc-swap 1.9.3 `rwlock-strategy` | It is a lock with the same preemption problem as round 1, on a crate whose default strategy has the open bug. |
| Round 3 (a): reader fallback that scans the ring for the newest readable slot | Needs per-reader "last seen" state that `LatestSlot::load(&self)` does not have (it would be a thread-local or an API change), and the scan must reason about slots mid-install. The tag claim removes the bad state instead of tolerating it. |
| Round 3 (c): readers try the two newest slots | Still needs proof that both cannot be locked at once by paused publishers; with N publishers they can. |
| A seqlock over `Arc` | It cannot be written in safe Rust: a pointer read racing a free needs hazard pointers or an epoch. |

### Consequences

- **Real guarantee.** The publisher is wait-free with respect to readers: no reader, however it is scheduled, can make it wait. Readers never wait on a publisher: no publisher, however it is scheduled, holds the head slot's lock, and a load attempt fails only if the head moved during it. A publish's work is a scan of the allocated slots (8, plus 8 per segment that pinned readers forced), plus at most one segment allocation. The only cross-thread wait left is two *publishers* racing to append a segment, and production slots have one publisher. Loads never block and are lock-free. Any change that makes a publish wait on a reader's lock is a regression against this ADR.
- A load can only return a value published into that cell. This is tested in `tests/concurrency.rs` (cross-instance foreign read, single-reader non-regression) and in `swap.rs` (chaos, pinned growth, racing publishers).
- Cost: one 8-slot segment per cell (8 `RwLock`s, about 128 bytes more than round 1) and a briefly retained previous value. Segments are freed only with the cell.
- **Not yet migrated** (still on arc-swap 1.9.2 `DefaultStrategy`, exposed to #210): `multiview-audio` `AudioStore` window, `multiview-engine` `migration.rs` `DeviceLoadBoard`, and `ArcSwap` uses in `multiview-input`, `multiview-overlay`, `multiview-compositor` (watermark), `multiview-hal` (load) and `multiview-cli`. They were left out to keep #133 scoped to the invariant-#2 primitive. Each should move to `SwapCell`'s pattern until #210 is fixed upstream. They are tracked in the PR's `## Concerns`.
