# ADR-W025: Object scope restricts visibility — collection reads and realtime delivery filter to the principal's allowlist

- **Status:** Accepted
- **Area:** Web/API Stack
- **Date:** 2026-06-21
- **Source:** PR #205 BOLA review panel (tracked task #140) — extends [ADR-W005](ADR-W005.md)

## Context

[ADR-W005](ADR-W005.md) mandates per-object (BOLA, OWASP API1) authorization on
**every resource** in addition to the coarse RBAC role: a principal may carry an
explicit object allowlist (`Principal::scoped_object_ids`) and is denied any id
outside it even when its role would otherwise permit the action. The per-object
handlers enforce this on the path id — `GET /api/v1/cast/sessions/{id}`,
`GET /api/v1/devices/{id}`, and the device/cast mutations all call
[`authorize_object`](../../crates/multiview-control/src/auth.rs) on the addressed
id and return `403` for an out-of-scope object.

ADR-W005 left one thing implicit: whether the scope restricts only *mutation* of
a single object, or also the *visibility* of objects across the read surfaces —
the collection `GET`s and the realtime WebSocket/SSE stream. The PR #205 review
panel found that the two collection reads (`list_cast_sessions`, `list_devices`)
and the realtime snapshot+delta surface ([`realtime.rs`](../../crates/multiview-control/src/realtime.rs))
returned the **full** device/cast surface to a scoped principal — every row and
every event, regardless of its allowlist. That is a pre-existing surface-wide gap
on `main` (not introduced by #205), tracked as task #140. The task-140 review
panel's exhaustive sweep additionally found the same class on the AMWA NMOS Node
API: the IS-04 LIST routes (`/x-nmos/.../devices|senders|receivers`) were
role-gated only, and the IS-05 single-receiver connection **read**
(`GET .../receivers/{id}/active`) was not per-object authorized even though its
sibling stage `PATCH` was. A subsequent round (corrected sweep methodology: grep
for device-id *references*, not just device collections) found two further
classes: (a) config objects that **embed** a managed device id in a field rather
than being a device collection — device-projected sources/outputs
(`body.device_ref`, ADR-M009) and sync-group members (`body.members[].device`,
ADR-M010); and (b) two **whole-system artifacts** — the config export (`GET /config/export`)
and the support bundle (`POST`/`GET /support/bundle`), gated only by role, which
compose every device + ref + member id into one document a scoped principal could
read wholesale. All folded into this decision.

The decision is bounded by invariant #10 (isolation): the realtime path is
best-effort, bounded, and **physically incapable of back-pressuring the engine**
([ADR-P001](ADR-P001.md), [ADR-RT004](ADR-RT004.md)). Any per-principal filtering
on that path must be a pure read-side projection — no blocking, no new awaits, no
channel the engine can stall on.

## Decision

**An object-scoped principal's allowlist restricts VISIBILITY, not just
mutation.** Because a single-object `GET` returns `403` for an id outside the
allowlist, a collection read or realtime stream that returns that same object is
an enumeration leak — it tells a scoped principal an object it may not read
exists, and discloses its contents. By parity, the read surfaces must filter to
the allowlist:

1. **`list_cast_sessions`** ([`routes/cast_sessions.rs`](../../crates/multiview-control/src/routes/cast_sessions.rs))
   — after the role gate, retain only rows for which
   `authorize_object(&principal, &record.id)` succeeds. An unscoped principal
   (`scoped_object_ids: None`, e.g. admin/unrestricted operator) sees every row,
   exactly as before.
2. **`list_devices`, `list_sources`, `list_outputs`, `list_sync_groups`,
   `list_layouts`, `list_players`, `list_overlays`, `list_probes`, `list_salvos`**
   ([`routes/{devices,sources,outputs,sync_groups,media_players,overlays,probes,salvos}.rs`](../../crates/multiview-control/src/routes/)
   and `list_layouts` in [`routes/mod.rs`](../../crates/multiview-control/src/routes/mod.rs))
   — the same per-**row** filter on the resource's own id. Each resource is gated
   by its own id on the single `GET` (`get_source`/`get_output`/`get_sync_group`/
   `get_layout`/`get_player`/`get_overlay`/`get_probe`/`get_salvo` all
   `authorize_object(&id)`), so the *list* must drop out-of-scope rows, not merely
   redact embedded fields (a redacted-but-present row would still leak the row's
   own id). Sources/outputs/sync-groups additionally redact an embedded
   out-of-scope device ref on the *surviving* in-scope rows (item 5); the other
   five resources carry no embedded device ref, so the row filter alone is the
   complete fix (parity with `list_devices`/`list_cast_sessions`). The
   **definitive sweep**: every collection whose single `GET /{id}` calls
   `authorize_object` on its own id is row-filtered; collections that authorize no
   caller object id (`list_alarms`, `list_tally`/`list_profiles`, `list_logs`,
   `list_health`, `list_discovered` untrusted hints, the licence/telemetry/mesh
   singletons) are role-only by design and need no filter.
3. **Realtime WS/SSE** ([`realtime.rs`](../../crates/multiview-control/src/realtime.rs))
   — [`SessionStream`](../../crates/multiview-control/src/realtime.rs) carries an
   optional object-scope allowlist set from the connecting principal. A delta
   whose event scopes to an `authorize_object`-axis id outside the allowlist is
   dropped (returns `None`, exactly like the resume/conflated skips):
   `device.*`/`cast.session.*` by their device/session id, `media.player_state`
   by its player id, and `tile.state` by its bound input id (`object_authz_scope_id`).
   The connect-time `device.status` snapshot frames skip out-of-scope device ids,
   and the connect-time **tiles** snapshot drops tiles bound to an out-of-scope
   input (placeholder tiles with no input are kept). An unscoped principal's
   stream is unchanged. The filter is a per-client read decision on the
   already-delivered event — the engine's publish path (`broadcast::send`) is
   untouched (invariant #10).
4. **AMWA NMOS Node API** ([`nmos/mod.rs`](../../crates/multiview-control/src/nmos/mod.rs))
   — the IS-04 LIST routes filter to the allowlist: `list_devices` by the
   device's own id; `list_senders` / `list_receivers` by the resource's own id
   **or** its `device_id` link (`nmos_resource_in_scope`, so a device-scoped
   principal sees that device's senders/receivers). The IS-05 single-receiver
   surface is **deliberately asymmetric** between read and write:
   - the connection **READ** (`get_active`) uses the device-link gate
     (`authorize_receiver`) — a visibility concern, matching the receiver LIST;
   - the staged-connection **WRITE** (`patch_staged`) uses the **strict
     own-receiver-id** gate (`authorize_object(&id)`) — a mutation must not widen
     to "any receiver of a device I'm scoped to", so the write authz is exactly
     the pre-existing behaviour (non-weakening).

   A known out-of-scope receiver is `403`; an unknown id stays `404` (a missing
   id is reported as not-found, never disclosed as forbidden).
5. **Embedded device-id references** (config objects that *reference* a managed
   device in a field rather than being a device collection) — redacted for a
   scoped principal by `redact_out_of_scope_device_refs` / its body-level core
   `redact_device_refs_in_body`
   ([`routes/mod.rs`](../../crates/multiview-control/src/routes/mod.rs)), applied
   on the single `GET`, on the **surviving** in-scope list rows (after the item-2
   row filter), and on the audit `detail` bodies (item 7), for:
   - **sources / outputs** — `body.device_ref` (the ADR-M009 device-projection
     link): the key is removed when the referenced device is out of scope.
   - **sync groups** — each `body.members[].device` (ADR-M010): the `device` key
     is removed from an out-of-scope member (the member entry + `offset_ms`
     stay). The resource itself is still gated by its own id (so an in-scope
     principal sees the row); only the embedded out-of-scope device id is hidden,
     by parity with a single-device `GET` `403`. No-op for an unscoped principal.
6. **Whole-system artifacts** — a single document that composes **every** device,
   `device_ref`, and sync-group member id. Per-field redaction cannot apply (a
   partial config is not a valid runnable document), so an **object-scoped
   principal is denied** (`403`) by the shared
   [`require_unscoped_for_whole_system`](../../crates/multiview-control/src/routes/mod.rs)
   gate; such artifacts are confined to a principal that can see the whole system.
   Three surfaces:
   - the **config export** (`GET /api/v1/config/export`,
     [`routes/config.rs`](../../crates/multiview-control/src/routes/config.rs));
   - the **support bundle** (`POST` + `GET /api/v1/support/bundle[/{id}]`,
     [`routes/support.rs`](../../crates/multiview-control/src/routes/support.rs));
   - the **diagnostics snapshot** (`POST /api/v1/diagnostics/snapshot` +
     `GET /api/v1/diagnostics/{id}`,
     [`routes/telemetry.rs`](../../crates/multiview-control/src/routes/telemetry.rs)).

   The support bundle and diagnostics snapshot both compose a `config` section
   that embeds the whole config redacted only for secrets/URLs (**not** ids); for
   each, both the compose (`POST`) and read (`GET`) are gated. Unscoped
   admin/operator/viewer keep all three. The whole-system config *writes*
   (`revert-to-start`, `promote`) return only status (no device ids) — not a
   read-leak, so unchanged.
7. **The change-audit log** (`GET /api/v1/audit`,
   [`routes/audit.rs`](../../crates/multiview-control/src/routes/audit.rs)) —
   every entry carries an `object_id` AND a `detail` body that, for an Update,
   is the full resource body (device ids, `device_ref`, members). Unfiltered it
   re-discloses every object id and the very refs items 2/5 hide. For a scoped
   principal: an explicit `?object_id=<out-of-scope>` is denied `403` (a
   per-object probe, like a single-object `GET`); entries are retained only when
   their `object_id` is in the allowlist; and each surviving entry's `detail`
   body is run through `redact_device_refs_in_body`. No-op for an unscoped
   principal.

The scope axis filtered is the **object** axis (`scoped_object_ids`,
[`authorize_object`](../../crates/multiview-control/src/auth.rs)), matching the
per-object handlers. The output axis (`scoped_output_ids`,
[`authorize_output`](../../crates/multiview-control/src/auth.rs)) is unchanged —
it gates the cast-target rendition on `start_cast_session`, a write, and is not a
read-visibility concern here.

On the realtime path, scope is matched against the event's object id as the
**dedicated authz** helper
[`object_authz_scope_id`](../../crates/multiview-control/src/realtime.rs)
derives it — the device id for `device.*`, the session id for `cast.session.*`,
the **player id** for `media.player_state`, and the **bound input id** for
`tile.state` (when one is bound). These are exactly the ids an `authorize_object`
read handler gates: `get_device`, `get_cast_session`, `get_player` (and the
player transport verbs), and `get_input_streams` respectively. The connect-time
device-status **and tiles** snapshots filter on the same axis
([`id_in_object_scope`](../../crates/multiview-control/src/realtime.rs)), so the
baseline cannot leak an object the deltas then hide.

The helper is **intentionally narrower** than
[`event_scope_id`](../../crates/multiview-control/src/realtime.rs) (used to set
the envelope `id` for the `ids` topic filter): `event_scope_id` also returns an
id for `timing.status` (a program/output stream id) — which **no** REST read
`authorize_object`-gates — so filtering it by `scoped_object_ids` would be a
wrong-axis check that could over-restrict. `object_authz_scope_id` therefore
returns `None` for `timing.status`, for a `tile.state` carrying **no** bound
input (a placeholder/no-signal tile has no object to authorize), for
`device.discovered` rows (no registry id yet), and for the
`$control`/alerts/audio firehose — all gated only by the connect-time role
(`Action::Read`), never by object scope.

## Rationale

- **Read/write parity is the only coherent BOLA posture.** If `GET /{id}` is a
  `403` but the object still shows up in `GET` (collection) and on the stream,
  the per-object check is cosmetic — the attacker enumerates and reads the same
  data through the unfiltered surfaces. The strongest single-object guard in the
  codebase already treats scope as a visibility boundary; the collection and
  realtime reads were the outliers.
- **Filtering is cheaper and safer than the alternatives.** A per-row
  `authorize_object(...).is_ok()` is the identical predicate the handlers already
  use — no new policy, no second source of truth. On the realtime path the filter
  is a `match`/lookup on an event already pulled from the broadcast; it adds no
  await and cannot block, so invariant #10 holds by construction.
- **Default deployments are unaffected.** Object scoping is opt-in
  (`scoped_object_ids: Some(..)`); the common admin/operator/viewer principals
  are unscoped and see everything exactly as before. The change only narrows what
  an explicitly-scoped principal sees.

## Alternatives considered

| Alternative | Rejected because |
| ----------- | ---------------- |
| Leave collection/realtime unfiltered (scope = mutation only) | The enumeration + disclosure leak the panel found; contradicts the single-`GET` `403` and ADR-W005's "BOLA on every resource". |
| Filter the REST collections but not realtime | The realtime stream re-leaks the same objects (snapshot + every delta) the collections now hide — half a fix. |
| Add a server-side filtered subscription / per-scope broadcast channel | Would put policy on the engine publish path and risk a per-client channel the engine could stall on (invariant #10). A read-side projection on the existing shared subscription is strictly safer. |
| Return `403` on the whole collection/stream for any scoped principal | Over-restrictive: a scoped principal legitimately reads its own objects; it must see its in-scope rows/events, just not others'. |

## Consequences

- **Easier:** the BOLA posture is now uniform — single-object, collection, and
  realtime reads all honour the same allowlist; a future scoped role
  (config-declared API keys) gets correct visibility for free.
- **Committed to maintain:** any **new** collection read (REST or NMOS) whose
  single `GET /{id}` calls an `authorize_object` axis must apply the same per-row
  object filter; any new realtime event whose id is an `authorize_object` axis
  must be added to
  [`object_authz_scope_id`](../../crates/multiview-control/src/realtime.rs) (the
  authz helper, not the broader `event_scope_id`) so the realtime filter and any
  connect-time snapshot can gate its id; and any new config field that **embeds a
  managed device id**
  must be added to
  [`redact_out_of_scope_device_refs`](../../crates/multiview-control/src/routes/mod.rs)
  (the sweep is for device-id *references*, not only device-typed collections). A
  new collection/field that forgets the filter is a re-introduced leak — covered
  by the BOLA tests added with this ADR.
- **Invariant #10 (isolation):** preserved. The realtime filter is a pure
  per-client read-side projection on events already received from the bounded
  broadcast; it adds no blocking, no await, and never touches the engine publish
  path. A dropped (out-of-scope) event is simply not forwarded to that one
  client — the same shape as the existing lagged-skip / resume / conflated skips.
- **No API/schema change.** Response shapes are unchanged; a scoped principal
  simply receives a subset. Unscoped principals (the default) are byte-for-byte
  unaffected.
