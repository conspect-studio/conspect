# Multiple active programs (multi-program engine)

> Status: design brief feeding ADR-0030. Verification-hardened against the as-built
> code (file:line cited inline). Where the text *proposes* new surface it says so and
> marks confidence. Where the code already does the thing, the code wins.

## 0. The problem

Today Multiview runs **exactly one program**: one fixed-cadence output clock drives one
composited NV12 canvas per tick (`out_pts = f(tick)`), encoded **once**, fanned to N
transports (encode-once-mux-many, invariant #7). That single program is owned end-to-end
by `Pipeline` in `crates/multiview-cli/src/pipeline.rs` — one `cadence` (`pipeline.rs:228,377`),
one `Arc<Layout>` (`pipeline.rs:954`), one source-store map (`pipeline.rs:425`,
`ingest_plans`), one `OutputClock` + `CompositorDrive` + `StreamEgress` built inline in
`drive_streaming` (`pipeline.rs:951–1027`). The config root models exactly one
`canvas`+`layout`+`sources`+`cells`+`outputs` (`crates/multiview-config/src/lib.rs:89–134`).

The operator now needs the engine to run **several concurrent output pipelines**, each
independently startable/stoppable, where each *program* is one of:

- **(A) Multiview** — composite many sources into a canvas (today's behaviour).
- **(B) Guarded passthrough** — one source straight to an output, **remux with no re-encode
  while healthy** (no decode, no encode, no composite), but **fails to a pre-baked BLACK /
  SMPTE-bars + 1 kHz-tone slate on any disruption** (invariant #1 preserved — §3, ADR-0030 §4).
- **(C) Transcode** — one source decoded then re-encoded to a different
  codec/res/bitrate/container.

Multiple may run at once: a 3×3 multiview→HLS, a direct RTSP→SRT passthrough, and a
720p RTMP transcode of one camera — simultaneously, on commodity hardware. **Efficiency
is the product**: a source used by several programs/tiles is decoded **once**
(decode-once-use-many); a **healthy** passthrough does **zero** decode/encode (failing to a
pre-baked slate on loss, at packet-copy steady-state cost — §3).

## 1. The unified Program model

A **Program** is one self-contained, independently-supervised, independently-stoppable
output pipeline = **one placement island** (ADR-0017/0018: the whole
`decode→composite→encode` island lands on one GPU; `select.rs:196` `PipelineDemand`).
Modelled as a tagged enum over three kinds, all sharing a process-global source layer:

| Kind | Pipeline graph | Output clock? | Encode? | Decode? |
|------|----------------|---------------|---------|---------|
| **A Multiview** | sample shared `TileStore`s → `CompositorDrive::compose(tick)` → `ProgramEncoder` → `StreamEgress` fan-out | **Yes** — own `OutputClock` at the program's fps | once / canvas | shared, per source |
| **C Transcode** | shared decoded source → [scale to target] → `ProgramEncoder` → `StreamEgress` fan-out | **Yes** — own `OutputClock`; PTS re-stamped from tick (#3) | once / rendition | **shared** with any tile of the same source |
| **B Guarded passthrough** | own `Demuxer` → `GuardedPacketSource` (copy ∥ pre-baked slate) → mux sinks | **Yes (degenerate)** — compositor-less clock paces the liveness decision; copy is source-paced (§3) | **none** while healthy (slate baked once at start) | **none** while healthy |

Kinds A and C share the **identical encode tail** (`ProgramEncoder` → `StreamEgress` → N
`PacketMuxSink`s) that is already built and tested (`pipeline.rs:1027`, fan-out at
`pipeline.rs:1659–1741`; ADR-0025/0026). Kind B reuses the **same per-sink mux consumer**
(`PacketMuxSink`, `Muxer::add_stream_from_parameters` at `mux.rs:136` =
`avcodec_parameters_copy`) but with a packet *producer* fed from `Demuxer::read_packet`
(`demux.rs:196`) instead of an encoder — the fan-out *shape* is unchanged, only the
packet source differs. **The remux primitives already exist; there is no packet-copy
consumer wired today** (confidence: high — `build_outputs` at `pipeline.rs:2988` only
knows encoder-fed sinks; `rg "remux|passthrough|-c copy"` over the pipeline returns
nothing).

## 2. N-concurrent-programs engine architecture

### 2.1 Per-program output clocks — NO master clock (invariant #1, per program)

`EngineRuntime` is **already per-program-shaped and isolation-clean**: it owns exactly
one `clock`, one `time_source`, one `seed_nanos` read at construction
(`runtime.rs:173–193`), one `ticks_emitted` counter (`runtime.rs:180,437`), and its loop
(`run_inner`, `runtime.rs:370–439`) awaits **only** the pacer deadline
(`runtime.rs:405`) — never an input, never a consumer, never another runtime. **Nothing
in `EngineRuntime` assumes it is the only one in the process** (verified: `OutputClock`
is constructed only in `Pipeline::drive_streaming` `pipeline.rs:951` and in tests). There
is no global/master clock today.

**Decision (confidence: high): one independent `OutputClock` + `EngineRuntime` per
clocked program (A/C); all programs share ONE read-only `Arc<dyn TimeSource>`
(`MonotonicTimeSource`).** Rationale:

- Different cadences (25 vs 60 fps) require independent exact-rational tick counters
  (`out_pts = f(tick)`, invariant #3 — *never float fps*). A master tick cannot serve
  both without resampling one, violating #3.
- A master clock is a shared fault surface. Per-program clocks make invariant #1's
  "one program stalling NEVER stalls another" **structural** (each runtime awaits only
  its own pacer) rather than merely careful.
- A shared `TimeSource` gives a common monotonic reference (read-only, wait-free
  `now_nanos()` at `runtime.rs:193`) for cross-program A/V alignment and future PTP
  discipline, **without** a shared *pacing* dependency. The correct reading of
  "master clock vs per-program clocks" is: **shared time *source*, independent output
  *clocks*.**

### 2.2 `Program` actor + `ProgramSet` supervisor

- **`Program` is the actor** (one per output island): owns its own clock + egress +
  `StopSignal` (`runtime.rs:115`, already `Send+Sync`, checked once/tick at
  `runtime.rs:386`) + outbound `EnginePublisher` (per-program isolated state), and runs
  on its own `tokio` task. `MultiviewProgram` is **literally today's
  `Pipeline::drive_streaming` body** (`pipeline.rs:930–1192`) refactored so the
  per-program pieces are *owned by the struct* — a move, not a rewrite, preserving every
  existing invariant test and the `DropOnOverload`/bounded-teardown machinery.
- **`Program` implements `Actor`** (`supervisor.rs:165`, `fn name()` + async run-shaped
  loop) so the **existing `Supervisor` + `RestartPolicy`** (`supervisor.rs:181,61`,
  capped backoff) restarts a *crashed program* with bounded backoff while every **other**
  program keeps emitting. Failure is contained to one task (confidence: high — the
  `Actor`/`Supervisor` pattern already exists and is decoupled from any clock by design).
- **`ProgramSet` is the supervisor/coordinator** — the only thing that knows there are
  many. It owns the shared `Arc<dyn TimeSource>`, the `SourceRegistry` (§4), the
  `PlacementController` (`placement.rs`), and the `ProgramId → ProgramHandle` map.
  `start(spec)` admits (placement `select_device`, `placement.rs:57`), constructs the
  program (which reads its own seed from the shared time source), and `tokio::spawn`s it —
  **other programs are untouched, no shared lock on their data plane**. `stop(id)` raises
  that program's `StopSignal` only; its loop returns after its current tick, its
  `StreamEgress` drains + finalises within the bounded `SINK_WEDGE_GRACE`
  (`pipeline.rs:130`, `1155–1172`); its task joins. **No other program's clock is
  touched.**

### 2.3 Isolation (invariant #10, PER program)

The **only** shared state across programs is: (a) the read-only `Arc<dyn TimeSource>`,
(b) the `SourceRegistry`'s lock-free per-source `TileStore`s (read via wait-free
`ArcSwapOption::load`, `latest.rs:34,92` — readers never block the writer or each
other), and (c) the off-hot-path `PlacementController` (sampling only). **None lets one
program back-pressure another's tick loop.** Each program's egress is its own
`DropOnOverload` consumer with `LIVE_QUEUE_CAP=4` (`pipeline.rs:113,148`); control/preview
tap each program via its own `EnginePublisher` (drop-oldest, wait-free).

**Required new CI chaos gate** (mandated by the guardrails for any new engine→outside
relationship): **wedge program B's egress and assert program A's `ticks_emitted`
(`runtime.rs:225`) keeps advancing on cadence.** Because each `StreamEgress` is
per-program and already `DropOnOverload`, this isolation falls out of the
per-program-egress design — the gate proves it.

## 3. Guarded passthrough and invariant #1 (PRESERVING, not excepting)

A naive remux has no encoder, so on input loss it can fabricate nothing — which is why
**naive packet-copy is rejected as a floor** (it would violate the operator's hard rule:
*any disruption fails to BLACK or SMPTE-bars + 1 kHz tone*). **Guarded passthrough makes
the byte copy invariant-#1-PRESERVING** without paying a steady-state encode. The full,
adversarially-verified design (12 load-bearing claims, 8 held / 4 refined) is in
**ADR-0030 §4 + the robustness ladder**; the essentials:

> **A passthrough is a degenerate, compositor-less `EngineRuntime`** ticking at the
> program's cadence, gated only on its own `pacer.wait_until` `.await` — **never on the
> demuxer**. The byte copy runs on a **separate** egress thread. It is **source-paced for
> DATA, clock-paced for the LIVENESS DECISION**: each tick a `PacketLiveness` watchdog
> (video + audio independent) `classify()`s an `AtomicI64 last_packet_at_ns` the copy
> thread Release-stamps from the shared `TimeSource` (arrival instant, not packet PTS).
> The clock thread flips a copy-vs-splice `AtomicU8`; the egress thread Acquire-reads it.
> A stale read **fails safe** (biases toward splice). So #1 holds per program: a valid
> coded AU — input packet **or** a pre-baked IDR-led slate AU — is emitted every tick.

- **Pre-bake-once slate.** Probe the input's coded params at program start; encode **once**
  (encoder then released — **no held NVENC session**) a short IDR-led, closed-GOP, B-free
  black/SMPTE + 1 kHz/silence loop in **bit-identical** params (SPS/PPS/VPS copied from the
  input's extradata, in-band before the slate IDR). Cache as `Arc<[EncodedPacket]>`,
  CRF/CQ, < 5 MB, O(1) in outage length. Failover replays inert bytes through the existing
  encoder-less `PacketMuxSink` → zero scarce sessions at rungs 1–2.
- **Re-stamp (#3 for the copy path).** Per-stream monotonic clamp+offset: `dts' =
  max(raw+offset, last_dts+1)`, `pts' = max(raw+offset, dts')`; **the clamp — not any
  FFmpeg flag — is what prevents `av_interleaved_write_frame`'s non-monotonic abort**.
  `avoid_negative_ts=make_zero` is a one-shot leading shift; `max_interleave_delta` is an
  interleave-flush knob, **not** an abort guard (don't set it "small"). Never `out_pts=f(tick)`
  (collapses B-frame reorder). Pathological timestamps → drop down the ladder.
- **Recovery gates on a TRUE IDR, not `is_key`.** FFmpeg flags HEVC CRA / H.264
  recovery-point I-frames as "key"; re-anchoring there decodes garbage. A strict-IDR header
  classifier (`is_idr`: H.264 nal==5; HEVC 19/20, reject CRA/BLA; AV1 KEY+show_frame+!show_existing+tid0+seq-hdr)
  discards input until a clean RAP; no-IDR inputs (GDR/open-GOP) descend the ladder.
- **The robustness ladder (cheapest valid rung first):** **(1) matched-slate-splice** (0
  encode, 0 session; TS/SRT/RTSP/RTMP-matched + fMP4-matched, splices silently) → **(2)
  container-discontinuity** (`EXT-X-DISCONTINUITY`+new `EXT-X-MAP` / TS `discontinuity_indicator`+PMT
  `version++`; still 0 encode) → **(3) full-transcode** (the only rung that holds a session;
  auto-selected for unstable params, inexpressible discontinuity, build-time codec/container
  incompatibility, or guaranteed click-free audio). Each program declares a `robustness_floor`
  (default `SlateOnLoss`); marginal cases fall through to rung 3 automatically.
- **Three prerequisite code gaps** (not optional): `Demuxer` `AVIOInterruptCB`+`rw_timeout`
  (recovery teardown); the `is_idr` classifier; a `dump_extra(freq=keyframes)`/Annex-B framing
  BSF stage so PS repeat in-band at both seams. Isolation (#10): the egress
  pull+`write_packet` runs on a dedicated thread wrapped in drop-oldest + `SINK_WEDGE_GRACE`
  detach, so a wedged push peer is shed, never back-pressures the watchdog.

Cross-program isolation holds: a passthrough runs on its own task with its own clock, so it
stalling never stalls program A. Audio failover is **video-clean / soft-step, not pop-free**
(coded-domain AAC seam transient); sample-accurate click-free ⇒ the transcode floor.

**Compatibility is a build-time gate, not a run-time surprise.** Validate
codec/profile-vs-container (e.g. HEVC into FLV/RTMP is illegal) during `build_programs`;
if incompatible and policy allows, downgrade kind to transcode with a logged
substitution (mirroring `resolve_encoder`'s existing fallback-with-log at
`pipeline.rs:312`). **Prefer transcode over copy when source timestamps are pathological**
(`streaming-gotchas.md:130` — aggressive clamping corrupts reorder).

## 4. Efficiency: decode-once-use-many (the operator directive)

This is the **most important new engine-level mechanism**, and the one place the design
must NOT be naïvely "N independent pipelines."

Today each `Pipeline` owns its source stores (`pipeline.rs:425`) and decode threads, so
two `Pipeline`s sharing camera-1 would decode it twice — violating the directive.

**Decision (confidence: high): hoist the per-source `TileStore` + `IngestPlan` out of
`Pipeline` into a process-global, ref-counted `SourceRegistry` owned by `ProgramSet`.**
- **One ingest/decode actor per distinct *physical* source**, keyed by canonical source
  identity (URL + decode params), publishing into **one `TileStore<Nv12Image>`** (the
  existing lock-free last-good store). Sources stay **top-level in config**
  (`MultiviewConfig::sources`, `lib.rs:97`) — *not* nested per-program — which is exactly
  what makes decode-once expressible.
- **Decode-once-use-many is literally `Arc::clone` of a `TileStore` handle.**
  `CompositorDrive` already holds `Arc<TileStore>` and only ever *samples* it
  (`drive.rs:79`); N programs sampling one store at different cadences is already
  supported with zero contention (lock-free triple-buffer, `latest.rs:92`; invariant #2).
- **Ref-counted lifetime:** the registry holds a refcount = number of programs
  referencing each source; the ingest actor starts on the first reference and tears down
  on the last release. A source outlives any single program but dies when the last
  program stops (confidence: high on the requirement; medium on the exact handle type —
  the ingest lifecycle is lifted from the per-`Pipeline` `IngestSupervisor` to process
  scope).
- **Decode-at-display-resolution (#6) across consumers:** decode **once at the *supremum*
  requested resolution** across all consumers, and each program scales down in-shader at
  composite (NV12-throughout, #5 — scaling is cheap; today `cell_pixel_size`
  `pipeline.rs:541` picks one tile size from one layout, which must become a max over all
  programs' bindings). Confidence: medium — confirm via the cost model that the largest
  consumer dominates; the alternative (per-(source,resolution) decode instances) is less
  efficient. NVDEC does one fused decode-resize per session; a second tile size costs one
  on-GPU `scale_cuda`, not a second decode (efficiency.md, ADR-E001).
- **Passthrough bypasses the decode pool entirely.** It needs coded packets, not NV12, so
  it opens its own demuxer for the raw elementary stream. The registry exposes **two
  facets** per source, built lazily: a decoded `TileStore<Nv12Image>` (A/C) and a
  coded-packet tap (B). A source used **only** by passthrough never spins a decoder; a
  source both passthroughed **and** tiled is demuxed once and the packet stream forked to
  (a) the passthrough muxers and (b) the decoder feeding the store — the **headline
  efficiency win** of unifying the source layer (the one acceptable "double-touch": coded
  bytes vs NV12 are genuinely different data).

### 4.1 Shared GPU residency + placement co-location

A shared decoded NV12 surface is read zero-copy by multiple programs **only when they are
on the same GPU island**; a program pinned to a different GPU pays the documented one
cross-vendor host round-trip (ADR-0004/0017/0018 — no GeForce P2P). **Consequence:**
placement should **co-locate programs that share a source** on one GPU. `select.rs`
already exists and gates on free VRAM + the discovered NVENC concurrent-session ceiling
(`select.rs:22–23,90`, `LoadWeights::nvenc_session`, `PipelineDemand::opens_encode_session`
at `select.rs:202`), with `Pins::pin_pipeline` for affinity (`select.rs:136`). **Proposal
(confidence: medium):** extend the existing "existing-affinity tie-break" (ADR-0018) from
"tiles of the same scene" to "programs sharing a source" — a new score term, not a new
mechanism. The CUDA context (~84–115 MB) is paid **once** per process when all programs
share one context; a process-per-program design would multiply it — so all programs run
**in one process**.

### 4.2 Admission control + degradation across the whole set (invariants #7, #9)

- **Encode-once-mux-many (#7) is per-program-per-rendition** — two programs are two
  canvases → two encodes; that is correct and required, not a regression. *Within* a
  program, `StreamEgress` already encodes once and fans out (`pipeline.rs:1659`).
- **Admission must run over the *union* of programs, crediting shared decode once.**
  The `Planner::admit` gate (`planner.rs:181`) and `CostBudget` (decode/composite/encode
  Mpix/s, `cost.rs:24–37`) are per-plan today. The `SourceRegistry` computes the
  **de-duplicated** decode demand (one `TileLoad` per physical source at its supremum
  resolution), then each program adds its composite + encode load. Naively summing
  per-program decode would over-count by exactly the sharing factor — **the cost-model
  change is what makes the sharing savings real.** The `CostBudget` struct itself carries
  no VRAM/IO term (`cost.rs:24` is Mpix/s only), but `select.rs` already gates VRAM +
  NVENC sessions at placement; the union-admission proposal feeds both. Passthrough admits
  ~free on the three GPU engines (0 Mpix/s) but counts against a host-IO/bandwidth budget
  (proposal: a new additive dimension; efficiency.md — bandwidth/VRAM is the real wall).
- **Degradation (#9) becomes cross-program**, ranked cheapest-impact-first **across the
  set** (operator-assignable per-program `priority`; default: shed
  transcode/passthrough-grade before the primary multiview). Per-program bounded queues
  still drop-never-grow (`LIVE_QUEUE_CAP`, `pipeline.rs:113`). Each program keeps its own
  clock (#1) — a global shed lowers a program's *quality* or refuses *admission* of a new
  one; it never stalls a running program.

### 4.3 Worked savings (the prompt's scenario: 3×3 multiview→HLS using C1–C9, RTSP→SRT
passthrough of C1, 720p RTMP transcode of C1)

C1 is referenced by 3 programs. **Naive per-program pipelines:** C1 demuxed 3×, decoded
2× (tile + transcode) → 2 decode sessions + 3 demuxes. **Shared:** C1 demuxed **once**,
decoded **once** (the tile's decode at supremum-res is reused by the transcode), packets
forked to the passthrough → **1 decode + 1 demux**. Decode saving on C1: 50% (2→1);
demux: 67% (3→1); one 1080p30 decode session ≈ ~62 Mpix/s of NVDEC reclaimed; CUDA
context paid 1× not 3×; one shared surface pool not 2–3. **For a source used by k
programs, shared decode turns decode/demux/context/VRAM from O(k) to O(1); only the
per-program encode/composite stays O(programs)** because each rendition is genuinely a
distinct bitstream (efficiency.md, ADR-E004). **Passthrough = 0 decode/composite/encode/
NVENC-session — mis-classifying it as transcode is the most expensive avoidable mistake.**

## 5. Config / API / UI surface

### 5.1 Config — `[[programs]]` (backward-compatible)

Add `#[serde(default)] programs: Vec<Program>` to `MultiviewConfig` (`lib.rs:89`). A
`Program` carries `id`, optional `display_name`, `autostart`, an **internally-tagged**
`ProgramKind` (`#[serde(tag = "kind")]`, never untagged — conventions §5, ADR-0010), and
its own `outputs: Vec<Output>` (reusing the existing `Output` enum + `gpu_pin` +
per-output `audio` verbatim). `ProgramKind::Multiview { canvas, layout, cells, overlays }`
(own fps → per-program cadence), `Passthrough { input_id, robustness_floor }`
(default `SlateOnLoss`; never a `Reject` — the engine descends the ladder, ADR-0030 §1/§4),
`Transcode { input_id, encode }`.

**Backward compat (mandatory):** the existing top-level `canvas`/`layout`/`cells`/
`overlays`/`outputs` (`lib.rs:93–110`) desugars to exactly one implicit
`Multiview` program (`id = "main"`). Mechanism: make legacy `canvas`/`layout` `Option`,
add `programs` (default empty); `into_programs()` synthesizes from the legacy block when
`programs` is empty, and `validate()` (`lib.rs:232`) **rejects** populating both (ambiguous).
Bump `schema_version` to 2 (v1 parses via the legacy path). New cross-program validation:
unique program ids; **unique output labels across all programs** (no two programs bind the
same RTSP mount / NDI name / push URL); each `input_id` resolves to a declared `sources`
entry. **Sources stay top-level** (shared), referenced by `input_id` — matching the
existing `CellSource.input_id` indirection (`schema.rs:440`).

### 5.2 Management API — programs as a first-class resource

- **CRUD** (new `PROGRAM_KIND = "program"`, new `routes/programs.rs` over an
  `InMemoryProgramStore` on `AppState`, mirroring `routes/outputs.rs`): `GET/POST/PUT/
  DELETE /api/v1/programs[/{id}]` with `ETag`/`If-Match`→412, `Idempotency-Key`, RFC 9457,
  audit-after-write. Wire into `api_router()` (`mod.rs:435`).
- **Lifecycle + live-apply** (each returns 202 + op-id via the existing `submit_accepted`
  at `mod.rs:252`, inheriting #10 — bounded bus, shed-to-503-on-full, cannot
  back-pressure the engine): `POST /api/v1/programs/{id}/start|stop|apply` and a
  no-engine-submit `POST /api/v1/programs/{id}/plan` returning the **live-apply class**
  before applying (invariant #11; `management-capability-matrix.md`).
  `Command` (`command.rs:81`) gains program-scoped variants: `StartProgram{op,program}`,
  `StopProgram{op,program}`, `ApplyProgram{op,program,..}`; existing `SwapSource`/
  `ApplyLayout` gain a `program: String` field; `operation_id()`/`kind()`
  (`command.rs:154,169`) extend. The current global `Start`/`Stop` (`command.rs:82–88`)
  become deprecated aliases targeting `"main"`.
  - **Class-1 (hot)**, within a Multiview program: layout swap, tile rebind
    (`SwapSource`), bitrate, color-tag relabel, add/remove a non-pinned output.
  - **Class-2 (reset, make-before-break, `OutputRunState::Migrating` already exists at
    `event.rs:101`)**: changing the `ProgramKind` discriminant (A↔B↔C — **always
    Class-2**, the load-bearing rule), canvas res/fps/pixfmt, codec/profile/level,
    bit-depth, GOP, HDR enable.

### 5.3 Realtime — per-program state

- New `Topic::Programs` (`programs`) on `topic.rs:16` + `as_str` (`#[non_exhaustive]`
  already permits it).
- New `program.state` event → `ProgramState { state: ProgramRunState, kind:
  ProgramKindTag, outputs: u32, fps: Option<String> }`, all tagged on `t` (never
  untagged). `ProgramRunState` reuses `OutputRunState` semantics (`event.rs:94`;
  `Migrating` surfaces a Class-2 A↔B↔C make-before-break to the UI).
- Scope existing `OutputStatus`/`TileState` to a program via the `Envelope.id`
  (`envelope.id = "{program}/{output}"`), and add `program: Option<String>` to
  `OutputStatus` specifically (it has no id today) via
  `#[serde(default, skip_serializing_if)]` (back-compat; matches the `TileState.input`
  precedent). Correlation (`CorrKey`, `realtime.rs:91`) and the connect-time snapshot
  (`realtime.rs:381`) extend to carry the per-program `ProgramState` list (snapshot⊕deltas
  = truth, ADR-RT003).

### 5.4 UI

New `programs` nav entry + routes (list / new / `:id` editor) in `web/src/app/
{navigation,router}.tsx`. **Programs list** (TanStack-Table, mirroring `LayoutsPage`):
name, **kind badge** (multiview/passthrough/transcode), run-state badge (from
`program.state`), output count, start/stop (fires the 202 lifecycle POST + shows op-id
progress); `usePrograms` via `makeListHook('programs', …)` (`queries.ts:142`).
**Kind-switched editor:** Multiview → the existing `LayoutEditorPage` parameterized by
program id; Passthrough → source + outputs + `robustness_floor`, with a computed
**rung badge** ("will splice (remux)" / "will splice (discontinuity)" / "will transcode")
plus "video-clean / audio soft-step" — the efficiency-and-robustness contract made visible;
Transcode → source + `TranscodeSpec` + outputs. **Before any apply**, call `…/plan` and
show the Class-1 (seamless) vs Class-2 ("resets N outputs / consumers reconnect") badge +
confirm. The dashboard becomes a multi-program overview (one card per program).

## 6. Invariant ledger

| # | Holds because |
|---|---------------|
| **#1** | Each program owns an independent `OutputClock`+`EngineRuntime` that awaits only its own pacer (`runtime.rs:405`); passthrough (B) runs a **degenerate, compositor-less clock** that fails to a pre-baked slate on loss (§3) — **#1-preserving, not an exception**: a valid coded AU is emitted every tick. |
| **#5/#6** | One shared decoded NV12 store per source (`SourceRegistry`), decoded at the supremum consuming resolution, scaled per consumer in-shader. |
| **#7** | Per-program-per-rendition encode (two canvases = two encodes, correctly); within a program, `StreamEgress` encodes once and fans out (`pipeline.rs:1659`). |
| **#9** | Cross-program cheapest-impact-first shed with operator priority; per-program bounded `DropOnOverload` queues (`pipeline.rs:113,148`); union admission credits shared decode once. |
| **#10** | N independent off-hot-path egresses + per-program publishers; shared state is read-only/wait-free (`latest.rs:92`); new CI chaos gate proves wedging one program doesn't stall another's `ticks_emitted`. |
| **#11** | `…/plan` surfaces Class-1/Class-2 per program before apply; `kind` change is always Class-2 (make-before-break via the existing `Migrating` state). |
