# ADR-M001: Unified REST+WS resource model and /api/v1 naming with explicit ownership boundaries

- **Status:** Proposed
- **Area:** Management
- **Date:** 2026-06-02
- **Source brief:** [management-capability-matrix.md](../research/management-capability-matrix.md)

## Decision

Adopt a single versioned control surface rooted at /api/v1 with top-level resources /api/v1/sources, /api/v1/layouts (containing canvas/cells/overlays), /api/v1/outputs (containing encode/color/audio/subtitles/container/failover/adaptive), /api/v1/renditions, /api/v1/program(+/preview), and /api/v1/system/* (capabilities, policy/adaptive, policy/resilience, observability, config, users, tokens, secrets, audit, settings), plus cross-cutting /api/v1/assets, /api/v1/secrets, /api/v1/discovery/ndi and telemetry /api/v1/events, /api/v1/ws/*, /metrics. Ownership is assigned to a single writer per concern: Source owns per-input ingest/decode/color/jitter/reconnect/audio-attributes/subtitle-ingest/qos; Layout.canvas owns resolution/fps/pixfmt/working-color-space; Layout.cell owns geometry/fit/overlays/binding; Output owns encode/color-tag/audio-mapping/subtitle-output/container/failover; System policy owns adaptive/resilience defaults with per-entity PATCH override; TilePolicy is one object exposed via both cell inspector and System QoS table. Inline source specs in layouts are convenience-only with a first-class promote-to-managed-input action.

## Rationale

A single source of truth per parameter prevents conflicting controls across the four area designs (audio attributes vs routing, per-tile QoS in layout vs policy, per-input resilience override vs global default). Consistent /api/v1 noun resources with sub-resources keep the OpenAPI/Scalar doc coherent and the UI screens map 1:1 to resources.

## Alternatives considered

Per-area independent APIs (rejected: duplicate/conflicting controls, e.g. color set in both source and cell with no precedence); flat non-nested resources (rejected: loses the natural canvas->cell->overlay and output->encode hierarchy); embedding all source config inline in layouts (rejected: scatters secrets and policy across documents).

## Consequences

Requires a documented precedence rule (entity PATCH overrides policy default; cell color override coexists with source override as a per-tile compositor uniform) and a clear contract that canvas changes are validated/sequenced by the Output area because outputs bear the reset cost. Promote-to-managed-input must be implemented to avoid inline-spec secret sprawl.
