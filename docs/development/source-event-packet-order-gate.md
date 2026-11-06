# Required packet-order regression before armed automation

Independent review scenario (exact90k): establish old video raw900000 (10s) -> normalized0; queue old video936000 (10.4s); demux reads new-epoch SCTE start18000 (0.2s); queue new video raw0 (backwards10.4s >10s discontinuity threshold). Separate metadata channel may deliver18000 before paced decoder consumes936000. Draining metadata with old snapshot maps event−9.8s, immediately Active on old picture400ms. Later new frame reanchors440ms; correct new event would be640ms. Clearing metadata only AFTER new decoded frame cannot undo false earlier event.

Use deterministic barriers, no sleeps. Before new frame latched: no new-epoch active badge/API event/action reservation. After new epoch: map640ms correctly or explicit unavailable/degraded, never use old mapping. Converse old cue before new video must not use new mapping. Preserve demux packet/discontinuity sequence through delayed decoder output or use ordered envelope/provenance fencing. Do not infer mapping by metadata drain time. SCTE announcement PTS itself may be far future/past and cannot alone identify epoch.

This is an integration blocker for trusted boundaries/automation. Current native observation checkpoint does not prove this property. Lead must resolve before production automation or claim source clocks reliable under discontinuities.
