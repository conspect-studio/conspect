# ADR-W015: Typed resource validation + config export — honest management CRUD

- **Status:** Accepted
- **Area:** Web/API stack · sources/outputs/overlays management
- **Date:** 2026-06-10
- **Source:** [management-capability-matrix](../research/management-capability-matrix.md); builds on ADR-W005 (auth), the resource CRUD routes, and invariant #11 (live-apply classification)

## Context

The control plane exposes full CRUD for sources, outputs, and overlays, but the bodies are stored
as **opaque `serde_json::Value`** with no validation: `POST /api/v1/sources/cam1` accepts any JSON,
the OpenAPI document describes the body as an unconstrained object, and the generated SPA client
types are `unknown`. Worse, the UI can store a "source" the engine could never run (wrong kind tag,
missing `url`), and the operator only finds out at the next config-file authoring session. Stores
are in-memory: UI edits are lost on restart and there is no path from UI state back to
config-as-code, even though `MultiviewConfig::to_toml()` exists.

Live hot-apply of resource mutations (spawn/teardown ingest, make-before-break output migration) is
a separate engine lane (make-before-break ADR in progress) and is **not** decided here.

## Decision

1. **Typed validation at the API boundary.** `POST`/`PUT` on `/api/v1/sources`, `/outputs`,
   `/overlays` deserialize the submitted body into the canonical config types
   (`multiview_config::Source`, `Output`, `Overlay`) with `serde_path_to_error`, rejecting invalid
   bodies with `422 application/problem+json` whose `detail` carries the field path and serde
   message. Valid bodies are stored as today (JSON round-trip preserved — storage stays `Value` so
   unknown-but-valid optional fields survive). Layout bodies keep their existing validation path.
2. **Documented schemas.** The OpenAPI components gain schemas for the source/output/overlay body
   shapes (Doc mirror types in `openapi_schemas.rs`, the established pattern), so `/docs` and the
   generated TypeScript client describe the real per-kind fields instead of `any`.
3. **Config export.** `GET /api/v1/config/export` composes the current stores
   (sources/outputs/overlays + layouts + the seeded canvas) into a `MultiviewConfig` and returns it
   as TOML (`Content-Type: application/toml`, read role). This closes the loop honestly **today**:
   edit in the UI → export → persist as the config file → restart picks it up. The SPA exposes it
   as "Export configuration".
4. **Honest apply semantics surfaced.** Resource mutation responses carry an explicit
   `apply: "restart"` marker (header `X-Multiview-Apply: restart` + documented), and the UI states
   that stored resource edits take effect via config export + restart, while the live actions that
   ARE hot (swap, apply-layout, routing, salvos) stay clearly labelled as live. When the
   make-before-break lane lands, the marker flips per-class without breaking clients.

## Rationale

Garbage-in CRUD is worse than no CRUD: it teaches operators the UI lies. Validation against the
single source of truth (the config schema types) means UI, API, file, and engine can never drift on
what a valid source/output is. Export reuses `to_toml()` (already round-trip-tested in
multiview-config) rather than inventing a second serialization. Surfacing "restart" honestly is the
management-capability-matrix discipline (inv #11): every change declares how it applies; we refuse
to pretend a stored doc is live.

## Alternatives considered

- **utoipa `ToSchema` derives on multiview-config** (rejected for now: adds an optional utoipa dep +
  feature to a foundational crate for marginal gain over Doc mirrors; revisit if mirror drift bites —
  a unit test asserts mirrors deserialize the same fixtures the config types do).
- **Hot-apply resource mutations now** (rejected: ingest spawn/teardown and output migration are the
  make-before-break engine lane, in flight separately; doing a half version here would violate the
  no-partial-ship rule on an invariant-#1-adjacent path).
- **Persist stores to SQLite** (rejected: a second persistent store competing with config-as-code
  creates a two-masters problem; export keeps the config file the single durable truth).

## Consequences

Invalid bodies that previously 2xx'd now 422 — the SPA forms are being rebuilt kind-specific in the
same push, so no shipped client regresses. Doc mirror types must track schema.rs (guarded by a
fixture test). Export omits secrets by construction (`SourceAuth` is a `secret_ref`, never a
secret). The `youtube` source kind and per-kind output fields (codec, LL-HLS part/segment/GOP,
RTSP latency profile, per-output audio) become visible API contract — the SPA must render them.

Amendment (2026-06-10, post-review): the literal `export` segment is **reserved** in the
config-versioning namespace — `/api/v1/config/export` (static route) wins over
`/config/{target}`, so a versioning target named `export` cannot be read (GET returns the
export document) or committed (PUT is `405`). Pinned by test.
