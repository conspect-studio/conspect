# Multiview — Broadcast Cues & Automation Ingress

**Area:** Control plane / Engine / Input / Output / Config / Web — cue ingest, normalization, and action dispatch.
**Status:** Design brief (Proposed) — docs-only; implementation follows in dependency-ordered waves.
**Drives:** [ADR-0072](../decisions/ADR-0072.md) (cue model + bus + action vocabulary + Class-1/2),
[ADR-0073](../decisions/ADR-0073.md) (GPI/GPO/GPIO/relay — Pi GPIO mode),
[ADR-0074](../decisions/ADR-0074.md) (SCTE-104 + SCTE-35 ingest + emit),
[ADR-0075](../decisions/ADR-0075.md) (HLS/DASH manifest cues),
[ADR-0076](../decisions/ADR-0076.md) (BXF / automation metadata exchange).
**Extends:** [production-switcher.md](production-switcher.md) (the macro/verb surface cues fire),
[decoupled-routing.md](decoupled-routing.md) ([ADR-0034](../decisions/ADR-0034.md), SCTE-35 crosspoint),
[realtime-api.md](realtime-api.md) (the command bus + drop-oldest event lanes),
[managed-devices.md](managed-devices.md) + [display-out.md](display-out.md) (the control-plane node/driver tier the Pi GPIO node mirrors),
[iso-program-recording.md](iso-program-recording.md) ([ADR-0037](../decisions/ADR-0037.md), arm/disarm recording),
[multi-program.md](multi-program.md) (aux routes), [switcher-audio.md](switcher-audio.md) (fade-audio-bus action).
**Backlog:** `CUE-*` in [`../development/feature-intake-2026-06-13.md`](../development/feature-intake-2026-06-13.md).

> **A cue is an event sampled at the frame boundary, never a pacing signal.** Every cue —
> a relay closure on a Raspberry Pi GPIO pin, a SCTE-104 message from a studio automation
> system, a SCTE-35 section in an ingested transport stream, an `EXT-X-DATERANGE` tag in an
> HLS manifest, a BXF schedule row — is decoded by a control-plane actor into one normalized
> `Cue` value, looked up against an operator-authored rule table, and dispatched as ordinary
> switcher/engine `Command`s through the *same bounded command bus* live operators and macros
> already use. Nothing in this subsystem touches the output clock or can back-pressure the
> engine (invariants #1 and #10 hold by construction — a cue is just another best-effort
> client). The verbs cues fire — cut, auto, take, load/play VT, show/hide DSK lower-third,
> set aux route, fade audio bus, mute source, start/stop recording, emit SCTE-35, emit a
> manifest cue, fire a GPO, send a WebSocket event, call a webhook — **already exist** in the
> production-switcher, routing, recording, and notify subsystems; this brief maps onto them
> and adds only the *ingress decoders* and the *cue→action rule table*.

> **Vendor posture.** This brief builds exclusively on open, published standards: ANSI/SCTE 104,
> ANSI/SCTE 35, SMPTE ST 2010 (SCTE-104 in VANC), SMPTE ST 291M (ancillary data), RFC 8216 (HLS;
> the SCTE-35 mapping is `EXT-X-DATERANGE` with `SCTE35-OUT`/`SCTE35-IN`/`SCTE35-CMD` attributes —
> `EXT-X-CUE-OUT`/`EXT-X-CUE-IN` are de-facto/vendor compatibility tags, not RFC 8216 tags),
> MPEG-DASH inband `emsg` + MPD `EventStream`,
> SMPTE ST 2021 (BXF), and AMWA NMOS IS-07 (event/tally over the network). No vendor SDK is
> bundled and no proprietary spec is redistributed; where a behaviour is industry consensus
> without a governing standard it is labelled **de-facto industry practice**. See
> [CODE_OF_CONDUCT.md](../../CODE_OF_CONDUCT.md).

---

## 0. Headlines

1. **One cue bus, five ingresses, one action vocabulary.** A `Cue` is a normalized value
   (`source`, `kind`, optional `pts`/`break_duration`, `event_id`, free-form `attributes`)
   produced by any of five decoders — GPIO, SCTE-104, SCTE-35, manifest, BXF — and consumed by
   a single rule table that maps `(matcher) → [action]`. The decoders differ; the dispatch is
   uniform. §1.
2. **The cue substrate already exists in-tree.** SCTE-35 (`splice_info_section`) and SCTE-104
   (`multiple_operation_message`) are already parsed to a normalized `CueEvent`
   (`crates/multiview-input/src/scte/mod.rs:88` `CueEvent`,
   `crates/multiview-input/src/scte/scte104.rs:161` `Scte104Message::parse`,
   `crates/multiview-input/src/scte/splice35.rs` with CRC-32 validation); a re-emit path exists
   (`SpliceInfoSection::reserialize_with_pts_adjustment`,
   `crates/multiview-input/src/scte/splice35.rs:309`); a **virtual GPI/GPO value machine** with
   polarity + edge detection exists (`crates/multiview-engine/src/tally/gpio.rs:90` `GpiPoint`,
   `:141` `GpoPoint`, `:54` `Edge`, `:28` `Polarity`); SCTE-35 PIDs are already discovered
   (`crates/multiview-input/src/mpegts/selection.rs` `scte35_pids`); a webhook notifier builder
   exists (`crates/multiview-control/src/notify/webhook.rs`). This brief **wires these to the
   action surface**, it does not re-invent them. §1, §2, §3.
