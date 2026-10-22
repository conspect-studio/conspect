# Multiview — Control Surfaces: Generic MIDI Mapping + the RM-LP350G Profile

**Area:** Control plane / Web-API stack / Config / Managed devices — physical operator-surface ingress, action mapping, and feedback.
**Status:** Design brief (Proposed) — docs-only; implementation follows in dependency-ordered waves.
**Drives:** [ADR-0086](../decisions/ADR-0086.md) (control-surface model + MIDI mapping — generic surface abstraction, RM-LP350G as a profile, controls → switcher verbs / macros / audio faders, feedback).
**Extends:** [production-switcher.md](production-switcher.md) (the switcher verb surface a surface drives; [ADR-W021](../decisions/ADR-W021.md) external-control contract),
[switcher-audio.md](switcher-audio.md) (the fader/mute targets; [ADR-0077](../decisions/ADR-0077.md) input strips + send matrix),
[managed-devices.md](managed-devices.md) ([ADR-M008](../decisions/ADR-M008.md) — a surface is a **managed device** with a compiled-in driver),
[realtime-api.md](realtime-api.md) (the bounded command bus + drop-oldest feedback lanes),
[broadcast-cues.md](broadcast-cues.md) (a sibling ingress that maps external events onto the **same** verbs — the rule-table precedent).
**Backlog:** `SURF-*` in [`../development/feature-intake-2026-06-13.md`](../development/feature-intake-2026-06-13.md).

> **A control surface is an event ingress, not a new authority.** A physical panel — a fader moved, a
> button pressed, a T-bar dragged — is a managed device whose driver decodes hardware messages into
> one normalized `SurfaceInput` value, looks it up against an operator-authored mapping table, and
> dispatches it as the **same** switcher / audio / macro `Command`s that the REST verbs
> ([ADR-W021](../decisions/ADR-W021.md)) and broadcast cues ([broadcast-cues.md](broadcast-cues.md))
> already fire onto the bounded control-plane command bus
> (`crates/multiview-control/src/command.rs:476`). Feedback — lamp/LED state, motor-fader position,
> meter bridges — is the **mirror** of engine-owned live state ([ADR-RT008](../decisions/ADR-RT008.md))
> sampled and pushed back to the surface, exactly as the realtime layer pushes it to the SPA. Nothing
> in this subsystem touches the output clock or can back-pressure the engine (invariants #1 and #10
> hold by construction — a surface is just another best-effort client). This brief adds only the
> *transport drivers* (generic MIDI first; HID/serial/OSC as siblings), the *generic surface
> abstraction*, and the *control→action mapping table*. No new verbs.

