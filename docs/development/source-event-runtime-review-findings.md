# Runtime integration findings to resolve before final gate

Active worker843: current source_event_runtime.rs reviewed by lead while integration ongoing; these are completion requirements, not final verdict.

1. Sample.sampled_at currently not checked by API valid; stale held sample may retain Available forever if output stops. Add monotonic freshness bound and test stopped sampling. A repeated/held source frame cannot be refreshed as live indefinitely merely by output ticks; gate source state or cursor progression explicitly.
2. NativeEvents.ingest drops cue entirely before first mapping exists; early/preroll cues need bounded pending raw buffer and replay into first valid mapping. On generation reset pending old mapping evidence must not carry across. Add early-before-frame fixture.
3. RuntimeReader.sample find_map returns first by event key, not nearest upcoming/active priority; API rows only overlay (one) loses upcoming/history/assets. Preserve bounded event rows separately from chosen compactbadge. Don't claim full event panel/history until done.
4. Unknown/unavailable/degraded state currently suppresses all overlays immediately (policy holds zero); configurable policy remains unfinished. At minimum truthful marker/timing state shouldn't become false empty history.
5. Explicit ID3 detection but capture only SCTE true still missing actual ID3 metadata path. No ad inference.
6. Re-read source-event-live-wiring-next.md and SOURCE_EVENT_MAIN_SCOPE_APPROVAL.md: narrow main wiring permission already granted. Finish actual pipeline capture/provider invocation, not more models.
7. Lifecycle retirement helper clears context but returned producer evidence must retire/drop on every exit. Generation fence check at delayed bake required against captured frame, not fresh global mapping; immutable same-frame projection may remain valid as visual evidence but never action authority.
