# ADR-RT010: Live authorization revocation on established WS/SSE realtime sessions

- **Status:** Accepted
- **Area:** Realtime API
- **Date:** 2026-07-10
- **Source:** PR #211 cross-vendor auth panel, reviewer A1 (pre-existing finding); realtime security backlog task #9

## Context

The realtime transports (`GET /api/v1/ws`, `GET /api/v1/events`) authenticate and
authorize the connecting [`Principal`](../../crates/multiview-control/src/auth.rs)
**once, at connect time**:

- The role gate `role.require(Action::Read)` runs pre-upgrade in the
  `RealtimeViewer` extractor (WS) / at the top of `sse_handler` (SSE).
- The principal's object-scope allowlist (`scoped_object_ids`) is copied into the
  `SessionStream` as a fixed `Option<Vec<String>>` (`with_object_scope`), and the
  ADR-W005/ADR-W025 per-object read filter (`object_authz_scope_id`) reads only
  that captured value for the connection's whole life.

Nothing re-checks authorization after connect. If, **mid-session**, the principal's
scope is narrowed or revoked, its role is downgraded, or its API key is revoked, the
established connection keeps delivering deltas for now-unauthorized objects (and keeps
its role-gated firehose) until the client happens to reconnect. **Reconnect is the
only reauthorization point.** For a revocable authorization model that is a
persistent BOLA/authz hole (OWASP API1) on a long-lived connection — and, critically,
a **scope narrowing** leaves objects the client already cached *displayed*, because
the ADR-W025 filter is a silent drop with no "remove this object" signal.

Constraints that bound the answer:

- **Invariant #10 (isolation, [conventions §5](../architecture/conventions.md)).**
  The control/realtime plane must be *physically incapable* of back-pressuring the
  engine: no lock the engine holds, no `.await` that blocks the engine, no channel
  into the engine, no back-pressure on the engine publish path
  ([ADR-P001](ADR-P001.md), [ADR-RT004](ADR-RT004.md)).
- **Compose with the shipped realtime read pipeline.** The fix must not regress
  the ADR-W025 object-scope filter, the [ADR-RT009](ADR-RT009.md) connect-time
  watermark, or resume-by-seq ([ADR-RT003](ADR-RT003.md)) — all read-side,
  per-connection decisions taken **before** `issue_seq` so the per-connection seq
  stays gapless.
- **Today there is no runtime authorization-mutation surface at all.** `ApiKeyStore`
  is an in-memory map provisioned once at boot; there is no revoke/disable/role-edit
  route, no cookie-session store, and no generation/epoch concept. This ADR
  introduces the first such surface *and* the realtime layer that honors it, so a
  future key-management API (a documented follow-up) inherits live revocation for
  free rather than silently re-opening this hole.

## Decision

Adopt an **auth-generation re-resolve** model (a wait-free generation counter that
each session samples, re-resolving the *current* principal on a change), with a
**hybrid response**: a graceful in-place `$resync` for scope changes, a forced
disconnect for loss of read access.

### Authorization source (`crates/multiview-control/src/auth.rs`)

`ApiKeyStore` becomes the single, interior-mutable source of truth for API-key
authorization:

- Interior-mutable `keys: RwLock<HashMap<String, KeyRecord>>` and a wait-free
  `generation: AtomicU64`. `register(&mut self, …)` stays `&mut self` (construction
  path via `RwLock::get_mut`, no lock, no test churn).
- **Runtime mutators (the revocation hook), each bumps the generation while holding
  the write lock** so a session that observes the new generation is guaranteed to
  read the mutated map: `revoke(&self, key_id) -> bool` (removes the key), and
  `set_principal(&self, key_id, Principal) -> bool` (replaces role/scope, keeps the
  secret digest).
- **Re-resolve reads:** `generation(&self) -> u64` (wait-free `Acquire` load) and
  `principal_for_key(&self, key_id) -> Option<Principal>` (`None` = revoked).

Locks are poison-resilient (`unwrap_or_else(PoisonError::into_inner)`). This store is
control-plane only; the engine never touches `state.api_keys`.

### Session re-resolution (`crates/multiview-control/src/realtime.rs`)

`SessionStream` gains `live_authz: Option<LiveAuthz>` where
`LiveAuthz { store: Arc<ApiKeyStore>, key_id, role, generation }`, installed via
`with_live_reauth(...)` **only** for store-managed API-key principals (local-admin
and JWT principals are not in the store; they keep connect-time authz — a JWT
denylist is separate future work). A new `reauthorize(&mut self) -> ReauthOutcome`:

