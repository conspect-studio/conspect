# ADR-MV006: Internally-derived tally from switcher bus state — per-tick composition-graph derivation, one arbiter for internal + external facts, and TSL codec spec-reconciliation

- **Status:** Proposed
- **Area:** Broadcast Multiviewer
- **Date:** 2026-06-11
- **Source briefs:** [production-switcher.md](../research/production-switcher.md) (tally section), [broadcast-multiviewer-features.md](../research/broadcast-multiviewer-features.md) §2
- **Extends:** [ADR-MV002](ADR-MV002.md) (TSL UMD ingest/egress + external-bus arbitration;
  this ADR supersedes its ports-default wording, §4). Relates: [ADR-MV005](ADR-MV005.md) (IS-07/router bridges), [ADR-0037](ADR-0037.md) (ISO recording → amber), [ADR-0054](ADR-0054.md) (switcher state machine + render-plan resolver), [ADR-0055](ADR-0055.md)/[ADR-0056](ADR-0056.md) (transitions/keyers this derives over), [ADR-RT008](ADR-RT008.md) (realtime tally lane).

## Context

[ADR-MV002](ADR-MV002.md) pinned **external** tally: TSL/IS-07/router facts arriving from a
facility's own switching infrastructure. Its Alternatives section rejected *internal-only* tally —
a multiviewer must reflect the external bus — but left **internally-derived** tally unspecified,
because Multiview had no internal bus state to derive from. The production-switcher layer
([ADR-0054](ADR-0054.md)) changes that: Multiview now *is* a switcher, and its own PGM/PVW/keyer/
transition state is an authoritative tally source that must merge with the external ones.

As-built inventory (paths verified in-tree 2026-06-11):

- **BUILT, unwired** — the pure tally arbiter: `multiview-engine/src/tally/{arbiter,profile,gpio}.rs`.
  `TallyArbiter::resolve` consumes per-tick `TallyFact { tile: u32, state, priority }` lists and
  **returns** resolved `TallyState`s under a `ConflictPolicy` (`Priority` default with
  red > amber > green > off urgency tie-break; `ColorUrgency`) plus an anti-glitch `LatchPolicy`.
  It never sends, never blocks — invariant #10 by construction.
- **BUILT, unwired** — TSL UMD codecs: decoders `multiview-input/src/tsl/{v31,v40,v50}.rs`,
  encoders `multiview-output/src/tsl/{v31,v40,v50}.rs`. Pure byte-in/value-out codecs; no
  UDP/TCP/serial socket exists anywhere, and `multiview-config/src/tally.rs` carries no
  listen/port config.
- **BUILT echo** — `Command::SetTallyOverride` in `multiview-cli/src/control.rs` (arm at
  `control.rs:1025`) publishes a `TallyState` **echo** with the inline comment "No tally arbiter
  is wired into the software engine yet".
- **BUILT, unspawned** — `run_tally_ingest` in `multiview-control/src/tally_ingest.rs`
  (exported at `multiview-control/src/lib.rs:146`): the lossy lagged-skip engine→control mirror
  loop. It has **zero spawn sites** in the control plane or the binary.
- **BUILT** — the publication path: `EnginePublisher` drop-oldest broadcast
  (`multiview-engine/src/isolation.rs`), `Event::TallyState`/`TallyTarget{Tile,Element}`
  (`multiview-events/src/event.rs:460`), control-side `TallyMirror` + REST
  (`multiview-control/src/routes/tally.rs`), and the overlay render models
  (`multiview-overlay/src/tally.rs` LH/Text/RH border regions, `umd.rs` labels).
- **DOC-ONLY** — the switcher state machine, render-plan resolver, and PVW bus this ADR derives
  over ([ADR-0054](ADR-0054.md)/[ADR-0055](ADR-0055.md)/[ADR-P007](ADR-P007.md), this design pass).
- **Doc drift** — all four files in `multiview-engine/src/tally/` (`mod.rs:3`, `arbiter.rs:3`,
  `profile.rs:3`, `gpio.rs:3` — verified by grep) cite **ADR-MV001** for
  the tally surface; ADR-MV001 is the monitoring/alarm ADR. The correct citations are
  [ADR-MV002](ADR-MV002.md) and this ADR.
