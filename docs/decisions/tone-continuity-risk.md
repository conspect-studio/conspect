# Synthetic tone continuity: output-clock risk note

Status: candidate on `fix/live-tone-continuity`, not deployed.

## Failure and decision

The previous bars tone was written by a separate producer into a two-second append ring. A forced pause after 24,000 published frames lets the bus's tick 13 read start at frame 24,960, ahead of the producer head. The store returns 1,920 silent frames. A larger fixed lead only delays this failure.

The candidate makes the bars `AudioStore` a deterministic pure tone source. `ProgramBus::mix` seeks it to the exact integer sample position from `SampleClock::total_at` and reads the requested sample budget. The store constructs a local `ToneGenerator` at that absolute frame. No producer thread, wake-up schedule, PCM ring, source input, network call, mutex or wait participates. Every requested sample follows the same 1 kHz, −18 dBFS phase function; the discrete pre-route block remains audible under program mute, and normal route gains remain in `Mixer`.

## Invariant #1/#10 and R3 assessment

`ProgramBus::mix` is a general API documented as output-clock-safe; this change adds bounded O(frames × channels) sine work for *bars routes only*. Current production `Pipeline::consumer_main` drives the bus on the separate bake consumer, after the engine emits a canvas. The protected engine clock has no new call, lock, wait, or dependency. An alternate caller could invoke `ProgramBus::tick` on its output clock: this candidate would then increase synchronous CPU cost and must be measured before that caller adopts it. Many bars sources multiply the CPU work, though each requested block stays bounded by the caller's sample budget. The output clock remains independent of absent audio producers; no claim of zero DSP cost is made.

The deterministic chaos regression calls `tick_to` across skipped ticks through tick 300 at fractional 30000/1001 cadence without a producer, compares every interleaved sample with an absolute-frame reference, then mutes and restores program gain and removes the route. The native bars program test verifies encoded 1 kHz output. Full loaded matrix and independent review remain release gates.

## Limits

A file, HLS source, player or anchored timeline store never opts into this mode. No source identity is inferred from audio values. This candidate does not solve unrelated finite-file decode or loaded-track failures. The tone block allocation remains the existing bounded per-tick `AudioBlock` shape; `ToneGenerator` uses a local value and no persistent unbounded queue.
