# ADR-W030: `GET /api/v1/system/capabilities` — honest default-build capability surface

- **Status:** Accepted
- **Area:** Web/API
- **Date:** 2026-07-11
- **Source:** management-completeness surfacing (task #9/#176, [management-capability-matrix](../research/management-capability-matrix.md) §2.7); operator direction (design-first, option A)

## Context

The management-completeness contract (AGENTS.md §H) requires every controllable/observable
engine property to be reachable through a versioned API resource. Today only
`GET /api/v1/preview/capabilities` (transport negotiation, §2.8, ADR-P006) exists; there is
**no** endpoint that reports what codec/compositor backends this build can use, the
**effective build-profile licence** (a compliance surface — ADR-0012 requires "report the
effective license per built artifact"), or the mandatory NDI attribution.

The matrix §3.4 sketches a rich JSON report (per-device codec profiles, NVENC session budget,
VRAM, host PSI). **That report type does not exist.** `multiview-hal` exposes deliberately
serde-free planner priors (`Capability`, `DeviceCaps`, `AdapterReport` — none derive
`Serialize`); the rich telemetry is an aspirational SA-1+ milestone
(`multiview-cli/src/capability_warn.rs` calls it "the full `CapabilityReport` (SA-1+)"). So this
endpoint must **assemble an honest DTO** from what the running binary actually knows —
`hal::probe()`, the compiled Cargo features (`cfg!`), and the compositor's resolved
`AdapterReport` — not serialize a phantom type.

Binding constraints: invariant #10 (control plane must not couple to / back-pressure the
engine), rule 6 (no modelled-but-unfilled fields), rule 27 (no aspirational reporting), and the
licensing model (AGENTS.md §G / ADR-0012): default = LGPL-clean & redistributable; `gpl-codecs`
→ GPL; NDI is proprietary, runtime-loaded, never vendored, attribution mandatory.

## Decision

Add `GET /api/v1/system/capabilities` (role: **viewer/read**, system-global — no per-object
BOLA axis) returning a new `multiview_control::system::SystemCapabilities` DTO:

- `backends: Vec<BackendCapability>` — the codec backends at the decode + encode stages plus the
  software composite path, each carrying `available` (from `hal::probe(kind, stage)`), plus
  `max_resolution?` and `decode_resize?` (present only when available; `decode_resize` only on the
  decode stage). The hardware compositor tier is **not** emitted as `available: false` codec-style
  rows — `hal::probe` has no environment probe for the portable compositor backends — it is
  reported by `compositor` (below).
- `compositor: { class, device_type?, driver? }` — the SA-0 composite-usability classification
  (`AdapterClass`/`AdapterReport`, ADR-0035) of the resolved wgpu adapter.
- `build: { effective_license, redistributable, features, ndi }` — the compliance surface,
  **inline** (see §2.7-vs-§3.4 note below).
- `ndi_attribution?: { trademark, url }` — present **iff** the `ndi` feature is compiled; carries
  the mandatory `"NDI® is a registered trademark of Vizrt NDI AB"` + `https://ndi.video`
  (AGENTS.md §G).

The DTO lives in `multiview-control` with **primitive/enum fields only — no `multiview-hal`
types** — so control keeps zero dependency on hal. The **CLI** (which already depends on hal and
resolves the `AdapterReport` for the silent-software-fallback warning) maps hal → DTO at startup
and installs it via a new **static** `AppState::with_capabilities(SystemCapabilities)` builder
(mirroring `with_live_apply`/`with_live_sources`). It is a one-shot startup snapshot: **no engine
channel, no runtime coupling** (invariant #10). The default `AppState` carries a coarse honest
software-only snapshot so the route is always present and truthful even when the binary wires
nothing.

`effective_license` is an enum serialized to the exact compliance strings `"LGPL-clean"` /
`"GPL"`, resolved by the pure `BuildInfo::resolve(gpl_codecs, ndi, features)` in control (tested
exhaustively) from booleans the CLI reads via its own `cfg!(feature = …)`. `redistributable` is
`true` for every shippable build (there is **no** `nonfree` Cargo feature — the ADR-0012
non-redistributable axis is not compiled; NDI is runtime-loaded, never linked).

### The default-vs-SA-1+ boundary (rule 6 / rule 27)

The DTO declares **only** fields the default/software build honestly fills. It deliberately
omits per-codec profiles, NVENC session budget, VRAM, and host-PSI telemetry — those need
feature-gated backend-crate code and **GPU-hardware validation** (rule 26) and are the separate
tracked lane **task #180** (`SA-1+ vendor-caps deep probe`, blocked by this). No empty-collection
or `None` placeholder stands in for that unprobed telemetry, and the UI renders no aspirational
gauges. `compiled_in` (backend feature-compiled-but-device-absent) is likewise deferred to #180,
which has the hal-side feature introspection; here `available` (compiled **and** present) is the
honest single signal.

### §2.7-vs-§3.4 inconsistency

Matrix §2.7 rows route `effective_license`/`ndi_attribution` through a separate `GET
/system/build`, while §3.4's JSON embeds `build{}` inline in the report. We resolve to **inline**
`build{}` on `/system/capabilities` (one round-trip for the About page); no separate
`/system/build` alias is added.

## Rationale

- **Honest now beats aspirational later.** A complete, truthful default-build surface is a real
  deliverable (rule 6): the route, DTO, and page are wired end-to-end for exactly what the
  software/default tier produces. The deep probe is genuinely separable (needs hardware) and is
  tracked, not dropped.
- **Compliance correctness.** Reporting `effective_license` wrong (a GPL build advertised
  LGPL-clean) is a licensing misstatement, so the mapping is a pure, exhaustively-tested function
  pinned to §G/ADR-0012, not a best-effort string.
- **Isolation.** A static startup snapshot cannot back-pressure or couple to the engine; control
  stays hal-free by construction (the CLI owns the hal→DTO map).

## Alternatives considered

| Alternative | Rejected because |
| ----------- | ---------------- |
| Serialize a `multiview-hal` `CapabilityReport` | No such serde-able type exists; hal priors are deliberately serde-free (planner-only). |
| Build the rich §3.4 telemetry now | Needs feature-gated backend code + GPU-hardware validation (rule 26); would force aspirational/empty fields (rule 6/27). Tracked as #180. |
| Put the DTO in `multiview-hal` (derive `Serialize`) | Couples hal to serde/wire concerns and control→hal; breaks the serde-free planner layer. |
| A live capabilities channel from the engine | Violates invariant #10 for zero benefit — capabilities are fixed at startup (compiled features + resolved adapter). |
| Separate `GET /system/build` (per §2.7) | Extra round-trip for the About page; `build{}` inline (per §3.4) is simpler and sufficient. |

## Consequences

- New always-present read endpoint + `SystemCapabilities` OpenAPI schema; the web client and a
  `CapabilitiesPage` (Backends / Compositor / Build+About) consume it. Encoder-option gating and
  the effective-licence/NDI-attribution display now have a real source.
- **Committed to maintaining** an exact `effective_license` mapping as codec-licensing features
  change; a licence-mapping unit test asserts the strings per feature-combo and CI/Codex verify
  it against §G/ADR-0012.
- **Invariant #10 preserved:** `with_capabilities` is a static snapshot; control keeps no hal
  dependency. The engine is untouched.
- Task **#180** owns the SA-1+ deep probe (per-codec profiles, NVENC sessions, VRAM, PSI,
  per-backend `compiled_in`) behind the relevant backend features + GPU runners.
