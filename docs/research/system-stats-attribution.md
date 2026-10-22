> **Design brief — System Resource Attribution: our load vs co-tenant.** Authoritative
> research/design record for a Frigate-style system-stats surface that visually separates
> *this Multiview instance's* resource use from *everyone else's* on the same box — who owns
> the encoder, why the CPU is pegged, whether the idle GPU is ours to use. Produced for the
> 2026-06-13 operator feature-request intake. This is a **design for an unbuilt UI/attribution
> surface**; sections describing current Multiview behaviour are verified against the code, and
> every reference to *existing* code names a real, verified path. Where a fact could not be
> verified against an external source it is prefixed **(unverified)**.

> **Vendor posture.** Frigate is named here **only** as an open-source *conceptual* reference
> for the shape of a system-stats page (per-process CPU/memory rows, per-GPU charts, a detector
> chart). Multiview copies no Frigate code, layout, or trademarked term; it builds the same
> *kind* of operator view from open OS/vendor telemetry (NVML, Linux `/proc`, cgroup v2, DRM
> fdinfo) and its own already-shipped event model. See [CODE_OF_CONDUCT.md](../../CODE_OF_CONDUCT.md).

# Multiview — System Resource Attribution: Our Load vs Co-Tenant (Encoder Ownership, CPU/GPU)

**Area:** Telemetry / HAL / Control / Web (Efficiency-adjacent)

**Status:** Design brief (Proposed) — docs-only; implementation follows in dependency-ordered waves.

**Drives:** [ADR-0061](../decisions/ADR-0061.md) (per-process vs system-wide resource attribution + the stats UI).

**Extends:** [self-aware-placement.md](self-aware-placement.md) / [ADR-0035](../decisions/ADR-0035.md) (the `HealthWarning` model, `GET /api/v1/health`, the ours-vs-total `self_*` signals, the silent GPU→CPU-fallback detector), [gpu-monitoring-and-scheduling.md](gpu-monitoring-and-scheduling.md) / [ADR-0017](../decisions/ADR-0017.md) (the per-device live-load probe + per-GPU gauges), [gpu-placement-engine.md](gpu-placement-engine.md) / [ADR-0018](../decisions/ADR-0018.md) (placement reasons net-of-co-tenant load), [web-api-stack.md](web-api-stack.md) (the realtime/REST exposure stack). Sibling intake brief: [observability-logging.md](observability-logging.md) (the structured-log lane; this brief is the *metric/attribution* lane, not the log lane).

**Backlog:** `STATS-*` in [`../development/feature-intake-2026-06-13.md`](../development/feature-intake-2026-06-13.md).

