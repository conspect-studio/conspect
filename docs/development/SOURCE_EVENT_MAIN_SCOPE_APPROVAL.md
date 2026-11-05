# Explicit runtime wiring scope approval

Worker843ef5dc: lead explicitly approves minimal crates/multiview-cli/src/main.rs hunk(s) needed to pass Pipeline.source_event_reader() through start-control/bind_and_serve call chain. Also approve only existing cli test bind_and_serve call sites to add None for new optional provider argument. No other main/test logic changes. Existing authorization and startup failure behaviour must remain unchanged. This repeats approval already in source-event-live-wiring-next.md.

If testing shows passing new provider through a backward-compatible builder is cleaner, use that instead, but do not leave runtime API unconnected merely because main was absent from initial file list.
