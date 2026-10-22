# ADR-W021: Switcher control surface — REST verbs, operator panel, and external-control friendliness

- **Status:** Proposed
- **Area:** Web/API stack
- **Date:** 2026-06-11
- **Source brief:** [production-switcher.md](../research/production-switcher.md)
- **Relates to:** [ADR-W017](ADR-W017.md) (bare-verb actions), [ADR-W008](ADR-W008.md) (202 +
  operation id), [ADR-RT008](ADR-RT008.md) (realtime taxonomy), [ADR-M012](ADR-M012.md) (resource
  model), [ADR-0055](ADR-0055.md) (transition engine), [ADR-P007](ADR-P007.md) (preview bus +
  monitor tiers), [ADR-W011](ADR-W011.md) (no-color-alone), [ADR-W015](ADR-W015.md) (typed forms),
  [ADR-W016](ADR-W016.md) (in-app docs), [ADR-W023](ADR-W023.md) (preview transport ladder)

## Context

No switcher surface exists anywhere in `multiview-control` or `web/` (verified by module and grep
survey). The nearest primitives, all verified in the worktree: the routing crosspoint plan/take
pair (BUILT server-side — `plan_route` at `crates/multiview-control/src/routes/routing.rs:107`,
`take_route` at `:140`, classifying via `routing::classify`,
`crates/multiview-control/src/routing.rs:222` — but typed-and-unconsumed in the SPA); the shared
202 helper `submit_accepted_body` (`crates/multiview-control/src/routes/mod.rs:314`); salvo
arm/take/cancel end-to-end; and a documented raw-`fetch` bypass of the generated client for most
SPA mutations (`web/src/api/operations.ts:1-21` states the spec-lag/ergonomics reasons). Three
realtime hooks each open their own WebSocket (`new RealtimeConnection` at
`web/src/realtime/useEngineEvents.ts:159`, `useSystemMetrics.ts:443`, `useHealth.ts:180`). No
client code path sends `Idempotency-Key` (only docs prose mentions it). There is no global
keyboard-shortcut infrastructure and no `fast-check` dependency in `web/package.json`. This ADR
pins the control surface (the brief's §11.2 REST/realtime + §11.3 SPA requirements) so the
switcher lands on the typed, convention-conforming path from day one.

## Decision

1. **REST verbs — bare-verb POSTs per [ADR-W017](ADR-W017.md), kebab paths, parameters in the JSON
   body.** The action surface:
   - `POST /api/v1/switcher/mix-effects/{id}/preview` — PVW crosspoint set (`{"source": …}`; the
     endpoint behind point 7's `1–9` keys)
   - `POST /api/v1/switcher/mix-effects/{id}/program` — direct program punch (`{"source": …}`; a
     plain cut per [ADR-0055](ADR-0055.md); the endpoint behind point 7's `Shift+1–9`)
   - `POST /api/v1/switcher/mix-effects/{id}/cut` · `/auto` · `/ftb` — FTB addressing, pinned:
     FTB is a per-program **master stage after the DSKs** ([ADR-0056](ADR-0056.md)); the REST
     verb addresses it via the M/E that owns the program canvas (one M/E in the MVP), so
     `…/mix-effects/{id}/ftb` is the *address*, not the stage location.
   - `POST /api/v1/switcher/mix-effects/{id}/transition` — set the armed transition (kind, rate,
     next-transition element set)
   - `POST /api/v1/switcher/mix-effects/{id}/tbar` — **conflated absolute setter**: the body
     carries an absolute position as an integer `position` in `0..=10000` (basis points; integers
     on the wire, never floats — invariant #3), quantised half-up at the boundary onto the
     engine's `u16` fixed-point position (`0..=65535`; wire `10000` ⇒ `65535` = complete) per
     [ADR-T015](ADR-T015.md) §3. The SPA conflates pointer moves client-side
     (latest-wins per animation frame) and the engine treats each landing value as idempotent
     absolute state — never one command per input sample.
   - `POST /api/v1/switcher/downstream-keyers/{id}/on-air` · `/off-air` · `/auto` · `/tie`
   - `POST /api/v1/switcher/aux-buses/{id}/route`
   - `PATCH /api/v1/switcher/audio` — program master audio `{master_gain_db, master_mute}`; the
     boundary converts it to [ADR-0059](ADR-0059.md)'s engine seam commands
     (`MasterEnvelope`/mute — the engine command shape stays ADR-0059's; this ADR owns the path)
   - `POST /api/v1/media/players/{id}/load` · `/cue` · `/play` · `/pause` · `/stop` · `/seek`
     (`seek` body: integer `frame`, or `ms` converted via exact rationals per
     [ADR-T015](ADR-T015.md) — the [ADR-0057](ADR-0057.md) transport verb)
   - `POST /api/v1/macros/{id}/run`
   - Memory (snapshot) recall rides the BUILT salvo arm/take verbs, extended with the
     recall-scope mask body `{scope: {sources, keyers, transition, audio}}` pinned in
     [ADR-M012](ADR-M012.md)
   Commands are **explicit-state and idempotent** wherever a toggle would be ambiguous: `ftb` takes
   `{"engaged": true|false}`, `on-air`/`off-air` are separate idempotent verbs (repeat = no-op
   `200`), `tie` takes `{"tied": true|false}` — required both for the unambiguous-outcome
   correlation rule ([ADR-RT008](ADR-RT008.md)) and for external-control friendliness (point 13).
   Durations are integer `rate_frames` **or** integer `rate_ms` (mutually exclusive; ms converted
   via exact rationals per [ADR-T015](ADR-T015.md)). Example (IPv6-first per conventions §10):
   ```
   curl -X POST 'https://[2001:db8::a]:8443/api/v1/switcher/mix-effects/me1/auto' \
     -H 'Authorization: Bearer <key>' -H 'Idempotency-Key: 7f0c2a4e-…' \
     -H 'Content-Type: application/json' -d '{}'
   ```
2. **Plan/take split per invariant #11 — the routing precedent, reused.** Every state-changing
   take is classified Class-1 / Reset-lite / Class-2 *before* applying, with a dry-run endpoint
   mirroring `POST /api/v1/routing/plan`: `POST /api/v1/switcher/mix-effects/{id}/plan` accepts
   the same action body (`{"action": "auto", …}`) and returns the classification without applying.
   Response split: **`200 {class, applied}`** for immediate Class-1 actions (`cut`, M/E
   `preview`/`program`, `tbar`, DSK `on-air`/`off-air`/`tie`, player
   `play`/`pause`/`stop`/`seek`, `transition` arming, master audio gain/mute) and
   **`202 Accepted` + operation id** for timed or asynchronous operations (`auto`, `ftb`, DSK
   `auto`, player `load`/`cue`, `macros/{id}/run`), resolving on the
   [ADR-RT008](ADR-RT008.md) lifecycle events via `corr`. Aux `route` is deliberately in
   neither static list: its response follows the per-take classification of
   [ADR-M012](ADR-M012.md) §9 (the `take_route` precedent) — `200` for a Class-1/Reset-lite
   move, `202` + operation id for a Class-2 format-change move. `Idempotency-Key` is accepted on **every** action; callers must handle the
   documented replay shape (`kind: "replay"`, `routes/mod.rs:328`).
3. **Desired-state resources: copy the sync-groups domain file-set verbatim** (the BUILT
   implementation template, all paths verified): `ResourceKind` marker + store alias
   (`crates/multiview-control/src/resource_store.rs:93-94`, `:207`), `TypedCollection` variant
   validating the [ADR-M012](ADR-M012.md) config types
   (`crates/multiview-control/src/typed_resources.rs:71-84`, `:176-177`), a
   `routes/switcher.rs`-family CRUD file (per `routes/sync_groups.rs`), `AppState` field +
   builder + `seed_resources` arm (`crates/multiview-control/src/state.rs`), OpenAPI registration
   incl. the `rest_routes()` parity table (`crates/multiview-control/src/openapi.rs:300`) with
   `*Doc` mirrors in `openapi_schemas.rs`, and a per-domain integration-test file (per
   `tests/sync_groups.rs`). Collections: `switcher/mix-effects`, `switcher/downstream-keyers`,
   `switcher/aux-buses`, transition presets/wipe patterns, `media/assets`, `media/players`,
   `macros`. **Live state stays engine-owned** and is mirrored read-only at
   `GET /api/v1/switcher/state` from the [ADR-RT008](ADR-RT008.md) latest-wins registry — never
   stored in a resource repository (the devices desired/live split).
4. **Spec-first, typed end-to-end.** Every endpoint above enters the utoipa `ApiDoc` (+
   `rest_routes` table, which a test asserts) **before** UI work, then
   `cargo xtask gen-openapi` → `npm --prefix web run generate:api`, so the SPA consumes the
   switcher exclusively through the typed `openapi-fetch` client + TanStack Query hooks. The
   raw-`fetch` bypass documented at `web/src/api/operations.ts:1-21` exists because the spec
   lagged the control plane and the typed client lacked `If-Match`/202 ergonomics — the switcher
   must not extend it; the shared submit helper (point 6) supplies those ergonomics on the typed
   path. The AsyncAPI side is [ADR-RT008](ADR-RT008.md)'s same-push obligation.
5. **SPA shell: one lazy route, one shared connection.** `/switcher` registers as a route-level
   `lazy()` chunk (the `web/src/app/router.tsx` precedent) plus one `NAV_ITEMS` entry
   (`web/src/app/navigation.tsx:35`); all strings Lingui-wrapped with `lingui extract`/`compile`
   in the same push. Before any switcher topic is consumed, build **one shared realtime
   connection service** (a module-level `RealtimeConnection` with a topic-keyed listener
   registry) and migrate the three existing socket-per-hook consumers onto it —
   `useEngineEvents`/`useSystemMetrics`/`useHealth` each open their own WS today (verified above);
   a switcher panel adding tally + switcher + meters on that pattern would multiply sockets
   further. Panel components consume via the passive-query pattern (`useLiveTiles` precedent),
   never owning sockets.
6. **Operation-correlation layer + `Idempotency-Key` in the shared submit helper.** The submit
   helper stores each `AcceptedBody.operation_id` and resolves it against envelope `corr` on the
   [ADR-RT008](ADR-RT008.md) lifecycle events, replacing today's fire-and-toast handling
   (`submitOperation` only surfaces the id). The same helper generates `Idempotency-Key` via
   `crypto.randomUUID()` on every action POST — today **no** client code sends the header.
   Server-side prerequisite, verified: `IdempotencyStore` is an unbounded `Mutex<HashMap>`
   (`crates/multiview-control/src/concurrency.rs:166-168`) — eviction/TTL lands **before** the
   switcher makes per-action keys routine.
7. **Global keyboard-shortcut subsystem — small, in-house, scope-aware.** None exists (verified:
   no hotkey library, no app-level keydown handling). Decision: build a ~100-line scope-aware
   handler rather than adopt a hotkey library, because the hard requirements are integration
   points, not key-map parsing: suppression when `event.target` is an editable element or a Radix
   dialog is open, Lingui-localised visible shortcut reference, live-region announcements, and
   strict-typed registration — a library supplies none of these and adds a dependency for the
   trivial part. Bindings: number keys `1–9` = PVW selection, `Shift+1–9` = PGM punch (direct
   program cut), dedicated CUT and AUTO keys (defaults documented in the on-panel reference).
   Every shortcut is paired with an on-screen button (no keyboard-only functions); bus changes are
   announced via a live region.
8. **Bus/tally affordances per [ADR-W011](ADR-W011.md) — never color alone.** Reuse the shipped
   patterns: `TallyLampBadge` (icon + colour *name* + label,
   `web/src/components/TallyLampBadge.tsx`) and the `LiveDot` shape convention
   (`web/src/components/SystemFooter.tsx:24`). Program = red/filled + "PGM" label; preview =
   green/outline + "PVW" label; on-air keyer and FTB states get the same icon+name treatment.
9. **Monitors: JPEG stills first, WHEP as the upgrade lane.** Replicate the program slot per bus
   (the `ProgramSlot = Arc<ArcSwapOption<Nv12Image>>` pattern,
   `crates/multiview-cli/src/preview.rs:28`, served by `CliPreviewProvider`, `preview.rs:38`,
   through the JPEG routes in `crates/multiview-control/src/routes/preview.rs`) so the panel shows
   ~1 Hz PGM/PVW/clean stills honestly labelled as stills. Sub-second WHEP monitors arrive via the
   [ADR-W023](ADR-W023.md) `<PreviewSurface>` ladder and the preview crate — a separate fidelity
   lane ([ADR-P007](ADR-P007.md)), not a blocker for the panel.
10. **Multi-box composition editor: import the layout-editor pure model — do not fork.** The
    framework-free view-model in `web/src/layout/model.ts` (`NormalizedRect` `:67`, `clampRect`
    `:148`, snap/validate, presets incl. `'pip'` `:376`) is the geometry substrate for the
    multi-box/DVE composition editor ([ADR-0056](ADR-0056.md)); the konva canvas + accessible
    form dual-view pattern is reused likewise.
11. **In-app docs:** one `DOCS_REGISTRY` entry (`web/src/docs/registry.ts:17`) + a lazy
    `/help` concept page (buses, transitions, keyers, FTB, tally, macros as sections/keywords) +
    `HelpLink` affordances from the panel — the [ADR-W016](ADR-W016.md) system; registry tests
    enforce searchability.
12. **Testing:** extract shared Playwright fixtures (auth stub, REST catch-all,
    `routeWebSocket` envelope pusher, crash guard) into `web/e2e/helpers/` **first** — the five
    existing specs each re-declare their mocks (verified: `web/e2e/` contains only spec files) —
    then write the switcher specs (keyboard cut/auto in a real browser, no-color-alone bus state).
    Add `fast-check` (absent from `web/package.json`) and property-test the pure TS bus/transition
    model, per the repo guardrail for stateful pure logic.
13. **External-control friendliness as a first-class contract.** The surface above is deliberately
    the shape a third-party control-surface gateway or hardware key-panel bridge needs (de-facto
    industry practice for switcher integrations): a long-lived WS with a **full `$snapshot` on
    connect plus deltas** (no polling), **stable operator-authored string ids** (M/E, source,
    keyer, player, macro ids from config — never array indices), **idempotent single-shot
    commands** (explicit target state, safe to repeat), and **boolean feedback queries** — the
    `GET /api/v1/switcher/state` mirror and the `$snapshot` expose per-element booleans
    (source-on-program, keyer-on-air, ftb-active, media-playing) so a gateway computes button
    feedback without diffing layouts. Richer per-source usage-kind facts (`{on_aux[],
    used_as_key_fill, used_as_key_source, recording}` — derivable from the same render-plan walk
    [ADR-MV006](ADR-MV006.md) already performs) are a **post-MVP extension** of the same mirror,
    noted here so the shape stays additive. An official gateway module, an OSC namespace, and a
    MIDI surface adapter are
    **post-MVP adapter items** in the [backlog](../development/production-switcher-backlog.md);
    this contract is what makes them thin.

## Rationale

Reusing the shipped conventions is the whole design: bare verbs are ADR-W017 shipped practice
(salvos/alarms/devices), the plan/take split is the BUILT routing classifier surfaced per
invariant #11, the sync-groups file-set is the proven six-piece domain template, and the 202 +
`corr` loop is ADR-W008 — the switcher adds vocabulary, not mechanics. Spec-first is load-bearing,
not stylistic: the verified raw-fetch bypass exists precisely because endpoints shipped ahead of
the spec, and a switcher panel built that way would freeze the drift in. The shared connection
service must precede new topics because the per-hook-socket pattern is already at three sockets per
session and a switcher panel is the heaviest realtime consumer yet. The T-bar-as-absolute-setter
(conflated, integer wire) is the only shape that survives both invariants: latest-wins sampling
means a lost update is healed by the next one, and the engine applies whatever position the
frame-boundary drain last saw — pointer-event rates never become command storms. The in-house
shortcut handler is justified by scope: the deliverable is a scope/a11y/i18n integration, and every
binding must have an on-screen twin anyway, so a key-map DSL dependency buys nothing the panel can
use. External-control friendliness falls out of decisions already made for the SPA (snapshot +
deltas, correlation, idempotency) — pinning it as a contract keeps future adapters from needing
API changes.

## Alternatives considered

**Per-action WebSocket RPC** (commands as inbound WS frames) — rejected: the repo convention is
REST commands + realtime events (ADR-W008/[ADR-RT002](ADR-RT002.md)); the WS session does not even
read inbound frames today (verified, [ADR-RT008](ADR-RT008.md)), and REST gives per-action OpenAPI
schemas, RBAC, `Idempotency-Key` scoping, and problem+json for free — an RPC channel would
duplicate all of it inside one opaque socket. **A hotkey library dependency** — rejected in favour
of the small in-house handler (point 7): the requirement set is suppression contexts, localised
help, live-region announcements, and strict typing — none of which a key-map library provides —
and the parsing it does provide is trivial at this binding count; a dependency would also have to
pass the no-`any` lint wall. **Per-panel sockets** (each switcher widget opens its own WS, the
current hook pattern) — rejected: multiplies connections per session, repeats snapshot delivery
per socket, and makes the connect-time `$snapshot` race visible to users; the shared service with
a topic-keyed registry is strictly cheaper and is required anyway before `$subscribe` lands.
**Toggle-style commands** (`POST .../ftb` flips state) — rejected: a repeated or replayed toggle
inverts intent, breaking both idempotent-retry semantics and the unambiguous-outcome correlation
rule; explicit target state costs one body field.

## Consequences

The control plane gains one new domain family built entirely from existing parts; the OpenAPI and
AsyncAPI documents, the generated SPA types, and the `rest_routes` parity table all grow in the
same push (drift breaks tests by construction). The SPA gains its first shared realtime service —
migrating the three existing hooks is a prerequisite refactor with its own tests, and after it the
per-hook-socket pattern is retired. The submit helper becomes the single mutation path (typed
client + `Idempotency-Key` + correlation), which also gives salvos/tally/routing pages the upgrade
for free; the unbounded `IdempotencyStore` gains eviction before per-action keys ship. The panel
is keyboard-operable end-to-end with on-screen parity and announced state changes (WCAG 2.1 AA
posture per [ADR-W011](ADR-W011.md)); e2e verification runs in a real browser per the standing
Playwright rule. Monitors are honest: stills are labelled stills until the WHEP lane lands.
External gateways get a stable contract from day one — snapshot, deltas, string ids, idempotent
verbs, boolean feedback — so the post-MVP gateway module, OSC namespace, and MIDI adapter are
adapters over this surface, not new surfaces. Risk accepted: until per-client `$subscribe` lands, the panel receives
the full firehose like every other client; publisher-side conflation bounds it
([ADR-RT008](ADR-RT008.md)).
