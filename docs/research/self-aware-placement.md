# Research brief — Self-aware placement: SENSE → DETECT → WARN → PLAN → APPLY

- **Status:** Design brief (verification-hardened)
- **Area:** Efficiency / HAL / Engine / Control / Web
- **Decision:** [ADR-0035](../decisions/ADR-0035.md)
- **Builds on (reference, do not duplicate):**
  [ADR-0018](../decisions/ADR-0018.md) (adaptive affinity-first GPU work-placement —
  the placement decision engine), [ADR-0017](../decisions/ADR-0017.md) (monitoring +
  least-loaded ranking), [ADR-0026](../decisions/ADR-0026.md) (encode-once-mux-many),
  [ADR-0003](../decisions/ADR-0003.md)/[ADR-0004](../decisions/ADR-0004.md) (capability
  layers / zero-copy islands), [ADR-R004](../decisions/ADR-R004.md) (make-before-break
  Class-2 migration), [ADR-R009](../decisions/ADR-R009.md) (`/livez` in-process only),
  invariant **#9** (resource-adaptive degradation), and the GPU-placement principle
  (load *informs* placement; affinity is a hard gate; a single pipeline is never
  fragmented or migrated mid-flight).
- **Citation correction (load-bearing):** the placement decision engine is **ADR-0018**
  (*"Adaptive affinity-first GPU work-placement with deliberate split + closed-loop
  re-placement"*), **not** ADR-0027. ADR-0027 is *synthetic sources are first-class
  `SourceKind`s*; ADR-0026 is *encode-once-mux-many*. Any prior note attributing
  placement to ADR-0027 is a mis-citation — this brief grounds on **ADR-0018** and its
  source brief [gpu-placement-engine.md](gpu-placement-engine.md).

> Source-of-truth ordering, per `CLAUDE.md`: the **Rust code is the ultimate source of
> truth**; where this brief and code disagree, the code wins and this brief is updated.
> Every code claim below was read at the cited symbol; line numbers drift, so symbols
> are authoritative.

---

## 1. Why this exists — the product goal fails *silently* today

The headline product promise is blunt: **even commodity hardware should EASILY do a
2×2 4K multiview with subtitles.** Efficiency *is* the product. That promise depends on
the GPU actually carrying decode, composite, and encode — and on the operator being
*told*, loudly and actionably, when it isn't.

**The verified live failure.** On the GPU test box the wgpu compositor **silently fell
back to the CPU reference**: the host had no usable Vulkan adapter (missing Vulkan
loader / the NVIDIA `graphics` driver capability not granted to the container), so
`GpuCompositor::new()` returned `Err`. NVML happily reported the RTX 4060 *the whole
time*. The result: compositing ran on ~5 CPU cores, the GPU sat ~80% idle, and **there
was no warning anywhere** — not in the UI, not on the health endpoint, not even at
`warn` level in the log. The single most expensive, most user-visible misconfiguration
in the product produced *zero* signal.

That single incident is the entire motivation. It is simultaneously:

- a **DETECT** failure (nobody checked whether the adapter we got was a *real* GPU, or
  whether an NVML-discovered GPU contradicts a CPU-resolved compositor),
- a **WARN** failure (there is no structured, actionable health-warning model to carry
  "GPU present but compositing is on CPU — here is the fix"), and
- the same **co-tenant headroom signal** the **PLAN** loop needs to decide *where* to
  put load.

The fix is one coherent subsystem — **SENSE → DETECT → WARN → PLAN → APPLY** — that
makes the mismatch *detected, surfaced, actionable*, and reuses that signal to *place*
load. This brief is the *why*; [ADR-0035](../decisions/ADR-0035.md) is the decision; the
backlog (§8) is the dependency-ordered build.

---

## 2. As-built vs missing — the honest map

This subsystem is mostly *wiring existing, tested machinery together*, plus one new
keystone type. The table below is the ground truth (verified against code).

| Stage | State | What exists / what's missing |
|---|---|---|
| **SENSE** | **Built** | Off-engine poller (`multiview-cli/src/system_metrics.rs`, `SAMPLE_PERIOD ≈ 750 ms`) calls `load_source.poll() -> Vec<DeviceLoad>` + `poll_self_share() -> Vec<SelfShare>`, publishes `Event::SystemMetrics` via the engine's **drop-oldest** `EnginePublisher::publish_event` (a single non-blocking `broadcast::send`, never awaits a subscriber → **inv #10 holds**). `DeviceLoad` (`multiview-hal/src/load.rs`) carries `gpu_busy_frac`, `vram_used_bytes`/`vram_total_bytes`, `enc_util_frac`, `dec_util_frac`, `nvenc_session_count`, `compute_busy_frac` — **every field `Option` (unknown ≠ fabricated zero)**. `SelfShare` (`load.rs`) carries ours-vs-total per-process attribution (NVML `running_*_processes` + `process_utilization_stats`); `SelfCpuSampler` (`system_metrics.rs`) does the `/proc` self-vs-host CPU ratio. |
| **DETECT** | **Missing (keystone)** | No `CapabilityReport` exists (`rg CapabilityReport crates/` → 0). DETECT today is **presence-only**: `EnvProbe` (`multiview-hal/src/probe.rs`) checks device nodes and returns a conservative baseline `DeviceCaps`; ADR-0003 *explicitly defers* usability queries (NVENC `NV_ENC_CAPS`, `cuvidGetDecoderCaps`, wgpu adapter enumeration) to the backend crates — **unbuilt**. ADR-0003 records the constraint that motivates this: *"NVML cannot report codec capability — must use SDK caps APIs or probes."* The compositor *retains* the adapter (`GpuContext::adapter()`, `multiview-compositor/src/gpu/device.rs`) and could read `adapter().get_info()` — but **`rg get_info` over `crates/` returns 0**: the lever exists, unused. |
| **WARN** | **Missing** | No Health/Warning type. But the transport (drop-oldest publisher), severity ladder (`AlertSeverity{Info,Warning,Critical}`), dedupe (`Alert.key`), raise/clear coalescing (`Alert.active`), topic (`Topic::Alerts`), ingest pattern (`alarm_ingest`), store (`AlarmRepository`), and REST list pattern (`routes/alarms.rs::list_alarms`) **all exist and are wired end-to-end**. |
| **PLAN** | **Built, unwired** | The whole brain is pure, tested, and referenced **only** by `pub use` re-exports (`multiview-engine/src/lib.rs`) — never constructed in run code: `select_device()` (`multiview-hal/src/select.rs`), `plan_split()` (`multiview-hal/src/split.rs`), `PlacementController::observe()` (`multiview-engine/src/placement.rs`), `ControlLoop` (`multiview-engine/src/degrade.rs`). Verified: `rg select_device|PlacementController|ControlLoop crates/multiview-cli/src` → **0 hits**. |
| **APPLY** | **Missing** | The composite backend decision is the single line `multiview-cli/src/pipeline.rs` → `drive.with_backend(RunBackend::select(true))`. `RunBackend::select(true)` (`multiview-compositor/src/backend.rs`) catches every GPU-init error, logs at **`info`** (not `warn`), returns `Self::Cpu`; the resolved `kind()`/`is_gpu()` are **read by nothing** in the CLI. Decode/encode device placement is not selectable at build (see §5). |

**The gap, stated once:** SENSE produces exactly the per-device, ours-vs-total signal
PLAN needs — and it **never reaches** `select_device` or a `PlacementController`. DETECT
produces nothing about *usability*, so APPLY cannot tell "real GPU" from "software
adapter that happened to enumerate," and the silent fallback has no observable.

---

## 3. Architecture — one subsystem, five stages, one data join

```
            ┌──────────── off the output-clock thread, build-time + slow tick ───────────┐
            │                                                                             │
 SENSE  ─▶ DeviceLoad[] + SelfShare[]  ──┐                                                │
 (built, 750ms poller)                   │                                                │
                                         ▼                                                │
 DETECT ─▶ CapabilityReport  ── per-(Stage,DeviceId) {present, usable, in_use, reason} ───┤
 (NEW, build-time probe)                 │                          │                     │
                                         │ MISMATCH cross-check     │ candidate set       │
                                         ▼                          ▼                     │
 WARN  ─▶ HealthWarning (Event::HealthWarning{Raised,Cleared})   PLAN ─▶ select_device()  │
 (NEW shapes, reused machinery)   │  Topic::Alerts                (ADR-0018, built/unwired)│
                                  │  drop-oldest publisher (inv #10)        │              │
                                  ▼                                         ▼              │
                          GET /api/v1/health                      APPLY: backend+DeviceId  │
                          (NEW, read-only)                        per stage at BUILD time  │
                                  │                                         │              │
                                  ▼                                         ▼              │
 UI ─▶ HealthBanner + HealthPanel + "What runs where" (joins placement→GpuMetrics.id)─────┘
```

The whole subsystem runs **off the output-clock thread**: capability probing and the
initial placement are **build-time** (the clock is not yet constructed); the runtime
re-plan (phase 2) runs on a **separate slow control tick** (§5). Inv #1 (output never
stalls) and inv #10 (control/preview/telemetry never back-pressure the engine) are
preserved by construction — every emission goes through the existing drop-oldest
publisher, and the planner never sits on the clock thread.

