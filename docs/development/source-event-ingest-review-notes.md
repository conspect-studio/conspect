# Ingest helper active implementation review notes

Worker29cf3 scope. Current partial compile error: SwapCell<Option<RegistrationContext>> cannot implement SwapPointer; use SwapCell<Option<Arc<RegistrationContext>>>. No change to frame value Copy metadata required.

Retirement must set retired=true and clear context; do not poison connection counter to u64::MAX, because a new lifecycle must be able to reconnect with strictly newer counter. Only real checked counter exhaustion disables permanently.

ContextReader.matches must also compare read source identity to its own source (current struct only contains context pointer, so same numeric tuple from another source could match). Keep latest degraded flag and registry currentness gating explicit. Separate source lifecycle, connection, mapping and service epoch; never compare lifecycle directly to service epoch.

Current source_event_service provides mark_degraded() for incomplete upstream evidence; test pending due active partial compilation. HLS observer719bc uses optional start_with_observer preserving old start. Service evidence must retire on fetch-generation invalidation, not just check atomic before send; generation-specific retirement handle or strict newer service registration ordering is required.

Lead will not modify active ingest worker files; these are required review notes for its finalization.