> The operator's ask, verbatim in spirit: *system stats must clearly, visually distinguish
> **this** instance's load from everyone else's — who is using the encoder (us? someone else?),
> why is CPU pegged (that's not us?), is the RTX 4060 idle because we can't use it or because we
> simply aren't?* The honest finding is that **most of the attribution plumbing already ships**
> (per-process NVML/NVENC, DRM fdinfo, `/proc` self-vs-host CPU, the `GpuMetrics.self_*` wire
> fields, the `HealthWarning` model). What is **missing** is (a) a dedicated, Frigate-style
> **stats page** that turns those already-emitted ours-vs-total numbers into the operator's
> "us vs them" picture across CPU, GPU, encoder, decoder, and memory; (b) a **per-co-tenant**
> CPU/encoder breakdown (today we know *our* share and the *host total* — not *which other
> process* is the culprit); and (c) a **cgroup-v2-aware** CPU denominator so "the host is pegged
> but not by us" is correct inside a container. This brief specifies that surface and grounds it
> in the existing model so we do not build a second source of truth.

---

## 0. Headlines

- **The model is the message: every resource gets an *ours / co-tenant / free* split.** Across
  CPU, GPU compute, NVENC, NVDEC, and VRAM the stats page should render three stacked bands —
  **ours** (this instance), **co-tenant** (everyone else), **free** (headroom) — so a single
  glance answers "is that load us?" This is the same net-of-co-tenant reasoning the placement
  engine already does ([gpu-placement-engine.md](gpu-placement-engine.md) §0); the page is its
  operator-facing mirror.

- **Most attribution data already exists and is on the wire — do not rebuild it.** `GpuMetrics`
  (`crates/multiview-events/src/event.rs:148`) already carries, per GPU, the **device-wide**
  totals *and* our share: `self_compute_util` (`:185`), `self_encoder_util` (`:188`),
  `self_decoder_util` (`:191`), `self_encoder_sessions` (`:197`) alongside the totals
  `encoder_sessions` (`:172`) and the discovered `encoder_session_ceiling` (`:175`). `SystemMetrics`
  carries whole-host `cpu_util` (`:209`) **and** our `self_cpu_util` (`:220`). The probe that fills
  them ships (`crates/multiview-hal/src/load.rs`: `DeviceLoad` `:220`, `SelfShare` `:320`; NVML
  per-process pass `:893`–`:956`; the per-process DRM fdinfo media tracker `:1196`). **This brief
  consumes that, it does not re-derive it.**

- **The genuinely new work is three things.** (1) A **stats page** — Frigate-style charts that
  render the ours/co-tenant/free split for every resource, time-series sparklines at the existing
  ~1 Hz cadence, and a per-GPU "what runs where" join. (2) A **per-co-tenant attribution roll-up**
  so the page can name the *biggest other consumer* of CPU/encoder, not just "not us". (3) A
  **cgroup-v2 CPU denominator** so the "that's not us" verdict is correct inside a container.

- **It ties straight into the health warnings (ADR-0035), it does not duplicate them.** The
  `sustained-cpu-saturation` and `gpu-present-no-vulkan-adapter`/`software-*-on-gpu-host` warnings
  (catalogued in [ADR-0035 §5.1](../decisions/ADR-0035.md)) are the *alert* surface; this stats
  page is the *continuous* surface that shows the numbers behind them and links each active
  warning to the chart that explains it. The "RTX-4060-idle-while-5-cores-burn" confusion is
  literally the `gpu-present-no-vulkan-adapter` case rendered as a chart.

- **Read-only, sampled, isolated by construction (inv #1/#10).** Every number here is *sampled*
  off the existing ~750 ms / ~1 Hz poller and published through the same drop-oldest broadcast as
  `SystemMetrics`; nothing on this path can pace, stall, or back-pressure the output clock. The
  page reads the realtime stream; the engine never awaits it.

---

## 1. The need — "the RTX 4060 is idle while five cores burn"

The motivating incident is the one [self-aware-placement.md](self-aware-placement.md) §1 records:
on the GPU test box the wgpu compositor **silently fell back to the CPU reference** (no usable
Vulkan adapter), compositing ran on ~5 CPU cores, and **the RTX 4060 sat ~80 % idle the whole
time** while NVML happily reported it present. The operator's reaction is the requirement: looking
at a system monitor, *"the CPU is pegged — is that us? the GPU looks idle — can we not use it, or
are we just not? who is even on the encoder?"* The product must answer those questions **at a
glance**, on its own stats page, without the operator SSHing in to run `nvidia-smi` and `htop` and
correlate PIDs by hand.

Four distinct confusions the page must dissolve, each with the resource and the discriminator that
resolves it:

| Confusion | Resource | The discriminator that answers it | Where the signal lives |
|---|---|---|---|
| *"CPU is pegged — is that us?"* | CPU | `self_cpu_util` vs `cpu_util` (ours vs host total) | `SystemMetrics.self_cpu_util` `event.rs:220` / `.cpu_util` `:209` |
| *"Who's on the encoder — us or the NVR?"* | NVENC | `self_encoder_sessions` vs `encoder_sessions` (ours vs device total) | `GpuMetrics` `:197` / `:172` |
| *"Is the GPU idle because we *can't* use it, or because we *aren't*?"* | GPU compute | `self_compute_util` low **AND** a `gpu-present-no-vulkan-adapter` warning ⇒ *can't*; low **AND** no warning ⇒ *aren't yet* | `GpuMetrics.self_compute_util` `:185` + `HealthWarning` (ADR-0035) |
| *"VRAM is filling — is that our pools or a co-tenant?"* | VRAM | our `mem_used_bytes` (`SelfShare` `load.rs:333`) vs device `vram_used_bytes` (`DeviceLoad` `:229`) | per-process NVML pass `load.rs:916`–`:949` |

The first three discriminators are **already on the wire**. The page is what turns them into the
answer. The fourth (our VRAM share) is sampled in `SelfShare.mem_used_bytes` but **not yet mapped
onto `GpuMetrics`** for the UI — a small, honest gap §4 records.

**Why a co-tenant story matters at all.** Multiview is explicitly designed to run **alongside**
other tenants — a Frigate NVR, another encoder, a desktop — and the placement engine reasons
*net of* that co-tenant load ([gpu-placement-engine.md](gpu-placement-engine.md) §0: *"whole-system,
fluctuating external load is first-class"*). The operator deserves the same net-of-co-tenant view
the planner already computes. Attribution is therefore not a vanity metric: it is the operator's
window into the exact quantity the scheduler is steering on.

---

## 2. Attribution sources — what each resource can honestly tell us

The honest rule, inherited from [gpu-monitoring-and-scheduling.md](gpu-monitoring-and-scheduling.md)
§1: **attribution availability is vendor- and OS-asymmetric, and unknown is a first-class state,
never a fabricated zero.** This section is the per-resource matrix of *what a per-process / ours-vs-
total split can be derived from*, with each external API verified by name.

### 2.1 GPU: per-process NVML (NVIDIA) — the cleanest path, with GeForce caveats

NVIDIA exposes per-process GPU attribution through NVML:

- **`nvmlDeviceGetProcessUtilization`** returns, per PID, recent SM (compute), memory, **encoder**,
  and **decoder** utilisation as percentages (the `nvmlProcessUtilizationSample_t` fields
  `smUtil`/`memUtil`/`encUtil`/`decUtil`; the newer `nvmlProcessesUtilizationInfo_v1_t` adds JPEG/OFA)
  — verified against the NVML reference [N1]. This is the source for *our* `compute_util`/`encoder_util`/
  `decoder_util` and, by reading **every** PID, for the **per-co-tenant** roll-up §3.2 wants.
- **`nvmlDeviceGetMemoryInfo`** gives device-wide VRAM `{total,free,used}` bytes (the authoritative
  pressure signal — *not* `nvmlDeviceGetUtilizationRates().memory`, which is memory-controller
  busy %, the verified trap in [gpu-monitoring-and-scheduling.md](gpu-monitoring-and-scheduling.md)
  §1) [N1]. Per-process VRAM comes from `nvmlDeviceGet{Graphics,Compute}RunningProcesses` `usedGpuMemory`.
- **`nvmlDeviceGetEncoderStats`** gives the device-wide `(sessionCount, averageFps, averageLatency)`
  — the **total** encode-session count across *all* processes [N1]. The **NVENC concurrent-session
  ceiling** is a per-*system* hard limit, not enumerable via NVML, a moving driver number (8/system
  on Video Codec SDK 13.0; reported as 12 in 2025) — Multiview tracks its own count and discovers
  the ceiling, exactly as [ADR-0017](../decisions/ADR-0017.md) §1.1 records.

**Verified caveats this brief honours.** `nvmlDeviceGetProcessUtilization` and the per-session
enumeration are **frequently `NOT_SUPPORTED` / return 0 on consumer GeForce** [N1, and the
gpu-monitoring brief §5 risk 2]. When the per-process call is blind, the page **must not invent a
co-tenant breakdown**: it falls back to "ours (from our own session bookkeeping) vs device-total vs
free", with the co-tenant band shown as *"other (un-attributable)"* rather than a fake per-PID list.
This is already the posture of the as-built code (every `SelfShare` field is `Option`, `load.rs:320`).

### 2.2 GPU: DRM fdinfo (Intel i915, AMD amdgpu) — per-process, Linux

On Linux, per-client GPU engine busy time is exposed through **DRM fdinfo**: each process's open DRM
client fds carry cumulative-nanosecond `drm-engine-<class>` counters under `/proc/<pid>/fdinfo/<fd>`;
two snapshots a fixed interval apart divided by wall-clock give a busy fraction — verified against the
Linux kernel "DRM client usage stats" documentation [L1] and the i915 fdinfo work [L2]. Multiview
already ships the reader: `FdinfoMediaTracker`/`FdinfoMediaSnapshot` and `parse_fdinfo_merged_media_frac`
(an existing seam in `crates/multiview-hal/src/load.rs`), walking the proc root.

Two reader caveats the DRM usage-stats doc [L1] mandates and the design must honour: (a) where a
driver also exposes `drm-engine-capacity-<class>`, the busy fraction divides the per-class ns delta
by `wall-clock × capacity` (a class can back multiple physical engines), not by wall-clock alone, or a
multi-engine GPU is under-reported; (b) the counters are **not guaranteed monotonic** across snapshots
(driver/client churn can reset or step them back), so a negative delta must clamp to zero / be dropped
as Unknown rather than wrapped — never rendered as a spurious spike.

Two honest limits, inherited from [gpu-monitoring-and-scheduling.md](gpu-monitoring-and-scheduling.md)
§1:

- **AMD merges encode+decode into one "Media engine" figure from VCN4** — the page shows a single
  combined "media" band on such parts, never a fake enc/dec split. The as-built code already sets
  **both** `enc_util_frac` and `dec_util_frac` from the merged term (`load.rs:1319`) — the UI must
  label it honestly as combined, not double-count it.
- **fdinfo is inherently per-process**, so the *device-wide* Intel figure for "everyone else" needs
  either the **i915 PMU** whole-device counters (owned by the `multiview-i915pmu` FFI leaf crate,
  `load.rs:1327`–`:1333`) **or** a walk of *all* PIDs' fdinfo. For the co-tenant roll-up, walking all
  PIDs is the per-process-true path but costs more on tiny boxes (§5); the PMU gives the device total
  cheaply but not a per-co-tenant breakdown. The page degrades to "ours (fdinfo) vs device-total (PMU)
  vs free" when a full PID walk is too costly.

### 2.3 CPU: `/proc` self-vs-host, and the cgroup-v2 denominator

CPU attribution already ships at the **ours-vs-host** granularity: `SelfCpuSampler`
(`crates/multiview-cli/src/system_metrics.rs:251`) differences `/proc/self/stat` (`utime+stime`,
fields 14+15, `:313`) against the `/proc/stat` aggregate `cpu` line (`:330`) over one `SAMPLE_PERIOD`,
yielding our fraction of total host capacity on the **same scale** as `cpu_util` (`:238`–`:242`), so
`self_cpu_util` and `cpu_util` compose directly. That is exactly "is the pegged CPU us?".

Two additions this brief drives:

1. **A cgroup-v2 CPU denominator (containers).** Inside a container, "the host CPU is at 95 %" is
   misleading if our cgroup is *limited* to two cores and is using both — we are not the culprit, the
   host is busy with other cgroups. Reading **`cpu.stat` `usage_usec`** (cgroup-v2 basic CPU accounting:
   `usage_usec`/`user_usec`/`system_usec`, verified against the kernel cgroup-v2 accounting patch series
   [C1]) for our own cgroup, plus `cpu.max` for the quota, lets the page state "**our cgroup**: 1.9/2.0
   cores; **host**: 95 % — the saturation is **outside our limit**." This is the precise "that's not us"
   signal the operator asked for, and it is **read-only** std file I/O — no native dep. The "outside our
   limit" verdict is **only** honest when our cgroup is *below* its quota: if our usage is at/near
   `cpu.max` (or `cpu.stat` `nr_throttled`/`throttled_usec` is climbing) we are saturated *within our own
   quota* and are also contributing to host load, so the page must separate the four quantities — our
   cgroup utilization, quota pressure/throttling, host utilization, and the inferred other-host remainder
   — and only assert "not us" when our quota is *not* the binding constraint. (unverified) the
   exact cgroup mount path resolution (`/sys/fs/cgroup/<...>/cpu.stat`) must handle both unified-hierarchy
   and delegated-container layouts; the reader follows `/proc/self/cgroup` to locate it.

2. **A per-co-tenant CPU roll-up (the "who *is* it then?" answer).** Today we know ours and the host
   total but not *which other process* is burning the CPU. A bounded, off-hot-path walk of `/proc/<pid>/stat`
   for the **top-N** non-self processes (by `utime+stime` delta over the interval) names the biggest
   co-tenant — e.g. *"co-tenant: `ffmpeg` (pid 4123) 310 %, `frigate.detect` 95 %"*. **"Ours" here is the
   whole instance, not just `/proc/self`:** because `self_cpu_util` is sampled from `/proc/self/stat`
   alone, any child/helper/worker process we spawn would otherwise be mis-named as a co-tenant. The walk
   therefore defines the ours-set by membership (our process group / our cgroup, resolved via
   `/proc/self/cgroup`) and **excludes that entire set** from the co-tenant top-N. This is the same
   information Frigate's per-process table shows for its own children [F1]; Multiview shows it for the
   *other* tenants on the box. It is strictly read-only with **bounded output and memory** (top-N, a
   bounded heap, not a full sorted table); the scan still touches each candidate PID, so cost is bounded
   by cadence (§5), not by N.

### 2.4 The unifying shape: `(ours, co_tenant, free)` per resource

Every source above reduces to the same triple, computed honestly with unknowns preserved:

```
ours       = our per-process share              (self_*; may be Unknown)
device_tot = device/host-wide total             (DeviceLoad / cpu_util; authoritative)
co_tenant  = max(device_tot - ours, 0)           when both known, else Unknown
free       = max(capacity - device_tot, 0)       (capacity = 1.0, ceiling, or total bytes)
```

`co_tenant` is a **derived** quantity (total minus ours), so it inherits the unknown-propagation rule:
if either operand is unknown, `co_tenant` is **Unknown** and the band renders as "un-attributable",
never as a misleading zero or a fabricated number. This is the one invariant of the whole surface.

---

## 3. Ours vs system-wide vs co-tenant — the model

### 3.1 Three scopes, named once

- **Ours** — this Multiview instance/process tree, defined as a **PID set** (our process group / our
  cgroup membership, resolved via `/proc/self/cgroup`), not just `/proc/self`. Source: the `self_*`
  fields + (new) our-cgroup `cpu.stat`. This is what *we* can act on (place differently, degrade, encode
  elsewhere). Note the as-built `self_cpu_util` covers only the current process; until a child/helper is
  folded into the ours-set, the co-tenant walk (§2.3) must still exclude the whole set so our own
  workers are never named as a co-tenant.
- **System-wide** — the whole device/host total. Source: `DeviceLoad`/`cpu_util`/`vram_used_bytes`.
  This is the authoritative pressure (it is what actually saturates the silicon).
- **Co-tenant** — system-wide *minus* ours. The quantity the placement engine already reasons net-of
  ([gpu-placement-engine.md](gpu-placement-engine.md) §0). This is what we *cannot* act on but must
  *plan around* (and what the operator most wants named).

The page shows all three for every resource, with co-tenant always derived (§2.4) so there is no
second source of truth and no possibility of the three disagreeing.

### 3.2 The per-co-tenant roll-up (the new attribution artifact)

Beyond the scalar "co-tenant = total − ours", the operator wants the **biggest other consumer
named**. This brief drives one new, small, sampled artifact — a **co-tenant roll-up** carried on the
realtime stream alongside `SystemMetrics`:

- **CPU:** top-N non-self PIDs by `utime+stime` delta, each `{pid, comm, cpu_frac}` — from `/proc`.
- **NVENC / GPU (NVIDIA):** the non-self rows of `nvmlDeviceGetProcessUtilization` /
  the running-process VRAM list, each `{pid, enc_util?, compute_util?, vram_bytes?}` — where the
  GeForce caveat (§2.1) permits; otherwise a single "other (un-attributable)" entry.
- **GPU media (Linux i915/amdgpu):** non-self PIDs from the fdinfo walk, `{pid, media_frac}`.

This artifact is **bounded** (top-N, default ~5), **conflated** (latest-only, drop-oldest), and emitted
through the existing engine publisher — it can never grow unbounded or back-pressure the engine (§5,
inv #10). It is the literal answer to *"that's not us — so who is it?"*

### 3.3 Where the model is computed (and where it must NOT be)

- **Sampling** stays on the existing off-engine poller (`system_metrics.rs`, `SAMPLE_PERIOD`
  `:35` ≈ 750 ms → ~1 Hz `sampled_hz`). The new co-tenant walk and cgroup read run **there**, on the
  same off-hot-path task, never on the output-clock loop.
- **Derivation** (`co_tenant = total − ours`, top-N sort) is a **pure function** of one sample —
  unit-testable with injected `SystemMetrics`/process lists, no hardware, mirroring the existing pure
  `map_gpu`/`assemble_metrics` functions (`system_metrics.rs:83`, `:146`).
- **The output-clock loop touches none of it.** Per inv #1/#10 the engine never samples, never walks
  `/proc`, never awaits the stats consumer. The page is a *read* of a stream the engine *drops into*.

---

## 4. Surfacing — a Frigate-style stats page + the HealthWarning tie-in

### 4.1 The stats page (the operator's "us vs them" view)

The page is **data/graphics, not layout** (the operator's framing): charts and meters, not the
multiview canvas editor. Conceptually modelled on a Frigate-style system page (per-GPU charts +
per-process rows + a detector-style chart) [F1], built entirely from Multiview's own events. Sections:

1. **CPU band** — a stacked ours/co-tenant/free bar with a time-series sparkline, the **named top-N
   co-tenants** beneath it, and (in a container) the **cgroup line** ("our cgroup 1.9/2.0 cores; host
   95 %"). Answers *"is the pegged CPU us?"* directly.
2. **Per-GPU cards** — one card per `GpuMetrics` device showing **four** stacked ours/co-tenant/free
   bars: **compute**, **encoder (NVENC)**, **decoder (NVDEC)**, **VRAM**. **Merged-media exception:** on
   parts that expose a single combined media engine rather than separate enc/dec (AMD VCN4, §2.2), the
   card must render **one combined "media" bar** (or an explicit shared-source visual), never two
   independent encoder/decoder bars fed from the same merged term — splitting it would double-count. The
   card count is therefore four bars on split-engine parts, three (compute, media, VRAM) on merged ones.
   Encoder also shows the **session counter** `self/total/ceiling` (e.g. *"NVENC sessions: 2 ours / 7
   total / 8 ceiling"*) — the literal "who's on the encoder" answer; the ceiling is a discovered/estimated
   `Option` (§2.1) and is labelled as such, never presented as an authoritative hard cap. On a blind
   vendor a band greys to "n/a" (never a false zero, WCAG value+shape per
   [ADR-W011](../decisions/ADR-W011.md)).
3. **"What runs where" join** — each placed pipeline stage `{decode, composite, encode}` keyed to its
   `GpuMetrics.id`, so the operator sees *our* landing sites against the live ours/co-tenant load (this
   is the same panel [self-aware-placement.md](self-aware-placement.md) §8 drives via `placement.snapshot`;
   this brief consumes it, does not redefine it).
4. **A "detector-style" workload chart** — the Frigate analogue is detector inference time; Multiview's
   analogue is **per-tile composite/encode cost** (megapixels/sec and encode fps from the cost model).
   This is the chart that shows *what our load is doing*, complementing the *who owns the silicon* bands.

All series render at the existing ~1 Hz cadence from the realtime stream — **no REST polling**, the
engine never awaits the client (inv #10). Colour is never the sole signal (value+shape+text), reusing
the existing `SystemPage.tsx`/`SystemFooter.tsx` WCAG meter convention.

### 4.2 The HealthWarning tie-in (ADR-0035) — charts that explain the alerts

The stats page is the **continuous** surface; the [ADR-0035](../decisions/ADR-0035.md) `HealthWarning`
model is the **alert** surface. They link both ways:

- **`gpu-present-no-vulkan-adapter`** (the silent-fallback warning, shipped: `WarningCode`
  `event.rs:319`–`:325`) renders, on the affected GPU card, as **compute "ours" ≈ 0 while the device
  is present** with a callout *"GPU present but we're on CPU — here's the fix"*. The chart **is** the
  RTX-4060-idle picture; the warning carries the remediation.
- **`sustained-cpu-saturation`** (catalogued in [ADR-0035 §5.1](../decisions/ADR-0035.md), not yet in
  the shipped enum — see below) renders as the CPU band saturating with **the `self_cpu_util` finger
  pointing at us or at a co-tenant**. The new per-co-tenant roll-up (§3.2) lets the warning *name the
  culprit* when it isn't us.
- **`nvenc-session-ceiling-hit`** renders as the encoder session counter hitting `total ≈ ceiling`,
  with the page showing *how many are ours* — so the operator knows whether to reduce *our* sessions
  or chase a co-tenant NVR.

**Honest scope note (avoid duplicating shipped work).** [ADR-0035](../decisions/ADR-0035.md) §5.1
*catalogues* `sustained-cpu-saturation`, `nvenc-session-ceiling-hit`, `vram-pressure`,
`software-decode-on-gpu-host`, `software-encode-on-gpu-host`, but the **shipped `WarningCode` enum only
has `GpuPresentNoVulkanAdapter` + the three config codes** (`event.rs:319`–`:340`). Emitting the
metric-threshold codes is **ADR-0035's** SA-1+ scope, not this brief's — this brief **drives the stats
page and the per-co-tenant attribution data**, and *links* each warning to its chart. It must not
re-define the warning model or re-litigate the catalog; it extends `GpuMetrics`/`SystemMetrics` only
with the co-tenant roll-up + our-VRAM share, and adds the page.

### 4.3 The small data-model gaps this brief does own

To render the bands the page needs three additions, all additive and `Option`/`Vec`-shaped so they are
non-breaking wire changes (registered in `asyncapi.rs` + `openapi_schemas.rs`, the hand-listed
registries [self-aware-placement.md](self-aware-placement.md) §5):

1. **Our VRAM share on `GpuMetrics`.** `SelfShare.mem_used_bytes` is sampled (`load.rs:333`,
   `:916`–`:949`) but `map_gpu` (`system_metrics.rs:83`) does **not** map it onto the wire today — add
   `self_vram_used_bytes: Option<u64>` so the VRAM band can split ours vs co-tenant.
2. **The co-tenant roll-up** (§3.2) — a new conflated field/event `{cpu: Vec<CoTenant>, per_gpu:
   Vec<(DeviceId, Vec<CoTenant>)>}` where `CoTenant = {pid, comm, cpu_frac?, enc_util?, compute_util?,
   media_frac?, vram_bytes?}`, all `Option`.
3. **The cgroup CPU line** (§2.3) — `cgroup_cpu_usage_frac: Option<f32>` + `cgroup_cpu_quota_frac:
   Option<f32>` on `SystemMetrics`, both `None` off-Linux / outside a cgroup.

The existing `name`/`encoder_session_ceiling` `GpuMetrics` fields are hardcoded `None` today
(`system_metrics.rs:87`,`:94`) — populating them is already [self-aware-placement.md](self-aware-placement.md)
§8 scope; this page consumes them once populated.

---

## 5. Efficiency budget (mem / cpu / gpu / io)

Per house rule, an explicit budget. This surface is **read-only telemetry** and must stay negligible.

- **CPU.** The base poller already runs at ~1 Hz. The **new** costs are (a) one cgroup-v2 `cpu.stat`
  read (one small file, O(1)) and (b) the per-co-tenant `/proc/<pid>/stat` walk. The walk is the only
  non-trivial item: on a box with thousands of PIDs at ~1 Hz it is a real cost ([gpu-monitoring-and-
  scheduling.md](gpu-monitoring-and-scheduling.md) §5 risk 7 flags the analogous fdinfo walk). Top-N
  bounds the **output and memory** (a single pass keeping a bounded heap, not a full sorted table) but
  **not the scan**: deriving the top-N still costs one `/proc/<pid>/stat` read per candidate PID, so the
  real mitigation is cadence/cost, not a smaller N — make the interval/N **config-exposed** and default
  to a **coarser** co-tenant cadence (e.g. every Nth base tick) than the headline ours-vs-total split.
  The headline split (the operator's primary "is it us?" answer) costs nothing new — it is already on the
  wire.
- **GPU.** Zero additional GPU work for NVIDIA (the per-process NVML pass already runs). For the
  per-co-tenant *Linux* GPU breakdown, a full all-PID fdinfo walk is the cost; default to the cheaper
  i915-PMU device-total + our-fdinfo path and make the all-PID walk opt-in, per §2.2.
- **Memory.** Bounded by construction: top-N (default ~5) co-tenant entries, latest-only conflation,
  no history buffer in the engine (the *page* keeps a short ring for sparklines, client-side). No
  unbounded growth anywhere on the data plane.
- **IO.** Std file reads of `/proc` and `/sys/fs/cgroup` — no native dep, no network. The realtime
  emission reuses the existing drop-oldest broadcast; the co-tenant roll-up is **conflated** so a slow
  client never accumulates backlog.
- **Net.** The headline ours/co-tenant/free split is **free** (already emitted). The incremental cost
  is the bounded top-N walk + one cgroup read per (coarsened) tick — well within a telemetry budget,
  and fully gated so a tiny box can dial it down.

---

## 6. Invariant audit (how this design respects them)

- **Inv #1 (output clock untouchable).** Nothing here runs on the output-clock loop. Sampling +
  derivation are on the existing off-engine poller; the page is a *read* of a stream. No path blocks
  or paces the engine; a dead poller leaves the engine running on its last sample. ✔
- **Inv #10 (no back-pressure).** Every number is emitted through the **same drop-oldest
  `EnginePublisher::publish_event`** as `SystemMetrics` (one non-blocking `broadcast::send`); the
  co-tenant roll-up is bounded + conflated; the page consumes the conflated realtime channel and the
  engine never awaits it. A chaos/soak test should assert that flooding the co-tenant/stats events
  while every subscriber stalls delays no output tick — the same gate [self-aware-placement.md](self-aware-placement.md)
  §9 requires. ✔
- **Inv #6 (decode-at-display-res).** Not touched — this is observation only; it *reports* on decode
  placement, it does not change it. ✔
- **Inv #8 (fixed color order).** Not touched — no pixel path. ✔
- **Read-only telemetry (the ADR-0035 posture).** This surface only ever *reads* OS/vendor counters
  and *derives* splits; it never gates, places, or throttles. Placement decisions remain
  [ADR-0018](../decisions/ADR-0018.md)'s; this is their operator-facing mirror. ✔
- **IPv6-first.** No new bind surface — it rides the existing control-plane realtime/REST stack, which
  is dual-stack `[::]` per [conventions §10](../architecture/conventions.md). Any future dedicated
  metrics endpoint inherits that. ✔
- **Vendor-neutral.** Frigate is a conceptual reference only; all data comes from open OS/vendor APIs
  (NVML, `/proc`, cgroup v2, DRM fdinfo) verified by name in §2. No vendor SDK is bundled or
  redistributed. ✔

---

## Open questions

1. **GeForce per-process blindness — how graceful is graceful?** When
   `nvmlDeviceGetProcessUtilization` returns `NOT_SUPPORTED` (common on consumer GeForce, §2.1), the
   page cannot give a per-co-tenant NVENC breakdown — only "ours (our own bookkeeping) vs total vs
   free". Is "other (un-attributable)" acceptable to the operator, or should the page fall back to
   *naming* co-tenant encoders via a CPU-side `/proc` heuristic (process holds an NVENC device fd)?
   The fd heuristic is fuzzy; default is the honest "un-attributable" band, flagged for operator input.
2. **cgroup mount-path resolution (unverified).** The exact `/sys/fs/cgroup` layout for our cgroup
   varies (unified vs delegated container, systemd slices). The reader follows `/proc/self/cgroup`,
   but the robust set of layouts to support needs validation on the real deploy images (the same
   "validate on the deploy ffmpeg/host" lesson as the HLS-WebVTT fix). Defaulting to `None` when
   unresolved is safe (the host-total CPU answer still works).
3. **Per-co-tenant walk cadence/cost on big multi-tenant boxes.** Top-N over thousands of PIDs at the
   base cadence may be too much on a busy NVR host. The default coarsens the co-tenant cadence (§5);
   is a fixed coarse default right, or should it auto-tune off the measured walk cost?
4. **macOS attribution.** Per-process GPU/encoder util has **no public API** on Apple
   ([gpu-monitoring-and-scheduling.md](gpu-monitoring-and-scheduling.md) §1) — the macOS page can show
   ours-vs-host CPU (from `host_processor_info`/`/proc`-equivalent) but the GPU bands are largely "n/a".
   Is a CPU-only attribution page acceptable on macOS for v1? (Proposed: yes — honest n/a beats a
   private-API dependency.)
5. **Relationship to a Prometheus scrape.** [ADR-0017](../decisions/ADR-0017.md) §4.1 already defines
   per-GPU `multiview_gpu_*` gauges. Should the co-tenant roll-up also be a (bounded-cardinality)
   gauge set, or stay realtime-stream-only to avoid PID-label cardinality blow-up? (Proposed:
   realtime-only for per-PID detail; a single `multiview_cpu_cotenant_ratio` scalar gauge is safe.)

---

## References

External standards (web-verified 2026-06-13 unless marked unverified):

- **[N1]** NVIDIA, *NVML API Reference — Device Queries* (`nvmlDeviceGetProcessUtilization` →
  `nvmlProcessUtilizationSample_t{smUtil,memUtil,encUtil,decUtil}` / `nvmlProcessesUtilizationInfo_v1_t`;
  `nvmlDeviceGetMemoryInfo`; `nvmlDeviceGetEncoderStats`; the `.memory` = memory-controller-busy trap).
  docs.nvidia.com/deploy/nvml-api/. Verified.
- **[L1]** *DRM client usage stats*, Linux Kernel documentation — `/proc/<pid>/fdinfo/<drm_fd>`
  `drm-engine-<class>: <ns>` per-process cumulative engine counters. docs.kernel.org/gpu/drm-usage-stats.html.
  Verified.
- **[L2]** Intel i915 fdinfo per-client engine-utilisation work (Phoronix / dri-devel patch series).
  Verified (per-client engine + memory stats via fdinfo).
- **[C1]** Linux kernel cgroup-v2 basic CPU usage accounting (`cpu.stat`:
  `usage_usec`/`user_usec`/`system_usec`). cgroup-v2 accounting patch series (lkml). Verified for the
  field set; **(unverified)** the precise container mount-path resolution.
- **[F1]** Frigate system-stats model — per-process CPU/memory rows, per-GPU usage, detector
  inference-speed chart (`/api/stats`, `frigate_*` metrics). docs.frigate.video/configuration/metrics.
  Verified as a **conceptual reference only** (no code/layout copied).
- **[N4]** NVENC concurrent-session ceiling — per-system limit, moving driver number (Video Codec SDK
  13.0: 8/system; 2025 reports: 12). Already verified in [gpu-monitoring-and-scheduling.md](gpu-monitoring-and-scheduling.md)
  §1.1 [N4][N5][N6]; carried here by reference.

Multiview internal (verified in-tree):

- `crates/multiview-events/src/event.rs` — `GpuMetrics` (`:148`; `name:155`, `encoder_sessions:172`,
  `encoder_session_ceiling:175`, `self_compute_util:185`, `self_encoder_util:188`,
  `self_decoder_util:191`, `self_encoder_sessions:197`), `SystemMetrics` (`:206`; `cpu_util:209`,
  `self_cpu_util:220`, `gpus:226`, `sampled_hz:231`), `WarningCode` (`:319`), `HealthWarning` (`:357`).
- `crates/multiview-hal/src/load.rs` — `DeviceLoad` (`:220`), `SelfShare` (`:320`;
  `mem_used_bytes:333`, `encoder_sessions:337`), NVML per-process pass (`:893`–`:956`), DRM fdinfo
  media tracker (`:1196`; proc-root walk `:1220`–`:1240`; merged-media set `:1319`), i915-PMU leaf
  hook (`:1327`–`:1333`).
- `crates/multiview-cli/src/system_metrics.rs` — off-engine poller (`SAMPLE_PERIOD:35`), `map_gpu`
  (`:83`), `assemble_metrics` (`:146`), `SelfCpuSampler` (`:251`; `/proc/self/stat`+`/proc/stat`
  `:303`–`:330`), `publish_event` (`:485`).
- `crates/multiview-control/src/routes/health.rs` — `GET /api/v1/health` `list_health` (`:67`);
  `warning_ingest.rs` + `WarningRepository` (`crates/multiview-control/src/warning_ingest.rs`,
  `warning_store.rs`).
</content>
</invoke>