---

## 4. DETECT — the runtime `CapabilityReport` (the keystone)

`CapabilityReport` is the **single DETECT output consumed by both WARN and PLAN**. It
fuses three sources that exist today but are disconnected:

1. **Presence/intent** — `EnvProbe`/`ProbeOutcome` (`probe.rs`, device-node presence) and
   the live NVML `DeviceLoad`/`SelfShare` (`load.rs`). "A GPU is discovered."
2. **Actual usability** — the built-but-unread runtime signals: the compositor
   `adapter().get_info()`, a *throwaway* libav `ctx.open_as(codec)` for encode/decode.
   "The HW backend actually opens at our target resolution/format."
3. **The join (the misconfig gate)** — a per-stage **MISMATCH = (hardware discovered:
   NVML/`DeviceLoad` reports ≥1 GPU **OR** `EnvProbe` Present) AND (the resolved backend
   is software-tier / the HW backend is present-but-unusable)**.

Shape: `multiview_hal::capability::CapabilityReport` — a per-`(Stage, DeviceId)` table of
records `{ stage, backend, device {id, name, vendor}, present: bool, usable: bool,
in_use: bool, reason: Option<UnusableReason> }`, assembled **at pipeline build time**
(off the output-clock thread, refreshed on re-plan). Align its naming/shape with the
already-specified `CapabilityReport` in ADR-M007 so there is **one** source of truth, not
two.