1. `None` live_authz → `Unchanged` (non-store principal).
2. Load `store.generation()`; if unchanged → `Unchanged` (**wait-free fast path,
   one atomic load, no lock**).
3. On a change, `store.principal_for_key(key_id)`:
   - `None` → `Disconnect(Revoked)`.
   - role fails `require(Action::Read)` → `Disconnect(RoleRevoked)`.
   - else adopt the new role; if `object_scope` differs (narrow, widen, or
     `None`↔`Some`) → update it and return `ScopeChanged`, else `Unchanged`.

The per-object filter (`event_in_object_scope`) and the connect-snapshot filters now
read this **live** `object_scope`, so the exact ADR-W025 predicate is unchanged — only
its source becomes re-resolvable.

### Transport response (both `run_ws_session` and `sse_handler`)

Each transport calls `reauthorize()` at the top of every pump iteration **and** on a
low-rate `REAUTH_TICK` (5 s) `tokio::select!` arm, so revocation takes effect
per-delta on an active stream and within one tick on an idle one:

- `Unchanged` → continue.
- `ScopeChanged` → emit `Event::Resync(Resync { reason: AuthzChanged,
  resubscribe: [tiles, devices] })` (a server-initiated `$resync` = full
  **rebuild**, not merge — ADR-RT003) followed by the connect snapshot set
  (`tiles_snapshot_frame` + `devices_snapshot_frames`) now filtered to the **new**
  scope. The client drops all cached objects on those topics and rebuilds from the
  new-scope snapshot: a narrowed-away object is absent (display hole closed), a
  newly-visible object is present. All frames go through `issue_seq` (gapless;
  resume-by-seq intact).
- `Disconnect(_)` → WS closes with code **4403** (forbidden scope; RFC 6455
  private-use range, reserved by [ADR-RT005](ADR-RT005.md) §12); SSE ends the
  stream. The client re-authenticates on reconnect (fresh scope, or denied).

A new `#[non_exhaustive]` variant `ResyncReason::AuthzChanged`
(`crates/multiview-events/src/subscription.rs`, serialized `authz_changed`) is added;
the AsyncAPI spec is regenerated (`cargo xtask gen-asyncapi`).

## Rationale

- **Auth-generation over live-scope-per-delivery.** Re-resolving the full principal
  on a generation change costs one atomic load per delivery on the hot path and a
  short control-plane read-lock only on the rare change; a "read the live scope on
  every delta" model would take the lock on every event. The generation gate keeps
  the steady state wait-free.
- **Hybrid over pure force-disconnect.** A scope *change* while the principal can
  still read does not require tearing down the connection: a server-initiated
  `$resync` is a first-class, already-specified rebuild directive (ADR-RT003) that
  robustly closes the **display** hole (the server controls the rebuild — the client
  cannot "resume past it" and keep a stale cached object, which a
  disconnect-then-resume could). Loss of read access (revoke / non-reading role) has
  no valid continued stream, so it disconnects. This is the most surgical response
  per scenario:

  | Scenario | Response |
  | --- | --- |
  | Scope narrowed (drop cached objects) | `$resync` + new-scope snapshot (in place) |
  | Scope widened (gain objects) | `$resync` + new-scope snapshot (in place) |
  | Role downgraded below read | Disconnect 4403 |
  | Key / session revoked | Disconnect 4403 |

- **Single source of truth.** Making `ApiKeyStore` itself the live source (rather
  than a parallel override registry) means `verify` and the realtime re-resolve
  read the same map; there is no divergence between "what authenticates" and "what
  the session honors".
- **Invariant #10 upheld — stated and proven.** The engine publish path is
  `broadcast::send` on `state.engine.events`, which never reads or locks
  `state.api_keys`. `reauthorize()` is a per-session read-side decision: a wait-free
  `AtomicU64` load on the fast path; a short control-plane `RwLock` **read** only on
  a generation change (a lock the engine never holds). The `REAUTH_TICK` timer, the
  `$resync`, and the re-snapshot are per-client writes to this client's own socket;
  the engine is never awaited, no channel is added into the engine, and nothing can
  back-pressure the publish path. The `$resync`/re-snapshot/out-of-scope drops all
  precede `issue_seq`, so the per-connection seq stays gapless and resume-by-seq, the
  ADR-W025 filter, and the ADR-RT009 watermark are intact.

## Alternatives considered

