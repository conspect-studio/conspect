# ADR-M012: Switcher management resource model — additive config blocks, desired/live state split, warm-restart snapshot, and Hot/Reset-lite/Class-2 apply classification

- **Status:** Proposed
- **Area:** Management
- **Date:** 2026-06-11
- **Source briefs:** [production-switcher.md](../research/production-switcher.md),
  [media-playout.md](../research/media-playout.md)
- **Extends:** [ADR-M005](ADR-M005.md) (Class-1/reset-lite/Class-2 + dry-run plan),
  [ADR-M006](ADR-M006.md) (config-as-code round-trip), [ADR-0010](ADR-0010.md)
  (declarative schema, tagged-never-untagged), [ADR-0034](ADR-0034.md) (decoupled
  routing — the additive-block precedent this ADR copies)
- **Relates to:** [ADR-0054](ADR-0054.md) (switcher architecture),
  [ADR-0055](ADR-0055.md) (transitions), [ADR-0056](ADR-0056.md) (keyers),
  [ADR-0057](ADR-0057.md) (media library/players), [ADR-0058](ADR-0058.md)
  (alpha media), [ADR-0059](ADR-0059.md) (switcher audio),
  [ADR-W021](ADR-W021.md) (control surface), [ADR-RT008](ADR-RT008.md)
  (realtime), [ADR-T015](ADR-T015.md) (timing), [ADR-0030](ADR-0030.md)
  (programs / MP-5), [ADR-R010](ADR-R010.md) (make-before-break),
  [ADR-M008](ADR-M008.md) (the Device desired-state precedent),
  [ADR-W017](ADR-W017.md) / [ADR-W018](ADR-W018.md) (bare verbs, live-apply header)

## Context

The production-switcher layer ([production-switcher.md](../research/production-switcher.md))
adds a family of new management resources — mix/effects stages, downstream keyers,
aux buses, transition presets, a media library, media players, and macros — to a
config crate that has **none** of this vocabulary today (verified by survey: no
M/E, bus, transition, keyer, media or macro type anywhere in
`crates/multiview-config/src`). What the crate does have is a proven, repeatable
additive pattern this ADR deliberately copies rather than re-deciding:

- **The root is extensible.** `MultiviewConfig` is `#[non_exhaustive]` with
  `#[serde(default)]` resource collections
  (`crates/multiview-config/src/lib.rs:114-179`, BUILT). The `[routing]` block is
  the template for a block that supersedes legacy fields: an absent table
  **desugars** from `cells`/`audio`/`outputs`
  (`MultiviewConfig::desugared_routing_table`, `lib.rs:718`) and a present table
  is checked for **consistency** with the legacy bindings, rejecting an
  inconsistent both-populated document (`validate_routing`, `lib.rs:780`, with the
  rejection at `lib.rs:799-816`).
- **Unions are internally tagged, never `untagged`** (`#[serde(tag = "kind")]`,
  ADR-0010): `SourceKind` at `crates/multiview-config/src/schema.rs:215-217` is
  `#[non_exhaustive]`, so new variants are additive.
- **Validation is two-level**: per-item `validate()` plus document-level
  `validate_*()` cross-reference passes called from `MultiviewConfig::validate`
  (`lib.rs:284-305`), first-violation `ConfigError` strings, typed variants for
  capability refusals (the `ConfigError::AudioCapability` precedent).
- **Desired vs runtime state is already split once**: a `Device` carries
  **desired state only**; runtime status "has no representation in this model"
  and is never exported (`crates/multiview-config/src/device.rs:1-14`, ADR-M008,
  BUILT end-to-end with the control-plane `DeviceStatusRegistry` live mirror,
  `crates/multiview-control/src/devices/registry.rs:25`).
- **Reserved well-known names exist as consts**: `MAIN_PROGRAM = "main"`
  (`crates/multiview-config/src/routing.rs:231`) and `PROGRAM_TRACK = "prog"`
  (`crates/multiview-config/src/audio.rs:292`).

Verified gaps the new schema must close (not work around):

