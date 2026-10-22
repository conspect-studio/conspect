# ADR-M014: SA-1+ vendor-caps deep probe — rich per-device/host capability telemetry (extends ADR-W030)

- **Status:** Proposed <!-- flips to Accepted when the #180-A code lands (repo #97 pattern) -->
- **Area:** Management (HAL / Control / Web)
- **Date:** 2026-07-11
- **Source:** task #180 (the SA-1+ deep-probe half deferred from #9/#176 by [ADR-W030](ADR-W030.md));
  [management-capability-matrix](../research/management-capability-matrix.md) §3.4; [ADR-M007](ADR-M007.md)
  (CapabilityReport gate); [ADR-0017](ADR-0017.md) (`DeviceLoad` live-load model); [ADR-0035](ADR-0035.md)
  (self-aware placement, SA-1+); read-only design pass (agent session, 2026-07-11); cross-vendor design
  review (Codex, 2026-07-11); operator direction (design-first, rule 3)

## Context

[ADR-W030](ADR-W030.md) shipped the **honest default-build** capability surface —
`GET /api/v1/system/capabilities` returning `multiview_control::system::SystemCapabilities`
(codec backends, compositor class, effective licence, NDI attribution) — and **deliberately
omitted** the rich [§3.4](../research/management-capability-matrix.md) telemetry (per-device codec
profiles/levels, NVENC session budget, VRAM, engine topology, host cgroup/PSI). W030 named that
omission **task #180**, "blocked by this" (W030 §"the default-vs-SA-1+ boundary"), because it needs
feature-gated backend-crate code and **GPU-hardware validation** (rule 26). This ADR is the design
record for #180.

**The load-bearing reframe (verified read-only against `multiview-hal/src/load.rs`, 2026-07-11):
the *live* half of the telemetry already exists and is trusted by the placement path — but only a
*narrow static slice* of what #180 surfaces is already produced.** Being precise about that slice is
the whole A/B split below; the earlier draft over-claimed it, so this records the verified fields.

[ADR-0017](ADR-0017.md)'s `DeviceLoad` (`load.rs:220-250`), produced by the runtime-loaded NVML
poller (`nvml-wrapper`, feature-gated `cuda`/`vaapi`/`qsv`) on a ~1 s **off-hot-path** tick and
consumed by `select.rs`, carries **exactly** these fields:

- **live gauges** (excluded from caps — §1): `gpu_busy_frac`, `vram_used_bytes`, `enc_util_frac`,
  `dec_util_frac`, `nvenc_session_count` (the live *count*, not a ceiling), `compute_busy_frac`.
- **static facts** (surfaceable): the `DeviceId` — `vendor`, `stable_id`, `pci_bus_id` — and
  `vram_total_bytes`.

That static slice — **device identity + total VRAM** — is the *only* part of the rich §3.4 surface
already produced and hardware-validated today. Everything else §3.4 wants — device **model**, **driver
version**, **engine topology** (NVENC/NVDEC counts), the NVENC **session-cap ceiling**, and **per-codec
profiles/levels** — is **not** in `DeviceLoad` and needs a **new vendor query** (`nvEncGetEncodeCaps`,
`nvmlSystemGetDriverVersion`, VAAPI/oneVPL/VT equivalents) that has **never been exercised**. (The
device *name* is a partial exception: the sibling `NvmlLoadProbe::device_perf` read
(`PerfSignals.name`, `load.rs:1029`) already calls `nvmlDeviceGetName` — but it is surfaced with the
rest of the per-device vendor-caps block in §4, not with the `DeviceLoad` slice.)

The genuinely-new **GPU-free** work is therefore bounded to: **(a)** a **host** block (OS/arch/cores/RAM,
cgroup limits, PSI/thermal-sensor *presence*), pure `std`/`/proc`/`/sys`; and **(b)** plumbing the
already-produced `DeviceLoad` static slice to the API. The deep vendor-caps queries are a separate,
GPU-runner-gated slice.

Binding constraints: invariant **#1** (output clock never blocked), invariant **#10** (the control
plane must not couple to / back-pressure the engine — and control keeps **zero** dependency on
`multiview-hal`, the #263 / W030 boundary), **rule 6** (no modelled-but-unfilled fields; a seam with
only mock impls is a scaffold), **rule 26** (vendor queries validate on real GPU hardware), **rule 27**
(no aspirational reporting; report provenance, never a stale-as-current lie), and the LGPL-clean
licensing model ([AGENTS.md §G](../../AGENTS.md) / [ADR-0012](ADR-0012.md)).

