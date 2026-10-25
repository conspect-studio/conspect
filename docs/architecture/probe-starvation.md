# Design note — slow per-tick stages must not starve the control hook (#143)

Status: Implemented · Class: R3 · Invariants: #1 (output clock), #10 (isolation) ·
Relates to: [ADR-T018](../decisions/ADR-T018.md) (drop/repeat-to-cadence), #137

## Problem

ADR-T018's cadence-hold path (`EngineRuntime::run_inner` in
`crates/multiview-engine/src/runtime.rs`) handles overload by re-emitting the held
last-good frame once for every tick-period that has already gone by. Each repeat
calls the `state_of` projection, then reads wall-clock again to decide whether to
emit another one. The control hook (the management plane's command drain) and
the fresh compose run only after the burst ends.

In the CLI, `state_of` also runs the `FaultDetector` black/freeze probes. Under
#137 those probes took 50–120 ms against a 40 ms tick in an unoptimised build.
That made each repeat cost more than a period, so the burst never caught up:
every repeat moved the wall-clock target further away. The burst ended only when
`MAX_REPEATS_PER_TICK` (120) forced a resync, and the next burst started straight
away. The command drain and the compose ran about once every 121 ticks, so the
output carried roughly 5 s of stale picture per burst plus a PTS gap, and
operator takes appeared to do nothing. #137 made the probes cheaper, but any
stage that is slow enough can still cause the same failure.

## Decision

The fix has two parts. Both are local to the drive loop and add no new channel,
lock or `.await`.

1. **Probe work on a repeat is O(1).** `CompositedFrame` gains `repeat: bool`.
   The repeat path sets it on the held frame; fresh composites set it to `false`.
   A repeat re-emits a canvas that has not changed, so a projection with expensive
   stages can skip them and reuse its last result. The CLI does this: on a repeat
   it reuses the last `FaultDetector` verdict, so the probes run once per fresh
   compose, i.e. once per catch-up burst, not once per repeated tick. With that
   change a burst costs `repeats × (cheap projection)`, so it converges, and a
   probe costing 3× the period settles into a steady pattern of 1 fresh frame and
   2 repeats. Output stays contiguous, one frame per tick, with no resync gap.
   Time-based alarm dwell is unaffected, because the dwell machines are keyed on
   `pts` and the next fresh sample observes the elapsed time.

2. **The control hook runs at least every N ticks, whatever the stages cost.**
   The repeat path calls the control hook after every
   `MAX_REPEATS_BETWEEN_CONTROL` (8) consecutive repeats, so command drain runs
   at least once every 9 emitted ticks. This bound holds even for a projection
   that ignores `repeat` and is slow on every frame. The hook is the same
   frame-boundary seam as on the fresh path: it must not block, it does O(1) work
   per call, and it caps per-tick work itself (for example `MAX_REPOINTS_PER_TICK`).
   Calling it during a burst therefore cannot stall the clock. A layout or source
   swap it applies is picked up by the next fresh compose.

## Why not the alternatives

- **Only skipping probes when behind (decimation inside the engine).** The
  engine cannot see inside `state_of`. Decimating the whole projection would
  stop publishing state and frames to the bake queue on some ticks, which breaks
  "one frame per tick". The `repeat` flag lets each projection decide what to
  skip.
- **Computing the burst length once and emitting without re-reading the clock.**
  This does not fix a projection that is slow on every frame. The next wake
  finds the loop behind again, and the burst still keeps the hook away. Part 2 is
  needed either way.
- **Moving the probes off the hot loop onto their own task.** This is the right
  long-term home for heavy analysis. It needs a new engine-to-task channel and a
  fresh inv #10 proof, which is out of scope for a minimal fix.

## Invariant impact

- **#1:** every emitted tick is still exactly one frame stamped `pts = f(tick)`.
  Repeats still re-stamp, and the run-ahead and resync-floor rules are
  unchanged. Off the overload path, repeats never happen, so the new branch never
  runs and behaviour is byte-identical to before. The only change in the
  emitted sequence under overload is an improvement: no more cap-resync gaps.
- **#10:** no new channel, `.await` or lock. The only extra work is more calls to
  the existing non-blocking control hook during bursts, bounded at one call per
  9 ticks.
- **Ordering:** a hook call during a burst happens between two repeats. Commands
  it applies affect the next fresh compose, exactly as before; only the latency
  drops.

## Proof

`crates/multiview-engine/tests/probe_starvation_chaos.rs` uses `ManualTimeSource`
and `CooperativePacer`, with no real sleeps, so it is deterministic on a loaded
host.

- `slow_probe_holds_one_frame_per_tick_drains_commands_and_recovers`: the probe
  costs 3× the period on fresh frames and drops to 1× after 300 ticks. The test
  checks that indices are contiguous (frames == ticks), that the last index tracks
  wall-clock, that the control gap is at most 9 ticks, and that the tail is fresh
  every tick once the cost drops.
- `undecimated_slow_projection_cannot_starve_the_control_hook`: the projection
  costs 3× on every frame, repeats included. The test checks the hook-gap bound,
  strictly increasing PTS, completion within the time limit, and recovery.

Before the fix, the gaps were 121 ticks and the indices were non-contiguous.

## Reversal

Revert the commit. The field defaults to `false` on fresh frames, and the
constant is the only new public API.
