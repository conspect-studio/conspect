# Recorder writer — proposed off-clock handoff and shutdown

Status: **proposal; not implemented or panel-approved**. This note defines the
contract for a later R3 implementation. It does not claim that recording, arm
checks, telemetry, or a recorder writer exists today. The cross-vendor R3 panel
must review the ownership, packet conversion, memory and shutdown protocol before
implementation.

## Verified boundary and proposed attachment

The live output path offers each `StreamItem` through the bounded hot queue with
`send_or_forced_shed` and counts a full queue; the offline `BlockForExact` path
is explicitly different (`crates/multiview-cli/src/pipeline.rs:6654-6685`,
`7524-7539`). `multiview-bake-consumer` is created as a distinct OS thread and
calls `consumer_main` (`pipeline.rs:7974-7977,8033-8044`). On that thread the
consumer bakes the frame (`pipeline.rs:9933-9937`), and drains encoded worker
events into the rendition dispatcher (`pipeline.rs:9825-9849`). The dispatcher
handles encoded video events and stages each packet (`crates/multiview-cli/src/rendition_dispatch.rs:885-922`);
its stage checks packet and byte bounds (`rendition_dispatch.rs:261-308`).

Proposed attachment: on that **bake-consumer thread**, after encoded events are
received and before ownership is moved into the selected rendition's staging
path, offer a bounded copy of each selected Program encoded packet to its
recorder. Include program and selected discrete audio and preserve stream kind,
PTS, DTS, duration, keyframe and codec parameters; do not copy raw canvas frames
or re-encode. `EncodedPacket::payload` exposes borrowed coded bytes and timing
accessors (`crates/multiview-ffmpeg/src/packet.rs:99-114,170-200`). Exact
ordering, dynamic rendition moves, codec-parameter changes and multi-track
completeness require an implementation design review; a raw packet tap must not
claim a muxable atomic output window merely because individual packets arrived.

The off-clock claim is conditional on the thread boundary above and the live
nonblocking queue. Copy cost still consumes bake-consumer capacity: excessive
copying can fill the hot queue, cause output substitution/fatal behavior, and
therefore is not automatically safe merely because it is on another thread.
Admission must impose fixed packet-size and per-tick work bounds and reject
unsupported loads before arming. Any future move of dispatch or copying onto the
clock invalidates this design and requires fresh R3 review and real-clock proof.
Never copy, allocate, free, lock, await, touch disk, or wait for recorder work on
the clock. Offline blocking mode is not evidence for live isolation.

## Ownership, bounds and loss

Construct the channel and pool off-clock. Move the unique `Consumer<PacketSlot>`
and `BufferRecycler` into exactly one dedicated writer thread; move the unique
`Producer` and `BufferSource` into the bake consumer. Do not clone or wrap the
consumer in `Arc<Mutex<_>>`, share it with a shutdown supervisor, or add a second
reader. The current endpoints are non-Clone and operate through `&mut self`
(`crates/multiview-spsc/src/lib.rs:22-30,51-56,83-85`); the implementation must
preserve that structural ownership across construction, restart and teardown.

Preallocate N buffers of maximum P bytes and N queue slots during arm. The media
payload budget is N × P bytes per recorder, plus fixed queue/metadata and bounded
writer/muxer working memory; enumerate and cap each overhead and cap the number
of recorders globally before admission. Checked multiplication must reject an
overflow or over-budget request. Reject/count packets larger than P; never grow a
buffer. The existing pool allocates fixed buffers (`lib.rs:175-205`), and a
buffer dropped without recycling is excluded until teardown (`lib.rs:135-160`).
Thus at most N buffers can be stranded per admitted recorder, not an unbounded
allocation. It is a bounded capacity leak, still a recording failure requiring a
visible count. `Producer::push` returns the item when full (`lib.rs:51-65`): it
does **not** provide drop-oldest. The producer must not free or recycle an old
slot it does not own. Define a bounded overload policy explicitly: count and
reject the new packet when full or no buffer exists; recycle rejected ownership
safely off-clock and signal loss. This is drop-new, not the earlier drop-oldest
claim; the panel must approve the policy and GOP/keyframe recovery before code.

The landed ring currently exposes only a leaked-buffer count, not deferred drops,
hard drops, dropped bytes, stalled state or last error (`lib.rs:218-245`). Add
these as bounded recorder-owned telemetry, not invented ring capabilities:
monotonic attempted/admitted/lost records and bytes partitioned by reason,
queue/pool pressure, writer-stalled state, and a bounded sanitized last-I/O-error
class. Sample/publish off-clock. Raise and clear typed `HealthWarning` events;
existing warning ingest accepts raised/cleared events and coalesces them
(`crates/multiview-control/src/warning_ingest.rs:225-236,255-263`). Since that
subscriber may lag, a durable snapshot/reconciliation path is required; event
publication alone cannot prove health visibility. No loss or I/O error is silent.

