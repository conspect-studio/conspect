# ADR-0109: The output clock runs on a dedicated OS thread with an absolute-deadline sleep

- **Status:** Accepted
- **Area:** Core engine (invariant #1, with #10)
- **Date:** 2026-11-12
- **Source:** issue #311 (follow-up of the front door, [ADR-0107](ADR-0107-single-front-door-443.md)); test-server measurement of 13 Nov; #311 reopen (§8)
- **Amends:** [ADR-T001](ADR-T001.md) (pacing mechanism only; deadlines unchanged), [ADR-0030](ADR-0030.md) §2 (a `Program` runs on its own thread, not a task)

## Context

Every program's tick loop was a `tokio::spawn`ed task paced by `tokio::time::sleep`
(in steps of at most 10 ms) on the `#[tokio::main]` multi-thread runtime. The same
runtime runs the whole control plane: axum, WebSocket/SSE, config watch,
persisters, reapers, the legacy WebRTC driver and MoQ listener, and every
`tokio::spawn` a library makes from it.

Measured on the test server (main 11c61b04, release build, RTX 4060, 12 sources, 2
outputs, no viewers), from `GET /api/v1/metrics/program`: of 5,099 ticks, 5,031
were 4–8 ms late, 63 were 8–16.7 ms late, 2 were 16.7–33.4 ms late; max 28.9 ms.
ADR-0107 slice 1 measured the same in the build container: p50 4.5–5.6 ms and p99
8–14 ms **idle**. Every tick is late by milliseconds because the wake goes through
tokio's 1 ms timer wheel plus a worker hop, and any busy task on that runtime
delays it further. Invariant #1 asks for one frame per tick, on schedule,
independent of everything else in the process.

## Decision

Each program's tick loop runs on a **dedicated, named OS thread** (`clock-<program>`),
and the production pacer waits for each tick with a **blocking absolute-deadline
sleep** on that thread. The design note is this ADR; the code is
`crates/multiview-engine/src/clock_thread.rs`.

### 1. Where the thread boundary is

The boundary is the existing wait-free publish edge. On the clock thread runs
exactly what `EngineRuntime::run_inner` already ran: pace → (cadence-hold
repeats) → frame-boundary control hook → compose → projections (`state_of`,
`event_of`) → publish. Everything the loop touches was already non-blocking:

| Touch point | Mechanism | Blocks on a consumer? |
| --- | --- | --- |
| state snapshot | `LatestSlot` (framestore `SwapCell`: `try_write`, never waits on a reader) | no |
| events | `tokio::sync::broadcast::Sender::send` (sync, never awaits; overwrites the oldest) | no |
| encode/egress hand-off | `std::sync::mpsc::SyncSender::try_send` (drop and count on full) | no |
| routing aux tick (ADR-0108 §4.4) | one `try_send` while a routed feed is active | no |
| command drain (control hook) | `tokio::sync::mpsc::Receiver::try_recv` (sync) | no |
| preview / display / NDI taps | `SwapCell::store` | no |
| input sampling | per-source `TileStore` last-good (lock-free) | no |

No tokio API on the tick path needs a runtime *worker*: `broadcast::send` and
`mpsc::try_recv` are plain synchronous calls. The thread nevertheless **enters the
spawning runtime's context** (`Handle::enter`), so a library call that reaches for
`tokio::spawn` from a hook lands its task on the control runtime instead of
panicking; the clock never awaits such a task. No lock is shared with an async task
on the tick path.

`run_inner` keeps its `async` signature: its only `.await`s are the pacer wait and
(new) the successor's wait for a handoff cut (§4). On the clock thread the
production pacer completes inside `poll` (§2), so each tick is one synchronous
poll of the loop future by a minimal `block_on` (poll; park on `Pending`; no
reactor, no timer wheel). `CooperativePacer` and `ManualTimeSource` stay for the
deterministic tests, unchanged.

### 2. Pacing

Deadlines are unchanged and still come from the integer tick counter:
`deadline(i) = seed + pts_at(i)`, exact rational, never float fps, never
accumulated. The wait is re-sized from the absolute deadline after every wakeup,
so an early or late wake never compounds.

* **Linux:** `clock_nanosleep(CLOCK_MONOTONIC, TIMER_ABSTIME)` via rustix's safe
  wrapper (rustix owns the syscall; the engine keeps `#![forbid(unsafe_code)]`, so
  there is no `unsafe` block to annotate). `EINTR` resumes against the same
  absolute instant. The thread sets its timer slack to 1 ns (`PR_SET_TIMERSLACK`)
  so the kernel does not defer the wake by the default 50 µs.
  `std::time::Instant` (the `MonotonicTimeSource`) is `CLOCK_MONOTONIC` on Linux,
  so the sleep and the deadline arithmetic use the same clock.
* **macOS, Windows, other targets** (macOS and Windows are CI targets): a portable
  fallback. `std` has no absolute-deadline sleep, and a relative sleep may wake
  late by an amount that grows with its length (macOS timer-coalescing leeway,
  Windows timer resolution). So the fallback never sleeps more than **half** the
  time left: even a wake late by 100% of the request lands before the deadline.
  Within the last 500 µs it busy-spins (`hint::spin_loop`) on the time source.
  Not a yielding spin: on macOS `sched_yield` is `swtch_pri`, which depresses the
  thread's priority for a scheduler quantum (~10 ms). The first CI runs (one long
  sleep to 500 µs before the deadline, then a yield or busy spin) showed ~22 ms
  mean lateness and ~20% repeats on a saturated macOS runner, i.e. the long
  sleep overshooting; the halving schedule removes that. Cost: about
  log2(period / 500 µs) ≈ 6 wakes per tick plus at most 500 µs of spinning.
  On macOS the clock thread also marks itself QoS `USER_INTERACTIVE`
  (`pthread_set_qos_class_self_np`, no privilege, this thread only), the class
  Apple documents for animation and event handling: a default-QoS thread's timer
  wakeups are deferred behind every runnable thread on a saturated host, which a
  pacing schedule alone cannot fix. The one FFI call lives in the leaf crate
  `multiview-clocksys` (`unsafe_code = "deny"`, `// SAFETY:` note, mirrors
  `multiview-ntpsys`); it is a no-op that links nothing on other targets.