> **Vendor posture.** This brief builds on the open **MIDI 1.0** and **MIDI 2.0 / Universal MIDI
> Packet** specifications (MIDI Association) and the **USB Device Class Definition for MIDI Devices**;
> it references **Mackie Control (MCU)** and **HUI** *conceptually* as widely-understood
> control-surface feedback idioms (motor faders, LED rings, meter bridges) without redistributing or
> bundling any vendor spec. The **RM-LP350G** is named **nominatively and factually** (§4) under the
> same clean-room posture this repo applies to ZowieTek devices
> ([managed-devices.md](managed-devices.md) §"Nominative trademark use only",
> `managed-devices.md:584`): "Works with …", plain text, no logos/trade dress, a non-affiliation
> disclaimer, and **no vendor SDK bundled**. Where the RM-LP350G's wire protocol cannot be verified
> from public sources, this brief is explicit (§4) and ships a generic profile-slot rather than a
> guessed binding. OSC ([Open Sound Control 1.0](https://opensoundcontrol.org/)) is documented as a
> sibling transport (§5), not implemented here. See [CODE_OF_CONDUCT.md](../../CODE_OF_CONDUCT.md).

---

## 0. Headlines

1. **A control surface is a managed device** ([ADR-M008](../decisions/ADR-M008.md)) with desired-state
   config (which physical surface, which mapping table) and engine-/control-plane-owned live state
   (last decoded input, current feedback). It rides the existing device registry, supervised
   reconnect, and alarm ladder — no new tier.
2. **One generic abstraction, many transports.** The model is `Surface → Controls → SurfaceInput →
   Mapping → Command`, plus `LiveState → Feedback → Surface`. The first transport is **generic MIDI**
   (USB-MIDI and 5-pin DIN, **MIDI 1.0 in MVP**) via a [`midir`](https://docs.rs/midir)-style binding;
   MIDI 2.0 / UMP is an additive upgrade behind the same abstraction, gated on `midir`/OS maturity
   (§2.4) — not current through `midir` today. HID and OSC are siblings behind the same abstraction
   (§2, §5).
3. **Mapping targets the verbs that already exist.** A control maps to a switcher verb
   ([ADR-W021](../decisions/ADR-W021.md)), a macro run ([ADR-M012](../decisions/ADR-M012.md)), or an
   audio fader/mute/send ([ADR-0077](../decisions/ADR-0077.md)). The surface adds **no new command
   vocabulary** — it is a thin client over the external-control contract W021 §13 was deliberately
   shaped to serve.
4. **Feedback is sampled mirror state, conflated.** Lamp/LED/motor-fader/meter feedback is computed
   from the [ADR-RT008](../decisions/ADR-RT008.md) latest-wins live-state mirror, conflated to a
   surface-friendly cadence (the realtime conflating-sampler, `realtime-api.md:173`), and written to
   the surface output port. The engine never waits for a surface to drain.
5. **The RM-LP350G is treated as a profile, honestly.** Verified public sources describe it as a
   **JVC** desktop **vMix** control surface over **USB** (§4). Whether it presents as class-compliant
   MIDI, USB-HID, or a vMix-proprietary endpoint is **not verifiable** from public material; this
   brief ships a named **profile slot** with a capture-and-confirm bring-up path, and flags the open
   question rather than asserting a binding (§4, Open questions).
6. **Invariants by construction.** Surface I/O is wholly control-plane: inputs are *sampled* events
   onto the bounded command bus (inv #1 — never a pacing signal); feedback is drop-oldest fan-out
   (inv #10 — the engine never awaits the surface). IPv6-first applies to any networked surface
   transport (OSC/RTP-MIDI) — bind `[::]`, bracket literals.

---

## 1. Generic control-surface abstraction

The product surface this design must serve is *"support for the RM-LP350G and future MIDI control
surfaces."* The durable answer is **not** an RM-LP350G driver — it is a generic surface model with a
profile per device. The model is five types, all in the control plane:

- **`Surface`** — a managed-device instance ([ADR-M008](../decisions/ADR-M008.md)): a `surface_id`,
  the transport binding (§2), and a reference to a `SurfaceProfile` + a `MappingTable`. Desired state
  only in config; live state (connected, last-input, current feedback frame) is mirrored read-only,
  exactly the Device desired/live split (`managed-devices.md`, ADR-M008).
- **`SurfaceProfile`** — the *physical* description of a model: the set of **controls** it exposes,
  each with a stable `control_id`, a `ControlKind`, and the transport-level address that identifies
  it (for MIDI: channel + message type + note/CC number; for HID: a usage/scancode; for OSC: an
  address pattern). A profile is data, not code, so a new surface is a new profile file plus an
  optional capture session — never a new driver.
- **`ControlKind`** (internally tagged, `tag = "kind"`, never `untagged` — conventions §9):
  - `Button { momentary | latching }` — cuts, takes, DSK on-air, macro triggers, bus crosspoints.
  - `Encoder { relative | absolute }` — trims, gains (endless rotary, relative deltas).
  - `Fader { absolute }` — audio faders, the **T-bar** (a fader whose target is transition progress).
  - `Joystick { axes }` — PTZ pan/tilt/zoom (maps to a managed-device PTZ verb, not the engine).
  - `Lamp` / `LedRing` / `MotorFader` / `MeterBridge` — **feedback** controls (output direction).
- **`SurfaceInput`** — the normalized event a driver emits per hardware message: `{surface_id,
  control_id, value}` where `value ∈ { Press | Release | Delta(i32) | Absolute(u16 basis-points) }`.
  Absolute values use the **integer basis-point** convention W021 already pins for the T-bar
  (`0..=10000`, never float — inv #3; [ADR-W021](../decisions/ADR-W021.md) §1 `tbar`).
- **`MappingTable`** — operator-authored rows `control_id → Action`, where `Action` is one of the
  existing verbs (§3). This is the **direct analogue of the cue→action rule table**
  ([broadcast-cues.md](broadcast-cues.md)): a surface is "another ingress that maps onto the same
  verbs," and the rule-table machinery, validation, and Class-1/Class-2 classification are reused, not
  re-invented.

**Direction in: input.** `decode(raw) → SurfaceInput → lookup(MappingTable) → Command → command_bus`.
The driver decodes; the mapping resolves; the command bus carries it; the engine drains it at the
frame boundary (`command.rs:515` `try_drain`). The decode→lookup→submit path is pure and
property-testable (a fuzzed MIDI byte stream must never panic, must drop-or-warn on malformed status
bytes — the "bad inputs are the purpose" rule applies to surfaces too).

**Direction out: feedback.** `RT008 live-state delta → FeedbackPlan(profile) → SurfaceOutput →
output port`. The feedback planner walks the surface's feedback controls and computes their target
state from the mirror: a `Lamp` over a "DSK1 on-air" fact, an `LedRing` over an encoder's audio gain,
a `MotorFader` over the program master fader, a `MeterBridge` over `audio.meter`
(`realtime-api.md:104`). This is exactly the **tally arbiter** pattern already in-tree — resolve
authoritative facts into per-lamp states (`crates/multiview-engine/src/tally/arbiter.rs:145`
`TallyArbiter`) — applied to a physical panel instead of an IP tally bus.

### 1.1 Conflict & echo (a real footgun, designed out)

Two operators (a physical fader and the SPA fader) can move the same target. The rule, inherited from
W021's "explicit-state, idempotent, latest-wins" contract: the **engine live state is the single
truth**; the surface fader is an *input device*, and its motor (if any) is a *feedback device* that
**follows** live state. A surface move emits an absolute `SurfaceInput`; the engine applies it
latest-wins; the resulting live-state delta drives the motor-fader back to the authoritative position.
A surface without motor faders simply shows a stale knob until the next touch — never a fight. To
avoid an echo loop (feedback nudging a touch-sensitive fader the operator is holding), motor-fader
feedback is **suppressed while the control reports "touched"** (MCU/HUI touch-sense idiom, §2.3) — a
per-control latch in the feedback planner, not engine state.

### 1.2 Efficiency budget (mem / cpu / gpu / io)

- **GPU:** zero. A surface never touches the compositor or any GPU path.
- **CPU:** negligible. MIDI is a low-rate byte stream (≪ 1 kB/s typical; a busy MCU meter burst is a
  few kB/s); decode is a small state machine. Feedback planning runs only on a live-state delta,
  conflated to ≤ ~30 Hz (meter bridge) / event-driven (lamps) — the realtime conflating-sampler
  (`realtime-api.md:173`), already built for the SPA.
- **Memory:** O(controls) per surface — a few hundred bytes of profile + a latest-value feedback slot
  per control. Bounded; no per-event allocation on the steady path.
- **I/O:** one MIDI in + one MIDI out port (or one HID/OSC endpoint) per surface; output is rate-capped
  and drop-oldest. A drop-oldest queue alone does **not** make the physical write non-blocking — so the
  device write runs on a **dedicated per-surface writer worker** fed by a bounded latest-value queue,
  with a write deadline; a blocked/wedged write trips disconnect-and-reopen on that worker and never
  stalls feedback planning, reconnect handling, the command bus, or (by isolation, §6) the engine.

---

## 2. MIDI transport (USB / DIN; MIDI 1.0 / 2.0) + a `midir`-style binding

### 2.1 Transports

- **USB-MIDI** — class-compliant MIDI over USB per the **USB Device Class Definition for MIDI
  Devices** (USB-IF; (unverified) v1.0 1999 / v2.0 2020 for MIDI 2.0). The OS enumerates a MIDI port;
  no kernel driver is needed for class-compliant devices.
- **5-pin DIN MIDI** — legacy serial MIDI via a USB-MIDI interface; appears as a port identically.
- **(unverified) RTP-MIDI / AppleMIDI** (RFC 6295 payload; the session protocol is de-facto) — MIDI
  over IP. If/when added it is a **networked** transport and is therefore IPv6-first: bind `[::]`,
  bracket IPv6 literals (conventions §10) — documented here for completeness, not in MVP scope.

### 2.2 The binding: [`midir`](https://docs.rs/midir)

`midir` is the chosen Rust binding (verified on docs.rs) **for the MVP MIDI 1.0 scope**: cross-platform
realtime MIDI with the exact backends Multiview targets — **ALSA seq** (Linux) and **CoreMIDI** (macOS)
— plus JACK and Web-MIDI; it supports virtual ports and full SysEx, and delivers input via a callback
per message ([midir docs](https://docs.rs/midir)). UMP / MIDI 2.0 support through `midir` is **not
assumed current** and is gated on upstream + OS maturity (§2.4). It matches the platform matrix (Linux x86_64/aarch64 + macOS;
no Windows). The binding sits behind an **off-by-default `midi` Cargo feature** on `multiview-control`
(or a small `multiview-surface` module within it — no new crate, per the [ADR-0054](../decisions/ADR-0054.md)
no-new-crates posture the switcher already follows), so the default `cargo check` stays pure-Rust and
GPU-free (conventions §3). `midir` is MIT-licensed (verify in `cargo deny`), keeping the default build
clean. The callback runs on a `midir` thread; it does the cheap decode → `SurfaceInput` and hands off
to the control-plane task over a bounded channel — it never blocks and never touches the engine.

### 2.3 MIDI message → `SurfaceInput` (MIDI 1.0)

- **Note On/Off** → `Button` press/release (velocity > 0 = press; Note Off or velocity 0 = release).
- **Control Change (CC)** → `Fader`/`Encoder`: absolute CC (0–127) maps to `Absolute` (scaled to the
  basis-point range, integer math, never float); relative CC (sign-magnitude or two's-complement, the
  common encoder encodings) maps to `Delta`. 14-bit CC (MSB/LSB pair) and Pitch-Bend give the
  finer-grained fader resolution the T-bar wants.
- **Pitch Bend** → high-resolution `Fader` (14-bit) — the natural T-bar/motor-fader channel in the
  MCU idiom.
- **System Exclusive (SysEx)** → device-specific: meter/LCD/segment feedback and some button feedback
  in MCU/HUI ride SysEx. The driver treats unknown SysEx as opaque and logs-and-drops (never panics).

### 2.4 MIDI 2.0 / Universal MIDI Packet

(unverified, from training knowledge) **MIDI 2.0** introduces the **Universal MIDI Packet (UMP)**,
**32-bit high-resolution** controllers, **per-note** controllers, and **bidirectional capability
negotiation (MIDI-CI)** — directly useful for higher-resolution faders and richer feedback. The model
above is UMP-ready **without changing the normalized contract**: the wire-level UMP 32-bit controller
resolution is *scaled down* into the same `Absolute(u16)` `0..=10000` basis-point range at decode time,
exactly as a 7-bit/14-bit MIDI 1.0 value already is — "basis points" is the normalized unit, not the
transport's bit-width, so the schema/API contract is unchanged across transport resolutions. (Should a
future need for raw sub-basis-point precision arise, it would be an explicitly versioned new value type
— e.g. `AbsoluteRaw32` — not a silent widening of `Absolute`.) MVP targets MIDI 1.0 (universally
available on hardware like the RM-LP350G class); MIDI 2.0 is an additive transport upgrade behind the
same abstraction, gated on `midir`/OS UMP support.

### 2.5 Feedback idioms (Mackie Control / HUI as conceptual references)

Verified industry references (Sound on Sound; Mackie HUI/MCU documentation) describe the
control-surface feedback vocabulary this design mirrors conceptually — **not** by copying any vendor
map:

- **LED / lamp** — a button's lit state echoes a boolean fact (on-air, selected-on-bus, armed). Driven
  by a Note On to the same note (MCU idiom) or a CC, per profile.
- **LED ring** — an encoder's value shown as a ring of LEDs; driven by a CC echo.
- **Motor fader** — a touch-sensitive motorized fader that follows automation; driven by Pitch Bend /
  14-bit CC, with **touch-sense suppression** (§1.1) so feedback doesn't fight a held fader.
- **Meter bridge** — per-channel level LEDs; driven by SysEx/CC at a conflated cadence from
  `audio.meter` (`realtime-api.md:104`).

A profile names which feedback idiom each control uses; the generic feedback planner emits the bytes.

---

## 3. Mapping: surface control → switcher verb / macro / audio fader

The `Action` a mapping row targets is exactly one of the **already-existing** verbs. No new commands.

| Control (example) | `ControlKind` | `Action` → existing verb | Source ADR |
|---|---|---|---|
| PGM/PVW bus buttons (1–12) | `Button` | `POST …/switcher/mix-effects/{id}/preview` or `/program` (crosspoint set) | [ADR-W021](../decisions/ADR-W021.md) §1 |
| CUT / AUTO | `Button` | `…/cut` (Class-1, 200) / `…/auto` (timed, 202) | [ADR-W021](../decisions/ADR-W021.md) §1–§2 |
| T-bar | `Fader (absolute)` | `…/mix-effects/{id}/tbar` `{position: 0..=10000}` (conflated, latest-wins) | [ADR-W021](../decisions/ADR-W021.md) §1 |
| DSK on-air / tie | `Button (latching)` | `…/downstream-keyers/{id}/on-air`·`/off-air`·`/tie` (explicit-state, idempotent) | [ADR-W021](../decisions/ADR-W021.md) §1 |
| FTB | `Button` | `…/mix-effects/{id}/ftb` `{engaged: bool}` | [ADR-W021](../decisions/ADR-W021.md) §1 |
| Audio channel fader | `Fader (absolute)` | audio strip fader / send level | [ADR-0077](../decisions/ADR-0077.md) §3 |
| Audio mute / PFL | `Button (latching)` | strip mute (gain-preserving) / PFL | [ADR-0077](../decisions/ADR-0077.md) §1 |
| Master fader | `Fader (absolute)` | `PATCH …/switcher/audio` `{master_gain_db}` | [ADR-W021](../decisions/ADR-W021.md) §1 |
| Rotary trim | `Encoder (relative)` | strip gain/trim delta | [ADR-0077](../decisions/ADR-0077.md) §1 |
| Macro button (M1…Mn) | `Button` | `POST …/macros/{id}/run` (salvo-shaped batch) | [ADR-W021](../decisions/ADR-W021.md) §1, [ADR-M012](../decisions/ADR-M012.md) §1 |
| PTZ joystick | `Joystick` | managed-device PTZ move (a device verb, not an engine command) | [managed-devices.md](managed-devices.md) |

**Why this is thin.** [ADR-W021](../decisions/ADR-W021.md) §13 deliberately defined the
external-control contract — stable operator-authored string ids, idempotent single-shot commands,
boolean feedback queries, snapshot+deltas — *so that* "an OSC namespace and a MIDI surface adapter are
adapters over this surface, not new surfaces." This brief is the cash-in of that promise: the surface
driver is a translator between hardware messages and the W021 verbs, and feedback is a translator
between the RT008 mirror and the panel.

**Class-1 / Class-2 and the command bus.** Every mapped action inherits its live-apply class from the
verb it targets ([ADR-M012](../decisions/ADR-M012.md) §9, inv #11) — a fader move is Hot/Class-1, an
aux re-point may be Class-2. A surface cannot change a verb's class; it only fires it. All actions
travel the **same bounded command bus** the SPA and cues use (`command.rs:476` `command_bus`,
`Command` at `:174`), so isolation (inv #10) and classification (inv #11) are properties of the bus,
not of the surface.

**Idempotency & rate.** High-rate controls (T-bar, faders) are **conflated client-side** to
latest-wins absolute setters before hitting the bus — never one command per MIDI byte — exactly the
T-bar discipline W021 §1 pins. This is the surface analogue of the SPA's per-animation-frame
conflation; it makes a fast fader sweep a stream of absolute states, not a command storm.

**Per-surface ingress lane (so one bad surface can't evict others).** Conflation alone is not enough:
a flooding surface must not be able to fill the *shared* command bus and cause its drop-oldest to evict
unrelated SPA/cue/operator commands. Each surface therefore drains through its **own bounded,
conflating ingress lane** *before* the shared bus; only admitted, normalized actions cross onto
`command_bus`. When a surface overruns its lane, only **that surface's own** events are dropped/conflated
(with overload telemetry) — never another client's traffic. The shared bus's own drop-oldest remains the
last-resort backstop for invariant #10, but it is not the first line of defence against a single noisy
surface.

---

## 4. The RM-LP350G profile (verify; profile-slot + open question)

**What is verified** (B&H, Markertek, Sweetwater, JVC pro product pages — see Sources): the
**RM-LP350G** is a **JVC** "CONNECTED CAM" **desktop vMix studio switcher control surface**. It adds
tactile controls to a system running **vMix** software: a low-profile **transition fader with T-bar**,
**12-channel PGM/PVW** switching with four groups of 12 downstream-key settings, **PTZ preset**
positions with a **joystick**, and it links over **USB**; vMix integration is by importing the
RM-LP350G's **shortcut template and activators** into vMix. Approx. street price ~US$1,099.

**What is NOT verified, and matters:** the operator's request names it as a *MIDI* surface, but **no
public source confirms the RM-LP350G's wire protocol.** The "import a shortcut template + activators
into vMix" workflow is consistent with **either**:
- a **class-compliant MIDI** device (vMix has a MIDI activators/shortcuts system), **or**
- a **USB-HID** device (keyboard/gamepad/consumer-control emulation) mapped via vMix shortcuts, **or**
- a **vMix-proprietary USB** endpoint.

These are materially different bindings. **We do not assert which it is.** The honest, shippable
design is:

1. **Ship the generic model (§1–§3) first** — it is the durable deliverable and serves *"future MIDI
   control surfaces"* regardless of any one device.
2. **Ship an `rm-lp350g` profile slot** — a `SurfaceProfile` file naming its controls by function
   (12× PGM, 12× PVW, T-bar, CUT/AUTO, DSK group buttons, PTZ joystick) with **transport addresses
   left as a capture-confirmed mapping**, not guessed numbers.
3. **Bring-up is capture-and-confirm, not reverse-engineering a spec.** A small **learn mode** (the
   driver echoes "control X sent message Y") lets an operator with the physical unit bind each control
   in minutes — the same workflow a generic MIDI surface uses anyway. This needs **no vendor SDK and
   no redistributed spec** — it observes the device's own emitted messages on the operator's bench.
4. **If the unit is HID, not MIDI**, the *same* abstraction applies via the HID transport sibling
   (§1, `ControlKind` addresses are transport-tagged); only the transport driver differs. The mapping
   table, feedback model, and verb targets are unchanged. This is why the generic-first design is the
   right hedge: it is correct whichever way the RM-LP350G's binding resolves.

**Vendor/trademark.** Nominative use only — "Works with the JVC RM-LP350G control surface", plain
text, no logos/trade dress, a non-affiliation disclaimer, mirroring the ZowieTek posture
(`managed-devices.md:584`). The profile ships **only** addresses captured on the operator's own
hardware or contributed by users — never a JVC- or vMix-published map redistributed without rights.
**For a class-compliant MIDI surface this is low-risk** (observing a device's own emitted standard
messages). **If the unit resolves to a proprietary USB/HID endpoint**, distributing a captured profile
in-tree may touch reverse-engineering / ToS territory, so any such proprietary-endpoint profile is
**gated behind explicit legal/operator review before in-tree distribution** — the capture-and-confirm
learn mode still works locally for the operator regardless. Defer to the operator on the
proprietary-protocol stance if the unit turns out to be a closed HID endpoint.

> **Open question carried forward (not silently resolved):** *Is the RM-LP350G class-compliant MIDI,
> USB-HID, or vMix-proprietary?* Until confirmed on hardware, the binding is a **profile slot**, and
> the brief does not claim MIDI support for this specific unit. See Open questions.

---

## 5. OSC as a future sibling

**Open Sound Control** ([OSC 1.0](https://opensoundcontrol.org/), (unverified) 1.1 update) is the
natural sibling transport for **software** and **networked** surfaces (TouchOSC, Lemur-style tablet
panels, custom controllers). It fits the generic abstraction with **zero model change**:

- An OSC **address pattern** (`/multiview/me1/program` …) is just another transport-level control
  address in a `SurfaceProfile`; an OSC argument maps to `Absolute`/`Delta`/`Press` like a MIDI value.
- Feedback is an outbound OSC message — the same `FeedbackPlan`, different framing.
- OSC is **UDP/TCP over IP**, so it is **IPv6-first**: bind dual-stack `[::]` (`IPV6_V6ONLY=false`),
  bracket IPv6 literals, loopback `[::1]` (conventions §10, [ADR-0042](../decisions/ADR-0042.md)).
  This is the one transport with a network surface, so the IPv6 discipline is load-bearing here.

OSC is **documented, not built** in MVP — listed so the abstraction is provably transport-agnostic
(MIDI now, HID for hardware that needs it, OSC for networked/software panels) and the mapping/feedback
machinery is shared across all three. A future `multiview-surface` OSC adapter is a transport driver,
not a new subsystem.

---

## 6. Invariants & isolation (stated explicitly)

- **#1 Output-clock — untouchable.** A surface is an input *sample*, never a pacing signal. A fader
  move, a button, a T-bar drag is an event placed on the bounded command bus and drained by the engine
  at the frame boundary (`command.rs:515`); the engine's cadence is independent of any surface. A
  dead, flooding, or malformed surface cannot slow or stall a single output tick.
- **#10 Isolation — the engine never awaits the surface.** Input flows surface → **per-surface bounded
  conflating ingress lane** (drops/conflates only that surface's own events on overrun, §3) → shared
  bounded command bus (`command_bus(capacity)`, drop-oldest as the last-resort backstop); feedback flows
  engine-owned live state → conflating sampler → surface output port, drop-oldest on a dedicated writer
  worker (§1.2), the engine never blocking on a USB/MIDI write. This is the
  [realtime-api](realtime-api.md) "physically incapable of back-pressuring the engine" rule
  (`realtime-api.md:21`) applied to a physical panel. A CI chaos test (wedge the surface output port;
  the engine keeps ticking) gates the lane, mirroring the ADR-0059 audio-consumer chaos test.
- **#3 Integer time / no float fps.** Absolute control values are integer basis points (T-bar
  `0..=10000`, W021 §1); durations are integer frames/ms (exact-rational conversion). MIDI velocities
  and CC values are integers; no float enters the timing path.
- **#6 / #8** — not engaged: a surface touches neither decode resolution nor the color pipeline.
- **IPv6-first** — only the OSC/RTP-MIDI network transports have a socket; they bind `[::]` and
  bracket literals (§5). USB/DIN MIDI is not networked.
- **Bad inputs are the purpose.** A surface is an untrusted byte source: the MIDI decoder is
  fuzz/property-tested to never panic on malformed status bytes, truncated SysEx, or running-status
  edge cases; it logs-and-drops the unknown and keeps the surface alive — the same posture inputs get.

---

## Open questions

1. **RM-LP350G binding (the headline unknown).** Is it class-compliant MIDI, USB-HID, or a
   vMix-proprietary USB endpoint? **Unresolved from public sources** (§4). Resolution requires the
   physical unit on a bench (enumerate the USB descriptor; observe emitted messages). Until then the
   brief ships the generic model + a capture-confirmed profile slot and does **not** claim MIDI
   support for this specific device. *Default if it is HID:* the HID transport sibling carries it
   unchanged; *default if proprietary/closed:* document the limitation and defer to the operator.
2. **Where does the driver live — feature on `multiview-control`, or a thin `multiview-surface`
   module?** Defaulting to an off-by-default `midi` feature *within* control (no new crate, matching
   [ADR-0054](../decisions/ADR-0054.md)); revisit only if HID + OSC + RTP-MIDI grow the surface code
   enough to warrant a crate. Reversible.
3. **Profile distribution & trust.** Profiles are data; should community-contributed profiles ship
   in-tree, or be operator-loaded files only? Leaning operator-loaded + a small curated in-tree set
   (generic MCU/HUI-style + the RM-LP350G slot) to avoid redistributing any vendor-published map.
4. **`midir` license & `cargo deny`.** `midir` is MIT (verify pinned), but its ALSA/CoreMIDI backends
   pull platform libs — confirm `cargo deny` stays green under the `midi` feature before it lands.
5. **HID transport library.** If the RM-LP350G is HID, which Rust HID crate (`hidapi` vs evdev on
   Linux) and how to keep it off the default build? Out of scope until §4 Q1 resolves.
6. **MIDI 2.0 timing.** UMP/MIDI-CI support depends on `midir`/OS maturity; track upstream before
   committing to a 2.0 feedback path.

---

## Sources

External standards & references (web-verified except where marked **(unverified)** / training-knowledge):

- MIDI Association — **MIDI 1.0** and **MIDI 2.0 / Universal MIDI Packet (UMP)**, **MIDI-CI** — <https://midi.org> (UMP/2.0 details **(unverified)**, training knowledge).
- USB-IF — **USB Device Class Definition for MIDI Devices** **(unverified)** (v1.0 1999 / v2.0 2020), training knowledge.
- [`midir`](https://docs.rs/midir) — Rust realtime MIDI (ALSA / CoreMIDI / JACK / WinMM / WebMIDI; virtual ports; SysEx) — web-verified on docs.rs.
- Mackie **MCU** / **HUI** — control-surface feedback idioms (motor faders, LED rings, meter bridges, touch-sense) — Sound on Sound / Mackie documentation, web-verified *conceptually* (no map redistributed).
- [Open Sound Control 1.0](https://opensoundcontrol.org/) — sibling transport (§5), web-verified.
- RM-LP350G — JVC desktop vMix control surface, USB, T-bar + 12-ch PGM/PVW + PTZ joystick — [B&H](https://www.bhphotovideo.com/c/product/1765918-REG/jvc_rm_lp350g_connected_cam_vmix_studio.html), [Markertek](https://www.markertek.com/product/jvc-rm-lp350g/jvc-rm-lp350g-connected-cam-vmix-studio-switcher-control-surface), [JVC pro](https://www.jvc.com/usa/pro/professional-video/camera-accessories/rm-lp350g/) — web-verified; **wire protocol unverified** (§4).
