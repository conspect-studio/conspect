# Live desired-state durability: in-progress control-plane design

This isolated branch introduces `desired.json` as the authoritative, versioned, atomic envelope for accepted source, output, audio-routing, and layout intent. The envelope contains the full validated configuration, stable output API identities, the saved layout library, selected layout ID, and the exact selected layout body. `active.toml` remains an observational snapshot of commands adopted by the live engine; shutdown persistence must not overwrite accepted intent.

## Acknowledgement contract

For modeled runs, covered endpoints hold `lock_config_mutation`, check the resource precondition, compose and validate a prospective complete configuration, write a same-directory owner-only temporary file and fsync it, rename it to `desired.json`, and then change the resource store/ETag or send a command. A failed write returns a repository 5xx before any covered store or command mutation. Source live application is separate from durable acceptance: an accepted store edit can advertise `restart` when the live command is unavailable. Apply-layout reserves a bounded bus slot before committing the selected body, and only enqueues after a successful durable write. Output IDs are mapped to configuration positions in the envelope, not inferred from resource names after the first commit. On restart, a valid desired envelope outranks a stale `active.toml`; corrupt accepted intent fails startup instead of silently restoring older state. The saved library is restored from the authoritative envelope in both SQLite and default builds; a boot seed must never overwrite an existing SQLite layout ID.

## Unfinished contract and release gate

This is **not** complete persistence. Overlay, probe, device, sync-group, salvo, tally, boot-file watch/revert, and other mutable surfaces remain outside the transaction. SQLite layout writes remain a secondary projection that can fail after the envelope commits, leaving the immediate API response inconsistent until restart. Versions/ETags for layout definitions are not in the envelope and restart may reset counters. No end-to-end production-host or release verification has occurred. In particular, do not merge or deploy this branch until all required endpoints share one transaction policy, a SQLite projection fault is handled, boot migration and rebind are tested, and independent review and CI pass.

## Isolation

All serialization, validation and disk writes run on the control plane; file I/O uses `spawn_blocking`. The engine never awaits a client or a disk write, and the existing fixed-cadence output loop is unchanged. A full command bus sheds at reservation, before a layout desired-state write.