* Every sleep step is capped at 10 ms (`STOP_POLL`, the same bound the async pacer
  had), so a raised `StopSignal` is observed within one step even on an unreachable
  deadline (a frozen clock).
* `RealtimePacer` picks the blocking sleep only when it runs on a clock thread
  (a thread-local marker set by the clock-thread spawner). Off a clock thread (a
  test or tool that still drives a runtime on a tokio task) it keeps the old async
  sleep, so it can never block an executor worker.

**Scheduling (optional, reported, never required).** The thread asks for niceness
−10 (Linux, `setpriority(PRIO_PROCESS, gettid())`, which on Linux applies to that
one thread; a unit test checks the call changes only the calling thread). Without `CAP_SYS_NICE` the call fails and the
thread runs at the default priority; either way the start-up log line
`output clock thread started` names the pacing mechanism, the timer slack and the
priority outcome. `SCHED_FIFO` is deliberately **not** used: a real-time policy lets
one bug that spins the loop starve the host, and the measurement does not need it.
Since the #311 reopen the thread also asks for a short EEVDF request size
(`CLOCK_SLICE`, 300 µs; no privilege; §8), also named in that log line.

### 3. Production wiring

* `ProgramSet::start` spawns the per-program `Supervisor` loop on a clock thread
  (`clock_thread::spawn`, `block_on`), owning the program (`'static`).
  `ProgramSet::stop`/`shutdown` raise the stop signal and await the thread's result
  through a oneshot with the existing 2 s grace (`PROGRAM_JOIN_GRACE`), so the
  control task never blocks on a join. An OS thread cannot be aborted: a thread
  whose loop cannot reach its stop check is **detached** at the grace (it owns only
  its own program and exits at its next stop check), which is the same bounded
  posture as before (#95).
* The daemon's pipeline (`Pipeline::drive_streaming`) and the software engine's
  forever run borrow their caller's pipeline, publisher and preview slot, so their
  loop future is not `'static`. They use `clock_thread::run_on_clock_thread`: the
  calling task hands its worker back to the runtime with `block_in_place`, and a
  scoped clock thread drives the borrowed loop future with `block_on`. The runtime
  keeps scheduling every other task on its remaining workers (the main runtime has
  one worker per core). Under a `current_thread` runtime (some tests)
  `block_in_place` is not allowed, so the future is awaited inline as before:
  correct, not on a dedicated thread. Ingest, encode and sinks were already OS
  threads; only the tick loop moved.

### 4. Program migration: thread handoff with no missed or doubled tick

`migration.rs` previously relied on tasks only for teardown. The new primitive is
`ProgramSet::start_successor(old_id, new)`:

1. NEW is built with OLD's seed (`Program::multiview_succeeding`, from
   `ProgramSet::seed_nanos`) and cadence, so tick `i` has the same index, PTS and
   deadline in both.