1. **MP-5 is unlanded** (DOC-ONLY): `ProgramId`/`ProgramSpec` exist and drive the
   single `"main"` program, but the `programs: Vec<ProgramSpec>` schema root,
   its desugar, and per-program cross-validation are explicitly deferred
   (`crates/multiview-config/src/program.rs:16-20` and `:167-169`). Consequently
   `validate_output` checks only that an output crosspoint's `program` string is
   **non-empty** (`crates/multiview-config/src/routing.rs:434-441`) — a document
   routing an output to a program that does not exist validates today.
2. **`SourceKind::File` carries only `path`** (`schema.rs:341-344`) — no loop,
   in/out marks, or end-of-file policy; no still-image kind; no way for a bus or
   cell to reference another program's output.
3. **Overlays are untyped**: free-string `kind` + verbatim
   `serde_json::Map` params (`schema.rs:664-677`) — lossless round-trip, but
   nothing a validator, capability matrix, or UI form can reason about.
4. **Core `Cell` crop/rotation are dead fields** (TYPE-LEVEL/SCAFFOLD): typed and
   validated in `crates/multiview-core/src/layout.rs:137-144` but the config
   mapper "does not yet surface them" (`lib.rs:543-546`) and no composite path
   consumes them (zero hits in `crates/multiview-engine/src/drive.rs`).

On the control side the domain plumbing to copy is BUILT: the generic versioned
resource store (`crates/multiview-control/src/resource_store.rs:191`,
monotonic `Version` → `ETag`/`If-Match` → 412), `TypedCollection` typed-body
validation returning 422 with the `serde_path_to_error` field path
(`crates/multiview-control/src/typed_resources.rs:60`, `:198`) and the
`x-multiview-apply` header (`typed_resources.rs:24`), config seeding
(`seed_resources`, `crates/multiview-control/src/state.rs:205`), and the
classify-before-apply routing pattern: `RouteClass{Class1, ResetLite, Class2}`
(`crates/multiview-control/src/routing.rs:43-51`) surfaced via
`POST /api/v1/routing/plan` and `/routing/{kind}/take`
(`crates/multiview-control/src/routes/routing.rs:107`, `:140`).

## Decision

### 1. Four new additive top-level blocks, exactly the routing precedent

Add to the `#[non_exhaustive]` `MultiviewConfig` root, all `#[serde(default)]`,
all absent-means-no-switcher (a v1 document with none of them parses, validates,
and runs unchanged; `schema_version` stays `1`, the
[ADR-0027](ADR-0027.md)/[ADR-0047](ADR-0047.md) additive precedent):

- **`switcher`** (`Option<SwitcherConfig>`): `mix_effects: Vec<MixEffects>`
  (per-M/E: id, program binding, flip-flop flag, default transition,
  upstream-keyer definitions per [ADR-0056](ADR-0056.md)),
  `downstream_keyers: Vec<DownstreamKeyer>`, `aux_buses: Vec<AuxBus>`,
  `transitions: Vec<TransitionPreset>` (the [ADR-0055](ADR-0055.md) taxonomy),
  `wipe_patterns: Vec<WipePattern>`, and `compositions: Vec<Composition>`
  (bus-selectable multi-box compositions, ADR-0056 §multi-box).
- **`media_library`** (`Option<MediaLibrary>`): asset declarations — stills,
  clips, audio — with import/validation metadata per [ADR-0057](ADR-0057.md) /
  [ADR-0058](ADR-0058.md).
- **`media_players`** (`Vec<MediaPlayer>`): player channels (bus-selectable
  sources) distinct from the library, with cue/loop/EOF-policy desired state
  per ADR-0057.