## Decision

Extend the capability surface additively and assemble it, as W030 did, from what the running
binary actually knows — never a serialized phantom type.

### 1. The invariant-#10 static-vs-live boundary (the whole safety story)

The caps DTO carries **static / semi-static** fields **only**:

- **static:** device vendor / stable-id / PCI-bus-id, `vram_total`, engine topology, device model,
  driver version, unified-memory flag, per-codec profiles/levels/bit-depth; host
  OS/arch/CPU-cores/`available_parallelism`/`total_ram`, cgroup `cpu.max` / `memory.max`, **PSI
  availability** and **thermal-sensor presence**.
- **semi-static:** the NVENC **session-cap ceiling** (a host-wide, driver-derived number — captured
  as the *ceiling*, never the live count).

The **live** gauges — VRAM *free*, NVENC sessions *used/available*, engine *utilisation %*, host PSI
*values*, thermal *readings* — **stay on the telemetry stream** (`Event::SystemMetrics`,
[ADR-0035](ADR-0035.md) / [ADR-RT004](ADR-RT004.md)) and are **excluded** from the caps DTO.

*Why this is non-negotiable:* folding a live gauge into `/system/capabilities` forces either
**per-request hardware probing** (a read that touches the engine/device — an invariant-#10 violation)
or a **snapshot that silently goes stale** (a rule-27 lie). The endpoint therefore stays exactly what
W030 built: a **static startup snapshot** installed via `AppState::with_capabilities`, clone-on-read,
route unchanged, no engine channel.

**Provenance, not a stale-as-current lie (rule 27).** Some static facts *can* drift after startup (a
cgroup limit re-written at runtime; the driver-derived session-cap ceiling). The snapshot therefore
carries an **`observed_at`** timestamp, and the wire contract states every caps field is a **fact as
observed at process startup** — a *known-vs-unknown* claim, never a *current-vs-stale* one. A consumer
that needs the live value reads the telemetry stream.

**The construction boundary is one-way (invariant #10 at *assembly*, not only at request-time).**
This is a concrete *mechanism*, not just an asserted outcome:

- ***Where* it is assembled — the control plane, never the data plane.** Caps assembly extends the
  **same one-shot `AppState::with_capabilities` seam W030 already fills** (`multiview-cli/src/control.rs`
  — installed there as "a one-shot snapshot, never an engine channel (invariant #10)"). That seam runs
  on the **CLI control-plane bring-up (Tokio / control-IO plane)**, which by [ADR-0009](ADR-0009.md)'s
  two-plane split is **not** the output-clock **data-plane OS thread**.
- ***What* the load pass actually is — a bounded vendor query, not a "non-blocking" read.** The pass is
  a single `LoadSource::poll()` — **a bounded vendor query** (an NVML/DRM `sample_all()` pass over the
  visible devices, per-pass-bounded by `PollInterval` ≈ 4 Hz, via the very `default_load_source()` the
  system-metrics task already selects; `load.rs:657`/`:705`, `system_metrics.rs`). It is **not** a
  cached-snapshot read and **not** a per-request probe — a *bounded startup query*, run off the clock
  thread, exactly once. (An NVML call is not "non-blocking"; it is *bounded* — the earlier draft's
  wording is corrected here.) Add the pure `host.rs` probe, stamp `observed_at`, install the immutable
  snapshot (clone-on-read).
- ***The ordering that guarantees engine-start-independence.*** The engine's `OutputClock` runs on its
  **own dedicated data-plane OS threads** and is **neither awaited by nor awaits** caps assembly. Caps
  borrows **no engine lock, channel, or poller** — it polls its *own* `LoadSource` (the CLI's,
  physically distinct from the engine's placement poller), so it is *structurally* incapable of coupling
  to or back-pressuring the engine. The **absent-fallback** — no accelerator (or a pure-Rust build) →
  `NullLoadPoller` → empty `poll()` → empty `devices` / `None` — is served cleanly as an *honest absent
  state*, never a "not-yet-ready" placeholder.

The **live** gauges stay on the **separate `Event::SystemMetrics` broadcast** the system-metrics task
publishes (drop-oldest, inv #10) — never the caps snapshot.

### 2. Schema — additive-only to `multiview_control::system` (primitives/enums, zero hal dep)

Add to `SystemCapabilities` (the container `#[non_exhaustive]`,
`#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]`). The optional / collection fields carry
`#[serde(default, skip_serializing_if = …)]` so an old client that omits them still round-trips —
**with one deliberate exception: `observed_at` is always serialized** (below). Fields are tagged
**[A]** (#180-A) or **[B]** (#180-B) per §4:

- `observed_at: <rfc3339 string>` [A] — startup capture time (provenance, §1). **Always serialized —
  no `skip_serializing_if`** — so it is a **required** field in the OpenAPI schema. It is stamped on
  *every* snapshot (even the absent-fallback carries a real capture time), and the §1 rule-27
  provenance guarantee depends on it being present on **every** response; letting it be omitted would
  make the anchor optional and re-open the stale-as-current gap. `devices` / `host` / `detection` stay
  optional / defaulted.
- `devices: Vec<DeviceCapability>` [A] — per detected accelerator.
- `host: Option<HostInfo>` [A] — the machine block.
- `detection: DetectionInfo` [A] — host-global probe status per layer.

New DTOs (every telemetry field `Option`/`Vec`, **"unknown is first-class, never a fabricated zero"**):

- `DeviceCapability { vendor [A], id [A], pci_bus_id? [A], vram_total_bytes? [A], model? [B],
  driver_version? [B], unified_memory? [B], engines?: EngineTopology [B], nvenc_session_cap? [B],
  codecs: Vec<CodecCapability> [B], caps: ProbeStatus [B] }` — the **[B]** fields, the `caps.rs` seam
  that fills them, and its tests all land **atomically** in #180-B (§4); #180-A serializes a
  `DeviceCapability` carrying only the **[A]** fields.
- `CodecCapability { codec, stage /* decode|encode */, max_width?, max_height?,
  profiles: Vec<String>, bit_depth?, bframes? }` [B].
- `HostInfo { os, arch, cpu_cores?, available_parallelism?, total_ram_bytes?, cgroup: CgroupLimits,
  psi: ProbeStatus, thermal_sensors: Option<Vec<String>> }` [A] — `psi` distinguishes present /
  confirmed-absent / failed / not-attempted; `thermal_sensors` is `None` = *not probed* vs `Some([])`
  = *probed, none present* (never conflating absence with probe-failure).
- `DetectionInfo { l1_ffmpeg: ProbeStatus, l3_probe: ProbeStatus }` [A] — the **host-global** layers.
  **L2 (per-device vendor caps) status is *not* global**: it rides each `DeviceCapability.caps` [B],
  because L2 can *succeed* on GPU A, be *unsupported* on B, and *fail* on C.
- `enum ProbeStatus { NotAttempted, Succeeded, Unsupported, Failed }` [A]
  (`#[serde(rename_all = "snake_case")]`, `#[non_exhaustive]`) — **"ran" ≠ "succeeded"**: distinguishes
  *not probed* from *probed-unsupported* from *probed-failed*.

Control keeps **primitive/enum fields only — no `multiview-hal` types** (the #263 / W030 boundary).
The extension is **backward-compatible, not byte-identical**: every existing W030 field serializes
**unchanged**, and the new fields are additive + optional/defaulted, so an old client that ignores
unknown fields still parses the response. (It is **not** byte-identical — `host`, `detection`, and
`observed_at` add keys even on a software build.) Requires `openapi.rs` `ToSchema` registration +
`cargo xtask gen-openapi` (regenerates `docs/api/openapi.json`) + web schema regeneration
(`web/src/api/system.ts`).

### 3. HAL API — new, pure + feature-gated (`multiview-hal`)

- **`host.rs` [#180-A] — pure, GPU-free, CI-testable, no new dependency.** A `HostInfo` probe over
  `std` (`os`/`arch`/`available_parallelism`) + `/proc` + `/sys` + cgroup v2 (`cpu.max`, `memory.max`)
  + `/proc/pressure` presence. Adds **no** crate dependency.
- **`caps.rs` [#180-B, entire] — the deep vendor-query seam.** A serde-free `VendorCaps` value + a
  `VendorCapsProbe` trait **and its real feature-gated impls, introduced together**: `cuda` → NVENC
  `nvEncGetEncodeCaps` (+ NVML `nvmlDeviceGetName` / `nvmlSystemGetDriverVersion`); `vaapi` →
  `vaQueryConfigProfiles` / `vaQueryConfigEntrypoints` / `vaGetConfigAttributes`; `qsv` → oneVPL
  `MFXQueryImplsDescription`; `videotoolbox` → `VTCopySupportedPropertyDictionary…` (coarse — macOS
  telemetry is limited, per M007). Every field `Option`; results **correlate to the existing
  `DeviceLoad`/`DeviceId` by PCI-bus-id / stable-id — never the NVML index** ([ADR-M007](ADR-M007.md)).
  Per rule 6, the trait, its impls, its call site, **and all of its tests** are #180-B — a
  `VendorCapsProbe` with only mock impls would be exactly the scaffold rule 6 forbids (the #36 class).
- **`probe.rs` [#180-B]** — the L2 refinement hook `EnvProbe` defers becomes the `VendorCapsProbe`
  call site (no behaviour change to the L1 presence baseline `EnvProbe` already returns).

The **CLI** (`multiview-cli/src/system_capabilities.rs`) remains the hal→DTO **bridge** (it already
owns the hal dependency). For **#180-A** it maps one off-hot-path `LoadSource::poll()` snapshot's
static slice (§1) + the `host.rs` probe into the DTO and installs it via `AppState::with_capabilities`
— the same static one-shot as W030. For **#180-B** it additionally correlates the `VendorCapsProbe`
output by `DeviceId`.

### 4. The #180-A / #180-B decomposition (rule 6 + rule 26)

The line is drawn on one question: **does the field need a vendor query the live-load path does not
already make?**

- **#180-A — host block + surface the `DeviceLoad` static slice.** Precisely: per-device
  `{ vendor, id (stable_id), pci_bus_id?, vram_total_bytes? }` (the **verified** `DeviceLoad`/`DeviceId`
  static fields, `load.rs:220-250`); the `HostInfo` block; `observed_at`; `DetectionInfo` (L1/L3);
  `ProbeStatus`. **GPU-free-completable:** the host reads are pure `std`/`/proc`; the device slice
  re-uses the [ADR-0017](ADR-0017.md) NVML snapshot already GPU-validated in production and adds **no
  new vendor query**, so it is exercised in CI against a `DeviceLoad`/`LoadSource` **fixture** — **no
  new GPU validation** (rule 26 met by *reuse*, not by claim). There is **no `VendorCapsProbe` and no
  mock-probe test** in #180-A. Ships as the **full fanned chain** hal (`host.rs`) → control DTO → cli
  bridge → web, **together**, post-#271 (rule 6: no partial merge).
- **#180-B — the deep vendor-query caps.** All the **[B]** fields of §2: device `model`,
  `driver_version`, `unified_memory`, `engines` (NVENC/NVDEC topology), the `nvenc_session_cap`
  ceiling, `codecs[]` (per-codec profiles/levels/bit-depth/B-frames/max-res), and per-device
  `caps: ProbeStatus` — filled by `caps.rs`'s `VendorCapsProbe` (NVENC / VAAPI / oneVPL / VideoToolbox).
  (`model` is the one field the sibling `device_perf` NVML read already fetches; it is surfaced here,
  with its per-device cohort, so the whole vendor-caps block lands atomically with ≥1 real impl.)
  **Double-gated:** **rule 6** — the seam ships **with real vendor impls, never as a bare/mock-only
  seam**; **rule 26** — the vendor queries have **never been exercised**, so they need **GPU-runner
  validation** (runners currently = 0). #180-B therefore **groups with #198 (HAL-1 FailureLedger IMPL,
  likewise GPU-runner-blocked)** and is held until GPU runners exist.

### 5. Territory + build order

| Lane | Files | Status |
| ---- | ----- | ------ |
| **LANE-GOV** | `docs/decisions/ADR-M014.md` (this) + README index | disjoint, **independently shippable now** (this PR) |
| **LANE-ENG-A** (`multiview-hal`) | `host.rs` (+ its tests) | **#180-A**; pure/GPU-free, authorable now, **no new dep**; ships in the #180-A chain post-#271 |
| **LANE-ENG-B** (`multiview-hal`) | `caps.rs` (`VendorCaps` + `VendorCapsProbe` + real impls + **all its tests**), `probe.rs` call site | **#180-B**; **GPU-runner-blocked** (rule 26), grouped with #198 — not authorable to "done" now |
| **LANE-API** (`multiview-control`) | `system.rs` (DTO extension), `openapi.rs`, `docs/api/openapi.json` | **#271-locked** (frees when #271 merges) |
| **LANE-CLI** (`multiview-cli`) | `system_capabilities.rs` (bridge) | disjoint file, compile-depends on the control DTO → sequences **after** LANE-API |
| **LANE-WEB** | `CapabilitiesPage` (DevicesPanel/HostPanel), `web/src/api/system.ts` | after API (schema regen) |

**rule 6:** #180-A lands as **one** fanned PR (hal `host.rs` → control → cli → web) post-#271; #180-B
lands as a **second** fanned PR (`caps.rs` + the **[B]** DTO fields + cli correlation + web) once GPU
runners exist. No partial merge in either.

### 6. TDD plan (red-first, rule 18)

**#180-A (GPU-free → CI-green):**

- **hal:** `HostInfo` fixture tests (parse `/proc` + cgroup fixtures; missing files → `None`;
  `psi` / `thermal_sensors` distinguish *not-probed* from *confirmed-absent*).
- **cli:** bridge correlation with a **mock `LoadSource`** yielding a fixture `DeviceLoad` → assembled
  `devices[]` static slice; assert no fabricated zero, and **empty poll → empty `devices`**
  (absent-fallback, §1). The bridge assembles from a **single injected `LoadSource::poll()` pass** (its
  own source, not the engine's poller) + the `host.rs` probe + an `observed_at` stamp — the mock
  exercises the one-shot, own-source construction boundary (§1) with no engine handle in scope.
- **control:** additive-DTO serde round-trip + **backward-compat** — assert every existing W030 field
  serializes **unchanged** and the new fields are additive/optional (old-client-ignores-unknown
  parses); **not** a byte-identity assertion. Assert **`observed_at` is always present** — serialize a
  snapshot with empty `devices` + `None` host and confirm the `observed_at` key is still emitted (the
  provenance anchor is never skipped; §2).
- **web:** DevicesPanel/HostPanel render + **empty-state** (no devices → honest "no accelerators
  detected", never an aspirational gauge); vitest.
- **route:** `GET` returns the snapshot under the viewer/read gate (no engine touch).

**#180-B (adds the GPU-runner-validated slice):**

- **hal:** mock `VendorCapsProbe` (present / absent / partial / **unsupported / failed** arms) — assert
  correlation-by-`DeviceId`, per-device `caps: ProbeStatus`, and unknown-first-class (all-`None` →
  honest empty, no fabricated zero); then the **real** impl validated on a GPU runner (rule 26).
- **cli/control/web:** the **[B]** fields plumbed + rendered, with the per-device `ProbeStatus`
  surfaced.

Each slice commits its failing test before implementation.

## Rationale

- **Surfacing beats re-probing — for the slice that is actually already produced.** The costly live
  probe (ADR-0017 `DeviceLoad` via NVML) is validated and trusted by placement; #180-A plumbs its
  **static slice** (device identity + `vram_total`) to the API for free and adds a pure host block. The
  deep vendor caps (model/driver/topology/codecs) are honestly **new** queries, so they carry a real
  GPU-validation gate rather than a false "reuse" claim. One probe feeds both placement and the API (no
  second source of truth, per M007).
- **The static/live split is the only invariant-#10-safe design.** Static facts belong in a startup
  snapshot **with a provenance stamp**; live gauges already have a bounded drop-oldest telemetry
  stream. Mixing them re-introduces the exact coupling W030's snapshot design removed.
- **Additive-only + all-`Option` + per-layer/per-device `ProbeStatus` = rule-6/27 honesty.** No field
  exists until a real probe fills it; the client is told, per host-layer and per device, whether a
  probe was *not attempted*, *succeeded*, *unsupported*, or *failed* — an empty-because-confirmed-absent
  is distinct from an empty-because-unprobed and from an empty-because-unbuilt (forbidden).
- **The A/B split makes the GPU-runner gate real, not a whole-feature blocker.** #180-A delivers useful
  telemetry GPU-free, CI-green; #180-B waits for runners alongside #198.

## Alternatives considered

| Alternative | Rejected because |
| ----------- | ---------------- |
| Re-implement a fresh live probe for the API | Duplicates ADR-0017's validated `DeviceLoad`/NVML poller → two sources of truth (M007 forbids). |
| Fold the live gauges (VRAM free, sessions used, util, PSI values) into the caps DTO | Forces per-request hardware probing (inv #10) or a silently-stale snapshot (rule 27); live telemetry already rides the drop-oldest stream (ADR-0035/RT004). |
| Claim model/driver/topology as "already-validated reuse" in #180-A | Only device identity + `vram_total` are in `DeviceLoad` (`load.rs:220-250`); the rest need a vendor query never exercised → a false rule-26 "reuse" claim. They are #180-B, GPU-gated. |
| Ship the `VendorCapsProbe` seam now with mock-only impls (defer real queries) | A seam with no real impl is a rule-6 scaffold (the #36 class); real vendor queries need GPU validation (rule 26). The seam **and all its tests** are #180-B, shipped with impls or not at all. |
| Put the new DTOs / deep-probe types in `multiview-hal` with `Serialize` | Couples hal to serde/wire and control→hal; breaks the serde-free planner layer and the #263/W030 zero-hal-dep control boundary. The CLI owns the hal→DTO map. |
| Three global `detection` booleans | Cannot express per-device L2 status (succeed A / unsupported B / fail C) and conflate "ran" with "succeeded"; replaced by per-device `caps: ProbeStatus` + host-global L1/L3 `ProbeStatus`. |
| Assert the extended JSON is byte-identical to the W030 snapshot | False once `host` / `detection` / `observed_at` are present; the correct contract is additive backward-compat (old clients ignore unknown fields). |
| Assert caps is built "off the output-clock thread" as an *outcome*, without naming the *mechanism* | Rules 16/27: an unmechanized safety claim is unverifiable and mis-implementable. §1 names the seam (control-plane `AppState::with_capabilities`, the ADR-0009 control-IO plane), the **bounded-vendor-query** nature of the single `LoadSource::poll()` pass (not a "non-blocking" read), and the own-source / no-engine-handle ordering that makes engine-independence structural. |
| Let `observed_at` use `skip_serializing_if` like the other additive fields | Makes the OpenAPI field optional and lets a response omit the provenance stamp — re-opening the §1 stale-as-current gap. It is always stamped at startup, so it is **always** serialized (a required schema field); the others stay optional/defaulted. |
| One monolithic #180 PR (A + B together) | B is GPU-runner-blocked (runners = 0); bundling parks A's GPU-free-shippable telemetry behind B indefinitely. |

## Consequences

- **New surface:** hal `host.rs` (`HostInfo`, #180-A) and `caps.rs` (`VendorCaps`, `VendorCapsProbe`,
  #180-B); additive `multiview_control::system` DTOs; OpenAPI + web schema regeneration. New schemas
  must be registered or codegen drifts (the W030 lesson).
- **Committed to maintaining the static/live boundary + provenance:** any future "add X to
  capabilities" must classify X static-vs-live (route a live X to telemetry, never the caps DTO) and,
  if X can drift, rely on `observed_at` rather than implying "current".
- **Invariants #1/#10 preserved:** a static startup snapshot **assembled on the control plane (not the
  output-clock data-plane thread) from a single bounded `LoadSource::poll()` pass over its own source**
  (§1), no engine lock/channel/poller borrowed, control stays hal-free by construction (the CLI owns the
  map). The engine is untouched.
- **rule 26:** #180-A validates GPU-free (host fixtures + a mock `LoadSource`); #180-B carries a
  GPU-runner validation gate (grouped with #198). CI stays green on the default/software build (empty
  `devices`, `host` filled).
- **Licensing (LGPL-clean, deny-clean).** #180-A's `host.rs` adds **no** dependency (pure
  `std`/`/proc`/`/sys`). All #180-B vendor queries live in **off-by-default, feature-gated** code —
  `cuda` (NVML/NVENC), `vaapi` (libva), `qsv` (oneVPL), `videotoolbox` (VT); the default `cargo check`
  adds no native dep and stays pure-Rust/LGPL-clean. Where they land: NVML via `nvml-wrapper` is
  **runtime-loaded** (libloading — no link-time escalation even under `cuda`); libva (MIT), oneVPL
  (MIT), and VideoToolbox (Apple system framework) are permissive; NVENC caps use the MIT
  `nv-codec-headers` already in the `nvidia` preset (ADR-0012). No `gpl-codecs`, no NDI. `cargo deny`
  is re-run and reported **when #180-B actually adds/changes a dependency** (rule 35) — verified on the
  real change, not asserted here.
- **Blocked by:** #271 (frees LANE-API for the control DTO) for **#180-A**; **GPU runners** (with #198)
  for **#180-B**. This ADR flips **Proposed → Accepted** when the #180-A code lands (repo #97 pattern).
