# Data-plane safety rules (engine + FFI + process)

These eight rules are the concrete, at-the-keyboard form of the canonical invariants in
[`conventions.md` §5](conventions.md) — above all **#1 (output-clock)** and **#10 (isolation)**.
They are non-negotiable for code in this repository.

> **Stable anchors.** These rules previously lived in the root `CLAUDE.md` §7. The numbering below
> is unchanged and is stable. Existing citations in source comments and docs of the form
> *"safety rule §5"*, *"safety rules §4"*, or *"CLAUDE.md §7.3"* refer to the correspondingly
> numbered rule on this page. Do not renumber them.

---

## 1. Never break the output-clock invariant

The output stage emits one valid, correctly-timestamped frame per tick, forever. **No code path on
the data plane may block waiting for an input, a client, or a lock that an input/client holds.**
Inputs are sampled; they never pace.

Why: input-clock-driven output *will* stall on input loss — reproduced across FFmpeg, GStreamer, VLC
and MEncoder — and a shared filtergraph is gated by its slowest input
([ADR-R001](../decisions/ADR-R001.md), [ADR-T001](../decisions/ADR-T001.md)).

## 2. Preview / control / realtime must never back-pressure the engine

Use watch/broadcast channels and bounded **drop-oldest** queues. The engine **never `.await`s a
client** and never sends on a channel a slow consumer can fill. Conflate high-rate telemetry (audio
meters ~10–30 Hz). If you add a channel from engine→outside, prove it cannot stall the engine.

Enforced by `crates/multiview-engine/tests/isolation.rs` (7 chaos tests: stalled subscriber, crashed
consumer, wait-free publish, drop-oldest ordering, resume-by-seq). The module docs of
`crates/multiview-engine/src/isolation.rs` record the regression this exists for — a
`Mutex<VecDeque>` ring a consumer could hold *did* back-pressure the engine and was removed.
([ADR-RT004](../decisions/ADR-RT004.md), [ADR-P001](../decisions/ADR-P001.md))

## 3. No `unwrap`/`expect`/`panic!` on the hot path

Decode→composite→encode→mux, the output clock, and the frame stores return or handle errors and
**hold last-good** rather than crashing. Mechanically enforced repo-wide (not just the hot path) by
the deny-level `unwrap_used`/`expect_used`/`panic` lints in root `Cargo.toml` `[workspace.lints]`;
`unwrap` is acceptable only in tests and in clearly non-hot startup/config code with a justification.

## 4. FFI safety

All raw libav/CUDA/Metal/Vulkan/NDI FFI lives behind safe wrappers — FFI is owned by
`multiview-ffmpeg` and the feature-gated backend leaves (`multiview-{ndi,rist,ntp,i915pmu}sys`,
`multiview-webrtc`). Those crates override `unsafe_code` from `forbid` to `deny`; every `unsafe`
block carries a `// SAFETY:` comment stating the invariant upheld.

- **libav context wrappers are `Send + !Sync`** (or Mutex-guarded). Distinct contexts on distinct
  threads are fine; per-context access must be synchronized. Internal frame-threading callbacks can
  fire from multiple threads — expose frames only after the decoder yields ownership.
  ([core-engine §12](../research/core-engine.md), [ADR-0009](../decisions/ADR-0009.md))
- **`extern "C"` callbacks run on foreign/decoder threads.** `get_format` and the log/IO callbacks
  are entered by libav on a thread Rust does not own. Keep them **allocation-light**, and **never
  let a Rust panic unwind across the FFI boundary** — unwinding into C is undefined behaviour, so
  the boundary function must `catch_unwind` and convert to a sentinel return
  (`AV_PIX_FMT_NONE` for `get_format`). This is why `get_format` is unavoidably `unsafe extern "C"`:
  no fully-safe hwaccel path exists in any binding ([ADR-0002](../decisions/ADR-0002.md)).
