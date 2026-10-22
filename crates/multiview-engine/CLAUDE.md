# multiview-engine — agent notes (PROTECTED OUTPUT CORE)

Fixed-cadence output clock, compositor drive, supervisor/actors, hot-reconfiguration, admission/degradation loop.
Inv #1 (output-clock): one monotonic clock emits exactly one valid, correctly-timestamped frame per tick, forever, independent of any input. No data-plane path may block on an input, a client, or a lock one holds.
Inv #10 (isolation): engine never `.await`s a client, never sends on a channel a slow consumer can fill. A new engine→outside channel must prove it cannot stall the engine (CI chaos gate).
Inv #9 (degradation): sense→estimate→plan→apply with hysteresis, shed load tile-by-tile cheapest-impact-first before program output is touched.
No `unwrap`/`expect`/`panic!` on the hot path; re-stamp all output PTS/DTS from the tick counter.
A change risking #1/#10: stop, write a design note, add a chaos/soak test.