2. NEW starts on its own clock thread and waits, off the emit path, for OLD's cut.
3. `Handoff::request()` asks OLD to hand off. Only **OLD's own loop** chooses the
   cut, at a frame boundary: `cut = next + 2` (`HANDOFF_LEAD_TICKS`). OLD emits every
   tick below `cut`, including any cadence-hold repeats, and returns
   `RunStop::HandedOff` without emitting `cut` (also after a catch-up `skip_to` that
   jumps past it).
4. NEW skips its clock to `cut` and paces to it; it has two full periods to see the
   cut before it is due.

Edge cases, each with a test:

* A catch-up resync (`skip_to` after `MAX_REPEATS_PER_TICK` repeats) is clamped
  to the cut, so a long deschedule never jumps OLD past it and leaves
  `[cut, floor)` to nobody.
* If OLD's loop ends for any reason (stop, budget, error) after a request but
  before it chose a cut, the exit publishes `cut = next` (the first tick it did
  not emit), so NEW never waits for a predecessor that is gone.
* OLD's clock never rewinds, so a re-run of OLD after a handoff emits nothing.
* If OLD does not choose a cut within the 2 s grace (a wedged or slow loop),
  `start_successor` withdraws the request (`Handoff::retract`, a compare-and-swap
  that fails only if OLD chose a cut meanwhile, in which case the handoff stands),
  stops and removes NEW and returns an error. OLD keeps running untouched.

The handoff state is one atomic (idle / requested / cut), every transition a
compare-and-swap: no lock, no channel, no await. Because OLD chooses a cut
strictly ahead of everything it has emitted and never emits it, and NEW begins
exactly at it, the union of both programs' emitted indices is contiguous with no
duplicate. The existing make-before-break crosspoint (`OutputCrosspoint`) and
`drain_stop` are unchanged.

### 5. Shutdown, panics, falling behind

* Stop while sleeping: the sleep wakes within `STOP_POLL` and the loop returns
  without composing (unchanged contract).