## Mux and shutdown

Only the writer thread calls blocking `PacketMuxSink::run_av_tracks`; it opens a
muxer, registers streams, drives `PacketSource`, then finalizes
(`crates/multiview-output/src/sink.rs:2131-2147,2150-2179`). `PacketSource`
returns an owned `EncodedPacket` (`sink.rs:125-139`): the future adapter must
specify a safe off-clock conversion from pooled bytes to this owned type,
including optional timestamps and decoder configuration, without falsely
claiming zero-copy. It must test packet fidelity, timing and mux output. The
present code does not provide this adapter.

A stalled disk can hold the writer indefinitely inside native I/O, so it stops
draining; the finite pool/queue then fills and admissions drop/count rather than
waiting. A separate off-clock supervisor detects lack of writer progress, reports
stalled and last error when known, and does not assume a stuck syscall can be
cancelled. Do not open an unlimited succession of replacement writers: fence each
recorder generation and cap detached wedged resources. This is a bounded leak,
not proof of successful finalization.

For shutdown, first stop all upstream producers and close admission with an
atomic producer-side handshake: every in-flight offer either finishes admission
or is accounted as rejected before the drain starts. Then signal end-of-stream
to the sole writer, drain queued packets and finalize the muxer up to a fixed
monotonic deadline. The writer alone pops and recycles. The supervisor waits only
until that deadline; after it, count every queued and in-flight item as
**unconfirmed/lost**, expose a terminal incomplete outcome, and fence the old
writer so it cannot publish or write into a replacement generation. Do not claim
actual loss for an in-flight write whose eventual outcome is unknown; keep it
separately counted as unconfirmed. A native I/O call cannot safely be killed by
Rust thread cancellation: the implementation must specify an OS-level timeout or
process isolation for a hard resource bound, or explicitly refuse re-arm while a
wedged writer remains. Merely dropping a `JoinHandle` is not a close or flush.
A pre-existing bounded-join test demonstrates the distinction between a wedged
consumer and a completed teardown (`pipeline.rs:8481-8507`); it does not prove
this recorder protocol. Normal shutdown must close the store only after the
writer has finished; a deadline failure must be observable after control shutdown.

## Arm admission and verification

Config validation is lexical/schema-only; it does not perform filesystem I/O.
The off-clock ARM admission service must resolve an authorized configured target,
check supported format/feed and budgets, safely open/probe the actual destination
with the engine's credentials, check free-space policy, and return a typed
refusal on failure before creating a producer or reporting an armed state. Avoid
TOCTOU by using the opened directory/file handle for subsequent writer creation;
a successful probe is not a guarantee against a later full disk. On any later
write failure the writer pauses/disarms and reports the typed warning and loss.

Required implementation evidence, before any merge:

1. Instrument the real live output tick path and baked/encoded handoff. Run a
   healthy baseline, then wedge the actual recorder writer in native write and,
   separately, inject ENOSPC at segment open/write/finalize. For a fixed tick
   interval assert one correctly stamped emitted frame for **every tick**, no
   extra/missing tick, and unchanged healthy output; recorder dropped-record and
   dropped-byte counters must rise with attributable reasons. Compare to the
   existing live hot-queue behavior, not an unattached ring or mock-only clock.
2. Flood beyond queue, byte and packet-size bounds; assert fixed high-water
   memory, exact accounting, no producer blocking/allocation/free and no output
   clock stalls. A deliberate blocking or partial-loss mutant must make the
   assertions fail. Measure copy-time/cadence under worst allowed packet size and
   recorder count; refuse arming if the admitted budget cannot meet it.
3. Race stop with producer offers; assert accepted = written + known lost +
   unconfirmed, including queued and in-flight packets. Test clean drain,
   timeout, permanently wedged muxer, stale generation, re-arm and no duplicate
   terminal outcome. Bound shutdown wall time and number of stranded writers.
4. Exercise ARM with missing, inaccessible, read-only and full destinations;
   assert typed rejection and no write on clock. Test late ENOSPC, recovery,
   warning raise/clear/coalescing and HTTP health/telemetry readback after lag.
5. Cross-vendor R3 panel and independent code review must settle packet ordering,
   resource bounds, shutdown fencing, async warning recovery and every invariant
   above. No success claim follows from unit tests alone.