- **`macros`** (`Vec<MacroDef>`): id + ordered steps, each step an
  internally-tagged action referencing declared resources, plus integer wait
  steps (`wait_frames`, or `wait_ms` converted via exact rationals per
  [ADR-T015](ADR-T015.md)); the control-plane sequencer that replays them is the
  control plane's
  ([production-switcher §10.2](../research/production-switcher.md); the
  realtime/`corr` shape is [ADR-RT008](ADR-RT008.md)'s, the run verb
  [ADR-W021](ADR-W021.md)'s).

Every union inside these blocks is **internally tagged** (`tag = "kind"`/`"mode"`,
never `untagged`), every struct `#[non_exhaustive]`, every duration an **integer
frame count at the output cadence** (the wire form may accept milliseconds;
conversion is exact-rational, never float — inv #3). Where a new block overlaps a
legacy field (none does at introduction; aux-bus output binding reuses the
existing `OutputCrosspoint` rather than duplicating it), the routing rule applies
verbatim: desugar the legacy form, reject an inconsistent both-populated document
(`lib.rs:780-816` is the template).

### 2. MP-5 is the prerequisite slice

The switcher program lands `programs: Vec<ProgramSpec>` as a schema root first —
the desugar (absent ⇒ the legacy top-level `canvas`/`layout`/`cells`/`outputs`
block synthesizes the single `"main"` program, exactly what the CLI does in code
today) plus document-level cross-validation that **every `OutputCrosspoint.program`
names a declared program**. This closes the verified gap in `validate_output`
(`routing.rs:434-441`, non-empty-only) before any switcher resource references a
program, and gives M/E program bindings and aux-bus encodes (ADR-0054's
build-vs-route split) a validated anchor. `ProgramId` stays the one validated
newtype (trimmed, non-empty, `TryFrom` — `program.rs:45`); all other new ids
follow the repo convention: free-form, non-empty, unique strings.

### 3. New `SourceKind` variants and the `File` extension (additive)

`SourceKind` is `#[non_exhaustive]` and internally tagged, so these are additive:

- **`Still { path }`** — decode-once, hold-forever (ADR-0057; the EOF-hold path
  proves the semantics).
- **`MediaPlayer { player }`** — binds a cell/bus to a declared media-player
  channel; validated against `media_players[]`.
- **`Program { program: ProgramId }`** — program re-entry (a program's output as
  a source), validated against `programs[]`.
- **`File`** grows optional transport fields: `loop` (bool), `in`/`out` marks
  (integers, exact-rational conversion per ADR-T015), and an EOF policy
  `{hold_last_frame | loop | black | auto_off}` (default preserves as-built
  behaviour: play once, hold last frame). Transport *semantics* are ADR-0057's;
  the schema carries only desired state.
- **Per-source `audio` attributes** (the [ADR-0059](ADR-0059.md) §2 schema
  placement): an AFV mode `afv = "fade_with_transition" | "hard_cut"`
  (default `fade_with_transition`), alongside the existing `AudioRouting`
  `gain_db`/`mute` fields the pipeline finally reads at build per
  [ADR-0059](ADR-0059.md) §4. Desired state only; live changes ride the audio
  control seam.

`is_synthetic()` (`schema.rs:385-392`) remains exactly the in-process synthetic
set — the new kinds are **not** synthetic; their live-apply classification is
pinned per-operation in §7 and surfaced via the ADR-W018 `x-multiview-apply`
header, with rows in the
[management-capability-matrix](../research/management-capability-matrix.md).

Document-level `validate_switcher()` performs the cross-reference pass (every
bus selector, keyer fill/key, transition `dip_source`/`border_fill_source`,
stinger player, macro step target, and `Program`/`MediaPlayer` source reference
names a declared resource) **plus a cycle check** over the composition/M-E
re-entry graph: compositions may nest and programs may re-enter, but a document
in which program A keys program B over program A (or a composition transitively
contains itself) is rejected at validation, not discovered at render time.

### 4. Surface the dead core `Cell` crop/rotation fields

Add `crop` and `rotation` to the config `Cell` and map them onto the
already-typed, already-validated core fields (`layout.rs:137-144`), deleting the
"does not yet surface them" default-spread at `lib.rs:543-546`. This is a small
additive schema change shipped with (or ahead of) the Lane-B DVE work that
finally wires them into the render path (ADR-0055 Wave 2) — schema and render
wiring land per the same-push rule, never schema-only as a new dangling field.

### 5. Typed graphics/keyer layer model; verbatim-Map demoted to compat escape

All **new** switcher surfaces (upstream/downstream keyer definitions, garbage
mattes, fill/key bindings, composition boxes) are fully typed structs with
per-item `validate()` — never free-string `kind` + verbatim params. The existing
untyped `Overlay` (`schema.rs:664-677`) is retained unchanged as the
compatibility escape for the monitoring-overlay stack (labels, meters, captions
— ADR-0054 keeps those off-clock); it is **not** the vehicle for any keyer or
switcher graphic, because untyped params cannot be 422-validated, classified, or
rendered into UI forms.

### 6. Reserved bus-name tokens

Following the `MAIN_PROGRAM`/`PROGRAM_TRACK` precedent, bus selectors get
reserved const tokens — `program` (post-FTB), `clean` (pre-DSK), `preview`,
`me<n>`, `aux<n>` (the ADR-0054 tap taxonomy) — and `validate_switcher()` rejects
user-declared ids that collide with a reserved token, so a selector string is
unambiguous forever.

### 7. Desired state ONLY in config; live state engine-owned and mirrored

The Device precedent (`device.rs:1-14`) is binding for the whole switcher
domain. **In config (desired state):** M/E definitions, keyer definitions,
transition presets, wipe patterns, aux-bus definitions, media library/player
declarations, macro definitions, initial crosspoints. **Never in config (live
state):** current bus crosspoints, T-bar position, transition phase/progress,
keyer on-air flags, FTB level, media-player transport position/state, macro
execution state. Live state is owned by the engine's switcher state machine
(applied at the frame-boundary control seam,
`crates/multiview-engine/src/runtime.rs:429-440`; [ADR-0054](ADR-0054.md)) and
mirrored **read-only** into the control plane via latest-wins registries plus
events (the `DeviceStatusRegistry` pattern, [ADR-RT008](ADR-RT008.md)). A config
export (ADR-M006) therefore only ever emits what was authored — it can never
freeze a half-completed transition into a committable file. Mutating live state
goes exclusively through the bounded command bus drained at the frame boundary
(inv #10); nothing in this schema can pace the engine (inv #1).

### 8. Warm restart is an explicit, separate, persisted snapshot resource

Off by default, control-plane-owned, and **never part of `MultiviewConfig`**:
an opt-in `warm_restart` setting enables a persisted snapshot of recallable live
switcher state (crosspoints, keyer on-air, FTB engaged, media-player
cue points — not mid-transition phase, which restores as *completed*), captured
debounced on change and on shutdown. On startup, if enabled and a snapshot
exists, the **control plane replays it as ordinary commands** after the engine
reaches steady state — the same arm/take batch shape as the engine salvo value
machine (BUILT: `crates/multiview-engine/src/salvo.rs:158-189`) — so there is
exactly one apply path and no engine-side restore mode. Absent or disabled, a
restart comes up in the config-declared initial state. The snapshot is excluded
from config export and version history (ADR-M006 scope is authored config only).

### 9. Every operation classified, with `/plan` dry-run (inv #11)

The routing plan/take split (BUILT: `routes/routing.rs:107/:140`,
`RouteClass` at `routing.rs:43-51`) extends to every switcher take per
[ADR-M005](ADR-M005.md): a `/plan` dry-run returns the classification before any
state-changing apply; immediate Class-1 returns `200 {class, applied}`,
timed/Class-2 returns `202` + operation id ([ADR-W021](ADR-W021.md) owns paths;
this ADR pins the classes):

| Operation | Class | Notes |
|---|---|---|
| Desired-state CRUD (presets, wipe patterns, keyer defs, macros, library entries) | **Hot** | no engine contact until referenced; takes effect at next arm/run |
| PVW crosspoint set | **Hot** | O(1) rebind precedent (`rebind_cell`) |
| Cut / direct program punch | **Hot** | immediate, 200 |
| Auto transition / FTB engage | **Hot (timed)** | 202 + completion event ([ADR-RT008](ADR-RT008.md)) |
| T-bar absolute progress | **Hot** | conflated latest-wins, never one command per sample |
| DSK on-air/off-air/tie; USK arm; keyer trim (clip/gain/matte) | **Hot** | frame-boundary state, immediate 200 |
| DSK auto | **Hot (timed)** | 202 + completion event (`keyer.on_air`/`keyer.off_air`, [ADR-RT008](ADR-RT008.md)) |
| Program master gain / master mute | **Hot** | per-sample envelope ramp ([ADR-0059](ADR-0059.md) §3); REST path [ADR-W021](ADR-W021.md) |
| Media player load/cue | **Hot, async readiness** | 202 until primed; decode spawns off-thread (ADR-W018 `LiveSourceHub` pattern) |
| Media player play/pause/seek/loop | **Hot** | |
| Macro run; memory/snapshot recall | **Hot** | salvo-shaped batch at one frame boundary |
| M/E / DSK / PVW-bus structural add/remove | **Hot via make-ready-then-swap** | allocation off-thread, swap at a frame boundary, admission-gated; demotes per ADR-M005 only if canvas format pins are touched |
| Aux-bus add (new independent encode) | **Hot for existing outputs, admission-gated spin-up** | 202; existing feeds untouched |
| Aux-bus source selection (crosspoint within the aux bus's own composite) | **Hot** | same rendition — the aux encoder keeps running; content re-points at a frame boundary (the `rebind_cell` precedent) |
| Aux output re-point between taps/programs | **Reset-lite** (IDR-aligned splice, RT-12) or **Class-2** (format differs ⇒ [ADR-R010](ADR-R010.md) make-before-break) | the encoded stream the output consumes changes |
| Canvas resolution/cadence/pixfmt change | **Class-2** | inherited verbatim from ADR-M005 |

**Memory (snapshot) recall scope — wire shape pinned here.** Memories extend
the BUILT salvo arm/take surface
(`crates/multiview-engine/src/salvo.rs:158-189`; verbs in
[ADR-W021](ADR-W021.md)) with a recall-scope mask: the recall body carries
`{scope: {sources: bool, keyers: bool, transition: bool, audio: bool}}`, each
defaulting to `true` (an absent mask is a full recall). A masked-out family's
live state is untouched by the recall, and the whole recall still lands as one
frame-boundary batch (the salvo-shaped batch in the table above). `transition`
covers the armed next-transition set and transition settings. The
capability-matrix row references this shape.

Capability-matrix rows for every entity/parameter pair land in
[management-capability-matrix.md](../research/management-capability-matrix.md)
(same design pass, separate change); the classes above are the binding input to
those rows.

### 10. Control-plane plumbing: one pattern, every collection

Each new collection (mix-effects, downstream-keyers, aux-buses, transition
presets, wipe patterns, compositions, media library, media players, macros) gets
the sync-groups domain file-set verbatim: a `ResourceKind` marker over the
versioned store (`resource_store.rs:191`; ETag/If-Match → 412, per-object BOLA),
a `TypedCollection` variant so every POST/PUT body 422s with the
`serde_path_to_error` field path (`typed_resources.rs:60/:198`), the
`x-multiview-apply` header (`typed_resources.rs:24`), `seed_resources` seeding
from the loaded config (`state.rs:205`), and OpenAPI registration — REST shapes
in [ADR-W021](ADR-W021.md).

Example (TOML authoring form; IPv6-first per conventions §10 — network examples
bracket IPv6 literals):

```toml
[[sources]]
id = "cam1"
kind = "rtsp"
url = "rtsp://[fd00:db8::21]:8554/cam1"

[[media_players]]
id = "mp1"
eof = "hold_last_frame"

[switcher]
[[switcher.mix_effects]]
id = "me1"
program = "main"
flip_flop = true

[[switcher.transitions]]
id = "house-mix"
kind = "mix"
rate_frames = 25        # integer frames at the output cadence (inv #3)
```

## Rationale

- **Copying the routing precedent is the lowest-risk schema move.** The
  desugar + both-populated-rejection shape is BUILT, property-tested, and proven
  to keep v1 documents routing identically (`lib.rs:718/:780`); four new
  defaulted blocks on a `non_exhaustive` root cannot disturb an existing
  document, and tagged unions round-trip TOML↔JSON robustly (ADR-0010).
- **MP-5-first turns a latent inconsistency into a validated anchor.** Every
  switcher resource references programs; landing the `programs` root with
  crosspoint cross-validation first means the new domain never inherits the
  unvalidated-program-string hole (`routing.rs:434-441`).
- **The Device precedent already answered the state-placement question.**
  Desired-only config keeps ADR-M006's GitOps story honest: exports are
  authored intent, reproducible and committable; live bus state in a config file
  would make every export a race against the T-bar. Warm restart as a separate
  opt-in snapshot gives operators continuity without contaminating the document
  model — and replaying it as ordinary commands preserves the single
  frame-boundary apply path (inv #1/#10).
- **Typed layers are the price of classification.** The capability matrix, the
  `/plan` classifier, 422 field paths, and UI forms all need machine-readable
  fields; the verbatim-Map overlay was the right call for lossless round-trip of
  monitoring overlays but cannot carry an on-air keyer.
- **Classes are pinned here so every surface agrees.** ADR-M005 demands the
  operator always knows whether an action is seamless before committing; pinning
  Hot/Reset-lite/Class-2 per operation in the resource-model ADR makes the REST
  layer (W021), matrix rows, and SPA all derive from one table.

## Alternatives considered

- **Live switcher state in config** (current crosspoints, T-bar, keyer on-air
  persisted in `MultiviewConfig`). Rejected: violates the Device precedent
  (`device.rs:1-14`), poisons config export/rollback (ADR-M006 — a rollback
  would "recall" a stale T-bar position as if authored), and creates a second
  authority over state the engine owns at the frame boundary. Restart semantics
  are served explicitly by the §8 snapshot instead.
- **One mega-resource** (a single `/api/v1/switcher` document holding all M/Es,
  keyers, players, macros). Rejected: defeats per-collection ETag/If-Match
  concurrency (one hot document = constant 412 churn between operators), defeats
  per-object BOLA scoping, and makes 422 field paths and audit granularity
  useless. The store/route machinery is generic — per-collection cost is one
  marker type each.
- **UUID-enforced ids** for the new resources. Rejected: every existing resource
  uses free-form non-empty unique strings (the only UUID anywhere is the GPU
  `DevicePin.stable_id`); operators and control surfaces want human-stable ids
  (`me1`, `dsk-bug`); `ProgramId`'s trimmed-newtype pattern covers the cases that
  key engine-side maps.
- **A `schema_version` bump + version-gated fields.** Rejected: all additions are
  defaulted and additive; the crate enforces only `schema_version >= 1`
  (`lib.rs:285-289`) and has no migration machinery — inventing one for a purely
  additive change is cost without benefit ([ADR-0027](ADR-0027.md)/[ADR-0047](ADR-0047.md)
  precedent).
- **Engine-owned warm-restart restore** (engine persists and reloads its own
  switcher state). Rejected: gives the engine filesystem I/O and a second
  startup path on the protected core, and a restore mode that bypasses the one
  command-drain apply seam — exactly what inv #10 exists to prevent.
- **Extending the untyped `Overlay` for keyers** (new `kind` strings + params).
  Rejected: unvalidatable, unclassifiable, and invisible to the capability
  matrix; keeps working only until the first on-air incident caused by a typo'd
  param key.

## Consequences

- Four new config blocks, three new `SourceKind` variants, a `File` extension,
  and surfaced `Cell` crop/rotation ripple to: per-item + `validate_switcher()`
  validation, `TypedCollection` arms, OpenAPI/AsyncAPI regeneration and the SPA
  generated types, TOML/JSON round-trip tests (`tests/roundtrip.rs` pattern),
  and docs/examples. All additive — existing documents parse and route
  identically (property-tested, the routing desugar precedent).
- MP-5 stops being deferrable: it is the first slice of the switcher schedule
  (Lane A), and its crosspoint→program validation is a behavioural tightening —
  a previously-"valid" document naming a nonexistent program now fails
  validation. This is the intended fix of a verified hole, called out in release
  notes; the desugared single-`"main"` path is unaffected.
- The desired/live split means GET on a switcher collection shows authored
  intent while live bus state arrives via the read-only mirror + events
  (ADR-RT008); clients must not expect a PUT on desired state to move the
  T-bar. The capability matrix and in-app docs must state this explicitly.
- The warm-restart snapshot is a new persisted artifact with its own lifecycle
  (off by default; bounded; excluded from export/versioning); enabling it is an
  explicit operator decision, and its replay-as-commands startup is observable
  on the event stream like any other operation.
- Reserved bus tokens constrain the id namespace forever; collisions fail
  validation with a clear message. Choosing them now (before any GA surface)
  is the cheap moment.
- Per-collection CRUD multiplies OpenAPI surface and `rest_routes` table
  entries; the cost is mechanical (the sync-groups file-set is a template) and
  buys uniform concurrency, audit, and 422 behaviour for free.
- Classification discipline binds the implementation: every new verb must ship
  with its `/plan` classification and its matrix row, and a take that cannot be
  classified is a design bug, not a runtime surprise (inv #11).
