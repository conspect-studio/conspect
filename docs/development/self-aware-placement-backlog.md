# Backlog — Self-aware placement (SENSE → DETECT → WARN → PLAN → APPLY)

PR-sized, dependency-ordered. Grounds: [self-aware-placement.md](../research/self-aware-placement.md),
[ADR-0035](../decisions/ADR-0035.md), [ADR-0018](../decisions/ADR-0018.md) (planner),
invariant #9 (degradation), the GPU-placement principle. **SA-0 is the smallest shippable
win — it makes the silent CPU fallback a clear, actionable UI message.** Each item is
TDD-first (failing test committed separately), keeps the default build GPU-free/LGPL-clean,
and must not regress inv #1/#9/#10.

> Citation note: the placement engine is **ADR-0018**, not ADR-0027. ADR-0027 = synthetic
> sources; ADR-0026 = encode-once-mux-many.

## Increment 1 — static capability-aware placement + warning surface

### SA-0 — DETECT+WARN the compositor mismatch (the smallest win; ship first)
- **Crates:** `multiview-hal` (probe), `multiview-events` (event), `multiview-control`
  (REST + ingest), `web/`.
- **Do:** at pipeline build, read `GpuContext::adapter().get_info()` (the unused lever)
  and resolve composite usability (`usable = adapter present AND device_type !=
  wgpu::DeviceType::Cpu`; treat `Gl` as software only when `device_type == Cpu` or the
  `driver` matches llvmpipe/softpipe/swiftshader; `{VirtualGpu,Other}` → warn at lower
  severity). Cross-check against discovered hardware (`DeviceLoad` ≥1 GPU **OR** `EnvProbe`
  Present, independently of which feature is compiled). On MISMATCH, emit a **latched**
  `HealthWarning` with code `gpu-present-no-vulkan-adapter` (message + the
  `NVIDIA_DRIVER_CAPABILITIES=graphics` / `libvulkan1` remediation) and **log at `warn`,
  not `info`**. Add a minimal `Event::HealthWarningRaised`/`Cleared`, route on
  `Topic::Alerts`, add `GET /api/v1/health`, and a `HealthBanner` that renders the
  warning (nothing when clean).
- **Reuse:** `Alert` machinery (key dedupe + `active` coalescing + `Topic::Alerts` +
  drop-oldest publisher); `alarm_ingest`/`AlarmRepository`/`routes/alarms.rs::list_alarms`
  as the copy-source; `useSystemMetrics.ts` as the hook template; `system_metrics.rs`
  SENSE for the "GPU discovered" half.
- **Out of scope:** decode/encode probes, `select_device` wiring, the full report table,
  the runtime loop. SA-0 is the compositor mismatch only.