3. **The action verbs are the switcher's verbs.** Cut / auto / take / direct-program-punch /
   DSK on-air/off-air (lower-third) / aux route / T-bar / master-audio fade are exactly the
   bare-verb commands [ADR-W021](../decisions/ADR-W021.md) and [ADR-M012](../decisions/ADR-M012.md)
   pin, applied by the BUILT frame-boundary `CommandDrain`. Run-macro reuses the production-switcher
   control-plane macro sequencer ([production-switcher §10.2](production-switcher.md)). A cue rule
   is, in effect, *"on this cue, submit this macro / these commands"* — one apply path. §6.
4. **Cues never pace anything (inv #1/#10).** GPIO samples are read on a control-plane poller
   task; SCTE-104 arrives on a TCP listener task; SCTE-35/manifest/DASH cues are *observed*
   passing through ingest (the engine already samples inputs, never paces to them); BXF is a
   slow file/SOAP poll. Every decoder publishes onto a **bounded drop-oldest** queue that the
   cue dispatcher drains; the dispatcher submits to the existing bounded command bus with
   `try_send`. No cue path holds a lock the output clock holds, and the engine never awaits a
   cue producer. §7.
5. **The GPIO node mirrors the display-node tier.** A Raspberry Pi running `multiview` in a
   **GPIO mode** (`multiview gpio-node`, the structural twin of `multiview node` in
   [display-out.md](display-out.md)) is a control-plane **device/driver actor**
   ([managed-devices.md](managed-devices.md) shape): it owns the physical pins via `rppal`
   edge detection + debounce and posts normalized GPI cues to its controller, and asserts GPO
   levels the controller requests. The engine only ever sees a `Cue` and a `Command`. §2.
6. **SCTE-35 is already a crosspoint; cues extend it with ingest + emit.** Decoupled routing
   ([ADR-0034](../decisions/ADR-0034.md)) routes the SCTE-35 *data PID* through the crosspoint
   with `pts_adjustment` continuity. Cues add (a) **reacting** to the SCTE-35 content (fire
   actions on splice-out/in) and (b) **emitting** SCTE-35 into the program transport and the HLS/DASH
   manifests on operator/automation command — closing the contribution→distribution loop. §3, §4.
7. **Emit is the symmetric ask.** Operators want Multiview to *originate* cues, not only react:
   fire a GPO closure, emit a SCTE-35 `splice_insert`/`time_signal` into the output transport,
   write `EXT-X-DATERANGE`/`EXT-X-CUE-OUT`/`-IN` into the HLS playlist and `emsg`/MPD `EventStream`
   into DASH, send a WebSocket event, call a webhook. These are *actions* in the same vocabulary
   a cue can trigger — so an ingested GPI can emit a downstream SCTE-35, and a SCTE-35 splice-out
   can fire a GPO. §3, §4, §6.
8. **No new crate.** Decoders live where their transport already lives — SCTE in `multiview-input`,
   manifest cues in `multiview-input`/`multiview-output`, GPO/SCTE/manifest *emit* in
   `multiview-output`, the GPIO node + SCTE-104 TCP listener + BXF poller + the cue rule table +
   dispatcher in `multiview-control` (the device-driver/notify home), shared `Cue`/`CueAction`
   types in `multiview-core`, schema in `multiview-config`, the wire event in `multiview-events`,
   the panel in `web/`. The driver model is a closed `#[non_exhaustive]` enum exactly as
   [managed-devices.md](managed-devices.md) §2.3 mandates. §1, §8.

---

## 1. The cue bus: ingress → normalized `Cue` → action(s)

### 1.1 The normalized `Cue`

Every ingress decodes its native message into one `Cue` value (shared type in `multiview-core`,
internally-tagged serde per conventions §9 — never `untagged`):

```
Cue {
  id:        CueId,                 // dispatcher-assigned, monotonic per run
  source:    CueSource,             // {Gpio{node,pin}, Scte104{listener}, Scte35{input,pid},
                                     //  Manifest{input,kind}, Bxf{feed}, Internal}
  kind:      CueKind,               // {SpliceOut, SpliceIn, TimeSignal, Edge(Rising|Falling|Level),
                                     //  ProgramStart, ProgramEnd, BreakStart, BreakEnd, Generic}
  event_id:  Option<u32>,           // SCTE event id / BXF event id where present
  pts:       Option<MediaTime>,     // normalized to the ns timeline (inv #3) where the cue carries one
  break_duration: Option<MediaTime>,// where carried
  observed_at: MediaTime,           // the output-tick the dispatcher sampled it at
  attributes: BTreeMap<String,String>, // free-form (segmentation_descriptor type, DATERANGE id, …)
}
```

The existing `crates/multiview-input/src/scte/mod.rs:88` `CueEvent` (with `CueKind::{SpliceOut,
SpliceIn, TimeSignal}` at `:72`) is the **SCTE-specific** seed of this type; `Cue` is the
superset across all five ingresses. `CueEvent.pts_time_ns()`/`break_duration_ns()`
(`scte/mod.rs:107-115`, exact `90k → ns` rational `100_000/9`) already give the ns projection,
so the SCTE→`Cue` adapter is a field map, not new math.

### 1.2 The rule table: `(matcher) → [action]`

Operator-authored config (a `cues` block on `MultiviewConfig`, [ADR-0072](../decisions/ADR-0072.md)):

```
CueRule {
  name:    String,
  match:   CueMatcher { source: Option<CueSource>, kind: Option<CueKind>,
                        attribute_eq: BTreeMap<String,String>,
                        min_interval: Option<Millis> },   // own debounce on top of the ingress debounce
  actions: Vec<CueAction>,           // the vocabulary of §6, executed in order
  enabled: bool,
}
```

The dispatcher is a pure value machine in the salvo/tally/alarm house style: it consumes a
`Cue`, finds the matching enabled rules, and emits the action list. It performs no I/O and
takes no lock the engine holds — exactly the
[production-switcher §10.2](production-switcher.md) macro-sequencer posture. A `CueAction` that
maps onto a switcher verb **desugars onto the same engine `Command`** a live operator would send
(the `Command` type in `multiview-control`, expected seam); a `CueAction` that maps onto an
out-of-engine effect (webhook, GPO assert, SCTE emit) runs on the control-plane dispatch task.
One apply path, no privileged channel.

**Effective-time scheduling.** A cue may carry a *future* splice/preroll time (a SCTE-35
`splice_time`/`time_signal` PTS, a SCTE-104 preroll, a BXF wall-clock boundary) rather than meaning
"fire now". The dispatcher computes an `effective_at` per cue — derived from the SCTE splice/time
data where present, else the cue's `observed_at` — and submits its actions at that tick through the
*existing* frame-boundary apply mechanism (the same wait-step the macro sequencer uses), so a cue
neither fires early nor paces the output clock (inv #1: scheduling is a control-plane timer, never a
data-plane wait). A cue whose `effective_at` is already in the past beyond a bounded
grace-window is treated as expired: fire-immediately-with-warning vs drop is an operator-selectable
per-rule policy. This keeps the same logical cue *consistently timed* across transports.

### 1.3 Class-1 / Class-2 classification (inv #11)

A cue does not change the apply class of the action it fires — it *is* the action's class.
Because cue actions are exactly the switcher/routing/recording verbs, they inherit those verbs'
classifications verbatim from [ADR-M012 §9](../decisions/ADR-M012.md) and
[ADR-0034 §8](../decisions/ADR-0034.md):

| Cue action | Class | Source of truth |
|---|---|---|
| Cut / direct program punch / PVW set | **Class-1 (Hot)** | ADR-M012 §9 (O(1) `rebind_cell`) |
| Auto transition / FTB / DSK auto | **Class-1 (Hot, timed)** | ADR-M012 §9 (202 + completion event) |
| DSK on-air/off-air (lower-third) / tie | **Class-1 (Hot)** | ADR-M012 §9 |
| Aux route / video/subtitle crosspoint | **Class-1 (Hot)** | ADR-0034 §8 (rides the video seam) |
| Fade audio bus / master gain / mute source | **Class-1 (Hot)** | [ADR-0059](../decisions/ADR-0059.md) §3 (per-sample envelope) |
| VT load/cue | **Class-1 (Hot, async readiness)** | ADR-M012 §9 (202 until primed) |
| Start/stop recording (arm/disarm) | **Class-1 (Hot)** | [ADR-0037](../decisions/ADR-0037.md) §7 (best-effort sink) |
| Run macro / memory recall | **Class-1 (Hot, batch)** | ADR-M012 §9 (salvo-shaped frame-boundary batch) |
| Emit SCTE-35 / manifest cue / fire GPO / WS / webhook | **Class-1 (Hot, side-effect)** | this brief §3/§4/§6 (out-of-engine effects, never reset) |

No cue action is Class-2 in the MVP set; a cue that needs a structural change (e.g. add an M/E)
is **out of scope** — cues operate the running show, they do not re-architect it. The plan/take
dry-run ([ADR-W021](../decisions/ADR-W021.md)) is available to preview a rule's classification
before arming.

### 1.4 The cue lifecycle event

Every decoded cue and every dispatched action publishes a `cue` realtime event
([ADR-RT008](../decisions/ADR-RT008.md) taxonomy) on the existing drop-oldest broadcast lane,
so the SPA shows a live cue log and an operator can confirm a rule fired. The event is
*observational* — emitting it never blocks the dispatcher (inv #10), and the **`send WebSocket
event` action** (§6) is a distinct, operator-authored cue effect, not this lifecycle telemetry.

---

## 2. Ingress: GPI/GPO/GPIO/relay — the Raspberry Pi GPIO node

Full decision record: [ADR-0073](../decisions/ADR-0073.md).

### 2.1 The model already exists; the transport is new

`crates/multiview-engine/src/tally/gpio.rs` already provides the **pure logical layer**:
`GpiPoint::sample(raw_high) -> Edge` (`:123`) runs a raw level through `Polarity` (`:28`,
`ActiveHigh`/`ActiveLow` for normally-open vs normally-closed wiring) and reports the `Edge`
crossed (`:54` `Rising`/`Falling`/`None`) with one-sample memory; `GpoPoint::request(active)
-> raw_high` (`:174`) is the symmetric output. The module docstring already states it is the
"protocol-agnostic logic underneath" physical relay/opto transports and IS-07. **This brief
adds the physical transport** — the bit that turns a `/dev/gpiochip` edge into a `GpiPoint::sample`
call and a `GpoPoint::raw_high()` into a pin write — and the cue mapping.

### 2.2 The GPIO node = a display-node-style control-plane actor

Per [display-out.md](display-out.md) §0, a Raspberry Pi already runs `multiview node` as a thin
display client enrolled as a managed device. The GPIO node is its structural twin: a **`multiview
gpio-node`** mode (or a capability of an existing node) that is a `displaynode`-family driver in
the closed `#[non_exhaustive]` device enum ([managed-devices.md](managed-devices.md) §2.3) —
**not a plugin, not `dyn Any`**. The node:

- Owns the physical pins through the `rppal` crate's `InputPin::set_async_interrupt`
  ([rppal docs](https://docs.rs/rppal/latest/rppal/gpio/struct.Gpio.html); GPIO via the
  `gpiochip` character device) for GPI, and `OutputPin` for GPO. `rppal`'s
  `set_async_interrupt` accepts an **optional debounce** argument and reports edges — the
  hardware-near half of the §2.3 trigger model. **(unverified-detail)** `rppal` was retired
  for new features as of 2025-07-01; the design treats the GPIO HAL as an injectable trait so
  `rppal` (or the kernel `gpiod`/`libgpiod` char-device interface, or `embedded-hal`) is a
  swappable backend — the cue logic never depends on the crate.
- Runs **socket-free in CI** (the `tower::oneshot`/feature-gated pattern
  [managed-devices.md](managed-devices.md) §2.3): the `rppal`/hardware backend is behind a
  `gpio` off-by-default feature; the default build compiles + tests the pure value machine with
  an in-memory fake pin source.
- Publishes normalized GPI cues to its controller over the existing managed-device control
  channel (IPv6-first, bracketed literals, dual-stack `[::]` per conventions §10), and accepts
  GPO assert requests. The controller's cue dispatcher treats an inbound GPI exactly like any
  other `Cue`. The node **cannot back-pressure the engine**: a hung node stalls only its own
  driver task ([managed-devices.md](managed-devices.md) §2's invariant-#10 proof verbatim).

### 2.3 The trigger model — edges, levels, min-pulse, debounce, retrigger-lockout

The operator's exact ask, pinned in [ADR-0073](../decisions/ADR-0073.md), layered as a pure
value machine on top of `GpiPoint`:

- **Rising edge / falling edge** — `GpiPoint::sample` already reports `Edge::Rising`/`Falling`
  (`gpio.rs:54`). A rule's `kind: Edge(Rising)` matches a rising-edge cue.
- **High level / low level** — a steady logical-active or logical-inactive level (post-polarity).
  A *level* matcher fires while the level holds (rate-limited by `min_interval`), distinct from
  an *edge* matcher that fires once per transition. `GpiPoint::is_active()` (`gpio.rs:107`) is the
  level read.
- **Minimum pulse width** — a debounced edge is accepted only if the new level *persists* for
  `min_pulse` (reject glitches shorter than the configured width). A rising edge arms a
  pending-confirm timer; the cue is emitted only if the level still holds at expiry. Pure: a
  monotonic-time-stamped pending state, no wall-clock float.
- **Debounce window** — after an accepted edge, further raw transitions inside `debounce` are
  ignored (contact-bounce suppression). `rppal`'s hardware debounce is the first line; the value
  machine enforces a second, transport-independent debounce so behaviour is identical for an
  IS-07/network GPI that has no hardware debounce.
- **Retrigger lockout** — after a cue fires, the point is *locked out* from firing again for
  `lockout` (distinct from debounce: debounce filters bounce on one transition; lockout rate-limits
  *intentional* repeats, e.g. an operator mashing a button). A locked-out point still tracks level
  for accurate edge detection on unlock.

All four timing parameters are integer durations on the monotonic clock (never float seconds,
safety rule #6); the model is property-tested (a generated bounce/pulse/repeat sequence yields a
deterministic accepted-cue stream). GPO assert is the inverse: a `fire GPO` action (§6) calls
`GpoPoint::request(true/false)` and the node drives the pin, optionally for a pulsed duration.

### 2.4 Efficiency

A GPIO node holds O(pins) `GpiPoint`/`GpoPoint` `Copy` value structs (a few bytes each) and one
async-interrupt task per active input pin (`rppal` uses the `gpiochip` char device — no polling
busy-loop). Memory and CPU are negligible; the cue rate is human/automation rate (≪ frame rate).
No allocation per sample (the value machine is `Copy`); the only network traffic is one small
cue/ack message per accepted edge.

---

## 3. Ingress: SCTE-104 (TCP + SDI VANC) and SCTE-35 (ingest + emit)

Full decision record: [ADR-0074](../decisions/ADR-0074.md).

### 3.1 SCTE-104 ingest — TCP listener + the VANC path

SCTE-104 (ANSI/SCTE 104) is the *operational trigger* an automation system sends to an
encoder/inserter; the parser already exists and is pure
(`crates/multiview-input/src/scte/scte104.rs:161` `Scte104Message::parse`, decoding the
`multiple_operation_message` with `splice_request_data` op `0x0101` and `time_signal_request_data`
op `0x0104`, `:25-28`). Per the
[broadpeak SCTE-104 overview](https://www.broadpeak.io/scte-104-the-key-to-streamlined-broadcasting/)
and [SMPTE ST 2010](https://global.ihs.com/doc_detail.cfm?document_name=SMPTE%20ST%202010&item_s_key=00513497),
SCTE-104 reaches Multiview two ways:

- **TCP (network side)** — a control-plane **TCP listener** task in `multiview-control`
  (dual-stack `[::]`, IPv6-first; the automation system connects and streams
  `multiple_operation_message`s). Each message → `Scte104Message::parse` → `cue_events()`
  (`scte104.rs:242`) → `Cue`. The listener is a bounded best-effort consumer: a flood is
  drop-oldest, a slow/dead automation peer cannot stall anything (inv #10). This is the MVP path.
- **SDI VANC (baseband side)** — SCTE-104 mapped into VANC via SMPTE ST 2010 on DID/SDID
  `0x41/0x07`, carried in ST 291M ancillary data
  ([EEG/ai-media SCTE-104 inserter data sheet](https://www.ai-media.tv/wp-content/uploads/Data-Sheets_SCTE_104_Inserter.pdf)).
  Multiview has no SDI capture today; the VANC path is **deferred** behind whatever SDI/ST-2110
  ANC ingest lands (the `multiview-input/src/st2110/v40.rs` ST 2110-40 ANC reader is the natural
  home — it already parses ancillary data). The decoder is the **same** `Scte104Message::parse`;
  only the carriage differs. Pinned as a named follow-on, not silently dropped.

### 3.2 SCTE-35 ingest — observe the data PID already discovered

SCTE-35 (`splice_info_section`, `table_id 0xFC`) is parsed with CRC-32/MPEG-2 validation
(`crates/multiview-input/src/scte/splice35.rs`, `CMD_SPLICE_INSERT 0x05`/`CMD_TIME_SIGNAL 0x06`
at `:29-32`), and SCTE-35 PIDs are already discovered in the PMT
(`crates/multiview-input/src/mpegts/selection.rs` `scte35_pids`). The cue subsystem **observes**
the decoded section as it passes through ingest and emits a `Cue` (`SpliceOut` on out-of-network,
`SpliceIn` on return, `TimeSignal` for bare `time_signal`). This is pure observation — the engine
*samples* inputs, never paces to them ([decoupled-routing.md](decoupled-routing.md) §0), so reading
the SCTE-35 content adds no data-plane coupling. `segmentation_descriptor` detail (program/break
boundaries, the AWS/broadpeak-documented `time_signal` + descriptor pattern) is surfaced into the
`Cue.attributes` so a rule can match `BreakStart` vs `ProgramStart` (parsing the descriptor body is
a named extension of the existing `splice35.rs` decoder).

### 3.3 SCTE-35 / SCTE-104 emit

The symmetric ask: Multiview *originates* cues downstream. The re-emit primitive exists
(`SpliceInfoSection::reserialize_with_pts_adjustment`, `splice35.rs:309`, CRC-32 rewrite via the
existing `mpegts/crc.rs` helper). Emit comes in two flavours:

- **Emit SCTE-35 into the program transport** — an `emit SCTE-35` action builds a fresh
  `splice_info_section` (`splice_insert` with `out_of_network`/`break_duration`, or `time_signal`
  + `segmentation_descriptor`) and inserts it on a dedicated output PID in the MPEG-TS muxer
  (`multiview-output`). PTS is stamped from the **output tick counter** (inv #3 — never a raw
  input PTS); `pts_adjustment` continuity from [ADR-0034](../decisions/ADR-0034.md) is reused so a
  re-stamped passthrough keeps cue alignment. This is a *new builder* (the in-tree code is a
  decoder + a `pts_adjustment`-only re-serialiser, [decoupled-routing.md](decoupled-routing.md)
  §DATA notes "no SCTE restamp target / re-serialiser exists" for a full build).
- **Emit SCTE-104 upstream** (Multiview as an automation source to a downstream encoder) — a
  TCP client that sends a `multiple_operation_message`. Lower priority (Multiview is usually the
  encoder, not the automation system); pinned as a follow-on in [ADR-0074](../decisions/ADR-0074.md).

Emit is an **out-of-engine side effect** on the output/mux path (the packet is fanned with the
program, inv #7 encode-once-mux-many is untouched — SCTE-35 is data, not a re-encode), classified
Class-1 (§1.3). A cue rule can therefore *re-originate*: ingest a GPI → emit a downstream SCTE-35;
or ingest a SCTE-35 splice-out → fire a GPO and a webhook.

---

## 4. Ingress: HLS/DASH manifest cues (emit + react)

Full decision record: [ADR-0075](../decisions/ADR-0075.md).

### 4.1 React — manifest cues on ingested HLS/DASH

Per [RFC 8216](https://datatracker.ietf.org/doc/html/rfc8216) and the
[AWS Elemental EXT-X-DATERANGE](https://docs.aws.amazon.com/elemental-live/latest/ug/scte-35-ad-marker-ext-x-daterange.html)
/ [Flussonic markers](https://flussonic.com/doc/iptv/markers/) descriptions, HLS carries cues as:

- **`EXT-X-DATERANGE`** with `SCTE35-OUT=`/`SCTE35-IN=` attributes (hex-encoded raw SCTE-35
  bytes + `ID`/`START-DATE`) — the RFC 8216 §4.3.2.7.1 form, UTC-anchored. Per RFC 8216 the
  break-out tag carries `PLANNED-DURATION` (the *expected* break length); the actual `DURATION`/
  `END-DATE` belongs to the corresponding `SCTE35-IN` and must not be stamped on the initial
  break-out tag when the true end is not yet known. The emit path (§4.2) therefore models a cue's
  planned vs actual duration as distinct fields and generates compliant `OUT`/`IN` tags; and
- **`EXT-X-CUE-OUT:DURATION=…`** / **`EXT-X-CUE-IN`** — the older de-facto/vendor form
  (playlist-position-anchored), **not an RFC 8216 tag**; widely deployed for compatibility.

Multiview already ingests HLS and parses playlists (`crates/multiview-input/src/hls.rs`); the cue
decoder reads these tags during manifest refresh and emits a `Cue` (the `SCTE35-OUT` hex decodes
through the **existing** `splice35.rs` parser → the same `CueEvent`; a bare `CUE-OUT` yields a
`SpliceOut` with the carried `DURATION`). DASH carries cues as **inband `emsg` boxes** (signalled
by an MPD `InbandEventStream`) and **MPD `EventStream`** elements, with SCTE-35 binary in the
`emsg` message data
([dash.js event handling](https://dashif.org/dash.js/pages/usage/event-handling.html));
`crates/multiview-input/src/dash/xml.rs` is the MPD-parsing home. Reacting is pure observation
(inv #1/#10), identical posture to §3.2.

### 4.2 Emit — write cues into the output manifests

An `emit HLS manifest cue` / `emit DASH event` action writes the cue into Multiview's own output
packager (`multiview-output` HLS/LL-HLS segmenter, the [ADR-0037](../decisions/ADR-0037.md)/ADR-0032
segmenter lineage). On cue:

- **HLS** — insert `EXT-X-DATERANGE` (with `SCTE35-OUT`/`-IN` carrying the hex of the emitted
  SCTE-35 section from §3.3) **and/or** `EXT-X-CUE-OUT`/`-IN` (operator-selectable form, since
  downstream support varies — both are de-facto deployed) into the live media playlist at the
  current segment boundary. The manifest write is on the packager's own task (off the output
  clock); a manifest-write failure degrades that rendition's cue, never the program (inv #1).
- **DASH** — emit an inband `emsg` box on the segment and/or an MPD `EventStream` entry under the
  SCTE-35 binary `schemeIdUri` (`urn:scte:scte35:2013:bin`) so a DASH player and an SSAI gateway see
  it. The exact scheme URI(s), `emsg` payload encoding, and `timescale`/presentation-time derivation
  are pinned in [ADR-0075](../decisions/ADR-0075.md) §2 (to verify before implementation).

The emitted SCTE-35 bytes, the TS-PID SCTE-35 (§3.3), the HLS `DATERANGE`, and the DASH `emsg`
are **one logical cue rendered into every active transport** — the multi-transport analogue of
encode-once-mux-many: build the splice once, render it per packager. This keeps an ad-break
signalled consistently to a TS affiliate feed, an HLS OTT feed, and a DASH OTT feed from one cue.
The cue is *generated* once but each transport anchors its timestamp differently — TS SCTE-35 by
PTS, HLS `DATERANGE` by UTC `START-DATE`, DASH `emsg` by the segment timeline — so the single
`effective_at` (§1.2) is the common reference each packager renders into its own native
time-base; without that shared effective time the same cue could be consistently generated but
inconsistently timed across transports.

### 4.3 Efficiency / resilience

Reacting costs one extra tag-scan per manifest refresh (HLS) or one `emsg`/MPD parse per segment
(DASH) — bounded, on the existing ingest task, conflated to manifest cadence. Emitting adds a few
bytes per cue to a playlist/segment already being written. A broken/oversized manifest cue is
warned + skipped (the "manage the libav log + recover, never falter" posture of
[iso-program-recording.md](iso-program-recording.md)) — a malformed cue tag from a bad upstream
HLS source must **warn and recover**, never stall the other renditions (the operator's
ingest-all-streams-isolated principle).

---

## 5. Ingress: BXF / automation metadata

Full decision record: [ADR-0076](../decisions/ADR-0076.md).

BXF (Broadcast Exchange Format, [SMPTE ST 2021](https://www.smpte.org/standards)) is the
traffic/automation-system schedule exchange: an XML document describing programs, breaks, events,
and as-run logs, exchanged as files or over a SOAP/HTTP web service **(unverified — ST 2021
defines an XML schema and a web-service binding; exact element names are not reproduced here, the
spec is not redistributed)**. BXF is **slow, scheduled, out-of-band** metadata — minutes-ahead
program/break boundaries — not a real-time trigger. Its role in the cue bus:

- A **BXF poller** task in `multiview-control` (file-drop watch or SOAP poll, operator-configured,
  IPv6-first) parses the schedule into a list of **future** cues (`ProgramStart`/`ProgramEnd`/
  `BreakStart`/`BreakEnd` at a wall-clock time) and an **as-run** sink (Multiview can write back an
  as-run log of what actually fired).
- Future cues are scheduled by the **existing macro-sequencer wait-step mechanism**
  ([production-switcher §10.2](production-switcher.md)): a BXF break at T becomes a scheduled cue
  that, at T (sampled at the frame boundary, never paced), enters the dispatcher exactly like a
  live GPI. BXF therefore *populates* the cue bus ahead of time; it does not add a new dispatch path.
- The parser is pure XML→value (the `dash/xml.rs` posture); the SOAP/web-service transport is
  behind an off-by-default feature and a clean-room, nominative implementation of the published
  binding (the [managed-devices.md](managed-devices.md) vendor posture) — **no vendor SDK, no
  redistributed schema**. Legal/ToS caveat: ST 2021 is a paid SMPTE standard; the implementation
  is built from the public binding description and operator-supplied schemas, and the operator
  decides the proprietary-protocol stance.

BXF is the lowest-priority, last-wave ingress — the real-time triggers (GPIO, SCTE-104/35,
manifest) deliver the operator's "broadcast cues" ask; BXF closes the traffic-system loop.

---

## 6. The cue-action vocabulary — mapped onto existing verbs + new effects

Full decision record: [ADR-0072](../decisions/ADR-0072.md). The operator's requested actions, each
**mapped onto an existing surface** (left) or added as a thin new effect (right). A `CueAction` is
an internally-tagged enum in `multiview-core`; the dispatcher desugars each into the cited command
or effect.

| Cue action (operator ask) | Mapped onto (EXISTING unless noted) |
|---|---|
| **Run macro** | The control-plane macro sequencer ([production-switcher §10.2](production-switcher.md)); `POST /api/v1/macros/{id}/run` ([ADR-W021](../decisions/ADR-W021.md)). A cue rule submitting a macro is the canonical case. |
| **Cut / auto switcher** | `Command` cut/auto via `…/mix-effects/{id}/cut`·`/auto` ([ADR-W021](../decisions/ADR-W021.md)); BUILT frame-boundary drain. |
| **Take** (PVW→PGM) | The same cut/auto verb (a take is cut-with-flip-flop, [production-switcher §5.3](production-switcher.md)). |
| **Load/play/stop VT** | Media-player transport `…/media/players/{id}/load`·`/cue`·`/play`·`/pause`·`/stop` ([ADR-W021](../decisions/ADR-W021.md), [ADR-0057](../decisions/ADR-0057.md)). |
| **Show/hide lower third** | DSK on-air/off-air `…/downstream-keyers/{id}/on-air`·`/off-air` ([ADR-W021](../decisions/ADR-W021.md)); a lower-third is a DSK graphic. |
| **Set aux route** | `…/aux-buses/{id}/route` ([ADR-W021](../decisions/ADR-W021.md)) / the RT-12 output←program crosspoint ([multi-program.md](multi-program.md)). |
| **Fade audio bus** | Master-audio fade `PATCH /api/v1/switcher/audio {master_gain_db}` → `MasterEnvelope` ([ADR-0059](../decisions/ADR-0059.md) §3, [switcher-audio.md](switcher-audio.md)). |
| **Mute/unmute source** | `RouteIntent::Audio {mute}` / per-strip mute envelope ([ADR-0059](../decisions/ADR-0059.md) §4) — gain-preserving, Class-1. |
| **Start/stop recording** | Recorder arm/disarm `POST …/recording/arm`·`/disarm` ([ADR-0037](../decisions/ADR-0037.md) §7) — best-effort, never stalls. |
| **Emit SCTE-35** | §3.3 — build a `splice_info_section` and insert on the output data PID (new builder over the existing decoder/re-serialiser). |
| **Emit HLS/DASH manifest cue** | §4.2 — write `EXT-X-DATERANGE`/`CUE-OUT`/`-IN` (HLS) + `emsg`/MPD `EventStream` (DASH) in the packager. |
| **Fire GPO** | §2 — `GpoPoint::request(active)` on a GPIO node (`tally/gpio.rs:174`), optionally pulsed. |
| **Send WebSocket event** | A `cue.user` event on the existing realtime broadcast lane ([realtime-api.md](realtime-api.md)); a thin operator-authored event distinct from the §1.4 lifecycle telemetry. |
| **Call webhook** | The webhook notifier builder (`crates/multiview-control/src/notify/webhook.rs`) — already isolated off the request path (inv #10); generalise its `WebhookPayload` from alarm-only to a cue payload. |

Two principles hold the table together:

1. **Desugar, never fork.** A cue action that operates the switcher submits the *identical*
   `Command` a live operator or macro submits — so a cue can never do something the API cannot,
   and the live-apply classifier, idempotency, and correlation all apply unchanged
   ([ADR-W021](../decisions/ADR-W021.md)). The strongest expression is *"run macro"*: most complex
   cue responses are best authored as a macro and triggered by a one-line rule.
2. **Effects are bounded side-channels.** Emit-SCTE / emit-manifest / fire-GPO / webhook / WS run
   on the control-plane dispatch task (or the packager/mux task for the transport emits), never on
   the output clock, each on its own bounded best-effort path. A failing webhook or a dead GPIO
   node degrades that one effect with a warning; the show continues (inv #1/#10).

---

## 7. Efficiency / resilience

- **Memory.** O(rules) small value structs + O(GPIO pins) `Copy` points + one bounded drop-oldest
  cue queue (depth 1–3 per ingress, [realtime-api.md](realtime-api.md) "bound every queue,
  drop-oldest"). No per-cue heap growth on the steady path; SCTE/manifest decoders are the
  existing bounded-allocation parsers. BXF holds one parsed schedule window, pruned as cues fire.
- **CPU.** Cue rate is human/automation rate (≪ frame rate); decoding is the existing pure parsers
  (SCTE bit-readers, HLS tag scan, MPD parse). The dispatcher is an O(rules) match per cue. GPIO
  edge detection is interrupt-driven (`gpiochip`), not polled.
- **GPU/IO.** Zero GPU. IO is one TCP listener (SCTE-104), the existing manifest IO (HLS/DASH),
  one file/SOAP poll (BXF), and the existing managed-device channel (GPIO node). Emits add a few
  bytes to packets/manifests already being written.
- **Resilience / bad inputs.** A malformed SCTE section surfaces as the typed `ScteError`
  (`scte/mod.rs:30`) and is dropped + warned, never panicked (the parsers are already panic-free,
  bounded). A bad HLS cue tag warns + recovers without touching sibling renditions
  (ingest-all-streams-isolated). A flood of GPI bounces is absorbed by debounce + lockout (§2.3).
  A dead automation peer / webhook endpoint / GPIO node stalls only its own task (inv #10). The
  output clock is **never** in any cue path.
- **Overflow policy per cue/action class.** Blind drop-oldest is correct for telemetry but wrong for
  a *safety-paired* cue (dropping a `SpliceIn` after a `SpliceOut`, a `fire GPO off`, or a
  `stop recording` leaves a stuck state). The queue is therefore class-aware: paired/critical cues
  (out↔in, on↔off) **coalesce-or-replace** (the newest supersedes a stale same-key entry rather than
  evicting its pair), and every overflow drop increments a per-class counter that raises an operator
  alarm (the salvo/alarm posture). This stays bounded and `try_send`-based (inv #10) — it changes
  *which* entry is evicted on overflow, never adding an unbounded queue or an engine await.
- **Invariants.** #1: no cue decoder, dispatcher, or effect runs on or blocks the output-clock
  thread; emits that touch the mux/packager run on those subsystems' own off-clock tasks and
  re-stamp from the tick counter (inv #3). #10: every ingress→dispatcher→bus hop is bounded
  drop-oldest with `try_send`; the engine never awaits a cue producer or consumer. #6/#8: cues
  carry no pixels and never reorder color. IPv6-first: the SCTE-104 listener, BXF poller, GPIO
  node channel, and webhook all bind/connect dual-stack `[::]` with bracketed literals (conventions §10).

---

## 8. Open questions

1. **VANC SCTE-104 timing.** The SDI/ST-2110-40 VANC path (§3.1) depends on ANC ingest that does
   not exist yet (`st2110/v40.rs` parses ANC but there is no SDI capture). Is the network-TCP
   SCTE-104 path sufficient for the operator's plants, or is baseband VANC a near-term need? The
   decoder is shared either way; only the carriage is gated.
2. **`segmentation_descriptor` depth.** How much of the descriptor taxonomy (program/chapter/break/
   provider-ad/distributor-ad, the AWS-documented set) must the MVP match on? The MVP proposes
   surfacing the descriptor *type* into `Cue.attributes` and matching on it; full descriptor
   modelling is a sized extension of `splice35.rs`.
3. **HLS cue form preference.** `EXT-X-DATERANGE` (SCTE35-OUT/IN, UTC) vs `EXT-X-CUE-OUT/IN`
   (position) have different downstream support. Proposed: emit **both** by default, operator-
   selectable. Is there a deployment that breaks on seeing both? (de-facto practice says no, but
   it is unverified for every player.)
4. **BXF transport.** File-drop vs SOAP web service — which do the operator's traffic systems use?
   ST 2021 defines both; the file path is far simpler and is proposed for the MVP, SOAP deferred.
   Also: is as-run write-back required in v1, or read-only schedule ingest first?
5. **Emit authority / safety.** Emitting a SCTE-35 ad-break or a GPO closure has real downstream
   consequences (it can trigger an actual ad insertion or a tally lamp on someone's rig). Should
   emit actions require an explicit RBAC scope / confirmation, distinct from operating the local
   switcher? Proposed: yes — emit is an outward-facing action (CLAUDE.md §0 confirmation posture),
   gated behind an `emit` capability per [ADR-0072](../decisions/ADR-0072.md).
6. **`rppal` longevity.** `rppal` is retired for new features (2025-07-01). The design abstracts the
   GPIO HAL behind a trait; should the default backend be `rppal` (mature, works today) or the
   kernel `libgpiod`/`gpiod` char-device interface directly (future-proof, more code)? Deferred to
   the implementer; the cue logic is backend-agnostic.
7. **Cue→tally interaction.** A GPI is today modelled under `multiview-engine/src/tally/` (the
   tally/GPIO co-location). Should a cue rule and a tally GPI share one point registry, or are they
   distinct concerns that merely share the `GpiPoint` value type? Proposed: shared value type,
   distinct registries (tally GPI drives lamps; cue GPI drives actions) — but an operator may want
   one pin to do both.
