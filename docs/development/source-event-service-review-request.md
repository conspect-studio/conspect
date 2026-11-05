# Required service correction before runtime integration

Owner: source_event_service worker. Review found snapshot() deep copies events on each degraded read (Arc::make_mut). Replace with lightweight read wrapper holding Arc<SourceEventSnapshot> plus atomic degradation/loss metadata; no full event clone on compositor read. Preserve immediate failclosed loss flag. Add deterministic publication replacement/drop barriers under tests and acquired-old-snapshot consumer epoch validation test.

Runtime pipeline worker confirms registration ownership needs design amendment: event clock identity must carry lifecycle plus connection/mapping generation independently of SourceFrameTime.generation used by captions. Do not change service registration to a global singleton. A bounded cloneable registration controller usable on ingest supervisor (never output thread) is acceptable after explicit coordination; worker owns all tracker effects and immutable snapshot publication.

Renderer worker strict clippy found additional service issues: try_observe needs must_use; AtomicU64::try_update is newer than workspace MSRV1.85 (use compatible fetch_update); new/register/shutdown need Errors documentation; worker shared/rx arguments trigger needless_pass_by_value. Resolve these in repair scope and run strict targeted service clippy.

Do not mark service ready for engine use until the deep-copy issue is resolved. Existing six tests establish only initial bounded service behaviour.
