# ADR-W026: Discovery-scope authorization axis (unified event scope model)

- **Status:** Accepted
- **Area:** Web/API
- **Date:** 2026-07-10
- **Source:** PR #235 review HIGH finding + operator decision + discovery-scope-axis-design workflow

## Context

The realtime WS/SSE stream delivers two non-object-scoped events to every authenticated reader, including object-scoped principals: `device.discovered` (untrusted mDNS inventory carrying management endpoints — the topology leak, ADR-0041 posture) and `timing.status` (program epoch/clock posture, `stream_id = ProgramId::MAIN`). The root mechanism is a fail-open classifier: `object_authz_scope_id` (realtime.rs) must carry a `_ => None → deliver` wildcard because `Event` is `#[non_exhaustive]` and `multiview-control` is a foreign crate — every future non-object event silently rides the firehose (e.g. `rist.link.stats` already does). Additionally, no discovery row carries a domain/site id today, and **no production path can mint a scoped principal at all**: the only key provisioning is the unscoped bootstrap admin (`provision_admin_keys`, fed by `MULTIVIEW_CONTROL_TOKEN`). `ApiKeyStore` digests are HMAC under a per-process, never-persisted pepper — key persistence in SQLite is therefore not a drop-in. Four designs were adversarially judged on correctness/#10, compat/blast-radius, and security/completeness; the convergent kill-shots were (a) the wildcard classifier footgun survives per-crate predicate designs, (b) raw `"main"` in `scoped_output_ids` puns two id namespaces, and (c) an unmintable axis is a lock with no keyhole (rule 6 violation).

## Decision

Adopt the **unified scope model** (proposal 1 spine): one total, compile-enforced classification `Event::authz_scope() -> AuthzScope<'_>` owned by `multiview-events` with an exhaustive, wildcard-free match; one shared fail-closed allowlist predicate `scope_permits` owned by `auth.rs`; consumed identically by the realtime delta filter, connect snapshot, and REST. Graft onto it:

1. **Namespaced program grants** — `timing.status` classifies as `AuthzScope::Program(stream_id)`, matched only against `program:<id>`-prefixed entries in `scoped_output_ids`; plain output authorization ignores `program:*` entries and config validation reserves the prefix (kills the `"main"` punning).
2. **Config-declared API keys** (`[[api.keys]]`, secret via `secret_env`) registered at startup through the existing `ApiKeyStore::register` and re-appliable via config-as-code + `set_principal` (RT010 generation bump) — the axis is mintable in the same push without reworking the pepper/digest scheme.
3. **Mint invariant** — key-affecting config applies require a fully-unscoped admin principal; all key create/re-scope/revoke operations emit audit events.
4. **Mesh redaction** — discovery-scoped principals get node-level-gated `/mesh/peers` and a redacted `/mesh/status` (no `via`, `peers_count`, peer-derived `role`).
5. **Fail-closed everywhere**: a scoped principal never sees an unlabelled (`domain: None`) row, on any surface.

Domain provenance: **operator-declared node config** (`[discovery] domain`), stamped by the observing node — never from responder payloads, TXT records, or mesh peer identity.

## The scope model (Principal + events + filter)

**`multiview-events` — the classification (new, replaces per-crate wildcard maps):**

```rust
/// Never serialized; deliberately NOT #[non_exhaustive] so adding an axis
/// breaks every consumer match until handled (the second ratchet).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthzScope<'a> {
    Public,                            // explicit, greppable, review-gated firehose declaration
    Object(&'a str),                   // vs scoped_object_ids
    Output(&'a str),                   // vs scoped_output_ids (plain entries only)
    Program(&'a str),                  // vs scoped_output_ids "program:<id>" entries
    DiscoveryDomain(Option<&'a str>),  // vs scoped_discovery_domains; None label = fail-closed
    ObjectAndOutput { object: &'a str, output: &'a str }, // conjunction: permit_object && permit_output
}
```

Adding an `Event` variant now fails compilation at the point of definition until its author classifies it — the firehose generator is removed, not patched. An in-crate golden-table test constructs a sample of every variant and asserts its classification.