| Alternative | Rejected because |
| --- | --- |
| **(a) Force-disconnect on any principal change** (close on narrow/widen/downgrade/revoke; client reconnects and rebuilds) | Simplest and fully correct, but every authz change drops the WS/SSE and forces a full reconnect + re-auth round-trip + snapshot rebuild for *all* the principal's sessions — a visible blip on a routine admin edit. Kept as the fallback and surfaced as an operator trade-off; not the default. |
| **(c) Live scope source read per-delivery** (no generation; every delta reads the current scope under a lock) | Correct but takes a control-plane lock on the per-event hot path of every session; the generation gate makes the steady state a single wait-free atomic load, taking the lock only on an actual change. |
| **Silent in-place filter swap on scope narrowing** (update `object_scope`, no `$resync`) | Closes the *stream* hole but not the *display* hole: the client keeps rendering objects it already cached that are now out of scope, because ADR-W025's filter has no removal/tombstone signal. The authz leak persists on the operator's screen. |
| **Per-key generation counters** (bump only the changed principal's sessions) | Avoids waking unrelated sessions, but adds per-key atomics and lookup for a negligible gain — authz mutations are rare admin events, and an unrelated session that re-resolves simply returns `Unchanged`. A single global `AtomicU64` is simpler and equally correct. |
| **Reuse an existing `ResyncReason`** (e.g. `SeqEvicted`) | Semantically wrong and un-auditable: a client/operator cannot distinguish an authz change from a replay-ring eviction. `ResyncReason` is `#[non_exhaustive]`, so `AuthzChanged` is a safe additive variant. |

## Consequences

- **Easier:** a future config-declared / REST key-management surface (revoke, role
  edit, scope edit) gets live revocation on established realtime sessions for free —
  it calls `ApiKeyStore::{revoke, set_principal}` and every live WS/SSE session
  re-resolves. The audit surface can key off the same generation.
- **Harder / committed to maintain:** `ApiKeyStore` is now interior-mutable
  (`RwLock` + `AtomicU64`, no `Clone`); `verify`/`verify_authorization` take a read
  lock (uncontended, cheap). The realtime pump grows a `select!` with a re-auth tick.
  The `resubscribe` topic list (`[tiles, devices]`) must name exactly the topics
  `build_resync_frames` re-snapshots — never a topic that is cleared but not rebuilt.
  `switcher` is deliberately excluded: it is not object-authz-scoped (the scope
  filter is Devices-domain) and is never re-snapshotted at connect or on resync, so
  listing it would strand a rebuild-not-merge client's switcher state. If a new
  object-scoped event class lands on a new topic **and** is re-snapshotted, add it
  here (mirrors the `object_authz_scope_id` coverage set).
- **Bounded latency (operator-facing):** on an active stream revocation is
  effectively immediate (re-checked per delta); on an idle stream it is bounded by
  `REAUTH_TICK` (default 5 s), tunable against per-session idle-wakeup cost. A change
  racing the connect handshake is caught **deterministically**, not left to a later
  bump: `resolve_principal` captures the authorization generation *before* it
  resolves the key and threads it as the session's live-authz baseline (`Some` for
  store keys, `None` for local-admin/JWT), and both transports run a pre-snapshot
  `reauthorize` gate — so a revoke or re-scope landing in the auth→install window
  disconnects (before any snapshot) or adopts the new scope for the first snapshot,
  rather than being retained until an unrelated future mutation. A change landing in
  the sub-tick window *after* the gate but before the snapshot send is caught by the
  first pump iteration — the same bounded latency the per-delta re-check gives the
  steady stream; fully closing that residual would require holding the store lock
  across the socket send, which invariant #10 forbids. Installing the live-authz
  handle for **every** store key (not a racy `principal_for_key().is_some()` re-probe)
  is what lets a revoked-in-window key disconnect instead of being mistaken for a
  non-store principal.
- **Scope for now:** revocation covers store-managed API keys. Auth-disabled
  (`local_admin`) principals are unscoped admins with nothing to revoke; JWT
  principals are re-minted per request from the external token and are not
  store-revocable (a local JWT denylist is separate future work). The `$resync`
  rebuild reproduces the connect snapshot set (tiles + device-status); cast sessions
  and media players have no connect snapshot (as at connect) and are relearned from
  deltas under the new scope.
- **Invariant #10:** touched and preserved (see Rationale). A change that risked it
  would require a chaos/soak test; this one adds no engine-facing lock, await, or
  channel.
