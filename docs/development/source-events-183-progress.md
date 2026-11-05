# Source events implementation ledger (#183)

Status: implementation in progress; NOT deployed or runtime-integrated. Base 8a2a9725, branch feat/source-event-automation-183. Reviewed foundation checkpoint ea91809c; tested original-manifest metadata retention seam 68c2ab6b. Neither is a complete deployable feature.

## Contract

See [ADR-0183](../decisions/ADR-0183-source-event-automation.md). Source-owned event state drives automatic overlay lifecycle and separately armed production automation. Vendor markers are vicinity evidence, not splice boundaries. CAID is reusable asset identity. Default automation must reject marker-only evidence. Manual routing invalidates automatic restoration.

## Foundations in this branch

- SCTE segmentation descriptor decoding with richer normalized events, preserving legacy projection. Targeted SCTE regression/property tests passed (24). Independent runtime wiring pending.
- HLS DATA declaration and bounded DATERANGE/CAID metadata parsing. Seven new targeted tests and 16 existing HLS tests passed. No fetch/poll runtime hookup yet.
- Pure bounded core source event tracker: exact source times, revisions, deduplication, cancellation, early end, uncertainty, retention epoch floors. Twelve targeted tests passed after epoch-precedence review fix.
- Pure overlay projection: 14 targeted tests and strict core clippy passed, including automatic clear and exact string epoch/time DTOs. No compositor rendering yet.
- Immutable event clock mapping: 6 new and 11 existing normalizer tests and strict clippy passed. Future events do not mutate video clock state.
- SCTE-to-core adapter: 8 targeted tests passed; runtime occurrence policy and source lifecycle wiring remain required.
- Bounded ID3 parser: 6 targeted tests and strict clippy passed. Unknown vendor frames remain metadata, never inferred breaks.
- CLI original-manifest metadata retention: red missing-field test followed by 17 native HLS fetch tests passed. Observation publication remains pending.
- Pure armed automation/override lease safety model committed 0f201098 after independent review: 11 tests and strict clippy passed. Source-scoped model, independent monotonic watchdog and revoked-lease tombstones; it does not execute production commands.
- Bounded runtime service 37cfce9d repaired in a1bcf4ce: lightweight Arc+scalar reads, serialized supervisor registration, 8 integration +3 deterministic race tests passed and strict native CLI clippy passed. Independent scoped review cleared repair; runtime consumer freshness/degradation validation remains required.
- Optional per-frame event clock context 77e2c164 preserves caption lifecycle; 2 framestore +1 engine metadata tests and 34 caption tests passed (one pre-existing network test ignored).
- Actual compositor badge renderer 804fc864 accepts same-frame projections, clips glyphs to source tiles and clears on empty input; 30 native overlay tests and strict native CLI clippy passed. Runtime invocation still pending.

These modules alone do not satisfy the feature. Test evidence is local and scoped, not full CI or deployment evidence.

## Latest local milestone (not deployed)

Authorized read API06d7374f and operator Events panel003cb8b2 are committed. Six source API +4 preview authorization tests pass. Eleven UI tests, full web lint and production web build pass; generated OpenAPI/TS and Lingui catalogs are updated. Independent UI review fixed valid uncertain/overrun handling; no browser receipt-clock countdown.

Active runtime worker has produced one passing injected-cue -> selected-frame projection -> real StreamBaker NV12 pixel-change/clear regression. Native library check passes, but production main provider wiring is still being completed by a separate worker. This is not live server evidence. HLS generation-fence/completeness repairs have20native tests passed; integration caller compilation remains active.

Full requested automation remains unfinished: no armed production executor, atomic bars+tone takeover/return, external hooks or persisted rule UI is deployed. These remain mandatory scope, not optional follow-ups.

## Required integration work

1. Complete immutable timestamp mapping without feeding cues into the video normalizer. Preserve source epoch, discontinuity and mapping validity.
2. Implement bounded timed-ID3 parser; unknown frames remain metadata. Apple PRIV transport timestamps are clock evidence only.
3. Tap original HLS manifests before rewrite, and demux SCTE/ID3 packets before discard. Support data renditions and ingest fallbacks, not only shared-fetch TS.
4. Own an off-clock bounded tracker service and immutable snapshot publication. Handle persistent replay/idempotency policy and retention explicitly.
5. Carry the event snapshot with the exact presented source-frame cursor through delayed bake. Add compositor overlays, saved source/layout bindings, timer tiles and web event panel.
6. Add authorized APIs/hooks, per-object checks, dry run and bounded audit. Reuse existing service authorization rather than bypassing it through direct command submission.
7. Implement prepared atomic video/audio/caption override leases with sample-boundary coordination, manual invalidation, explicit scope collision policy and emergency restore. Existing independent route commands are insufficient.
8. Integrate VT readiness and actual switching capability; do not treat mailbox acceptance or proposed ADR verbs as existing capabilities.
9. Add reliable-boundary rules and bars/bars+tone with deterministic AudioStore tone, channel mask and monitoring/program isolation.
10. Run targeted integration/security/chaos/soak tests, independent implementation review, CI and release integration. Coordinate actual server activation with release lead and prove live UI/event/automation behaviour before completion.

## Coordination constraints

Release lead session owns server deployment. Source-preview work remains separate and must not be overwritten. Existing timer geometry files belong to another release worker. Engine-efficiency session owns HLS fetch capacity/admission/deadline changes; agree exact metadata hook hunks before editing hls_fetch.rs. Do not copy whole files between branches. OpenAI-only constraint prevents cross-vendor review; record limitation rather than claiming that gate passed.