* A panic in an egress consumer (its own thread) never reaches the clock: the
  bounded `try_send` sees `Disconnected`/`Full` and counts a drop. A panic **on**
  the clock thread (a hook bug) is caught at the thread boundary
  (`catch_unwind`), logged, and reported to the joiner as an error
  (`ClockThreadPanic`) for `ProgramSet` programs; `run_on_clock_thread` resumes it
  on the caller, exactly as awaiting the loop inline would have. Exit behaviour on
  a fatal loop error is unchanged (issue #309 owns it).
* The per-program supervisor's restart backoff runs on the clock thread too:
  `Supervisor::supervise_until` sleeps it in 10 ms steps (the runtime context the
  thread entered drives the timer and wakes the thread) and ends supervision when
  the program's stop is raised, so stopping a failing program never waits out a
  backoff and never detaches a parked thread.
* Falling behind keeps the ADR-T018 repeat/skip accounting unchanged.

### 6. Lateness bound the tests assert

A tick falls "behind" (a repeat) only when the loop wakes more than one period
late. With the loop alone on its thread, its lateness is the OS wake latency plus
compose time, independent of the control runtime: on the build host the bare
absolute-deadline sleep wakes with p50 ≈ 17 µs, and the full `EngineRuntime` on a
clock thread with an idle runtime averages ≈ 0.3 ms. Under CPU oversubscription
(a shared CI runner, or this build host at load 80 on 56 cores) what remains is
the OS run queue, which no pacing mechanism controls. The chaos test therefore
asserts bounds that hold on a loaded runner (#321): with the control runtime
saturated (a CPU-bound task hogging every worker, blocking calls, a timer storm)
and a front-door flood, every tick is emitted (count within 2 of
elapsed/period) on every platform; on **Linux** additionally 0 repeats, p99
lateness at or below **half a period**, and a clock-thread mean below the old
task topology's under the same load.

**Known gap, macOS (#340).** On a CPU-saturated 3-core macOS CI runner (debug
build) the clock thread still emits one frame per tick, but falls behind and
repeats (~12–20% of ticks, ~17–20 ms mean) where the old task topology did not —
with the yielding spin, the halving sleep and QoS `USER_INTERACTIVE` alike. The
macOS chaos assertion is therefore cadence-only until #340 is investigated on a
real Mac (release build, Mach time-constraint policy). Production runs on Linux. Lateness in the chaos test is read
from the engine's own wait-free histogram (fine buckets), so nothing but atomics
sits on the tick path. The tight target is measured on the
test server after deploy: p99 < 2 ms and 0 ticks behind under idle, control-plane
load and front-door flood.

### 7. Measuring it

Since the #311 reopen the same endpoint also reports, as separate histograms on
the fine edges (each with `window_ticks`, `mean_ms`, `p50_ms`, `p99_ms`,
`max_ms`, `mean_total_ms`, `buckets_window`, `buckets_total`):
`emit_lateness` (the value of the `lateness_*` fields), `wake_lateness` (the
pacer's wait returning minus the deadline, before any work), `compose_duration`
(`before_compose` through `publish_state`, fresh ticks) and `compose_phases`
(`hook`, `composite`, `after_compose`, `project`). A tick whose deadline
passed while the previous tick's work ran records that backlog as its wake
lateness, so `wake_lateness` isolates the scheduler only while
`compose_duration` stays under a period. Each phase boundary is one
time-source read and a few relaxed atomics on the clock thread.

The original fields:

`GET /api/v1/metrics/program` gains, without changing any existing field, the
window and run **mean** (`lateness_mean_ms`, `lateness_mean_total_ms`), fine
quantiles (`lateness_p50_fine_ms`, `lateness_p99_fine_ms`) and fine buckets with
edges 0.05, 0.1, 0.25 ms below the original 0.5 ms (`fine_bucket_edges_ms`,
`fine_buckets_total`, `fine_buckets_window`). The coarse buckets are derived from
the fine ones, so recording still touches one atomic bucket.

### 8. The #311 reopen: where the remaining lateness came from, and the bound

The test server (10 cores, load 9, engine at 204 % CPU, 12 decoding sources, 2
NVENC outputs) measured emit lateness idle mean 4.1 ms, none under 2 ms, and
under control load (407 `GET /state`/s + 20 SSE clients) mean 8.6 ms with 86 of
3,598 ticks past half a tick; 0 repeats. The old metric was recorded after
`publish_state`, so it was wake **plus** the whole tick's work.

Measured on the build host with a release build (12 looping H.264 TS sources
decoded on the CPU, 4×4 grid, two HLS outputs, the CPU compositor because the
host has no GPU), with the split metric:

* **Idle wake lateness is not the problem.** Wake mean 0.02–0.05 ms, p99 ≤ 0.1 ms
  whenever the clock's core was not contended. The idle floor on the test
  server is the tick's own work (`compose_duration`), dominated by `composite`
  (on the server: GPU upload, submit and readback of 1080p); the hook,
  `after_compose` and `publish_state` are each ≤ 0.01 ms.
* **Under load the wake is what degrades**: with CPU-bound neighbours on the
  clock's core the wake tail grew to p99 8–33 ms. The clock thread is a
  default-weight `SCHED_OTHER` thread and on EEVDF (Linux 6.6+) a waking thread
  with the default ~3 ms request waits behind the running thread's slice.
  A C rig isolates it: a 29.97 fps clock thread (`TIMER_ABSTIME`, 1 ns slack,
  4 ms of work per tick) and 6 bursty CPU-bound threads (8 ms on, 3 ms off) on
  the same 2 cores, 600 ticks per run. The "nice −10" rows were run unprivileged
  as the equivalent relative weight (neighbours at +10):

  | Clock thread | wake mean | wake p99 | emit p99 (4 ms work) | ticks > ½ period |
  | --- | --- | --- | --- | --- |
  | default | 2.5–2.9 ms | 12–14 ms | 19–22 ms | 13–52 |
  | nice −10 (needs `CAP_SYS_NICE`) | 0.9–1.1 ms | 5.8–7.3 ms | 10–12 ms | 0 |
  | slice 300 µs | 0.16 ms | 3.0–3.8 ms | 20 ms | 10–20 |
  | slice 300 µs + nice −10 | 0.06–0.12 ms | 2.5–4.3 ms | 9–12 ms | 0 |

  On one core with six pure spinners: default wake p99 12.4–12.7 ms, slice 2.2–2.3 ms.
  (The first cut of this change never applied the slice: `sched_setattr` needs
  `attr.size` set, and `sched_getattr` does not fill it. Fixed in review; the
  read-back now must equal the clamped request to count as applied.)
  The slice fixes the wake; niceness (weight) is what keeps the clock's *work*
  from being time-shared with the neighbours.
* **A per-tick 1080p canvas deep copy** sat on the clock thread: the projection
  cloned the composited `Nv12Image` (3.1 MB, a fresh allocation) into the
  `Arc` the encode hand-off and preview share. 0.2–0.4 ms mean, but a fresh
  multi-megabyte allocation is an `mmap` + page faults that contend on the
  process's memory map with every other thread (178 on the server); its max
  was 27 ms under control load in the rig.
* **Not causes** (measured): no lock is shared with the control plane on the
  tick path; `publish_state` 0.02 ms; the routing aux hook and the switcher
  hook ≤ 0.01 ms. Capping the control runtime at 3 workers made the wake tail
  worse (fewer, busier workers); lowering the control workers' priority alone
  was inside the host's noise.

Changes:

1. `CompositedFrame::canvas` is an `Arc<Nv12Image>`: the compositor's output is
   shared by every consumer (encode hand-off, preview, display, NDI, the held
   last-good frame and its repeats). No canvas copy runs on the clock thread.
   Because consumers now release a canvas when the last of them lets go, the
   CPU compositor's output-plane return cache keeps up to three spare planes
   per kind (was one), so a steady run allocates no plane while up to three
   canvases are outstanding; any surplus return is dropped (bounded memory).
2. The clock thread asks for a 300 µs EEVDF request size
   (`sched_setattr(SCHED_OTHER, sched_runtime)` through the FFI leaf
   `multiview-clocksys`; it keeps `SCHED_OTHER` and the thread's weight, so it
   cannot take more CPU than before; no privilege; read back and logged; older
   kernels report `kernel has no custom slice`).
3. Niceness −10 stays best effort. **The deployment should grant it**:
   `AmbientCapabilities=CAP_SYS_NICE` in the systemd unit. It is the only lever
   that protects the tick's work, not just its wake, from a saturated host.

**Bound the design promises** (Linux ≥ 6.12, the clock thread with its slice):

* **Wake lateness:** p99 < 1 ms while the host is not oversubscribed (runnable
  threads ≤ cores most of the time, the test server's state: load 9 on 10
  cores), because a waking clock thread with a 300 µs request preempts a fair
  neighbour at its next scheduler tick instead of after the neighbour's slice.
  On an oversubscribed host the wake is bounded by the run queue, not the
  design: the rig above measured p99 2–4 ms.
* **Emit lateness** = wake + the tick's own work (`compose_duration`), which the
  design does not shrink: it is the compositor's cost at the canvas size (on
  the server ~4 ms for the GPU round trip). The design promises that nothing
  else is added: no wait on a lock, a client or the control plane, and no
  per-tick canvas copy. With `CAP_SYS_NICE`, emit p99 < 8 ms and no tick past
  half a period under control load on the test server; without it, the
  compose can be time-shared with busy neighbours and the half-period bound
  holds only while compose p99 stays under half a period minus the wake.
* 0 repeats while compose p99 stays below one period (unchanged, ADR-T018).

The R3 chaos test `crates/multiview-engine/tests/clock_slice_chaos.rs` pins a
real clock thread (2 ms of work per tick) and six spinning threads to one core
and asserts, where the kernel applies the slice: every tick emitted, 0 repeats,
every wake under 10 ms (the one-core oversubscribed ceiling; measured 4–6.7 ms
with the slice, 11.9 ms with the default request size), and the mean wake at
most half the default run's when that run was contended (measured 0.2–0.5 ms
vs 2.1–2.3 ms).

## Rationale

A thread removes both measured causes at once: no shared worker (a busy or
blocking control-plane task cannot delay the wake) and no 1 ms timer wheel (the
kernel's hrtimer wakes the thread at the deadline). It costs one sleeping thread per
program. The absolute-deadline sleep keeps ADR-T001's no-drift property by
construction.

## Alternatives considered

| Alternative | Rejected because |
| --- | --- |
| A separate tokio runtime for the clock | Still the 1 ms timer wheel and a worker hop: the idle p99 of 8–14 ms measured on a 2-worker program runtime in ADR-0107 slice 1 was exactly this topology. |
| `std::thread::sleep` (relative) on Linux | Re-sizing each relative sleep from the deadline avoids drift, but an EINTR restarts the interval and the default 50 µs timer slack applies; `TIMER_ABSTIME` with 1 ns slack is the precise primitive and costs nothing. |
| Busy-spin to every deadline | Burns a core per program forever. The fallback spins only the last 500 µs on platforms without an absolute sleep. |
| `SCHED_FIFO` | Needs privileges, and one spinning bug starves the host. Niceness gives most of the benefit safely; it is optional and logged. |
| Make `run_inner` fully synchronous now | Larger, riskier diff across every caller and test; keeping the `async` loop and driving it with a one-poll `block_on` gives the same behaviour on the thread. |

## Consequences

* Each program owns one OS thread (`clock-<id>`), visible in `top -H` / `ps -L`.
* `ProgramSet` teardown detaches (not aborts) a wedged clock thread at the grace.
* Linux builds link rustix's `thread`/`process`/`time` features (MIT/Apache-2.0,
  pure Rust, no native deps); macOS builds link `libc` through the new FFI leaf
  `multiview-clocksys`. The default build stays LGPL-clean and GPU-free.
* Tests: `crates/multiview-engine/tests/clock_thread.rs` (exact count and PTS over a
  long cooperative run, no drift at 29.97 over a simulated day on a real clock
  thread, migration handoff incl. repeats, resync past the cut and a predecessor
  that stops first, restart backoff and stop on a clock thread, shutdown while
  sleeping, a panicking consumer) and the
  R3 chaos test `crates/multiview-frontdoor/tests/clock_thread_chaos.rs` (saturated
  control runtime + front-door flood; reports the old task topology side by side).
