# ADR-W017: Action route style — bare verb path segments

- **Status:** Proposed
- **Area:** Web/API stack
- **Date:** 2026-06-10
- **Source brief:** [web-api-stack.md](../research/web-api-stack.md)

## Decision

Non-CRUD actions on a resource are expressed as `POST /<collection>/{id}/<verb>` — a bare verb (kebab-case for verb phrases) as the final path segment. This codifies what `multiview-control` already ships: `POST /api/v1/salvos/{id}/arm`, `/take`, `/cancel` (`routes/salvos.rs`), `POST /api/v1/alarms/{id}/ack` (`routes/alarms.rs`), and `POST /api/v1/config/{target}/rollback` (`routes/config.rs`). The new Devices surface ([managed-devices.md](../research/managed-devices.md), [ADR-M008](ADR-M008.md)) follows the same style: `POST /api/v1/devices/{id}/probe`, `/set-mode`, `/reboot`, `/identify`, `/test-pattern`. Actions are always `POST`; parameters travel in the JSON body, never in the verb segment; long-running actions return `202 Accepted` + operation id with the result on the realtime stream (ADR-W008). The `:verb` custom-method style sketched in older brief text is superseded.

## Rationale

Consistency with shipped routes is the strongest argument: the salvo/alarm/config action endpoints already use bare verb segments and are published in the OpenAPI document, so any other style would split the API in two. Bare segments are also the friction-free case for the whole toolchain — axum/matchit route templates and utoipa path templates treat a path parameter as a whole segment, so a mixed `{id}:verb` segment needs hand-rolled suffix parsing on both the router and the OpenAPI side, and `openapi-typescript`/`openapi-fetch` (the SPA's generated client) handle plain segments without special-casing. Finally, a bare segment has no encoding ambiguity: `:` in a path is legal per RFC 3986 but is inconsistently percent-encoded by intermediaries and client libraries, which would make action URLs the one place in the API where canonicalisation matters.

## Alternatives considered

Google-style custom methods (`POST /v1/salvos/{id}:arm`, AIP-136) — rejected: mixed literal+parameter segments are not expressible in axum's route templates without manual parsing, the `:` invites proxy/client encoding drift, and the shipped routes already diverge from it. Action-as-resource (`POST /api/v1/actions` with `{kind, target}` body) — rejected: one mega-endpoint loses per-action OpenAPI schemas, per-route RBAC and `Idempotency-Key` scoping, and clean `404` semantics for an unknown target. Pure state-transfer (`PATCH` a `state` field, e.g. `{"state": "armed"}`) — rejected: arm/take/reboot are commands with side effects and `202` semantics, not declarative state writes, and overloading `PATCH` conflates command submission with `ETag`/`If-Match` optimistic-concurrency updates.

## Consequences

Older brief text that sketches `:verb` routes needs a one-line reading note, not a rewrite: the capability-matrix brief's API column (`program:take`, `source:swap`, `:apply` in [management-capability-matrix.md](../research/management-capability-matrix.md)) and the `/config:export|:import` sketch in [web-api-stack.md](../research/web-api-stack.md) are all read as bare-verb routes (`/program/take`, `.../source/swap`, `/config/{target}/export`); where code and brief disagree, code wins. The verb shares the path namespace with sub-resources, so naming discipline is required: sub-resources are nouns, actions are verbs (or kebab-case verb phrases like `set-mode`), and a collection must not define a sub-resource with the same name as an action. Verbs are not globally reserved — `probe` on a device and a hypothetical `probe` sub-resource elsewhere can coexist — so each new action only has to be unambiguous within its own resource.
