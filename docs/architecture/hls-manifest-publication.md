# HLS manifest publication off the packet path (design note, #220)

## Problem

The live HLS sink republishes its media playlist on every closed segment through
`hls::live::atomic_write`: write `<name>.tmp`, `fsync`, `rename`. That ran inline on the
sink thread, between packets. On a VM disk the `fsync` measured p50 33 ms and max 245 ms
(176 calls over 5 ms in 60 s; a rendition output does several per segment close). While
the sink thread sat in `fsync` its 4-packet queue filled, the bake consumer's bounded send
waited, the 4-frame hot-loop queue filled, and the engine shed ticks — reported as
"dropped N under encoder overload" although bake (p99 17 ms) and NVENC (≤ 3 ms) were well
inside the 33.4 ms budget. That is a slow output back-pressuring the engine (invariant #10),
which then sheds output frames (invariant #1 holds — the clock kept ticking — but the
program faltered).

## Decision

Manifest durability stays exactly as strong (tmp + `fsync` + `rename`, prune only after a
successful publish, never unlink a segment any on-disk manifest still lists), but it moves
**off the packet path**:

1. `LivePlaylist` gains an opt-in background publisher
   (`enable_background_publish`). With it on, `push_closed_segment` renders the new
   manifest in memory (pure, microseconds) and hands `(text, newly retired segments)` to the
   publisher through a **latest-wins slot**: only the newest manifest matters, so a
   publisher that falls behind skips intermediate manifests instead of queueing them.
   Retired-segment lists from coalesced jobs are concatenated (bounded, see 4).
2. One named thread per playlist (`hls-publish`) does the disk work: `atomic_write`, then
   the existing prune rule (retain one further window, at least three segments; skip any
   file an on-disk manifest still references). The sink thread only takes the slot's mutex
   to swap a job in; the publisher never holds that mutex across I/O. The sink therefore
   never waits on disk.
3. A failed publish keeps its retired segments for the next successful publish (nothing is
   unlinked after a failed write, so every URI of the last good manifest stays valid) and
   records the error; the next `push_closed_segment` or `finalize` returns it, so a broken
   output still surfaces as before, one segment later.
4. Bounds: the slot holds one manifest. The carried retired list is capped
   (`MAX_CARRIED_RETIRED`); beyond the cap the oldest paths are dropped from the list —
   those files are then left on disk rather than growing memory (a disk that has failed
   every publish for that long is already a reported output failure).
5. `finalize` enqueues the `#EXT-X-ENDLIST` manifest and joins the publisher, returning
   any publish error. This blocks exactly as the synchronous finalize did (it runs at end of
   run, never per packet).
6. The rendition master playlist is static for a run: it is queued once, to its own
   background writer, when all its media playlists first exist (never re-`fsync`ed per close
   and never written inline). Pruning of the previous run's files still waits until the
   on-disk master equals the new text; `finish` publishes the master if a short run closed
   before its media writes landed, then reaps the prior generations.
8. **No write outlives its run.** `Drop` of an abandoned publisher (an errored sink) never
   waits for a write or fsync. It cancels a `PublishFence`; the background writer makes a
   manifest visible only inside `PublishFence::commit`, which holds the fence lock for the
   rename alone, so cancellation and publication are serialized — after `Drop` returns, that
   publisher can no longer rename. Every background write uses a unique temp name
   (`<name>.<pid>.<seq>.tmp`) and removes only its own temp, so two runs never share a temp
   inode. `Drop` can wait at most for one in-flight `rename` (metadata only), never an fsync.
7. A surfaced publish error re-queues the committed (failed) manifest and does not commit
   the close that observed it, so the next successful publish still lists every URI the
   failed manifest listed. The carried retired list is re-capped after every merge.

The synchronous mode (the default) is unchanged and keeps its existing tests: publish
failure returns `Err` from the same call and leaves committed state untouched.

## Evidence required

- Unit chaos test: an injected 300 ms manifest writer; 12 segment closes must each return
  in a few milliseconds, the final on-disk manifest must equal the last rendered one, and
  pruning must match the synchronous rule.
- Unit failure test: an injected failing writer must surface the error on a later call and
  must not prune any segment.
- Hardware: the production-host synthetic NVENC config must show no shed frames and no slow
  `fan_packets` iterations, where `main` sheds.
