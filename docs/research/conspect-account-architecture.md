# Conspect — the account-side subsystem (entitlement, mesh, two-pipe telemetry, support)

**Area:** Account / Licensing / cross-cutting (control plane + new leaf crates + web/)
**Status:** Design brief (Proposed) — docs-only; implementation follows in dependency-ordered waves.
**Drives:** [ADR-0050](../decisions/ADR-0050.md) (licensing/entitlement + enforcement-ladder-as-data +
never-off-air), [ADR-0051](../decisions/ADR-0051.md) (mesh discovery/relay),
[ADR-0052](../decisions/ADR-0052.md) (two-pipe heartbeat/telemetry + consent + retention),
[ADR-0053](../decisions/ADR-0053.md) (support/ticketing + context-pack + data-request approval + audit).
**Backlog:** `CONSPECT-*` in [`../development/work-schedule.md`](../development/work-schedule.md).

> **Naming.** "Conspect" is the internal name for the **account-side subsystem** of Multiview — the
> licensing/entitlement plane, the local-mesh discovery/relay plane, the two-pipe
> heartbeat+telemetry plane, and the support/ticketing plane. It is **not** a separate product and
> **never** touches the protected output core. Where this brief says "the engine", it means the
> [protected output core](core-engine.md) governed by invariant #1.

---

## 0. The non-negotiable framing (read this first)

Three rules govern everything below. They are the reason the subsystem is shaped the way it is.

