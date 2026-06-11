# ADR-R001: Continuous-output guarantee via inverted control flow, output clock, and per-tile last-good-frame stores

- **Status:** Proposed
- **Area:** Resilience & A/V
- **Date:** 2026-06-02
- **Source brief:** [resilience-and-av.md](../research/resilience-and-av.md)

## Decision

Make a single internal monotonic OUTPUT CLOCK the master of the system. Every tick it PULLS exactly one frame from the compositor and one frame-period of samples from the audio mixer; both ALWAYS return data. Each tile has a lock-free triple-buffer (or Arc-swap) last-good-frame store written by its decoder and read by the compositor, plus a per-tile state machine LIVE -> STALE(hold-last) -> RECONNECTING(card) -> NO-SIGNAL(slate), mirroring AWS MediaLive's repeat->black->slate ladder. Output PTS/DTS = f(tick_count): monotonic, gap-free. Inputs only ever write into buffers; they never drive output timing. Never use -copyts or timestamp output from an input.

## Rationale

This single inversion makes output cadence provably independent of every input. It is the NDI frame-synchronizer (push->pull, hold-last) and GstAggregator force-live / livesync model. Verification CONFIRMED that input-clock-driven output WILL stall on input loss (reproduced across FFmpeg/GStreamer/VLC/MEncoder) and that the output stage must own a free-running clock + synthetic fill; the chaos suite must inject total input blackout AND GPU loss and still see zero gaps.

## Alternatives considered

A mixer that waits for all inputs (deadlocks on one stall); a blocking/back-pressured output (lets upstream faults stop emission); a shared mutex-guarded single frame (contention/priority inversion); a queue per tile (latency + back-pressure into the decoder).

## Consequences

Output is continuous by construction. Requires a deterministic, owned clock+compositor+state-machine layer and pre-allocated buffer rings. Slate/card assets must be atlas-resident at startup so they draw without an upload at the instant of failure. Per-tile disturbances are contained to that tile.
