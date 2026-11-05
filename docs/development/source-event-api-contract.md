# Required source events API/UI fields (active implementation)

API worker 0eb3354 and UI worker f08e4: coordinate by this contract and source_events.rs. SourceEventDto currently lacks actual timer and asset fields; add these before UI finalization:

- optional timer: { kind: until_start | remaining | elapsed | overrun, nanoseconds: exact unsigned decimal string, max20 chars }. Server projects against selected source frame; UI never decrements from receipt clock.
- optional asset_id and correlation_id: bounded public text (max256 bytes), plain text escaped by UI, not raw wire metadata.
- optional semantic display label matching core overlay labels upcoming/active/ended/uncertain/overrun/observed, or derive safely from lifecycle+timer kind without inventing certainty. A timer kind overrun implies expected end unconfirmed, not confirmed end.
- origin/provenance remains separate; observed nonvendor metadata must not be labeled SSAI.

API owner: finalize the above timer/asset/correlation fields and regenerate web/src/api/schema.ts using repository OpenAPI generation command; UI worker explicitly waits for that route/type schema. Canonical field names: timer.kind, timer.nanoseconds, asset_id, correlation_id, label. Followup DTO owner event-model is implementing them now: SourceEventTimerKind snake_case until_start/remaining/elapsed/overrun; SourceEventLabel upcoming/active/ended/uncertain/overrun/observed. All optional fields require None in Rust literal callers until populated. UI consumes the generated types after lead regeneration. COMPLETED: lead ran cargo run --locked -p xtask -- gen-openapi and npm --prefix web run generate:api successfully; generated schema includes finalized timer/asset/correlation/label DTOs. cargo xtask alias is absent in this worktree, so use explicit runner. No receipt-time countdown fallback. Reject empty event_id and enforce vendor namespace only for VendorMarker (nonempty bounded); do not let malformed provenance claim SSAI.

Constructor validates bounded exact canonical decimal values and omits/rejects invalid rows with degraded flag. Every DTO integer identity (epoch, connection/mapping if exposed) is decimal string. Route authentication/authorization BEFORE provider. Generate web API types from OpenAPI using repo command; do not hand-edit generated schema silently.

The read-only panel must distinguish unavailable/stale/degraded vs valid empty history. Remove stale prior-source data on source switch, stop polling on close, and show cancelled/ended history distinctly from current overlay. No arming controls without implemented authenticated command endpoints.
