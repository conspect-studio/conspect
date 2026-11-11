# First-slice preview lease recovery (R3 review note)

This change is control-only except for one release/acquire atomic committed-frame counter in the switcher driver. It adds no clock locks, awaits, channels or client work. The existing bounded command FIFO and outcome ring remain unchanged. No AUTO, keyer, multi-cell or deployment capability is added.

The preview owner admits at most 64 reservations. One control mutex serializes enqueue, resolution, sweep and permanent detach. Sampling the committed-frame count after enqueue prevents a descheduled HTTP task from expiring a command before enqueue. Six committed frames exceed the 64-entry FIFO's four 16-command drain passes; skipped tick indices do not advance this count. The counter publishes after the immutable bus snapshot. The sweeper reads the counter before the snapshot.

The snapshot reconciles resources, never operation status. An obsolete reservation may have applied and subsequently been superseded. Only exact engine receipts publish operation outcomes. Successful receipts choose the current snapshot source, not receipt order (the existing engine emits a batch in reverse). Negative receipts release only their reservation. Missing receipts are reconciled on the off-clock cadence. No picture means retain until a picture exists or the owner detaches.

Detach permanently closes the run owner and releases pending/current demand under the same mutex; a new run constructs a new owner. The binary has an RAII cleanup guard for error/cancellation paths as well as explicit normal teardown. Late admission cannot resurrect a detached owner.

Focused chaos tests cover the 64-entry real FIFO storm, an aborted relay, skipped indices, repeated boot/release, late admission, cold-take timeout without PGM swap and the production sweep loop. Existing HTTP/picture tests remain required. No long soak or host deployment is part of this change.