The three probe sites **record their outcome instead of swallowing it**:

- **Composite** — call `GpuContext::new` and read `adapter().get_info()` (exposed at
  `device.rs::adapter()`, never read today). `usable = adapter present AND
  device_type != wgpu::DeviceType::Cpu`. The **load-bearing discriminator is
  `device_type != Cpu`** — Mesa's llvmpipe/lavapipe reports `device_type == Cpu` even
  though its `backend == Vulkan`, so `device_type` (not "exclude `Gl`") catches the
  software case. **Do not blanket-exclude `Backend::Gl`** — a legitimate GL-only real GPU
  reports `DiscreteGpu`/`IntegratedGpu` and must not be a false negative; treat `Gl` as
  software *only* when `device_type == Cpu` or the `driver` string matches
  llvmpipe/softpipe/swiftshader. Treat `DeviceType::{VirtualGpu, Other}` explicitly
  (warn at lower severity rather than silently accept) — this can only *under*-warn,
  never over-warn. The probe requests with `force_fallback_adapter: false`, so a software
  adapter is not silently substituted.
- **Encode** — go beyond `select_encoder`'s `find_by_name` presence check
  (`multiview-ffmpeg/src/codec.rs`) with a **throwaway** `ctx.open_as(codec)` at the
  target res/format (the real check, `encode.rs`), then drop it — distinguishing
  "registered" from "session-openable" (NVENC may be linked yet fail to open a session).
- **Decode** — the same throwaway open of the `*_cuvid` hwaccel decoder
  (`multiview-ffmpeg/src/hwdecode.rs`, currently unwired — `rg select_decoder
  crates/multiview-cli` → 0).

**No-false-positive rule (structural, not a config flag).** A capability warning fires
**only** on intent-vs-capability mismatch: *hardware discovered present* **AND** *resolved
backend is software/CPU-tier*. On a GPU-free or intentional `software`-only host both
halves are false → **zero** capability warnings. This is verified two ways: `EnvProbe`
returns `Absent` for every kind on a GPU-free host (feature-off arms are const-`Absent`,
covered by `env_probe_is_clean_when_feature_off_or_no_device`), and `DeviceLoad` is empty.
Today `pipeline.rs` hardcodes `RunBackend::select(true)` with **no operator
software-only knob** (`rg backend_override|cpu_only|software_only` → 0), so when the GPU
feature is compiled, intent is unconditionally "prefer GPU" — a GPU-present-but-CPU state
*cannot* be an intentional software choice, so it is never a false positive.