- **Spec verification finding** — the TSL codecs were re-verified against the official TSL UMD
  protocol specification (TSL Products' published spec PDF) and deviate from it in wire-visible
  ways (§4 below). ADR-MV002's *prose* matches the spec; the drift is implementation-vs-spec, and
  the existing round-trip property tests are structurally blind to it because encoder and decoder
  share one (wrong) value model.

## Decision

### 1. Internally-derived tally = a per-tick pure derivation over the composition graph

A source is **PROGRAM-tallied** iff it is *reachable from any on-air output through the live
composition* at this tick — the recursive **contributes-to-program** rule:

- **both** transition sources while a transition is in flight (mix/dip/wipe/DVE: the outgoing
  scene's sources stay red until the completion tick; flip-flop swaps state, never lamps,
  [ADR-0055](ADR-0055.md));
- the **FILL and KEY sources** of every on-air keyer — upstream keyers in an on-air scene and
  on-air downstream keyers alike ([ADR-0056](ADR-0056.md)). De-facto industry practice: a source
  feeding an on-air key, fill *or* key plane, is on-air;
- **stinger/media-transition media while it covers** the cut (from first visible frame to last,
  in integer frame bounds derived from the trigger metadata, [ADR-0055](ADR-0055.md)/[ADR-0058](ADR-0058.md));
- every source inside an **on-air multi-box composition** and inside a **nested M/E** that is
  itself selected into the on-air path — the recursion follows composition re-entry
  ([ADR-0054](ADR-0054.md)); a source on a nested M/E tallies only while that M/E contributes to
  program (de-facto industry practice for nested-bus tally);
- sources behind an engaged **FTB stay PROGRAM-tallied**: the composition is live behind black
  and is exposed instantly on release — dropping red during FTB invites a talent/camera move at
  exactly the wrong moment. (Safe-default policy decision; the derivation input already carries
  the FTB state, so a future config knob is additive.)

A source is **PREVIEW-tallied** iff it is reachable, under the same recursion, from the
**top-level M/E's PVW bus** ([ADR-P007](ADR-P007.md)). **Amber = ISO/record**: a source being
ISO-recorded ([ADR-0037](ADR-0037.md)) asserts `BusSource::Iso` (the third state TSL already
carries, `multiview-core/src/tally.rs`).

**The untallied-overlay anti-pattern (de-facto pitfall, named and closed).** Deployed software
switchers are documented by their own operators to omit sources that contribute through
overlay/downstream-key/composition layers from derived tally — a camera feeding an on-air box or
key shows no red lamp. Our derivation closes this *structurally*: it walks the **resolved render
plan** — the same placement list the compositor actually consumes this tick
([ADR-0054](ADR-0054.md) resolver; today's per-tick placement derivation lives in
`multiview-engine/src/drive.rs`) — not a parallel mental model. Anything composited is tallied;
nothing can be on-air and untallied, because the tally input *is* the composite input.

**Mechanics.** The derivation is a pure, synchronous function
`fn(resolved_render_plan, switcher_state, iso_state, tick) -> Vec<TallyFact>` evaluated at the
frame-boundary control seam (the per-tick hook at `multiview-engine/src/runtime.rs:432-439`,
Tick-widened per [ADR-0054](ADR-0054.md)) — allocation-pooled like the compose scratch, O(plan)
per tick, zero steady-state allocation. Source-level facts are expanded to tile-keyed facts via
the existing cell→source bindings (`CompositorDrive::effective_cell_source`,
`multiview-engine/src/drive.rs:276`) for the multiview lamp render, and kept source/element-keyed
for TSL/IS-07 egress. The one contract extension this requires: `TallyFact`'s key today is
`tile: u32`; it is generalised to the (already-shipped) `TallyTarget{Tile,Element}` shape — a
pure, additive change to a pure machine.

### 2. One arbiter, one ConflictPolicy — internal and external facts merge

Internal facts feed the **existing** `TallyArbiter` together with the external facts ADR-MV002
defined (TSL ingest, NMOS IS-07, router crosspoints per [ADR-MV005](ADR-MV005.md), GPI). There is
exactly one resolution point and one `ConflictPolicy`. Default fact priorities are pinned and
documented: **operator override = 255** (`SetTallyOverride` becomes a max-priority fact, see §3);
**internal derivation and external buses share one default priority**, so under the default
`Priority` policy ties resolve by colour urgency — *red from any bus wins*, deterministically —
which is the safe behaviour both when Multiview is the switcher and when it sits downstream of a
facility bus. Per-bus priorities are configurable in the `TallyProfile` for facilities that want
external (or internal) truth to dominate. The `LatchPolicy` anti-glitch latch applies to external
facts (jittery feeds); internal facts are exact by construction — a transition's both-red window
is frames `[arm_tick, completion_tick]`, never a debounced approximation.

### 3. Wiring (replacing the verified echo; spawning the verified loop)

- The arbiter is owned by the switcher control surface in `multiview-engine` and **sampled at the
  frame-boundary seam**: derive facts → `resolve()` → diff against the previous resolution →
  publish only changes as `Event::TallyState` on the existing **drop-oldest** `EnginePublisher`
  broadcast. The engine never awaits a tally consumer (invariant #10). Tally is edge-triggered
  and rides [ADR-RT008](ADR-RT008.md)'s existing **lossless** `tally.state` lane
  (publish-on-change, replay-ring — no conflation is needed because internally-derived tally
  changes only at switcher lifecycle edges, never per tick); a wedged consumer is bounded by the
  drop-oldest broadcast itself.
- `Command::SetTallyOverride` (`multiview-cli/src/control.rs:1025`) stops echoing and instead
  asserts/clears a priority-255 fact in the arbiter; the resolved (not echoed) state is what
  publishes. This removes the only place a lamp could disagree with the arbiter.
- `run_tally_ingest` (`multiview-control/src/tally_ingest.rs`) is **spawned at control-plane
  startup** alongside the other ingest loops, feeding the `TallyMirror` behind
  `/api/v1/tally` and the realtime stream. It reads the drop-oldest broadcast and lagged-skips;
  it cannot back-pressure the engine.
- Consumers of resolved state: the on-clock tally-border/UMD render models
  (`multiview-overlay/src/tally.rs`, `umd.rs`), the control mirror/REST/WS, TSL egress encoders
  (post-MVP socket wiring), and control-surface state-feedback queries ([ADR-W021](ADR-W021.md)).
- Sockets are IPv6-first per conventions §10: the TSL listener binds dual-stack `[::]`
  (`IPV6_V6ONLY=false`); egress examples lead IPv6 (`udp://[2001:db8::10]:4003`).

**Invariant posture.** Derivation + arbitration are pure and synchronous at the frame boundary —
they can no more stall a tick than the existing command drain (inv #1). Publication is the
existing drop-oldest broadcast; ingest is read-only lagged-skip (inv #10). No new engine-inward
channel is created.

### 4. TSL spec reconciliation — wire-visible fixes before any socket ships

Verified against the official TSL UMD protocol specification (TSL Products' published spec PDF,
<https://tslproducts.com/wp-content/uploads/Manuals/Control/tsl-umd-protocol.pdf>);
each deviation below is wire-visible and was confirmed by reading the in-tree codecs:

1. **v5.0 DLE is `0xFE`, not `0x10`.** The codecs define `DLE: u8 = 0x10`
   (`multiview-input/src/tsl/v50.rs:54`, `multiview-output/src/tsl/v50.rs:28`); the spec's
   byte-stream framing is `DLE = 0xFE`, `STX = 0x02`. Every framed v5.0 exchange with a
   spec-correct peer fails today.
2. **v5.0 CONTROL bit order is RH(0–1) / Text(2–3) / LH(4–5) / brightness(6–7).** The codecs map
   bits 0–1→**left**, 2–3→**right**, 4–5→**text** (`multiview-input/src/tsl/v50.rs:131-133`) —
   against a spec-correct peer all three lamp fields land on the wrong lamps (a three-way field
   rotation: our left shows the peer's RH, our right the peer's text tally, our text the
   peer's LH).
3. **Remove the invented DLE/ETX terminator.** The encoder appends `DLE ETX` after the stuffed
   payload (`multiview-output/src/tsl/v50.rs:103-121`) and the decoder treats it as a frame
   terminator (`multiview-input/src/tsl/v50.rs:182`); the spec defines DLE/STX opening +
   byte-stuffing only — no close sequence.
4. **Skip DMSGs whose CONTROL bit 15 is set.** Bit 15 flags control data ("not defined in this
   version" of the spec) — such DMSGs must be skipped, not text-parsed; the decoder's DMSG loop
   has no bit-15 check.
5. **v4.0 is rebuilt as v3.1 + CHKSUM(mod-128) + VBC + XDATA.** The repo implements a 19-byte
   message (address + one packed colour/control byte + 16 ASCII + full-8-bit-sum checksum,
   `multiview-input/src/tsl/v40.rs`); the spec's v4.0 is the v3.1 18-byte message followed by a
   mod-128 checksum, a VBC (variable byte count), and 2 XDATA bytes carrying LH(5:4)/Text(3:2)/
   RH(1:0) colour tally per lamp. The two layouts are mutually undecodable.
6. **Drop the invented v4.0/v3.1 byte-stream wrapper.** The v4.0 codec carries a DLE/STX
   (`0x10 0x02`) stream framing the spec does not define for v3.1/v4.0 at all
   (`multiview-input/src/tsl/v40.rs:40/:43` define `DLE = 0x10`/`STX = 0x02`, stripped by
   `decode_framed` at `:149`; `multiview-output/src/tsl/v40.rs:19/:22`, emitted by
   `encode_display_framed` at `:86-90`). Per the spec, serial delimiting for v3.1/v4.0 is the
   `0x80` address sync bit and UDP carries one message per datagram; the optional stream wrapper
   exists only in v5.0 (with DLE `0xFE`). Remove the v4.0 `decode_framed`/`encode_display_framed`
   path — or, if a stream wrapper is deliberately retained for byte-stream deployments, label it
   explicitly extra-spec and config-gated, never the default.

**Test posture (the load-bearing lesson):** the existing encode∘decode round-trip property tests
pass *today* against the wrong wire format — encoder and decoder share one value model, so
round-tripping **structurally cannot catch** a shared-model error. The fix ships with **golden
on-wire byte vectors hand-transcribed from the spec PDF** (decode golden→expected values; encode
values→golden bytes) as the floor, round-trip property tests retained on top, and the codecs
mutation-tested (`cargo mutants --in-diff` per the guardrails). These fixes land **before** any
socket wiring: today the codecs have no I/O, so the wire change costs nothing; after egress ships
it would be a breaking on-air change.

**Ports:** always configurable, with **no normative default** — the spec defines none, and
observed field conventions genuinely vary (40001 for v3.1/v4.0-over-UDP; both 4003 and 40003
seen for v5.0; byte-stream deployments use yet other ports). **This ADR supersedes
ADR-MV002's "default v5.0=40003" wording** (ADR-MV002 itself is not edited): the rule is
*documented conventions that vary; the port is always explicit config*.

### 5. Doc-drift fix

All four files in `multiview-engine/src/tally/` — `mod.rs`, `arbiter.rs`, `profile.rs`,
`gpio.rs` (each at line 3) — cite ADR-MV001 for the
tally surface; correct all four to [ADR-MV002](ADR-MV002.md) + ADR-MV006. The code edit is a
Lane E item
in the [production-switcher backlog](../development/production-switcher-backlog.md); recorded
here so the drift is not re-propagated by new code citing the old comment.

## Alternatives considered

- **Tally-from-events-only** (integrate transition/keyer lifecycle events control-side into an
  on-air set): rejected — derivation must be **composition-truth per tick**. An event integrator
  drifts from what is actually composited the moment an event is missed, reordered, or lagged
  (the broadcast is lossy *by design*), and the mid-transition both-red window exists only
  *between* lifecycle events. The render plan is the truth; derive from it.
- **Separate internal and external arbiters** (switcher lamps resolved in the switcher, external
  lamps in the existing arbiter, merged at render): rejected — two resolution layers means an
  ambiguous winner, two latch stages that can disagree, and no single auditable `ConflictPolicy`.
  One arbiter was the entire point of ADR-MV002's design.
- **Ship sockets first, reconcile the TSL wire format later**: rejected — every deviation in §4
  is wire-visible, so post-ship fixes are breaking changes against deployed peers; pre-ship they
  are free.
- **Treat the current bytes as a "compatibility dialect"** alongside the spec format: rejected —
  no peer speaks it; it is not a dialect, it is a defect.
- **Derive only top-level PGM/PVW (skip keyer/nested/multi-box contributors)**: rejected — that
  *is* the untallied-overlay anti-pattern this ADR exists to close.

## Consequences

- A red lamp now means exactly "this source can be seen on an on-air output this tick", including
  through keys, stingers, multi-box and nested M/Es — and that guarantee is structural (derived
  from the render plan), not best-effort.
- One more per-tick pure pass: O(render-plan) fact derivation + O(facts) arbitration,
  allocation-pooled, publish-on-change only — the efficiency budget rides the same standing
  review as the resolver it walks ([ADR-0054](ADR-0054.md)).
- `TallyFact` generalises its key from `tile: u32` to `TallyTarget` — additive on a pure machine,
  but every existing profile-mapping test is revisited in the same change.
- External facility tally can now *disagree* with internal truth; the single `ConflictPolicy`
  with pinned default priorities makes the winner explicit and auditable instead of accidental.
- This ADR supersedes ADR-MV002's ports wording (§4 — MV002 itself is unedited; readers of its
  ports sentence are redirected here); its interop matrix becomes honest before
  the first socket ships. Open question carried from research: whether a newer official spec
  revision exists than the PDF verified here — to be re-checked when the Lane E codec fix lands
  (golden vectors are transcribed from the spec revision in hand and dated).
- The multiview red/green/amber tally row of the MVP ([production-switcher.md](../research/production-switcher.md)
  phased plan) is unblocked: arbiter wiring + ingest spawn are MVP; TSL socket egress stays
  post-MVP, now with a spec-correct codec under it. NDI tally return (per-source tally metadata
  to NDI senders, behind the runtime-loaded `ndi` feature) and camera-tally egress are likewise
  **post-MVP consumers of the same resolved arbiter state** — named here, not designed in this
  pass.