**Folded leak set (cross-vendor review).** The exhaustive classifier initially re-drafted four object/output-bearing events as `Public`, re-ratifying the pre-W026 firehose. Corrected classifications: `RistLinkStats → Output(link_id)` (cname leaks peer topology); `InputStreams → Object(input_id)`; `InputConnection → Object(input_id)` (a new required `input_id` field was added — the event carried no id and was un-scopable); `Salvo{Armed,Taken,Cancelled} → ObjectAndOutput{salvo, head}` when head-scoped, else `Object(salvo)`. The `ObjectAndOutput` conjunction is the faithful twin of the salvo REST handler, which runs `authorize_object(salvo)` **and** `authorize_output(head)`; a most-restrictive single-axis scope would under-gate it.

**`multiview-control/src/auth.rs` — Principal + the one predicate:** `Principal` gains `scoped_discovery_domains: Option<Vec<String>>` (None = all domains incl. unlabelled rows — the compat default; `Some([])` = sees no inventory; `Some(list)` = only listed domains, unlabelled rows DENIED). Plus `is_discovery_scoped()`, and `AuthzScopes { objects, outputs, discovery_domains }` cloned out via `Principal::scopes()`. The single fail-closed rule, encoded once in `allowlist_permits` (`(None,_)=>true`, `(Some(list),Some(l))=>contains`, `(Some(_),None)=>false`) and `scope_permits(&AuthzScopes, AuthzScope)` (exhaustive; Output filters out `program:` entries, Program matches only `program:`-stripped entries). `authorize_object`/`authorize_output` become thin wrappers over `authorize_scope` so REST and realtime cannot fork semantics. No `Default` on `Principal` — the compiler enumerates every construction site (~26 literals, mechanical `None`).