1. **The output clock is untouchable (invariant #1).** Conspect enforcement **never** stops, stalls,
   or de-paces program output. The hardest enforcement rung the ladder can ever reach **still emits
   one valid frame per tick, forever.** Enforcement degrades only *conveniences* — it can lock
   reconfiguration, stamp a corner watermark into the canvas, and refuse to *start a new* engine
   instance. It can never take a running program off air. This is a product promise, not a
   nicety: a broadcaster's program is sacred. (See §6 for the full safety analysis.)

2. **Enforcement is data, not control flow.** The enforcement ladder is a **state machine exposed as
   a field on the licence resource** (`enforcement.level` + `enforcement.reasons[]`). The engine
   reads a single wait-free flag derived from that state; it never runs licence logic on the hot
   path. The portals, the API, and the operator all see the *same* state. There is no hidden code
   path that "decides" to enforce — the state decides, and every surface renders it.

3. **Two pipes, never co-mingled.** There is a **mandatory monthly licensing heartbeat** (the minimum
   contact that keeps an entitlement live) and an **opt-in daily telemetry** stream (anonymised
   product analytics). They are **separate transports, separate consent, separate copy, separate API
   surfaces, separate retention.** The heartbeat is not "telemetry you can't turn off"; it is the
   licensing keep-alive and it carries only what licensing needs (salted digests, never raw
   identifiers). The settings UI, the API docs, and the privacy copy must **never** present them as
   one switch. (See §7.)

Everything in this subsystem is **greenfield** — there is no existing lease, heartbeat, entitlement,
fingerprint, mesh, or consent primitive in the tree. The only precedent is the **NDI codec-licence
gate** (`multiview-input/src/ndi/license.rs`), which is a build/runtime *codec* gate, unrelated to
*account* licensing. Greenfield is an advantage: the new leaf crates collide with nothing and are
built first (§9, §13).

---

## 1. Companion documents are EXTERNAL governing artefacts (record this)

The Conspect Machine WebUI spec references three companion documents. **None of them is in this
repository** (verified by full-tree search: zero matches for `conspect`/`Conspect`, no §11 endpoint
table, no constant block). They are **external governing artefacts** — authoritative, but not under
version control here:

| Companion document | Status | What it governs | Our posture |
|---|---|---|---|
| **Licensing Architecture specification** | EXTERNAL — not in repo | The legal/commercial entitlement model, tier definitions, the licence-server side of the wire | This brief models only the **machine-side** behaviour the spec pins; tier semantics are taken as given |
| **Pricing & Hardware Platform Strategy v4** | EXTERNAL — not in repo | Tier→price→hardware-class mapping, partner/reseller model | Out of scope for the machine; the licence resource carries an opaque `tier` we render, never compute |
| **Portal HTML prototypes** | EXTERNAL — not in repo | The customer/admin portal screens that consume the same constants | We must keep **every constant exact** (§2) so the portals and the machine agree byte-for-byte |

**Consequence:** where the in-conversation spec is **silent** on a machine-side mechanism, this brief
**proposes** an approach and **explicitly flags it for operator confirmation** (collected in §14).
We do not invent commercial policy; we design the machine to obey it.

---

## 2. Hard constants (the portals use these — keep them EXACT)

These constants are load-bearing across the machine **and** the external portal prototypes. They are
recorded here as the single in-repo source of truth so the implementation, the tests, and the portals
agree. **Do not round, re-derive, or "tidy" any value** — a portal that shows "15 minutes" and a
machine that enforces 14 is a support incident.

### 2.1 Claim / transfer / pairing windows

| Constant | Value | Meaning |
|---|---|---|
| `CLAIM_CODE_TTL` | **15 minutes** | A pairing/claim code is valid for 15 minutes from issue, then expires unused |
| `CLAIM_REJECT_WINDOW` | **24 hours** | A rejected claim cannot be retried against the same machine for 24 hours |
| `TRANSFER_WINDOW` | **72 hours** | A device transfer (machine moves to a new owner) completes within a 72-hour window |
| `ACTIVATION_WINDOW` | **31 days** | The rolling window inside which an entitlement must see at least the minimum heartbeat contact |

### 2.2 Lease durations (the heartbeat-granted entitlement lease)

| Constant | Value | Meaning |
|---|---|---|
| `LEASE_FULL` | **35 days** | A successful heartbeat grants a 35-day entitlement lease (covers a missed monthly contact + margin) |
| `LEASE_GRACE` | **14 days** | After lease expiry, a 14-day grace period where conveniences still work but warnings escalate |
| `LEASE_HARD` | **90 days** | The absolute outer bound from last contact before the hardest rung (block-new-instance) applies |

> The lease arithmetic is intentionally generous: a machine that phones home **once a month** stays
> comfortably inside `LEASE_FULL`; a machine offline for a fortnight after expiry is still degrading
> only conveniences; only a machine that has not made contact for **months** reaches the hardest rung
> — and **even then the running program stays on air** (§6).

### 2.3 Fingerprint scoring (machine identity continuity)

| Constant | Value | Meaning |
|---|---|---|
| `FINGERPRINT_MATCH_STRONG` | **100** | A perfect match of the salted hardware-fingerprint component set |
| `FINGERPRINT_MATCH_THRESHOLD` | **70** | At/above 70 the machine is treated as the *same* machine (hardware drift tolerated); below 70 it is a *new* machine requiring re-claim |

The fingerprint is a **score over salted component digests**, never raw serials/MACs (§8). A
RAM upgrade or a swapped NIC drifts the score down but stays ≥ 70; a wholesale re-platform drops
below 70 and forces a fresh claim — exactly the desired behaviour.

### 2.4 Claim-code charset

The pairing/claim code uses a fixed, **ambiguity-free** charset and length so the portal and the
machine generate/validate identically:

- **Charset:** the 6-character set excludes visually ambiguous glyphs. Pin the **exact** alphabet the
  spec/portal uses (the spec defines it; record it verbatim in the implementation constant and the
  ADR — see §14 operator-confirm item if the alphabet glyph set is ever ambiguous).
- **Length:** **6 characters** (`CLAIM_CODE_LEN = 6`).
- Codes are **case-insensitive on input, canonicalised on store**, single-use, and bound to the
  `CLAIM_CODE_TTL` window.

> **Operator-confirm (§14):** the spec pins the *length* (6) and the *ambiguity-free* property; the
> exact glyph alphabet must be copied verbatim from the portal source so the two generators agree.

### 2.5 Cryptography

| Constant | Value | Meaning |
|---|---|---|
| Signature scheme | **Ed25519** | All licence assertions, mesh digests, and relay payloads are Ed25519-signed |
| Digest salting | per-deployment salt | Hardware/identity components are salted before hashing (§8) so digests are not reversible to identifiers |

---

## 3. Subsystem map (what the account side is made of)

Conspect is four planes plus a web surface. Each plane is wait-free / off-the-hot-loop and cannot
back-pressure the engine (invariant #10). The planes and their homes:

```
                        ┌────────────────────────── multiview-cli (wiring) ──────────────────────────┐
                        │                                                                            │
  ┌─────────────────────┴─────────────┐   ┌───────────────────────────┐   ┌──────────────────────────┴───┐
  │ ENTITLEMENT plane                 │   │ MESH plane                │   │ TELEMETRY plane               │
  │ crate: multiview-licence (NEW)    │   │ crate: multiview-mesh(NEW)│   │ crate: multiview-telemetry    │
  │ - entitlement/lease state machine │   │ - always-on mDNS announce │   │ (EXTENDED)                    │
  │ - enforcement ladder AS DATA      │   │ - peer inventory (signed) │   │ - heartbeat status surface    │
  │ - fingerprint scoring             │   │ - end-to-end-signed relay │   │ - two-pipe consent (LWW)      │
  │ - claim/transfer state machine    │   │   for offline machines    │   │ - metrics retention (consent-│
  │ - heartbeat CLIENT (build-flag)   │   │ - untrusted confirm-adopt │   │   independent local buffer)   │
  └─────────────────────┬─────────────┘   └─────────────┬─────────────┘   └──────────────────┬──────────┘
                        │                               │                                     │
                        └───────────────┬───────────────┴──────────────────┬──────────────────┘
                                        ▼                                  ▼
                        ┌───────────────────────────────────────────────────────────┐
                        │ CONTROL plane: multiview-control (EXTENDED)                │
                        │ - ~30 REST routes (§11) + OpenAPI 3.1 (utoipa)             │
                        │ - support/ticketing + context-pack + data-request approval │
                        │ - append-only audit store (sqlite-backed, feature-gated)   │
                        └───────────────────────────────┬───────────────────────────┘
                                                        ▼
                        ┌───────────────────────────────────────────────────────────┐
                        │ WEB plane: web/ (EXTENDED)                                  │
                        │ - 7 screens: /welcome /settings/account /licence /data     │
                        │   /mesh /system/actions /help                              │
                        │ - chrome: header chip · ladder banner · pending-action     │
                        │   strip · config-lock interceptor (tile watermark = engine)│
                        └───────────────────────────────────────────────────────────┘
```

The **engine** appears nowhere as a dependency of these planes. It exposes **five injection seams**
(§5) that the planes *feed* — all wait-free, all derived-from-data, none on the tick path.

---

## 4. The honest source-build caveat (state this plainly, per spec §4.1)

Multiview is **source-available**. A determined builder can compile the binary with the **heartbeat
client compiled out** (the entitlement crate's `heartbeat` feature off), or stub the entitlement
state to "always active". We state this honestly and design accordingly:

- **The licence terms bind regardless of the binary.** The
  [Multiview Source-Available Non-Commercial License](../../LICENSE) governs *use*, not the presence
  of an enforcement client. Removing the heartbeat does not grant a commercial right; it is a licence
  breach with the binary as evidence, not a loophole.
- **The official binaries are the licensed convenience.** Aperim's signed, distributed builds carry
  the heartbeat and the entitlement plane wired in. A customer running an official build gets the
  smooth claim/lease/renew experience; a self-modified build forfeits that convenience and operates
  in breach if used commercially.
- **Enforcement is therefore a convenience-and-evidence mechanism, not a DRM wall.** The ladder
  (§5/§6) makes the *official* product gently insistent (warnings → config-lock → watermark →
  block-new-instance) while **never** taking program off air. It is not, and is not designed to be, a
  cryptographic lock that a source build cannot bypass. We do not pretend otherwise in copy, docs, or
  the ADR. This honesty is a **requirement** (operator directive, spec §4.1), not a disclaimer.

**Design implication:** the heartbeat client lives behind a Cargo feature (`heartbeat`, on in the
shipped presets). The *entitlement state model* (the ladder, the lease arithmetic, the resource shape)
is **always compiled** so the API and UI render consistently even in a build with the client off — in
that build the licence resource simply reports `enforcement.level = "unlicensed-build"` honestly
rather than faking "active".

---

## 5. The five engine seams (all wait-free, all derived-from-data)

The engine exposes exactly five injection seams for Conspect. Each is greenfield, each preserves
invariants #1 and #10, and each consumes **data** the entitlement plane publishes (never licence
logic inline). These mirror the proven isolation primitives (ADR-I001: `arc_swap::ArcSwapOption` for
wait-free latest-state, `tokio::sync::broadcast` drop-oldest for events).

| Seam | Where | What it does | Safety |
|---|---|---|---|
| **S1 — Startup gate** | `SoftwareEngine::build()` (cli `run.rs`), **before** `EngineRuntime::new()` | Refuses to **create a new** engine instance when the ladder is at the block-new-instance rung. Mirrors the NDI pre-flight gate. | Pre-startup only — never inside the tick loop. Typed `LicenceError`. A running engine is never affected. |
| **S2 — Config-lock** | The existing control hook at the **frame boundary** (engine `runtime.rs`) | When the ladder says "config-locked", the frame-boundary hook **skips** `apply_layout`/`rebind_cell`/hot-reconfig — the running scene keeps playing. | O(1), allocation-free, wait-free read of one `Arc<AtomicU8>` (the ladder level). No `.await`, no lock. |
| **S3 — Tile watermark** | Post-composite, **before** publish (engine `drive.rs`) | When the ladder says "watermark", blits a corner watermark into the NV12 canvas. Reads ladder status; mutates only the canvas, never source tiles. | Read-only status; NV12-throughout (inv #5); no locks; no blocking; one frame per tick unchanged. |
| **S4 — Heartbeat status surface** | A separate tokio task (off the hot loop) → `LatestState<LicenceStatus>` | Publishes the current entitlement/ladder state wait-free for the control plane to read at `GET /api/v1/account/licence`. | Async I/O off the hot loop; atomic publish; lock-free read; status is read-only external. |
| **S5 — Local metrics buffer** | A separate tokio task → bounded rolling buffer (optionally sqlite) | Samples engine metrics on a cadence into a **consent-independent**, bounded, auto-pruned local store for the support context-pack (§10) — independent of telemetry consent. | Off the hot loop; bounded memory (drop-oldest); independent of the telemetry pipe (§7). |

**The single wait-free hand-off.** The entitlement plane publishes one `LicenceStatus` (level +
reasons + lease arithmetic) via `arc_swap`. The engine derives from it exactly two cheap atomics it
reads on the hot path: `config_locked: AtomicBool` (S2) and `watermark: AtomicBool` (S3). Everything
richer (days remaining, reasons, next-contact-due) is read only by the control plane (S4), off the
loop. This is the whole engine surface — two atomics on the tick path, derived from data.

---

## 6. The enforcement ladder — state machine exposed as data, with the never-off-air proof

### 6.1 The ladder as data

`enforcement` is a field on the licence resource:

```jsonc
"enforcement": {
  "level": "active",            // see the level table below
  "reasons": ["lease-valid"],   // machine-readable reason codes; the UI renders all of them
  "since": "2026-06-10T00:00:00Z",
  "lease": {
    "granted_at": "...", "expires_at": "...",   // LEASE_FULL = 35d from last good heartbeat
    "grace_until": "...",                       // + LEASE_GRACE = 14d
    "hard_at": "...",                           // + to LEASE_HARD = 90d from last contact
    "last_contact_at": "...",
    "next_contact_due": "..."                   // inside ACTIVATION_WINDOW = 31d
  }
}
```

The **level** is computed only in the entitlement plane (off the hot loop), from the lease arithmetic
(§2.2) + fingerprint continuity (§2.3) + claim/transfer state (§12). The engine, the API, the portals,
and the operator all read the **same** `level`. There is no second opinion.

### 6.2 The levels (each names exactly what degrades — and what never does)

| Level | Trigger | Engine start (S1) | Hot-reconfig (S2) | Canvas (S3) | Program output |
|---|---|---|---|---|---|
| `active` | Lease valid (within `LEASE_FULL`) | allowed | allowed | clean | **on air** |
| `warning` | Approaching expiry / inside grace (`LEASE_GRACE`) | allowed | allowed | clean | **on air** |
| `config-locked` | Lease expired past grace, before hard bound | allowed | **denied** | clean | **on air** |
| `watermark` | Further lapse toward `LEASE_HARD` | allowed | denied | **watermark** | **on air** |
| `block-new-instance` | Past `LEASE_HARD` (≈90d no contact) | **denied** | denied | watermark | **on air (running instances keep playing)** |
| `unlicensed-build` | Heartbeat client compiled out (§4) | allowed | allowed | watermark (honest) | **on air** |

The rightmost column is the product promise: **program output is "on air" on every rung, including the
hardest.** `block-new-instance` blocks *starting a new engine* — it does not, and cannot, touch a
running one.

### 6.3 The never-off-air safety analysis (invariant #1 + #10)

This is the load-bearing argument. It must survive adversarial review.

1. **No rung is on the tick path.** S1 runs once at startup; S4/S5 run on separate tasks; only S2/S3
   touch the loop, and both read a **single pre-derived atomic** (no licence logic, no allocation, no
   `.await`, no lock). The tick still does *pace → advance clock → control hook → compose → publish*
   in bounded O(1) work. **Inv #1 holds by construction.**

2. **The hardest rung degrades a convenience, not the output.** `block-new-instance` is enforced at
   `SoftwareEngine::build()` — **before** `EngineRuntime::new()`. A running engine never re-enters
   `build()`; it is therefore untouchable by the hardest rung. Config-lock and watermark are explicit
   *conveniences*: the scene keeps playing, you simply cannot reconfigure it or you see a corner
   stamp. **The output never stalls, never falters, never goes dark on any rung.**

3. **The entitlement plane cannot back-pressure the engine.** It publishes `LicenceStatus` via
   `arc_swap` (wait-free) and the engine *samples* it. The plane never holds a lock the engine holds,
   never sends on a channel the engine drains, and the engine never `.await`s it. A wedged or crashed
   entitlement task leaves the last-published status in place and the engine runs on. **Inv #10 holds
   by construction.**

4. **Fail-safe direction.** If the entitlement plane cannot determine state (crash, missing data,
   clock anomaly), it publishes `warning`, **not** a harder rung — the machine degrades toward
   leniency on internal failure, never toward taking output off air. Hardening only ever happens on
   *positive evidence* of lapse (a computed expired lease), never on absence of the plane.

> **Adversarial check (recorded):** the one way to break inv #1 would be to put lease arithmetic or a
> network call on the hot path. The design forbids both: the loop reads only the two pre-derived
> atomics. Any future change that adds licence logic to the loop is a #1-breaking change requiring a
> design note + chaos test (per CLAUDE.md §2).

---

## 7. Two pipes — mandatory heartbeat vs opt-in telemetry (never co-mingled)

This separation is a **hard requirement** (operator directive, spec). The two pipes share no
transport, no consent toggle, no copy, no API field, and no retention policy.

| | **Licensing heartbeat** | **Product telemetry** |
|---|---|---|
| Purpose | Keep the entitlement lease live | Anonymised product analytics |
| Cadence | **Monthly minimum** (within `ACTIVATION_WINDOW = 31d`) | **Daily** (when enabled) |
| Consent | **Mandatory** for the official build's licence to stay live (it *is* the licensing keep-alive, not analytics) | **Opt-in**, default off; revocable any time |
| Payload | Salted machine-fingerprint digest + lease request + signed assertions (§8) | Aggregated/anonymised usage counters; **never** raw identifiers |
| Transport | The licence-server endpoint (entitlement plane, `heartbeat` feature) | The telemetry endpoint (telemetry plane), distinct host/path |
| API surface | `GET /api/v1/account/licence`, `POST /api/v1/account/licence/heartbeat` | `GET/PUT /api/v1/account/telemetry`, consent under `/api/v1/account/consent` |
| UI surface | The **Licence** screen | The **Data** screen |
| Retention | Heartbeat history bounded, licensing-scoped | Local metrics buffer is **consent-independent** (§7.2) |

### 7.1 Consent model (last-writer-wins)

Consent is a small versioned document: `{ telemetry: bool, updated_at, updated_by }`. Concurrent edits
(portal vs machine UI) resolve **last-writer-wins** by `updated_at` — the simplest correct rule for a
single-value preference where staleness, not merge-loss, is the only risk. The heartbeat consent is
**not** in this document: it is implicit in running the official build, and the UI says so plainly.

### 7.2 Consent-independent metrics retention (S5)

The **local** metrics buffer (S5) is retained **regardless of telemetry consent**, because it serves
the *operator's own* support context-pack (§10), not Aperim analytics. Turning telemetry **off** stops
the *outbound* daily pipe; it does **not** stop the machine keeping a bounded local rolling buffer for
its own diagnostics. The Data screen states this distinction explicitly so "opt out of telemetry" is
never mis-sold as "the machine keeps nothing locally". The buffer is bounded, auto-pruned, and never
leaves the machine without an explicit operator-approved data request (§10).

---

## 8. Data minimisation (salted digests, never raw identifiers)

A hard rule across both pipes and the mesh:

- **Never transmit or store raw** serials, MAC addresses, media URLs, stream contents, file paths
  containing user data, or any direct hardware identifier.
- The machine fingerprint is a **score over salted component digests** (§2.3): each component
  (board/CPU/NIC/disk id) is salted with a per-deployment salt and hashed; only the **digest set** and
  the **score** are ever exposed. Two machines cannot be correlated by digest across deployments
  (different salt), and a digest cannot be reversed to its serial.
- Telemetry counters are **aggregated and anonymised** before they ever enter the daily pipe — no
  per-source, per-URL, or per-stream identifiers; counts and histograms only.
- Mesh digests (§9) are likewise salted; a peer learns *that* a neighbour exists and *its signed
  entitlement summary*, never its raw identity.
- The support context-pack (§10) is built from the local metrics buffer and **redacted**: it carries
  diagnostics, not media and not raw identifiers, and it leaves the machine **only on explicit local
  approval**.

This is both a privacy stance and a correctness property: the tests assert that no raw identifier
appears in any pipe payload, fingerprint digest, mesh announcement, or context-pack.

---

## 9. Mesh — discovery and relay (always-on mDNS, signed digests, end-to-end-signed relay)

Machines on a LAN form a **local mesh** so an **offline** machine (no internet path to the licence
server) can still have its heartbeat **relayed** by an online neighbour. This is greenfield — there is
no mDNS infrastructure in the tree (the managed-devices brief notes the same absence).

### 9.1 Discovery

- **Always-on mDNS announce + browse** (a Conspect service type) over IPv6-first multicast
  (`ff02::fb` link-local mDNS; IPv4 `224.0.0.251` legacy interop), per [ADR-0042](../decisions/ADR-0042.md).
- Each announcement carries a **signed digest summary** (Ed25519): the machine's salted fingerprint
  digest set + a signed entitlement summary (level + lease bounds) — **never** raw identity (§8).
- Discovered peers enter an **untrusted inventory**. Like SAP-discovered sessions
  ([ADR-0041](../decisions/ADR-0041.md)) and managed devices ([managed-devices](managed-devices.md)),
  a peer is **never** auto-trusted — the relationship is **confirm-adopt** by the operator.

### 9.2 Relay (end-to-end signed)

- An online machine can **relay** an offline neighbour's heartbeat request to the licence server and
  carry the signed response back. The relay payload is **end-to-end signed** by the originating
  machine and the licence server: the relaying machine is a **dumb carrier** that cannot read, forge,
  or alter the assertion (it lacks the keys). Relay integrity does not depend on trusting the relayer.
- Relay is **best-effort and bounded**: a relayer enqueues at most a bounded set of neighbour requests
  (drop-oldest), never blocks on a neighbour, and never lets mesh traffic touch the engine
  (invariant #10 — the mesh plane is a control-plane actor over watch/broadcast channels, the proven
  isolation shape).
- Relay is **opt-in per machine** and disclosed in the Mesh screen (a machine can be a willing relayer
  or decline). A relayed machine still authenticates end-to-end; the relayer earns no entitlement from
  carrying traffic.

> **Operator-confirm (§14):** the **licence-server wire protocol** (request/response framing, the
> server's Ed25519 key, key-pinning + rotation policy) is **not** specified in the in-conversation
> spec. This brief proposes Ed25519-signed, end-to-end-authenticated assertions with a **pinned server
> key + a documented rotation path** (overlapping key validity, signed key-rollover assertion), and
> flags the exact protocol + key-management policy for operator confirmation.

---

## 10. Support / ticketing — context-pack, data-request local approval, append-only audit

The support plane turns "my machine is misbehaving" into an auditable, privacy-respecting exchange.

- **Tickets** are control-plane resources (`/api/v1/account/support/tickets`): create, list, read,
  comment. Each carries a status and an append-only comment thread.
- **Context-pack:** on operator request, the machine assembles a **redacted** diagnostics bundle from
  the local metrics buffer (S5) + the licence/enforcement state + config-as-code (secrets as
  references, never values — [ADR-M006](../decisions/ADR-M006.md)). It contains diagnostics, **never**
  media, raw identifiers, or secrets (§8). It is attached to a ticket only on explicit operator action.
- **Data-request local approval:** if support (or the licence server) requests additional data, the
  request appears as a **pending action** the operator must **approve locally** before anything leaves
  the machine. No data egress without local approval — full stop. The pending-action strip (§11/web)
  surfaces these.
- **Append-only audit store:** every account-side action (claim, transfer, lease grant, enforcement
  level change, consent change, context-pack export, data-request approval/denial) is written to an
  **append-only** audit log (`/api/v1/account/audit`), sqlite-backed where the `sqlite` feature is on
  (ADR-I003 pattern), in-memory otherwise. Append-only = no update/delete route; entries are immutable
  and timestamped. This is the operator's evidence trail and a support precondition.

---

## 11. Endpoint → subsystem map (§11 — the ~30 routes)

All routes are under `/api/v1`, follow the established `multiview-control` contract (RFC 9457 problem
docs, `ETag`/`If-Match` on mutable resources, `202 + operation_id` for long-running ops,
`Idempotency-Key` on claim/transfer, RBAC `Principal`), and are added to the OpenAPI 3.1 doc
(utoipa). They are **all account-side** and live in new `routes/account/*.rs` + `routes/mesh.rs`
modules so they do not churn the existing route files (§13 coordination).

| # | Method · Path | Subsystem / crate | Notes |
|---|---|---|---|
| 1 | `GET /api/v1/account` | control ← licence | Account profile + tier (opaque, rendered) |
| 2 | `PUT /api/v1/account` | control ← licence | Edit mutable profile fields (`If-Match`) |
| 3 | `GET /api/v1/account/licence` | control ← licence (S4) | The licence resource incl. `enforcement` (§6) |
| 4 | `POST /api/v1/account/licence/heartbeat` | licence (`heartbeat`) | Force a heartbeat now → `202 + operation_id` |
| 5 | `GET /api/v1/account/licence/history` | control ← licence | Bounded heartbeat/lease history (licensing-scoped) |
| 6 | `POST /api/v1/account/claim` | licence | Begin claim with a 6-char code (`CLAIM_CODE_TTL`); `Idempotency-Key` |
| 7 | `POST /api/v1/account/claim/confirm` | licence | Confirm/accept a pending claim |
| 8 | `POST /api/v1/account/claim/reject` | licence | Reject; opens `CLAIM_REJECT_WINDOW` (24h) |
| 9 | `GET /api/v1/account/claim` | licence | Current claim state (pending/none/expired) |
| 10 | `POST /api/v1/account/transfer` | licence | Begin a device transfer (`TRANSFER_WINDOW` 72h); `Idempotency-Key` |
| 11 | `POST /api/v1/account/transfer/confirm` | licence | Confirm transfer |
| 12 | `POST /api/v1/account/transfer/cancel` | licence | Cancel a pending transfer |
| 13 | `GET /api/v1/account/transfer` | licence | Transfer state |
| 14 | `GET /api/v1/account/fingerprint` | licence | Salted digest set + score (§2.3, §8) — never raw |
| 15 | `GET /api/v1/account/enforcement` | control ← licence | Just the ladder state (level + reasons), for the chrome banner |
| 16 | `GET /api/v1/account/telemetry` | telemetry | Telemetry pipe state (opt-in, daily) |
| 17 | `PUT /api/v1/account/telemetry` | telemetry | Enable/disable the outbound daily pipe |
| 18 | `GET /api/v1/account/consent` | telemetry | Consent document (`{telemetry, updated_at, updated_by}`) |
| 19 | `PUT /api/v1/account/consent` | telemetry | Set consent (LWW, §7.1); `If-Match` |
| 20 | `GET /api/v1/account/metrics/local` | telemetry (S5) | Consent-independent local buffer (`?window=7d`) |
| 21 | `GET /api/v1/mesh/peers` | mesh | Untrusted discovered-peer inventory |
| 22 | `POST /api/v1/mesh/peers/{id}/adopt` | mesh | Confirm-adopt a peer (Class-1) |
| 23 | `DELETE /api/v1/mesh/peers/{id}` | mesh | Forget an adopted peer |
| 24 | `GET /api/v1/mesh/relay` | mesh | Relay settings + state (am-I-a-relayer, queue depth) |
| 25 | `PUT /api/v1/mesh/relay` | mesh | Opt in/out of relaying neighbours |
| 26 | `GET /api/v1/account/support/tickets` | control (support) | List tickets |
| 27 | `POST /api/v1/account/support/tickets` | control (support) | Create a ticket |
| 28 | `GET /api/v1/account/support/tickets/{id}` | control (support) | Read a ticket + thread |
| 29 | `POST /api/v1/account/support/tickets/{id}/context-pack` | control (support) | Build + attach the redacted context-pack (§10) → `202` |
| 30 | `GET /api/v1/account/support/data-requests` | control (support) | Pending data requests awaiting local approval |
| 31 | `POST /api/v1/account/support/data-requests/{id}/approve` | control (support) | Approve egress locally (§10) |
| 32 | `POST /api/v1/account/support/data-requests/{id}/deny` | control (support) | Deny egress |
| 33 | `GET /api/v1/account/audit` | control (audit) | Append-only account audit log (§10) |
| 34 | `POST /api/v1/system/actions/{action}` | control ← engine seam | System actions (restart-control, re-claim, refresh-lease) → `202` |

> "~30 routes" in the spec; the table lands at 34 to make claim/transfer/mesh/support each
> complete CRUD-or-command sets. The implementation may fold a few read pairs, but **claim,
> transfer, consent, mesh-adopt, data-request-approval, and audit must each exist as named routes** —
> they are the auditable account actions.

---

## 12. The claim / transfer / lease state machines (data, not control flow)

All three are pure state machines in `multiview-licence`, exposed as resource state and tested with
`proptest-state-machine`. Summarised here; the ADR pins the transitions.

- **Claim:** `Unclaimed → CodeIssued(ttl=15m) → {Confirmed → Claimed | Rejected(window=24h) | Expired}`.
  A `Rejected` machine cannot re-issue a code for 24h. `Claimed` carries the owner + the initial lease.
- **Transfer:** `Claimed → TransferPending(window=72h) → {Confirmed → Claimed(new owner) | Cancelled
  → Claimed(old owner) | Expired → Claimed(old owner)}`. The fingerprint score (§2.3) gates whether a
  transfer is "same machine, new owner" vs "new machine".
- **Lease:** `last_good_heartbeat → Active(35d) → Warning(grace 14d) → ConfigLocked → Watermark →
  Blocked(90d from last contact)`. Any successful heartbeat (direct or **relayed**, §9) resets to
  `Active`. The lease state **is** the enforcement ladder input (§6).

The state machines run only in the entitlement plane (off the hot loop). The engine reads only the two
derived atomics (§5). Fingerprint drift, clock skew, and relay are all modelled as inputs to these
machines, never as engine concerns.

---

## 13. Crate decomposition (greenfield first → collision-free)

The deliberate sequencing: **build the greenfield leaf crates first.** They depend only on
`multiview-core` (+ `events`), collide with nothing the parallel WebUI-owner session touches, and can
be authored, tested, and merged independently. The shared surfaces (`multiview-control` routes/OpenAPI,
`web/`) come **after** and are **sequenced/coordinated** (§15).

### 13.1 New crate: `multiview-licence` (greenfield, leaf)

- Lib `multiview_licence`. Depends on `multiview-core`, `multiview-events`, `thiserror`, `tracing`,
  `ed25519-dalek` (signing — `MIT OR Apache-2.0`, deny-clean), `chrono` (lease arithmetic, exact —
  no float). **No** `multiview-ffmpeg`, **no** GPU, **no** engine dependency (it is fed *by* the cli).
- Owns: the entitlement/lease/claim/transfer state machines (§12), the enforcement-ladder-as-data
  model (§6), fingerprint scoring (§2.3), the `LicenceStatus` published type (S4), and the
  **heartbeat client** behind a `heartbeat` Cargo feature (off in the default/CI build, on in shipped
  presets — §4). The state model is **always** compiled; only the network client is feature-gated.
- Default build is a pure shell (no network) so `cargo check --workspace` stays green and deny-clean.

### 13.2 New crate: `multiview-mesh` (greenfield, leaf)

- Lib `multiview_mesh`. Depends on `multiview-core`, `multiview-licence` (for the signed digest/summary
  types it announces), `multiview-events`, `tokio` (net, behind a `mdns` feature), `ed25519-dalek`,
  `thiserror`, `tracing`. mDNS announce/browse + the relay carrier.
- Default build is a pure shell; the `mdns` feature pulls the socket + a maintained mDNS library
  (operator-confirm the exact dependency — §14; candidates are `mDNS-sd`-class crates, deny-checked).
  The relay logic and signed-payload handling are pure-Rust and always compiled/tested.

### 13.3 Extend: `multiview-telemetry`

- Add the **heartbeat status surface** consumer (renders S4 for `/readyz`-adjacent reporting), the
  **two-pipe consent** model + the outbound daily telemetry pipe (opt-in), and the
  **consent-independent local metrics retention** buffer (S5). Keep the existing tracing/Prometheus
  surface untouched; the new pipes are additive and off-by-default for outbound.

### 13.4 Extend: `multiview-control`

- The ~34 account routes (§11) in **new** `routes/account/*.rs` + `routes/mesh.rs` modules; the
  OpenAPI additions; the support/ticketing + context-pack + data-request + append-only audit stores
  (in-memory default, sqlite under the existing `sqlite` feature, ADR-I003 pattern). `AppState` grows
  account stores; the existing routes are untouched.

### 13.5 Extend: `web/`

- 7 screens + global chrome (§web below). Generated API client regeneration is the **single
  coordination point** with the WebUI-owner session (§15).

### 13.6 Extend: `multiview-cli`

- Wires the planes: builds `multiview-licence` (+ heartbeat under feature), spawns the licence
  heartbeat task (S4), the local-metrics task (S5), the mesh announce/relay task, and threads the two
  derived atomics into `SoftwareEngine::build()` (S1) and `EngineRuntime` (S2/S3). This is where the
  greenfield crates meet the engine seams — additive, behind the account feature aggregation.

### 13.7 Web screens + chrome (the 7 + global chrome)

| Route | Screen | Consumes |
|---|---|---|
| `/welcome` | First-run claim/onboarding | claim routes (§11 6–9), `CLAIM_CODE_TTL` |
| `/settings/account` | Profile + tier | `GET/PUT /api/v1/account` |
| `/licence` | Lease state, heartbeat, history, transfer | licence + transfer routes; renders `enforcement` |
| `/data` | Telemetry consent + local metrics (the **two pipes**, distinct copy §7) | telemetry/consent/metrics routes |
| `/mesh` | Discovered peers, adopt, relay opt-in | mesh routes |
| `/system/actions` | Restart-control, re-claim, refresh-lease | `POST /api/v1/system/actions/{action}` |
| `/help` (account-side) | Onboarding/concept docs — **nested under the existing DocsLayout** to avoid the `/help` collision the SPA scout flagged | in-app docs registry |

Global chrome (in `AppLayout`, mobile-aware):
- **Header chip:** account/tier + a click-through to `/licence`; renders `enforcement.level` colour
  (no-colour-alone — pair with text/icon, ADR-W011).
- **Ladder banner:** a full-width callout (HealthBanner-style) above the existing health banner when
  `enforcement.level ≥ warning`, naming the reason + the remediation (e.g. "lease expires in N days —
  heartbeat now").
- **Pending-action strip:** surfaces data-request approvals (§10) and other operator-required actions.
- **Config-lock interceptor:** when `enforcement.level = config-locked`, resource forms (Sources,
  Outputs, Layouts, …) render read-only with an explanatory banner — the API also returns the lock,
  so the UI is a courtesy, not the gate.
- **Tile watermark** is an **engine** concern (S3), **not** a web concern — the SPA only *explains* it.

---

## 14. Operator decisions needed + companion docs missing (explicit)

### 14.1 Companion documents — EXTERNAL, not in repo (record)

- **Licensing Architecture specification** — external; governs tier/commercial semantics.
- **Pricing & Hardware Platform Strategy v4** — external; governs tier→price→hardware/partner mapping.
- **Portal HTML prototypes** — external; consume the §2 constants (must stay byte-exact).

### 14.2 Where the spec is silent — proposed approach + operator-confirm flag

| # | Topic | Proposed approach (this brief) | Needs operator confirmation because… |
|---|---|---|---|
| O1 | **Licence-server wire protocol** | Ed25519-signed, end-to-end-authenticated request/response over the entitlement endpoint; relay is a dumb signed carrier (§9.2) | The framing + endpoint + auth handshake are server-side policy not in the spec |
| O2 | **Ed25519 key-pinning + rotation** | Pin the server's public key in the binary; rotate via overlapping validity + a signed key-rollover assertion | Key custody/rotation is a security-policy decision with deployment impact |
| O3 | **Claim-code glyph alphabet** | 6-char, ambiguity-free; copy the exact alphabet verbatim from the portal source | Portal and machine must generate identically — the exact glyph set must match |
| O4 | **Partner / reseller model** | Treat partner attribution as an opaque field on the licence resource; no machine logic | Pricing & Platform Strategy v4 (external) owns the partner model |
| O5 | **mDNS dependency choice** | A maintained, deny-clean mDNS-sd-class crate behind the `mdns` feature | Adds a dependency to the supply-chain closure (`cargo deny`) — needs sign-off |
| O6 | **Salt provenance** | Per-deployment salt provisioned at claim time; never shipped in the binary | Where the salt comes from (server-issued vs locally-generated) affects digest correlation guarantees |
| O7 | **Tier → capability mapping** | Render `tier` opaquely; do **not** gate features by tier in v1 | Whether tier ever gates *features* (vs only commercial terms) is a product decision |

None of these blocks building the greenfield crates: they are leaf-pure and testable with a fake
licence-server/peer in-process. They block only the **live wire** integration, which is sequenced last.

---

## 15. Coordination with the parallel WebUI-owner session

The control-plane (`multiview-control`) and `web/` surfaces are shared with an active WebUI-owner
session (the SPA scout flagged the collision points). The plan:

1. **Greenfield first (no collision).** `multiview-licence` and `multiview-mesh` are brand-new leaf
   crates — they touch **no** file the WebUI session edits. Build, test, and merge them first. The
   `multiview-telemetry` extension is additive (new modules), low-collision.
2. **Control routes in new modules.** All account routes live in **new** `routes/account/*.rs` +
   `routes/mesh.rs` files (not the existing `routes/mod.rs` resource files). The **one** shared edit
   is route registration + OpenAPI in `openapi.rs`/`routes/mod.rs`; **one owner registers all
   account routes + schemas in a single coordinated change** (the LANE-API pattern from the existing
   work-schedule). The WebUI session is told the exact route/schema names up front so it can code
   against them.
3. **OpenAPI/schema regeneration is single-owner.** `docs/api/openapi.json` → `web/src/api/schema.ts`
   is regenerated **once per merged backend batch**, by one owner, never hand-edited — the schema
   sync the SPA scout called the critical path. Account-side screens consume the generated client.
4. **Chrome ownership.** `AppLayout.tsx`, `index.css` (`:root` tokens), `router.tsx`,
   `navigation.tsx` are co-review files. Account-side reuses the existing token palette (no new
   `:root` tokens), nests `/help` under the existing DocsLayout, and extends `NAV_ITEMS` with a
   grouped "Account" section rather than 7 flat items.
5. **i18n is collision-free** (Lingui content-hash keys); extract→compile in sequence after merge.

Net: the **backend greenfield crates carry no coordination cost**; the **control routes + screens are
sequenced behind a single route/OpenAPI registration owner**, exactly as the existing fanout plan
handles `openapi.rs`/`routes/mod.rs`.

---

## 16. Efficiency budget (standing review)

- **Hot path:** two atomic reads per tick (S2 config-lock, S3 watermark flag) + an occasional NV12
  corner blit when watermarking (bounded, region-limited, NV12-throughout — no full-canvas colour
  round-trip, reuses the ADR-0023 dirty-rect bake). **Zero** allocation, **zero** network, **zero**
  lock on the loop.
- **Off-hot-loop tasks:** heartbeat (monthly contact, ~1 request/30d), local-metrics sampler
  (cadence, bounded ring ≤ a few MB, auto-pruned), mesh announce/browse (link-local multicast,
  low rate), relay carrier (bounded drop-oldest queue). All are control-plane actors over
  watch/broadcast channels.
- **Memory:** bounded everywhere — heartbeat history capped, metrics buffer capped + pruned, mesh
  peer inventory capped, relay queue bounded drop-oldest, audit store append-only but rotated by
  size/age like the existing audit log.
- **No data-plane invariant moves.** Inv #1, #5, #10 are preserved by construction (§6.3).

---

## 17. Testing posture

- **State machines** (claim/transfer/lease/fingerprint) — `proptest-state-machine`, regressions
  committed; assert the §2 constants exactly (a property that 15m≠14m, 35d lease, 70 threshold).
- **Never-off-air** — a chaos/soak test drives every ladder rung (incl. `block-new-instance`) against
  a running engine and asserts the output clock still emits one valid frame per tick on every rung,
  and that a wedged/crashed entitlement task never stalls the loop (inv #1/#10 — the §6.3 proof made
  executable).
- **Two-pipe separation** — assert no telemetry field appears in a heartbeat payload and vice-versa;
  assert turning telemetry off does not stop the local buffer (§7.2).
- **Data minimisation** — assert no raw serial/MAC/URL/media appears in any pipe payload, fingerprint
  digest, mesh announcement, or context-pack (§8).
- **Relay end-to-end signing** — assert a relayer cannot forge/alter an assertion (it lacks the keys),
  and a tampered relay payload is rejected.
- **Held-out acceptance** suite (author never sees) per the guardrails.

---

## 18. Dependency-ordered backlog (CONSPECT-*)

The full backlog with effort + deps lives in
[`../development/work-schedule.md`](../development/work-schedule.md) (the **CONSPECT** epic). Summary
of the dependency order (greenfield → shared, never-off-air gated first):

1. **CONSPECT-0** (M) — constants + types crate scaffold (`multiview-licence` shell, §2 constants,
   `LicenceStatus`). *deps: —*
2. **CONSPECT-1** (L) — entitlement/lease/claim/transfer/fingerprint state machines as data (§6/§12).
   *deps: CONSPECT-0*
3. **CONSPECT-2** (M) — engine seams S1/S2/S3 + the never-off-air chaos test (§5/§6.3). *deps: CONSPECT-1*
4. **CONSPECT-3** (L) — heartbeat client (feature-gated) + S4 status surface + lease renew (§4/§7).
   *deps: CONSPECT-1*
5. **CONSPECT-4** (M) — `multiview-mesh` discovery (always-on mDNS, signed digests, untrusted
   confirm-adopt) (§9.1). *deps: CONSPECT-1*
6. **CONSPECT-5** (L) — mesh relay (end-to-end signed, bounded) (§9.2). *deps: CONSPECT-3, CONSPECT-4*
7. **CONSPECT-6** (M) — telemetry two-pipe + consent (LWW) + S5 consent-independent retention (§7).
   *deps: CONSPECT-0*
8. **CONSPECT-7** (L) — control routes batch A: licence/claim/transfer/fingerprint/enforcement +
   OpenAPI (§11 1–15). *deps: CONSPECT-1, CONSPECT-3 (single route/OpenAPI owner, §15)*
9. **CONSPECT-8** (M) — control routes batch B: telemetry/consent/metrics/mesh (§11 16–25).
   *deps: CONSPECT-6, CONSPECT-4*
10. **CONSPECT-9** (L) — support/ticketing + context-pack + data-request approval + append-only audit
    (§10, §11 26–34). *deps: CONSPECT-7, CONSPECT-6*
11. **CONSPECT-10** (M) — cli wiring: spawn the plane tasks + thread the two atomics (§13.6).
    *deps: CONSPECT-2, CONSPECT-3, CONSPECT-4, CONSPECT-6*
12. **CONSPECT-11** (XL) — web/: 7 screens + chrome + generated-client regen (§13.7), sequenced behind
    the WebUI-owner coordination (§15). *deps: CONSPECT-7, CONSPECT-8, CONSPECT-9*
13. **CONSPECT-12** (M) — live-wire integration + hardware validation: real heartbeat against the
    licence server, real mesh relay between two boxes (gated on the §14 operator confirmations).
    *deps: CONSPECT-10, CONSPECT-11*

**Critical path:** CONSPECT-0 → 1 → {2, 3} → 7 → 9 → 11 → 12. The greenfield crates (0–6) fan out;
the shared control/web work (7–11) sequences behind the single route/OpenAPI registration owner; the
live wire (12) waits on operator confirmations (§14) and hardware.

---

## 19. Invariant re-assertion

- **#1 (output clock):** preserved on every enforcement rung (§6.3); no licence logic on the tick path.
- **#5 (NV12-throughout):** the watermark blit is NV12, region-limited (§5/§16).
- **#10 (isolation):** every plane is an off-hot-loop control actor over wait-free / drop-oldest
  channels; the engine never `.await`s a plane and reads only two pre-derived atomics (§5, §6.3).
- **#11 (live-apply classification):** account actions are classified (claim/adopt = Class-1;
  config-lock is a *deny*, not a reset) and surfaced before apply.