- **Return buffers to pools via `Drop` — never run that `Drop` inside a Tokio async destructor.**
  A releasing `Drop` may only *offer* the owning actor with a non-blocking `try_send`; it must never
  block, spawn a thread, or join a decode thread
  ([ADR-0009](../decisions/ADR-0009.md), [ADR-0030](../decisions/ADR-0030.md)).

## 5. Bounded memory everywhere on the data plane

Queues drop, never grow. No unbounded channels into the engine. Frame buffers come from per-device
pools allocated at start, **never per-frame** — the steady-state compose→encode→mux path targets
zero heap/GPU allocation ([efficiency-budget](efficiency-budget.md),
[ADR-E005](../decisions/ADR-E005.md)).

## 6. Timestamps

Never feed raw input PTS to the encoder/muxer — **re-stamp from the tick counter**. Carry internal
time as i64 ns / exact rationals; **never float fps** (`29.97f` drifts ~3.6 s/hour and eventually
produces non-monotonic or duplicate PTS). Test-enforced by
`crates/multiview-engine/tests/clock.rs`. ([ADR-T001](../decisions/ADR-T001.md),
[timing-and-sync](timing-and-sync.md))

## 7. Color pipeline order is fixed

The nine-step order in [`conventions.md` §5](conventions.md) invariant #8 is never reordered. Range
is handled in-shader **exactly once** — a double expand (hardware decoder or an auto-`swscale` leg
converting before your code) is the classic silent corruption. **Tagging ≠ converting:** if shader
output does not match the declared tags the file is silently wrong with zero errors, so always tag
the output *and* verify with `ffprobe`. ([color](color.md),
[ADR-C001](../decisions/ADR-C001.md)–[C003](../decisions/ADR-C003.md))

## 8. Licensing discipline

Keep the default build **LGPL-clean**. Do scaling/compositing in-house — `scale_cuda` (LGPL,
JIT-compiled via `--enable-cuda-llvm`), **never `scale_npp`** (`--enable-nonfree` ⇒ not
redistributable). `gpl-codecs` (x264/x265) makes the whole build GPL — opt-in only. Never vendor the
proprietary NDI SDK; the `ndi` feature runtime-loads (`NDIlib_v6_load()`), stays inert until the
operator accepts the license, and carries mandatory trademark attribution. `deploy/ffmpeg-build.sh`
hard-rejects the forbidden configure flags, and CI `cargo deny` gates licenses and advisories.
([ADR-0012](../decisions/ADR-0012.md), [ADR-0031](../decisions/ADR-0031.md),
[ADR-0008](../decisions/ADR-0008.md), [conventions §7](conventions.md))

---

## Concurrency model (the shape these rules assume)

- **Two planes.** A **data plane** of dedicated OS threads runs the codec/composite/encode hot path;
  long synchronous codec/CUDA/VideoToolbox calls **must never** run on Tokio workers. A
  **control/IO plane** uses Tokio for networking and the HTTP/WS API.
  ([ADR-0009](../decisions/ADR-0009.md), [overview §4](overview.md))
- **One decode actor per source**, feeding a small bounded drop-oldest queue — per-source isolation
  prevents head-of-line blocking, so one dead RTSP camera never freezes the multiview.
- **Channels carry ref-counted pooled frame handles, never pixels.**
- **Zero-copy islands per vendor.** Keep decode→composite→encode on one device; cross-vendor on-GPU
  zero-copy **does not exist on desktop** — insert exactly one explicit, costed copy at any
  vendor/NDI/CPU boundary ([ADR-0004](../decisions/ADR-0004.md),
  [hardware-and-efficiency](hardware-and-efficiency.md)).

**If a change risks invariant #1 or #10, stop and write a design note plus a chaos/soak test** before
implementing. Those two invariants are blocking gates that do not relax
([ADR-G005](../decisions/ADR-G005.md), [ADR-G007](../decisions/ADR-G007.md)).

Depth: [pipeline](pipeline.md) · [core-engine](../research/core-engine.md) ·
[resilience](resilience.md) · [streaming-gotchas](../research/streaming-gotchas.md)