- **Inv:** build-time, pure; emits through the drop-oldest publisher (inv #10); CPU
  fallback preserved (inv #1).
- **DoD:** on a no-Vulkan GPU host the run still composites on CPU **and** the operator
  sees a clear banner + `/api/v1/health` entry + `warn` log. On a GPU-free or
  software-only host: **zero** warnings. Test the both-halves-required cross-check.

### SA-1 — `CapabilityReport` type + composite probe (formalise SA-0's probe)
- **Crate:** `multiview-hal` (`capability` module).
- **Do:** `CapabilityReport` = per-`(Stage, DeviceId)` `Vec<CapabilityRecord {stage,
  backend, device {id, name, vendor}, present, usable, in_use, reason}>`, aligned with the
  ADR-M007 shape. Move SA-0's composite probe into it as the first stage; carry the
  cross-check as a method on the report. Built once at build time, refreshable on re-plan.
- **Reuse:** `EnvProbe`/`ProbeOutcome` (presence), `DeviceLoad`/`SelfShare` (NVML),
  `RunBackend::kind()`/`is_gpu()`.
- **Inv:** off the output-clock thread.
- **DoD:** report assembled with composite `usable/in_use/reason`; SA-0's banner now reads
  from the report. Property test the cross-check truth table.

### SA-2 — Decode + encode usability probes (throwaway open)
- **Crates:** `multiview-ffmpeg` (probe helpers), `multiview-hal` (record).
- **Do:** add throwaway-open probes — `ctx.open_as(codec)` for `h264_nvenc` and the
  `*_cuvid` decoder at target res/format, then drop — distinguishing "registered"
  (`find_by_name`) from "session-openable" (`encode.rs::open_as`). Fill the Decode/Encode
  rows of `CapabilityReport`. Emit **latched** `software-decode-on-gpu-host` /
  `software-encode-on-gpu-host` on mismatch.
- **Reuse:** `codec.rs::find_by_name`, `encode.rs::open_as`, `hwdecode.rs::select_decoder`.
- **Inv:** build-time; bounded one-open-and-drop cost.
- **DoD:** on a host with NVENC linked-but-unopenable, encode row is `present=true,
  usable=false` and the warning fires; on a working NVENC host, no warning.

### SA-3 — Full `HealthWarning` model + `WarningCode` catalog + surface
- **Crates:** `multiview-events`, `multiview-control`, `multiview-telemetry`.
- **Do:** finalise `HealthWarning` (richer **sibling** of `Alert`: add `code`, `subsystem`,
  `remediation`, `since`, `current`/`expected`; **never mutate `Alert`**). `WarningCode`
  `#[non_exhaustive]`: the seven catalog codes. Register both new `Event` variants in
  `asyncapi.rs` + `openapi_schemas.rs`. `GET /api/v1/health` with `?severity`/`?active` +
  rolled-up worst severity. `WarningRepository` + `warning_ingest` (copy `alarm_ingest`
  exactly: swallow-and-skip, `Lagged` resubscribes at head). Telemetry: surface in
  `/metrics`, **not** `/livez`/`/readyz` (ADR-R009).
- **Reuse:** `Alert`/`AlertSeverity`, `Topic::Alerts`, `routes/alarms.rs`,
  `alarm_ingest`/`AlarmRepository`, `degradation::Hysteresis` (dwell for metric codes).
- **Inv:** inv #10 — same drop-oldest publisher; ingest never back-pressures.
- **DoD:** all seven codes round-trip event→ingest→`/api/v1/health`; AsyncAPI/OpenAPI
  codegen green; chaos test: flooding warnings while subscribers stall delays no tick.

### SA-4 — Metric-threshold + degradation warnings (dwelled / controller-gated)
- **Crates:** `multiview-hal` (thresholds), `multiview-engine` (degradation gate).
- **Do:** wire `vram-pressure`, `nvenc-session-ceiling-hit`, `sustained-cpu-saturation`
  (dwelled via `Hysteresis`-style raise-after-N/clear-after-M off `DeviceLoad`/`SelfShare`),
  and `degradation-active` (controller-gated by `Hysteresis::level() > 0`, Info→Critical at
  `DegradationAction::affects_program()` = first program rung). Fix the stale "rung 4"
  comments in `degrade.rs`/`tests/degrade.rs` to name `FasterEncoderPreset`.
- **Reuse:** `DeviceLoad::vram_used_frac`, `nvenc_session_count`, `SelfCpuSampler`,
  `degradation.rs` ladder + `Hysteresis`.
- **Inv:** metric codes fire only on sustained samples; off the output-clock thread.
- **DoD:** dwelled codes raise/clear with the asymmetric N/M; `degradation-active`
  escalates exactly at the program boundary. **Note:** keep `degradation-active` on the
  structural `pressure_from_plan` path; if ever fed live telemetry, add EWMA + sustained
  dwell (`PlacementController::note_sustained`) before `step()`.

### SA-5 — APPLY the report-gated composite placement (kill the silent fallback for real)
- **Crate:** `multiview-cli` (`pipeline.rs`).
- **Do:** replace the `RunBackend::select(true)` line: assemble candidates +
  `PipelineDemand` from the `CapabilityReport` (peak tile res from solved layout, per-stage
  `TileLoad` from `cost.rs`, predicted pool bytes, `opens_encode_session`) + one
  `DeviceLoad` snapshot from `load_source.poll()`; call `select_device(...)`; map the
  `Selection` to GPU compositing **only** when the chosen device is usable per the report,
  else CPU; stash the chosen `DeviceId`; emit the mismatch `HealthWarning`. **Keep the CPU
  fallback** (inv #1).
- **Reuse:** ADR-0018 `select_device` (built, tested — first run-code caller),
  `cost.rs::TileLoad`, the existing poller's `DeviceLoad`.
- **Inv:** pure, synchronous, build-time (clock not constructed) → cannot touch inv #1/#10.
- **DoD:** the composite backend decision is explicit, report-gated, and reported; the
  silent `info`-only fallback is gone; `select_device` has its first run-code caller.

### SA-6 — UI: HealthPanel + nav badge + "what runs where" v1
- **Crate:** `web/`.
- **Do:** `useHealth.ts` (folds `health-warning` raised/cleared into a `Map` keyed by
  `key`, drop-oldest); `HealthPanel` on `SystemPage.tsx` (severity glyph **+ text** per
  `AlarmsPage.severityPresentation`, mono `code`, callout `remediation`); nav text-count
  badge (WCAG 1.4.1, not colour); a v1 "What runs where" reading the stashed composite
  `{backend, device_id}` from SA-5 joined to `GpuMetrics.id`.
- **Reuse:** `useSystemMetrics.ts` (hook template), `SystemPage.tsx`/`SystemFooter.tsx`
  (WCAG meters), `AlarmsPage.severityPresentation`.
- **DoD:** Playwright e2e — on the mismatch fixture the panel + banner + badge render with
  the remediation text; clean host renders nothing. (Per repo guidance, drive a real
  browser for SPA verification.)

**End of Increment 1: the operator's headline ask is delivered — no runtime control-loop
change, no regression to inv #1/#9/#10.**

## Phase 2 — full closed loop (depends on Increment 1)

### SA-7 — Decode placement in `IngestPlan` (fused `-resize`, affinity-preserved)
- **Crates:** `multiview-cli` (`IngestPlan`), `multiview-ffmpeg` (`hwdecode.rs`).
- **Do:** `IngestPlan` carries decode `{backend, DeviceId}`; `StreamVideoDecoder::new`
  grows a backend param; wire `select_decoder` with a **fused `-resize`** (inv #6,
  decode-at-display-resolution). The backend+device come from the same `select_device`
  `Selection` so affinity holds by construction.
- **Reuse:** ADR-0018 `Selection`, `hwdecode.rs::select_decoder` (built, currently
  unwired).
- **DoD:** decode lands on the planner-chosen device; software fallback still works +
  warns.

### SA-8 — Encode placement (NVENC-vs-x264 by report+ceiling, pin GPU)
- **Crates:** `multiview-cli` (`resolve_encoder`), `multiview-hal` (`PlacementPolicy`).
- **Do:** `resolve_encoder` chooses NVENC-vs-x264 by report + session ceiling and **pins
  the GPU**. Discover the per-system NVENC ceiling at capability-probe time (consumer
  GeForce ≈ 8 concurrent sessions per *system*; from SDK caps / static table, **not** NVML)
  and wire it into `PlacementPolicy::with_nvenc_ceiling()` (no run code calls it today, so
  the gate is inert). Populate `GpuMetrics.encoder_session_ceiling` (display, orthogonal to
  the gate's policy ceiling).
- **Reuse:** `codec.rs` candidate ordering, the device-wide `nvenc_session_count` gate
  (`select.rs`).
- **DoD:** a co-tenant NVR near the ceiling gates our encode to another GPU; encode pins a
  `DeviceId`; the gate is reachable from run code.

### SA-9 — The slow control tick (the missing off-thread loop)
- **Crate:** `multiview-engine` + `multiview-cli` (wiring).
- **Do:** build the **off-output-clock-thread** slow control tick: a separate task/thread
  owning `PlacementController` + `ControlLoop`, reading `DeviceLoad` from a `watch`
  latest-slot fed by the **existing** poller (no second poller), running `observe()` /
  `step()` at ~1 Hz, publishing a small decision into a `watch` slot. The per-tick
  `FnMut(&mut CompositorDrive)` hook only **reads** that decision and applies O(1),
  non-blocking, frame-boundary-safe changes (ladder/backend), mirroring `command_drain`.
  **Never** call `observe()`/`poll()`/`select_device`/a `Migrate` from the per-tick hook —
  it runs on the output-clock loop.
- **Reuse:** `PlacementController::observe`, `ControlLoop::step`/`pressure_from_plan`
  (inv #9 ladder), the existing poller, the `command_drain` capped-apply pattern.
- **Inv:** the load-bearing correction — the hook is **on** the clock loop; the slow tick
  is the new off-thread mechanism. `watch` read with `borrow()`/`try_recv`, never
  `.changed().await` on the clock loop (inv #1/#10).
- **DoD:** chaos/soak test — sustained pressure re-plans on the slow tick without delaying
  a single output tick; no proposal fragments a live island; a pinned/in-flight pipeline
  never migrates (affinity-preservation made live, not just latent).

### SA-10 — Migrate path + measured-telemetry pressure refinement
- **Crate:** `multiview-engine` (supervisor).
- **Do:** execute `Migrate` as a whole-island **Class-2 make-before-break** on the
  supervisor path (ADR-R004), never on the clock thread. Refine `pressure_from_plan` with
  measured telemetry (with EWMA + sustained dwell before feeding `step()` when live
  telemetry is the input).
- **Reuse:** ADR-R004 migration, `PlacementController` proposals, `Hysteresis` /
  `note_sustained`.
- **DoD:** a migration relocates a whole island to a different GPU home with make-before-
  break and no output stall; cheapest-first ordering preserved (inv #9).

### SA-11 — `placement.snapshot` event + full "what runs where" panel
- **Crates:** `multiview-events`, `web/`.
- **Do:** add a `placement.snapshot`/`placement.changed` event (per pipeline+tile
  `{decode, composite, encode}` each `{backend, device_id}`) keyed to `GpuMetrics.id`;
  `usePlacement.ts` + the full "What runs where" section joining each stage's landing site
  to its live ours-vs-total load.
- **Reuse:** the drop-oldest publisher, `useSystemMetrics.ts`, the SA-6 v1 panel.
- **DoD:** the UI shows decode/composite/encode device per pipeline next to the live
  `self_*` load on that device; updates on re-plan.
