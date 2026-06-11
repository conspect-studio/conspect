# ADR-I004: Broadcast multiviewer (M10–M12) feature placement — modules inside the existing 16 crates, native/hardware behind off-by-default features

- **Status:** Accepted
- **Area:** Implementation Build-out
- **Date:** 2026-06-03
- **Source:** [broadcast-multiviewer-features.md](../research/broadcast-multiviewer-features.md), conventions §3 (frozen 16-crate list), `.multiview-build/broadcast-plan.md`
- **Realizes / governs:** [ADR-MV001](ADR-MV001.md)–[ADR-MV005](ADR-MV005.md)

## Decision

The M10–M12 broadcast multiviewer capabilities (the MV-series ADRs: content-aware alarms, TSL UMD tally, loudness/audio compliance metering, multi-head/salvo automation, NMOS + router-control IP integration) are implemented as **MODULES INSIDE the existing 16 crates** — **no new crates** are added, because conventions §3 freezes the crate list at 16. Each capability is a new `src/<module>.rs` (or `src/<module>/`) inside the crate that already owns its concern; native, hardware-bound, or network-bound parts (SNMP, ST 2110/-22, ST 2022-6/-7, PTP, JPEG XS, live serial/MQTT/router transports) sit behind **off-by-default Cargo features**, with the pure-Rust model (state machines, codecs, JSON schemas) always compiled and tested GPU-/hardware-free. The build-out sequences this as four waves (A–D) in `.multiview-build/broadcast-plan.md`. High-level crate→module mapping:

| Crate | Broadcast modules (high level) | Gated features |
|-------|--------------------------------|----------------|
| `multiview-core` | X.733 alarm severity types; multi-head layout (rotation/orientation, per-tile crop/ROI) | — |
| `multiview-events` | `Alarm*`/`Tally*`/`Salvo*` event variants; `Topic::Alarms`/`Topic::Tally` | — |
| `multiview-telemetry` | RFC 5424 syslog (pure); SNMP traps + MIB | `snmp` |
| `multiview-config` | salvo/scheduled-layout schema; probe & tally-profile schema | — |
| `multiview-engine` | X.733 alarm state machine + roll-up + penalty-box; probes (black/freeze/format/QoE); tally arbiter/profile/GPIO; salvo/scheduler/heads/cycle; PTP servo | `ptp` |
| `multiview-input` | TSL UMD v3.1/4.0/5.0 decoders; ST 2110/2022-6/-7 ingest; hitless reconstruction (pure algo) | `st2110` |
| `multiview-output` | TSL UMD encoders; ST 2110 egress + ST 2022-7 | `st2110` |
| `multiview-overlay` | tally/timer/identify/scopes/safe-area/caption-probe/multi-tz clock | — |
| `multiview-audio` | PPM/VU/sample-peak/true-peak ballistics; phase/goniometer/surround correlation; dialnorm metadata; channel map | — |
| `multiview-control` | webhook/email notify; alarm/loudness repository tables + routes; IS-07 (WS/MQTT); NMOS IS-04/05/08/10/12; SW-P-08/Ember+ router bridges | — (transports integration-tested) |
| `multiview-ffmpeg` | JPEG XS decode | gated |

## Rationale

Conventions §3 is the naming/structure source of truth and deliberately freezes 16 crates; adding crates for broadcast features would fragment the dependency graph and contradict it. Every MV capability maps cleanly onto a crate that already owns the relevant concern (alarms → engine, tally codecs → input/output, metering → audio, schema → config, IP/control → control), so they belong as modules, not crates. Keeping the pure model always-compiled and gating only the native/hardware transports preserves the default build's properties (pure-Rust, `cargo deny`-clean, fast, GPU-free CI) and keeps the high-leverage logic — the X.733 alarm state machine that all of M10 hangs off, the TSL UMD codecs, the tally arbiter, NMOS JSON models, the probe analysis — property-/golden-vector-testable with no hardware. This mirrors the build-out pattern already proven elsewhere (pure `classify`-style state machines over an injected `MediaTime`, `#![forbid(unsafe_code)]` + `#![warn(missing_docs)]`, native deps feature-gated). The wave ordering (A: shared type foundations and freeze `core`/`events`; B: alarm/UMD core; C: tally/operator surface; D: the heavy, mostly-gated IP tier last) keeps each wave's crates disjoint so they can be built in parallel without colliding.

## Alternatives considered

- **New crates per broadcast subsystem (e.g. `multiview-alarm`, `multiview-tally`, `multiview-nmos`)** — rejected: violates the frozen 16-crate list in conventions §3 and adds graph/build surface for concerns the existing crates already own.
- **One umbrella `multiview-broadcast` crate** — rejected: would either duplicate or invert the dependency direction (it would need to depend on engine/input/output/audio/control at once), creating coupling the per-crate module placement avoids.
- **Native transports compiled by default** — rejected: pulls hardware/network deps (SNMP, ST 2110, PTP, JPEG XS) and their licenses into the default build and CI; they must stay off-by-default with the pure model always available.

## Consequences

- The MV-series ADRs (MV001–MV005) describe *what* and *why*; this ADR fixes *where* the code lands and that it does so without new crates.
- `multiview-core` and `multiview-events` gain broadcast types early (Wave A) and are frozen after that wave, so downstream waves build against a stable type surface.
- Each gated feature (`snmp`, `ptp`, `st2110`, JPEG XS, …) carries its own deny/license obligations and is only locally verifiable where the hardware/peer exists; the pure modules remain the CI-tested default.
- The detailed module list and wave sequencing live in `.multiview-build/broadcast-plan.md` (a git-ignored transient planning doc); this ADR captures the settled high-level mapping and rationale so the decision survives independent of that file.