> **Blind-spot guard (fold into DETECT):** make the first conjunct OR-in the `EnvProbe`
> device-node/DRM presence signal **independently** of whether NVML/`cuda` is compiled,
> so a GPU box running an *asymmetric* build (neither `cuda`/NVML nor `gpu`/wgpu linked)
> still trips the misconfig detector. Otherwise that degenerate build is a genuine blind
> spot for the exact silent-fallback scenario.

---

## 5. WARN — the actionable health-warning model

No Health/Warning type exists, but the transport, severity ladder, dedupe, store, and
REST patterns all do. **Decision: model `HealthWarning` as a richer *sibling* of the
existing `Alert`** (`multiview-events/src/event.rs`: `{key, severity:
AlertSeverity(Info|Warning|Critical), title, detail, active}`) — **reuse** its `key`
dedupe + `active` raise/clear coalescing, and **add the operator-required actionable
fields it lacks**: `code: WarningCode` (stable `#[non_exhaustive]` enum), `subsystem`,
`remediation` (the *fix*), `since`, optional `current`/`expected`.

> **"Extend" means a new sibling type + new event variants, NOT mutating `Alert`
> in place.** `Alert` has live producers (`alarm_ingest`, `tally_state`) and is
> serialized over AsyncAPI; adding required fields to it is a breaking wire change. The
> safe path is `HealthWarning` as a new type carrying *new* `Event::HealthWarningRaised`
> / `Event::HealthWarningCleared` variants, mirroring `Event::AlertRaised`/`AlertCleared`,
> routed on `Topic::Alerts` (the existing operator-alert lane), emitted through the
> **identical drop-oldest publisher** as `SystemMetrics` (inv #10). These new variants
> must be added to the hand-listed `asyncapi.rs` registration and `openapi_schemas.rs`.

**Flap discipline** (reuses real machinery):

- **Latched (cannot flap):** capability-mismatch codes
  (`gpu-present-no-vulkan-adapter`, `software-decode-on-gpu-host`,
  `software-encode-on-gpu-host`) are **build-time facts** — raised once at pipeline
  build, cleared only on reconfigure/restart.
- **Dwelled:** metric-threshold codes (`vram-pressure`, `nvenc-session-ceiling-hit`,
  `sustained-cpu-saturation`) use raise-after-N / clear-after-M, mirroring
  `degradation::Hysteresis` (`multiview-hal/src/degradation.rs`).
- **Controller-gated:** `degradation-active` is driven by `Hysteresis::level() > 0`;
  severity escalates Info→Critical exactly when `DegradationAction::affects_program()`
  (`degradation.rs`) — i.e. when pressure crosses into a **program-affecting rung**
  (`first_program_level()` = `FasterEncoderPreset.rung()`, the first program rung above
  the preview + tile rungs; this is the rung the comment in `degrade.rs` still calls
  "rung 4" from an older ladder — that comment is stale and should be fixed to name
  `FasterEncoderPreset`). **Caveat:** `Hysteresis` is *asymmetric* — it dwells the
  *clear* side but raises promptly. That is correct for the **as-built** path because
  `degradation-active` is fed by `ControlLoop::pressure_from_plan` (a slow-moving,
  *structural* cost-model quantity that cannot raise-flap). **If `degradation-active` is
  ever gated on raw live telemetry, add the same EWMA + sustained-dwell that
  `PlacementController` already applies (`placement.rs::note_sustained`) *before*
  feeding `step()`** — do not rely on `Hysteresis` alone to debounce the raise side. Keep
  the *clear* strictly tied to `Hysteresis` recovery (`level → 0`).

**REST:** add a **new** read-only `GET /api/v1/health` (active warnings + rolled-up worst
severity; `?severity`/`?active` filters), modeled on `routes/alarms.rs::list_alarms`.
**Do NOT overload `/livez`/`/readyz`** — a capability warning must not flip liveness and
restart-loop the container (ADR-R009: `/livez` is in-process-only). Mirror engine
warnings into a control-side `WarningRepository` via a `warning_ingest` task (copy
`alarm_ingest` exactly — swallow-and-skip on upsert error, `Lagged` resubscribes at head;
**never a bounded mpsc back toward the engine**).

### 5.1 Warning catalog (Increment-1 surfaces the first three)

| Code | Severity | Subsystem | Debounce | Trigger (intent-vs-capability MISMATCH unless noted) | Operator message + remediation |
|---|---|---|---|---|---|
| `gpu-present-no-vulkan-adapter` | Warning | compositor | **Latched** (build) | `DeviceLoad`/`EnvProbe` reports a GPU **AND** `RunBackend::select` resolved `Cpu` / wgpu adapters == 0 / `device_type == Cpu`. Cite: `backend.rs::select`, `device.rs`. | "GPU *\<name\>* detected (NVML) but GPU compositing is **UNAVAILABLE** (no Vulkan adapter); compositing fell back to CPU (high CPU)." **Fix:** set `NVIDIA_DRIVER_CAPABILITIES` to include `graphics` (or `all`) and install `libvulkan1` + the `nvidia_icd.json` ICD. |
| `software-decode-on-gpu-host` | Warning | decode | **Latched** (build) | HW decode backend probed present **AND** decode resolved Software. Cite: `hwdecode.rs::select_decoder` (unwired), `decode_stream.rs::new` (no backend param). | "GPU *\<name\>* present but decoding runs in software; NVDEC was not used." **Fix:** ensure `NVIDIA_DRIVER_CAPABILITIES` includes `video`, the linked libav has the `*_cuvid` decoder, and `CUDA_VISIBLE_DEVICES` exposes the GPU. |
| `software-encode-on-gpu-host` | Warning | encode | **Latched** (build) | `h264_nvenc` present via `find_by_name` **AND** encode resolved `mpeg2video`/`libx264` **OR** the throwaway `open_as` failed. Cite: `codec.rs` (presence only), `encode.rs::open_as` (real check). | "GPU *\<name\>* present but encoding fell back to software; NVENC session did not open." **Fix:** include `video` in `NVIDIA_DRIVER_CAPABILITIES`; verify `h264_nvenc` opens a session (driver / session ceiling); check the NVENC build. |
| `nvenc-session-ceiling-hit` | Warning (Critical if it blocks a needed rendition) | encode | **Dwelled** | `nvenc_session_count >= ceiling` sustained. The **gate** uses the **device-wide** `load.nvenc_session_count` vs a discovered per-system ceiling (`select.rs`, threaded via `PlacementPolicy::with_nvenc_ceiling`) — **not** `GpuMetrics.encoder_session_ceiling` (UI display) and **not** `self_encoder_sessions`. | "NVENC encode sessions at the system ceiling (*\<n\>/\<ceiling\>*); new encode placement on this GPU is gated. *\<self_n\>* are ours, the rest co-tenant." **Fix:** consumer GeForce caps concurrent sessions **per system** (driver-imposed; not enumerable via NVML — comes from the SDK caps / a static table); reduce sessions, place encode on another GPU, or use a qualified card. |
| `vram-pressure` | Warning (Critical near OOM) | gpu | **Dwelled** | `DeviceLoad::vram_used_frac()` sustained above threshold. | "GPU *\<name\>* VRAM at *\<pct\>*% (*\<used\>/\<total\>*); headroom for new pipelines is limited." **Fix:** reduce tile count/resolution, free co-tenant VRAM, or place new load on a GPU with free VRAM. |
| `sustained-cpu-saturation` | Warning (Critical if program fps drops) | cpu | **Dwelled** | `cpu_util` high **AND** `self_cpu_util` high sustained (the `self_*` ratio points the finger at us, not a co-tenant). Cite: `SelfCpuSampler`. | "Host CPU saturated (*\<cpu_util\>*%); our process is *\<self_cpu_util\>*%." **Fix:** enable GPU compositing/encoding (see compositor/encode warnings), reduce tiles, or add cores. |
| `degradation-active` | Info while preview/tile rungs; **Critical** once `affects_program()` | placement | **Controller-gated** by `Hysteresis` (no extra debounce on the structural-pressure path; add EWMA+dwell if ever fed live telemetry) | `Hysteresis::level() > 0`. Cite: `degradation.rs` ladder + `affects_program` (boundary = first program rung) + `Hysteresis`. | "Resource-adaptive degradation active at level *\<n\>* (*\<action\>*); shedding cheapest-impact-first. *\<program-affected?\>*" **Fix:** reduce load or add capacity; program output is protected until the lower rungs are exhausted. |

---

## 6. PLAN — capability + load-aware placement (ADR-0018, built, unwired)

The PLAN brain exists, is pure + tested, and is the affinity-first engine of ADR-0018.
Reference it; do not duplicate. Salient, code-verified properties:

- **`select_device()`** (`select.rs`): pins → hard gates (capability / cost-budget /
  VRAM / NVENC-session-ceiling, `passes_hard_gates`) → DRF dominant-resource score
  (vram primary, enc/dec/compute, nvenc-session) with unknown-weight redistribution →
  headroom ceiling → deterministic tie-break.
- **AFFINITY is a HARD GATE** (ADR-0018 / GPU-placement principle): each `GpuCandidate`
  is an opaque **whole-island** host (decode+composite+encode on **one** `DeviceId`); a
  pin always wins (`pin_overrides_even_a_busier_gpu`); an unsatisfiable pin **rejects
  rather than relocates** (`unsatisfiable_pin_rejects_rather_than_relocating`).
- **`plan_split()`** (`split.rs`) is a **gain-gated, cross-GPU-copy-accounted LAST
  RESORT**: composite is never a cut target (`composite_is_never_a_cut_target`), it needs
  two distinct GPUs (`single_device_never_splits`), charges a `CrossGpuCopy`, and rejects
  below `min_gain` (`marginal_split_is_rejected_below_min_gain`). It is reached **only**
  on `RejectReason::NoCandidateFitsWholePipeline | AllOverHeadroomCeiling`
  (`placement.rs`) — both still keep composite whole and are gain-gated, so the affinity
  principle holds; `PinUnsatisfiable`/`NoCandidates` shed locally, never split. *Correct
  the earlier wording "only on NoCandidateFitsWholePipeline" to this two-reason set.*
- **Co-tenant headroom** is met by `DeviceLoad` **totals**: a near-ceiling NVENC from a
  co-tenant NVR gates that GPU out via the **device-wide** session-ceiling gate
  (`select.rs`, comparing `load.nvenc_session_count` — the count across *all* processes —
  against the discovered ceiling). This is the correct signal: the hardware ceiling is
  shared system-wide. `self_*` is the **ours-vs-total discriminator** (is the pressure
  ours or the NVR's?) — it is a **UI/attribution** field today (`system_metrics.rs`,
  `GpuMetrics`) and **plays no role in the gate**; it could later inform scoring/warnings
  but need not gate.
- **`PlacementController::observe(&[DeviceLoad])`** (`placement.rs`): EWMA +
  `Hysteresis` + dwell + anti-storm cooldown → `Hold`/`Shed`/`Migrate`(whole-island
  make-before-break)/`Split`. It **only proposes** — "the controller never executes
  anything." `ControlLoop` (`degrade.rs`): `pressure_from_plan` → step over the
  cheapest-first degradation ladder (inv #9).

**The unwired gap, stated honestly:** SENSE's `DeviceLoad`/`SelfShare` reach **only** the
UI; they are **never fed into `select_device` or `PlacementController`** (`rg
select_device|PlacementController crates/multiview-cli/src` → 0). Closing that is APPLY.
**Affinity-preservation in the runtime path is currently *latent*** (proven by unit tests,
not yet exercised in a running pipeline) — phase 2 must add a chaos/soak test asserting
no proposal ever fragments a live island and a pinned/in-flight pipeline never migrates.

---

## 7. APPLY — make the silent fallback loud (the seam is one line)

The entire composite backend decision is `multiview-cli/src/pipeline.rs` →
`drive.with_backend(RunBackend::select(true))`. `RunBackend::select(true)` catches every
GPU-init error, logs at **`info`** ("GPU compositor unavailable; falling back to CPU
reference"), returns `Self::Cpu`; its signature is documented as *"never returns an
error: the result is always a usable backend"* — i.e. the failure is **by-design silent
to callers** — and `kind()`/`is_gpu()` are read by **nothing** in the CLI. That is the
exact silent fallback.

**Two coupled facts make it silent, not one** — a fix that only rewrites the one line is
*insufficient*:
1. the **selection** is one line (the `RunBackend::select(true)` call), **and**
2. the **silence** is produced inside `select` (info-only log + infallible signature
   that discards the `Err`) **and** by the *absence of any reader* of
   `kind()`/`is_gpu()`/`CompositorDrive::backend_kind()` in run code.

**Increment-1 replacement seam (pure, synchronous, build-time — cannot touch inv #1/#10;
the clock is not yet constructed):**
1. assemble candidates + `PipelineDemand` from the `CapabilityReport` (peak tile res from
   the solved layout, per-stage `TileLoad` from `cost.rs`, predicted pool bytes,
   `opens_encode_session`) + **one** `DeviceLoad` snapshot from `load_source.poll()`;
2. `select_device(...)`;
3. map the `Selection` to `RunBackend::select(true)` **only** when the chosen device is a
   *usable* GPU per the report, else `RunBackend::cpu()`; stash the chosen `DeviceId` for
   decode/encode and the "what runs where" panel;
4. **emit a `HealthWarning` on any mismatch** (and log at `warn`, not `info`).

**Crucially, keep the CPU-fallback path itself** — per inv #1 the run must still proceed
on CPU when the GPU is unusable. The fix is to make that decision **LOUD and reported**,
never to make GPU init fatal.

**Phase 2 — decode + encode device placement + runtime re-plan:**
- `IngestPlan` carries decode backend + `DeviceId`; wire `hwdecode.rs::select_decoder`
  with a **fused `-resize`** per inv #6 (decode-at-display-resolution).
  `StreamVideoDecoder::new` currently takes **no** backend param (`decode_stream.rs`) and
  `IngestPlan` carries no decode-backend field — both must grow one, behind the same
  `select_device` `Selection` so affinity is preserved by construction.
- `resolve_encoder` chooses NVENC-vs-x264 by **report + session ceiling** and **pins the
  GPU**. Today it picks the first *present* candidate by feature + `find_by_name` (NVENC
  is reachable if linked), but **never by capability+load and never pins a GPU**
  (`codec.rs`, `pipeline.rs`).
- Wire the per-system **NVENC ceiling** into `PlacementPolicy::with_nvenc_ceiling()` (no
  run code calls it today — `rg` shows only `::new_default()`/`::default()`, so the gate
  is inert), discovered at capability-probe time (consumer GeForce ≈ 8 concurrent
  sessions per *system*; from the SDK caps / static table, **not** NVML).

**Runtime loop — the correct threading (a prior note got this wrong, fold the
correction):** the per-tick `FnMut(&mut CompositorDrive)` hook of `run_with_control`
(`runtime.rs`) runs **ON the output-clock loop, every tick** — its own doc says *"it
runs on the output-clock loop, so a stall there would falter program output (invariants
#1 + #10)."* It is **not** an off-thread slow tick; the "slow control tick" is design
*intent* (`degrade.rs`) and is **unbuilt**. Therefore:
- **Do NOT call `observe()` / `select_device` / `load_source.poll()` from the per-tick
  hook.** Build the missing **slow control tick OFF the output-clock thread**: a separate
  tokio task / std thread that owns `PlacementController` + `ControlLoop`, reads
  `DeviceLoad` from a **`watch` latest-slot fed by the EXISTING poller** (no second
  poller), runs `observe()`/`step()` at ~1 Hz, and publishes a small Plan/backend
  decision into a `watch`/non-blocking slot.
- The **per-tick hook then only READS that latest decision** and applies
  frame-boundary-safe, O(1), non-blocking changes (ladder level / backend flag),
  mirroring the existing capped `command_drain` pattern — never `observe()`, never
  `poll()`, never a `Migrate`.
- A `Migrate` is a **Class-2 controlled reset** executed by the supervisor
  make-before-break path, **never on the clock thread** (inv #1/#10 preserved; affinity
  preserved — whole island, never fragment).

---

## 8. UI — health banner + "what runs where"

SENSE is fully wired (`web/src/realtime/useSystemMetrics.ts` parses per-GPU + ours-vs-total
`self_*`; `SystemPage.tsx`/`SystemFooter.tsx` render WCAG value+shape meters — colour is
never the sole signal). WARN + the placement view are missing. Add:

- `useHealth.ts` — folds `health-warning` raised/cleared into a `Map` keyed by `key`,
  drop-oldest, copying `useSystemMetrics.ts`.
- `HealthBanner` mounted in `AppLayout.tsx` under `<header>` — **renders nothing when no
  active warnings** → no false alarm on a clean host.
- `HealthPanel` on `SystemPage.tsx` — severity glyph **+ text** per
  `AlarmsPage.severityPresentation`, mono `code`, callout `remediation`.
- `usePlacement.ts` + a **"What runs where"** section — a new `placement.snapshot` event
  carries per pipeline+tile `{decode {backend, device_id}, composite {…}, encode {…}}`
  **keyed to `GpuMetrics.id`** so the UI joins each stage's landing site to its live
  ours-vs-total load.
- Nav-item active-warning **count badge** — text, not colour (WCAG 1.4.1).

The "GPU present but unused" callout is UI-derivable (`gpus.length > 0 && composite == CPU
&& low self_compute_util`), but **prefer the engine-emitted
`gpu-present-no-vulkan-adapter` warning** when present (it carries the real remediation).
For the session-ceiling gate and the UI to have real values, `GpuMetrics` must populate
`name` (today hardcoded `None`, `system_metrics.rs`) and `encoder_session_ceiling` (today
hardcoded `None` — note this is a **display** field, *orthogonal* to the placement gate's
`PlacementPolicy` ceiling source).

---

## 9. Invariant audit (verified, not asserted)

- **Inv #1 (output never stalls):** capability probing + initial placement are
  build-time (clock not constructed); the runtime re-plan runs on a separate slow control
  tick off the output-clock thread; a failed/absent GPU still falls back to CPU (the run
  proceeds). The per-tick hook only *reads* a latest decision. ✔
- **Inv #9 (resource-adaptive degradation):** Increment 1 changes no runtime control loop.
  Phase 2 reuses the existing cheapest-first ladder + `Hysteresis`; the
  program-affecting boundary (`affects_program`) gates severity escalation. ✔
- **Inv #10 (no back-pressure):** every emission (`HealthWarning`, `PlacementSnapshot`)
  goes through the **same drop-oldest `EnginePublisher::publish_event`** as
  `SystemMetrics` (a single non-blocking `broadcast::send`); `warning_ingest` swallows-
  and-skips, `Lagged` resubscribes at head; the phase-2 `watch` load-slot is **read with
  `borrow()`/`try_recv`** (wait-free latest-slot), never `.changed().await` on the clock
  loop. A CI chaos/soak test must assert that flooding `HealthWarning` + `PlacementSnapshot`
  while every subscriber is stalled delays no output tick. ✔
- **GPU-placement principle:** affinity is a hard gate; load only *informs*; a single
  pipeline is never fragmented mid-flight; a `Migrate` is a whole-island Class-2
  make-before-break, never on the clock thread. ✔ (runtime affinity-preservation is
  *latent* until phase-2 wiring + its chaos test.)

---

## 10. Smallest-first increment vs full closed loop

**Increment 1 — static capability-aware placement + warning surface (ship first):**
DETECT (`CapabilityReport` + the three build-time probes + the NVML-vs-usable
cross-check) → WARN (`HealthWarning` sibling + `WarningCode` catalog + the new event
variants + AsyncAPI/OpenAPI registration + `GET /api/v1/health` + `WarningRepository` /
`warning_ingest`) → APPLY (replace the `RunBackend::select(true)` line with the
report-gated `select_device` composite decision; emit the mismatch warning; log at
`warn`) → UI (`useHealth` + `HealthBanner` + `HealthPanel` + nav badge). **This alone
kills the silent CPU fallback** — the decision becomes explicit and reportable. It is
bounded, build-time, and **cannot regress inv #1/#9/#10** (no runtime control-loop
change), delivering the operator's headline ask.

**Full closed loop (phase 2, depends on Increment 1):** decode + encode device placement
behind the same `select_device` `Selection` (affinity by construction); the missing
**slow control tick** owning `PlacementController`/`ControlLoop`; the `watch` load-slot
fed by the existing poller; `observe()` on the slow tick; measured-telemetry pressure
refining `pressure_from_plan`; runtime re-plan with hysteresis off the clock thread; the
`placement.snapshot` event + "What runs where" UI; and the chaos/soak test asserting no
fragmentation and no in-flight migration.