**Realtime filter (invariant #10):** `SessionStream.object_scope` becomes `scopes: AuthzScopes` + `with_scopes(...)`; the single gate at the existing drop point in `frame_for` becomes `if !scope_permits(&self.scopes, seq_event.event.authz_scope()) { return None; }` — still before `issue_seq` (no per-connection seq gaps). `object_authz_scope_id` is deleted. Pure borrowed match + small-Vec scans, no lock/await/alloc; identical proof shape to the shipped #211 object filter. Both WS and SSE funnel through the one `SessionStream` core.

**RT010 live re-scope:** `install_live_reauth` carries all three axes into the session; `reauthorize` compares the full `AuthzScopes`; any-axis change → `ScopeChanged` → `$resync` (`[Tiles, Devices]` set already covers both affected events on `Topic::Devices`; `timing.status` heals at the next ~1 Hz publish).

## Data provenance (where the domain id comes from)

A discovered row is **untrusted responder data** (unauthenticated mDNS) — the device can never assert its own domain. The only trustworthy fact at discovery time is which observer saw it:

- **Source of truth:** new `DiscoveryConfig.domain: Option<String>`, TOML `[discovery] domain = "site-a"`, validated non-empty, ≤64 chars, `[a-z0-9-]` (DNS-label-like). One node = one domain; per-NIC/per-VLAN split is out of scope.
- **Stamping chain (single origin):** `AppState.discovery_config` → the scan handler passes `state.discovery_config.domain.clone()` into `run_scan` → stamps BOTH `DeviceBroadcaster::discovered` (gains a `domain: Option<String>` param) AND the `DiscoveredService` inventory row before `inventory.upsert`. `DiscoveredService::from_raw` gains the `domain` param and explicitly never reads it from `raw`/TXT.
- **Rejected sources:** mesh peer identity (uncorrelatable salted digest — wrong trust direction); network-segment derivation (multi-homed, spoofable, not operator-legible).
- **Forward rule (DEV-B6 / ADR-0045):** the controller stamps the domain from **its own registry record of the enrolled node** — never from the reporting node's wire payload. Pinned by a doc comment + a test asserting the domain argument originates from local config.

## timing.status handling

`TimingStatus` classifies as `AuthzScope::Program(&t.stream_id)` (`stream_id == ProgramId::MAIN == "main"`; pinned by a test referencing the `ProgramId::MAIN` constant). Output-unscoped principals see it; output-scoped principals see it iff `scoped_output_ids` contains **`program:main`**. `program:*` entries are inert for plain output authorization, so a timing grant can never confer REST authority over an output named `main`. `multiview-config` output-id validation rejects ids containing `:`, reserving the prefix. `TimingStatus.groups` redaction for scoped grantees is deferred (see Residual risks).

## REST + connect-snapshot parity

- **Connect/`$resync` snapshot: no filtering work** — the snapshot set is `$hello` + tiles + per-device `device.status`; neither `device.discovered` nor `timing.status` has a snapshot frame. Pinning test only. `id_in_object_scope` reroutes through `allowlist_permits`.
- **`GET /api/v1/discovery/devices`:** after the role check, filter rows by `allowlist_permits(&principal.scoped_discovery_domains, row.domain.as_deref())` — unlabelled rows hidden from scoped principals; list-filtering, not 403 (ADR-W005).
- **`POST /api/v1/discovery/devices/scan`:** `authorize_scope(principal, DiscoveryDomain(node_domain))`.
- **`GET /api/v1/mesh/peers`:** node-level gate — scoped principal sees the list iff the node's own domain is set and in its allowlist, else `200 []`. `Peer` untouched (`deny_unknown_fields` version-skew).
- **`GET /api/v1/mesh/status`:** for discovery-scoped principals outside the node's domain, redact `via: None`, `peers_count: 0`, `role` neutral.
- **Timing REST:** none exists — WS-only enforcement is complete.

## Backward-compat & wire/serde/schema

- **Principals:** never serialized — no migration. New field defaults `None` = see-all. Two release-noted tightenings, both inert in production (no scoped keys exist yet): output-scoped keys lose `timing.status` until granted `program:main`; discovery-scoped keys see only labelled rows.
- **`DeviceDiscovered`** gains `#[serde(default, skip_serializing_if = "Option::is_none")] pub domain: Option<String>` (the existing `name` pattern) + `#[non_exhaustive]` + `new(driver, address, family)` + `with_name`/`with_domain` builders.
- **`DiscoveredService`** (`ToSchema`): same `domain` field/attrs — OpenAPI flows from the derive.
- **`DiscoveryConfig.domain`** + `[[api.keys]]`: `#[serde(default, ...)]` — old configs parse clean.
- **Spec regen in the same PR** (CI spec-freshness gate): `device_discovered_schema()` gains optional `domain`; `cargo xtask gen-asyncapi && gen-openapi`, commit `docs/api/{openapi,asyncapi}.json`; regenerate `web/src/api/schema.ts`.

## API + Web UI surface

Verified gap: **no key CRUD exists for any axis** — shipping enforcement without settability is the banned partial-ship. Resolution (avoids SQLite key persistence, incompatible with the per-process HMAC pepper):

1. **Config-declared API keys** — new `[[api.keys]]` in `multiview-config` (`key_id`, `secret_env`, `role`, optional `scoped_object_ids`/`scoped_output_ids`/`scoped_discovery_domains`). Registered at startup beside `provision_admin_keys` (bootstrap admin unchanged, always unscoped); config-as-code apply re-registers via `set_principal`/`revoke` (RT010 generation bump). Validation: unique `key_id`s, domain charset, `program:`-prefix reservation, `secret_env` presence checked with a clear error.
2. **Mint invariant:** any config apply touching `[[api.keys]]` requires a fully-unscoped admin principal; key create/re-scope/revoke emits an audit event.
3. **Read surfaces:** `GET /api/v1/auth/keys` (admin-only, metadata + three scope axes, never secrets/digests); whoami on `GET /api/v1/account` gains `role` + the three effective scope axes.
4. **Web UI:** Settings → API Keys panel; Settings → Discovery "Discovery domain" field + empty-state warning; discovery inventory Domain column. Client regenerated from OpenAPI.

## Consequences

- The firehose bug **class** is dead: an unclassified `Event` variant is a compile error in `multiview-events`; an unhandled `AuthzScope` axis is a compile error in `auth.rs`. `Public` is an explicit reviewed token.
- Invariant #10 holds structurally: the filter is a per-session wait-free read predicate at the existing pre-`issue_seq` drop point; zero engine/data-plane crates in the diff.
- **Fail-closed:** scoped principal + unlabelled row = deny, encoded once in `allowlist_permits`. Fail-open would make the axis a security no-op (domain is unset on every existing deployment). Deliberate type-encoded asymmetry: `TileState{input:None}` is `Public` (structural placeholder), `DeviceDiscovered{domain:None}` is `DiscoveryDomain(None)` (policy-unlabelled, denied to scoped keys).
- Blast radius: `multiview-events` (S), `multiview-config` (M), `multiview-control` (L), `multiview-cli` (S), `web/` (M), specs + docs. High-risk class → rule-21 **3-reviewer panel**.

## Alternatives rejected

- **Per-crate wildcard predicates:** triples the `_ =>` fail-open pattern; `#[non_exhaustive] Event` makes control-crate matches permanently un-exhaustive.
- **Raw `"main"` in `scoped_output_ids`:** puns program and output id namespaces — silent privilege coupling / grant breakage. Replaced by the `program:` namespace.
- **Program→outputs derivation map** for timing: stale under hot-reconfig — fail-closed availability bug and fail-open multi-program leak.
- **A third standalone timing axis:** fragments grants; the program-namespaced output axis expresses it with zero new Principal surface.
- **SQLite-persisted key CRUD:** persisted digests unverifiable under the per-process HMAC pepper.
- **Deferring key settability:** an unmintable axis is dead code and a rule-6 partial-ship.
- **`Peer.domain` field / per-row mesh filtering:** `deny_unknown_fields` version-skew + data-minimisation; node-level gating suffices.
- **Fail-open on unlabelled rows:** re-opens the leak for the default state of every existing deployment.
- **mDNS/TXT self-asserted or mesh-identity-derived domains; network-segment inference:** untrusted, uncorrelatable-by-design, or spoofable.

## Top residual risks

1. **Config-attested domain has no network binding (weakest-node trust).** A mislabelled node leaks its segment into the wrong domain; an unlabelled node blanks scoped inventory; a multi-VLAN node cannot split per interface; under future mesh-forwarded discovery, correctness depends on the controller stamping from its registry (never the wire) — pinned by test + doc comment.
2. **`TimingStatus.groups` delivered whole to `program:main` grantees** — cross-group sync topology beyond the grantee's outputs. Accepted for v1 (explicit operator opt-in); follow-up is per-session `groups` redaction.
3. **Classification authority is now cross-crate.** A future REST handler can gate a resource while the events-crate table says `Public`. Mitigated by the control-crate parity test + mandatory justification comments on `Public` arms — review-dependent.
4. **Tightening masquerades as breakage.** Newly-scoped keys on unlabelled nodes see empty discovery; output-scoped keys lose the epoch. Mitigated by whoami scope observability, the UI empty-state warning, and the release-note remedies.

## Implementation status (as-built)

**Delivered + gated in this lane** — the enforcement spine is complete end-to-end:
`Event::authz_scope()` (exhaustive, wildcard-free, incl. the folded leak set +
`ObjectAndOutput`); `DeviceDiscovered.domain` + `InputConnection.input_id` wire
fields; the shared fail-closed `scope_permits`/`authorize_scope` predicate with
the `program:` namespace + composite conjunction; `Principal.scoped_discovery_domains`;
the realtime delta **and** connect-snapshot filters routed through `scope_permits`
across all axes, with RT010 carrying all three axes into live sessions;
`[[api.keys]]` config + validation and cli startup registration (scoped keys are
**mintable**); the `GET /discovery/devices` domain filter + `POST /scan` gate
(REST twins of the stream filter); domain stamped from local config through the
single scan origin; regenerated OpenAPI/AsyncAPI + web types. Full TDD (RED→GREEN
per step); `cargo test --workspace`, workspace clippy `-D warnings`, and web
lint/build green.

**Deferred follow-ups (tracked, not silently dropped).** These are additional
management/observability surfaces on *other* resources; the discovery-scope axis
itself is fully enforced without them, and both new config knobs are already
API-reachable + operator-settable through the config-as-code resource
(`/config` import/export/apply):

1. **Mesh redaction** — `/mesh/peers` node-level gate + `/mesh/status` peer-field
   redaction for out-of-domain discovery-scoped principals. Defense-in-depth on
   the mesh resource (owned by the mesh lane); the discovery *inventory* leak is
   closed by the `/discovery/devices` filter.
2. **`GET /api/v1/auth/keys`** (admin read of key metadata + scopes) and the
   **whoami** scope extension on `GET /api/v1/account` — observability so a
   confined key can see why its view is empty.
3. **Dedicated web UI controls** — a Settings → API Keys panel, a "Discovery
   domain" settings field with empty-state warning, and a discovery-inventory
   Domain column. The web *client* already integrates the new API surface (green
   build); these are named typed panels layered on top.
4. **Live file-watch key re-registration** — an `[[api.keys]]` change is surfaced
   by the config diff and applies on restart; a hot re-apply path (diff →
   register/revoke/set_principal, RT010 generation bump) is the follow-up. The
   RT010 machinery it would drive is already in place and tested.

None of the deferred items weakens the enforcement: an out-of-scope principal
cannot receive an out-of-domain/out-of-output event on the stream, cannot read it
via `/discovery/devices`, and cannot spend the scan budget.

## Auth-panel follow-up (as-built)

The rule-21 cross-vendor auth panel on the delivered lane raised nine findings.
Three were confirmed defects on the new config/auth surface and are fixed in this
lane (RED→GREEN, separate commits); the rest are dispositioned with rationale.

**Fixed:**

- **Whole-system authorization spans every axis (SEC-10).**
  `require_unscoped_for_whole_system` denies a principal scoped on the object,
  output, **or** discovery-domain axis — keyed off the unified
  `AuthzScopes::is_global` / `Principal::is_global` predicate (unrestricted iff
  every axis is `None`), not a frozen per-axis check, so a new axis is covered in
  one place. The guard is also installed on the two whole-system config
  **mutations** that previously omitted it entirely (`revert-to-start`,
  `promote`) — each rewrites the entire running/boot document across all objects
  — and runs **first**, before the boot-model lookup, idempotency reservation,
  config composition, command submit, or boot-file write, so a scoped principal
  is denied (403) before any side effect or disclosure. (`redact_device_refs_in_body`
  keeps its object-scope check: device refs are objects, a distinct concern.)
- **Config cannot mint an administrator (F2).** `ApiKeyRole` carries no `Admin`
  variant: admin authentication is environment-only (the bootstrap
  `MULTIVIEW_CONTROL_TOKEN`, always unscoped), so a `[[api.keys]]` declaring
  `role = "admin"` is structurally unrepresentable and fails to parse
  (fail-closed). The bootstrap admin builds `Role::Admin` directly, never through
  `ApiKeyRole`, so `role_from_config` drops its admin arm and stays exhaustive.
- **Config keys cannot clobber the bootstrap admin (F3).**
  `register_config_api_keys` fails closed on a reserved (`admin`) or
  already-registered `key_id` — a HARD startup error, never a silent HashMap
  overwrite of an existing key's secret + principal (lockout / takeover). The
  reserved id is the shared `BOOTSTRAP_ADMIN_KEY_ID` constant.
- **Empty object grants rejected (F5).** `ApiConfig::validate()` rejects an empty
  `scoped_object_ids` entry, at parity with the existing output-grant check.

**Dispositioned (no code change; verified):**

- **`TilesSnapshot` classified `Public` (F1)** — correct by construction, not a
  leak. Its only producer is the per-session connect snapshot
  (`SessionStream::tiles_snapshot_frame`), which retains tiles by
  `id_in_object_scope` before send; `Tiles` is excluded from the resume ring, so
  nothing rides it unfiltered. The live tile **deltas** (`tile.state`) are
  `Object`-scoped through `scope_permits`. A whole-collection snapshot cannot be a
  single `AuthzScope::Object`, so `Public` + construction-time filtering is the
  correct model.
- **`InputConnection.input_id` required, no `serde(default)` (F4)** — safe.
  `multiview-mesh` carries no `multiview_events::Event` and never
  `InputConnection`; realtime events are ephemeral (live snapshot + delta over an
  in-memory resume ring), never persisted or sent cross-version, so there is no
  mixed-version wire to break.
- **Colon reservation in `scoped_output_ids` (F6)** — only the `program:` prefix
  needs reserving, and it is: `validate()` rejects a bare `program:`, and
  `scope_permits` filters `program:` entries out of plain-output authorization.
  A generic `:`-bearing entry is either a valid plain-output grant or inert — it
  can only ever authorize an output whose id it matches exactly, never pun the
  timing namespace. The `main` vs `program:main` pun is fully closed by the prefix
  separation, which fails closed (a timing grant can never confer plain-output
  authority).
- **Allowlist scan bound (F7)** — the allowlists are operator-authored config
  parsed once at startup (not attacker input); an over-large list fails slow at
  boot, never a runtime DoS. No fixed cap is imposed because it would
  over-restrict a legitimately large deployment (e.g. hundreds of scoped objects).
