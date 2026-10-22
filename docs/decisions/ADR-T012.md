# ADR-T012: Reference-clock / wall-clock source-selection contract — free-run vs PTP-grandmaster vs NTP-disciplined, ReferenceLoss failover, and the disciplined reference as a MEDIA-CLOCK REFERENCE only (never a pacer)

- **Status:** Proposed
- **Area:** Streaming/Timing
- **Date:** 2026-06-10
- **Source briefs:** [timing-architecture.md](../research/timing-architecture.md),
  [wall-clock-sync.md](../research/wall-clock-sync.md), [aes67-delivery.md](../research/aes67-delivery.md)
- **Builds on / relates to:** [ADR-T001](ADR-T001.md) (single monotonic timeline + fixed-cadence
  output clock = the sole pacer, invariant #1), [ADR-T003](ADR-T003.md) (unified timing model:
  per-input PTS normalisation, monotonic→ns rebase, output re-stamp from the tick counter),
  [ADR-0020](ADR-0020.md) (layered timing — names Layer B reference-lock and Layer D wall-clock),
  [ADR-T006](ADR-T006.md) (long-run drift loop), [ADR-0033](ADR-0033.md) /
  [ADR-T010](ADR-T010.md) (AES67 / ST 2110-30 audio I/O — PTP media-clock reference, never a pacer),
  [ADR-0038](ADR-0038.md) (per-source wall-clock trust, reuses this lock-state machine),
  [ADR-I004](ADR-I004.md) (broadcast-multiviewer M10–M12 feature placement)

> Numbered T012: T009 (per-tile ring publish), T010 (Dante/AES67 audio) and T011 (HLS rendition
> isolation) already occupy the recent Streaming/Timing slots, so the next free one is used.

## Context

Multiview already ships the *math* of a disciplined reference clock but has never pinned the
*policy* that selects between reference sources, governs failover, and bounds how the selected
reference is allowed to influence the output. Three pieces of as-built code each carry a local,
undocumented assumption that this ADR makes one authoritative contract for:

- **`multiview-engine/src/ptp.rs`** — the pure PTP / SMPTE ST 2059-2 servo (`PtpServo`), the
  `ReferenceTracker` lock-state machine (`Freerun → Acquiring → Locked → Holdover → Freerun`),
  the `ReferenceStatus` snapshot, and the off-by-default `ptp`-feature `phc` module (the
  `/dev/ptpN` PHC read via safe `rustix`, compile-verified only — no PTP NIC in CI). Its module
  docs already assert "PTP does **not** pace the output clock," but the *selection* and *failover
  policy* across sources is not written down anywhere normative.
- **`multiview-engine/src/sysref.rs`** — the system-clock (NTP/chrony) discipline classifier
  (`classify_system` over a normalised `adjtimex` reading), the `SystemRefTracker`, and the
  `ReferenceSelector` that arbitrates the system NTP discipline against the PTP reference into one
  `SelectedReference` for the badge. The selector's policy ("PTP outranks SYS whenever disciplined")
  lives only in a doc-comment.
- **`multiview-cli/src/wallclock.rs`** — the time-of-day source for the on-screen clock overlay.
  Its `SystemWallClock::default` documents an explicit **"deferred follow-up"**: it *assumes*
  `RefStatus::Locked` on the basis that the deployment host is NTP-disciplined, because true kernel
  lock detection was not yet wired. The `ntp`-feature `MeasuredSystemWallClock` (ENG-3b) since reads
  the live discipline state, but the contract for which source is authoritative, and what the badge
  must show on loss, was never pinned — so the "deferred follow-up" had no spec to close against.

This is also a **gate** for two planned subsystems that must not invent their own divergent answers:

- **AES67-5** (the AES67 / ST 2110-30 audio work item that builds the SDP `a=ts-refclk` /
  `a=mediaclk` signalling — `a=ts-refclk:ptp=IEEE1588-2008:<GMID>:<domain>`). It must know **which
  PTP profile and domain** the receiver/sender reference is, how the reference is selected against a
  free-running or NTP-disciplined fallback, and that the reference disciplines the *audio media
  clock* and SEND timestamp **only**, never the engine pacer (ADR-0033 §4 already states this; this
  ADR makes the source-selection half of it normative).
- **M12** (the broadcast-multiviewer wave-3 "IP-broadcast clock" capability under
  [ADR-I004](ADR-I004.md): facility-grade reference into the multiview, surfaced as a tally/health
  reference status). It needs the same single source-selection + failover contract so the
  multiview's reference badge and any northbound alarm agree on what "locked / holdover / lost"
  means facility-wide.

Without a pinned contract, each of these could pick a different precedence, a different
loss-to-free-run ladder, or — worst — a path that lets a flapping grandmaster perturb the output
cadence. Invariant #1 (the output tick is the sole pacer, `out_pts = f(tick)`) and invariant #10
(isolation) forbid the last; this ADR writes that prohibition into the reference-clock layer
explicitly.

## Decision

Pin a single **reference-clock source-selection contract** with four parts. It is *policy over the
already-built pure value machines* — it adds no data-plane code path and changes no behaviour of the
output clock.

### 1. The reference-source ladder and selection precedence

A Multiview instance has exactly **one authoritative timing reference** at any instant, chosen from
an ordered ladder. Higher rank wins **only while it is disciplined** (`LockState::Locked` or
`LockState::Holdover`); otherwise selection falls to the next rank:

1. **PTP grandmaster (ST 2059-2 profile)** — `RefSource::Ptp`. The highest-stratum media reference.
   Selected whenever the `ReferenceTracker` reports `is_disciplined()` (Locked or coasting in
   Holdover). The PTP profile is **SMPTE ST 2059-2** and the **PTP domain is explicit configuration**
   (default domain `127` per the SMPTE 2059 default profile; AES67/Dante deployments commonly use a
   different domain such as `0` — it MUST be configurable, never hard-coded, and MUST match the
   `<domain>` advertised in AES67-5's `a=ts-refclk:ptp=IEEE1588-2008:<GMID>:<domain>`). Multiview is
   a PTP **slave/receiver only** — it reads a PHC disciplined by external linuxptp
   (`ptp4l` + `phc2sys`); it never authors PTP messages and never runs BMCA itself (ADR-0033 §4).
2. **NTP-disciplined system clock** — `RefSource::Ntp` (badge label `SYS` is retained for the
   host-clock case; see §5 on the `Ntp`/`System` distinction). Selected when PTP is absent/not
   disciplined **and** the kernel reports the system clock synchronised within tolerance
   (`classify_system` → `Locked`/`Holdover`).
3. **Free-run** — `RefSource::System` in `RefStatus::Freerun`. The terminal fallback: the local
   monotonic oscillator with no external discipline. This is the **commodity shipping default** and
   the loss-ladder floor; it is always available and can never itself "fail."

`ReferenceSelector::select` is the authoritative arbiter and **MUST** implement exactly this
precedence: PTP-if-disciplined, else the system NTP discipline, else honest free-run. No other code
path may pick a reference. The selection is recomputed off the data plane (the PHC/`adjtimex`
samplers run on a dedicated sampled thread like the HAL load poller, never on the output-clock loop).

### 2. Failover and the ReferenceLoss ladder

Loss is governed entirely by the `ReferenceTracker` lock-state machine and its
`ReferenceConfig` thresholds (`stale_after_ns`, `holdover_window_ns`) — never by a wall-clock
deadline on the engine path:

- **Locked → Holdover** when no fresh in-tolerance sample arrives within `stale_after_ns`. The
  reference *coasts* on its last good frequency estimate; it remains `is_disciplined()` and stays
  selected. The badge shows `RefStatus::Holdover`.
- **Holdover → Freerun** when the last accepted sample is older than `holdover_window_ns`. The
  reference is abandoned; selection falls to the next ladder rank (NTP if disciplined, else
  free-run). This transition is a **`ReferenceLoss` event**: it MUST raise
  `AlarmKind::ReferenceLoss` (the `#[non_exhaustive]` alarm ADR-0033 §8 adds) and, on the overlay,
  MUST surface as `RefStatus::RefLoss` (the broken-link badge) on the first selection cycle after the
  drop — distinct from a clock that was *never* referenced (which reads `Freerun`). The overlay
  `RefStatus::RefLoss` variant exists precisely for this signalled-loss case; the current
  `SelectedReference::to_ref_status` mapping of `Freerun | Acquiring → Freerun` MUST be extended so a
  reference that has *dropped out of holdover* renders `RefLoss`, not bare `Freerun`. After the
  RefLoss has been surfaced once and selection has settled on the next rank, the badge tracks that
  rank's honest status.
- **Re-lock** is the servo's step path: a returning or better grandmaster produces a large offset the
  servo applies as a step of the *reference estimate*, then resumes slewing; acquisition re-runs
  (`Acquiring`) before declaring `Locked` again (`lock_samples` consecutive in-tolerance samples).
  Re-lock changes only the reference estimate — it does **not** re-step the output clock origin
  mid-run (see §3).

Commodity holdover is **TCXO / `CLOCK_MONOTONIC`-grade only**: do not promise sub-µs phase holdover
without dedicated timing hardware (OCXO/rubidium). For the never-falter guarantee this is sufficient
— it is the same monotonic clock the output already free-runs on.

### 3. The disciplined reference feeds the output clock as a MEDIA-CLOCK REFERENCE only — never as a pacer (invariant #1)

This is the load-bearing half of the contract.

- The output clock (`OutputClock`, `out_pts = f(tick)`) remains the **single pacing authority**. The
  tick counter and the exact-rational cadence are computed purely from `CLOCK_MONOTONIC` /
  `mach_continuous_time`; the selected reference can neither stall, speed up, nor skip a tick. A
  drifting, jittering, flapping, or absent grandmaster changes **only** the reference *estimate* and
  the *badge/alarm* — it never changes how many frames the output emits, nor when.
- The disciplined reference is consumed by three **off-hot-path, label/alignment-only** consumers,
  each of which already exists or is gated to its own feature:
  1. the **wall-clock reference badge / overlay** (`RefSource` + `RefStatus`), read at draw time on
     the overlay bake thread;
  2. **ingest phase-alignment / cross-source common-timeline mapping** for sources verifiably
     PTP-locked to the same grandmaster (Layer C of ADR-0020) — still *sampled*, never pacing;
  3. the **AES67 audio media clock and SEND timestamp** (ADR-0033 §4): the audio sample clock is
     slaved to PTP-TAI and the SEND RTP timestamp is PTP-anchored, while the video output clock stays
     free-running; a boundary resampler (ADR-0033 §5 / ADR-T006) reconciles the monotonic tick vs the
     PTP media clock by *resampling, never drop/dup*.
- **Optional rate discipline (Layer B, feature `ptp`, opt-in pro mode).** A deployment MAY discipline
  the output cadence's **rate** (syntonization) to the selected PTP reference via the servo's slew
  path — adjusting only the *rate of tick advance*, never `out_pts = f(tick)` and never which input is
  sampled. **Phase** alignment to the ST 2059-1 SMPTE-Epoch grid is permitted **only** as a one-time
  origin seed at start or relock; a **mid-run phase re-jam is a Class-2 (controlled-reset) change**
  (ADR-R004 / ADR-M005), never a hot per-tick correction, because it shifts frame boundaries. On
  reference loss the disciplined rate degrades **holdover → free-run** (last frequency estimate, then
  undisciplined monotonic) so the picture never stalls or steps. The commodity default leaves Layer B
  **off**: A free-runs, the reference is consumed for labels/alignment/AES67 only.
- **Isolation (invariant #10).** The samplers publish `ReferenceStatus` through a wait-free
  single-slot `LatestState` (drop-oldest, never a channel a slow consumer can fill); the engine never
  `.await`s a reference reader. A failed/denied PHC or `adjtimex` read is a missing sample that lets
  the tracker's staleness/holdover logic fire on the next tick — it stalls nothing.

### 4. Timescale at the consumption boundary: PTP time is TAI; published wall surfaces are UTC

The reference this contract selects is consumed by **UTC-defined** surfaces (ADR-M010's outbound
presentation epoch: HLS `EXT-X-PROGRAM-DATE-TIME` per RFC 8216, the RTCP SR NTP word, the control-WS
`timing.status` epoch), while the PHC under the standard linuxptp deployment this ADR names
(`ptp4l` ST 2059-2 + `phc2sys`) carries **PTP time = TAI** — currently 37 s ahead of UTC. Any
consumer deriving a wall-clock estimate from the PTP leg MUST therefore apply the configured
TAI−UTC offset (`[timing] ptp_utc_offset_s`, default `37`, integer seconds — sourced from `ptp4l`'s
`currentUtcOffset`, which tracks leap seconds) so the published value is always UTC and a
PTP↔system reference transition never steps the published timeline by the TAI−UTC difference. A
post-conversion disagreement of ≥30 s against the system clock indicates a misconfigured offset (or
a PHC on an unexpected timescale): the consumer MUST NOT publish the suspect PTP estimate — it
degrades to the system leg with a non-locked quality and warns. The selection ladder (§1) itself is
timescale-agnostic; this section governs only the wall-estimate consumers (`multiview-engine`'s
`epoch` module implements it for ADR-M010).

### 5. Source/status taxonomy (badge + API contract)

- `RefSource` is the public taxonomy: `Ptp` (label `PTP`), `Ntp` (label `NTP`, NTP-disciplined
  system clock), `System` (label `SYS`, undisciplined host clock / free-run floor). The
  `ReferenceSelector` MUST emit `Ntp` (not `System`) when the *measured* kernel discipline is locked
  via NTP/chrony, reserving `System` for the assumed/undisciplined fallback — closing the
  `wallclock.rs` "deferred follow-up" against the `ntp`-feature measured path, and making the badge
  honest about *which* discipline is active.
- `RefStatus` is the public lock state: `Locked`, `Holdover`, `RefLoss` (signalled loss out of
  holdover, per §2), `Freerun` (never referenced). All four are conveyed as **text + glyph**, never
  colour alone (accessibility; existing in `multiview-overlay/src/clock.rs`).
- The selected reference (source + status + offset/frequency estimate) is the single value surfaced
  to the overlay, the control-plane reference/health endpoint, AES67-5's SDP `ts-refclk` emit, and
  M12's facility-clock status. There is exactly one selection authority and one taxonomy.

## Alternatives considered

- **Pace the output from the PTP grandmaster (slave the tick to PTP-TAI).** *Rejected — breaks
  invariant #1.* A grandmaster can flap, fail BMCA, or step on a leap event; gating the tick on it
  reintroduces exactly the stall/step failure mode the output clock exists to prevent. PTP is a
  *reference* (rate/phase/time-of-day), categorically not a pacer (timing-architecture brief §0).
- **Let each consumer (overlay, AES67, M12) pick its own reference.** *Rejected.* Divergent
  precedence/failover would let the badge, the audio SEND timestamp, and the facility alarm disagree
  about lock state. One `ReferenceSelector` is the single arbiter.
- **Collapse `Ntp` and `System` into one `SYS` source.** *Rejected.* The `wallclock.rs` follow-up is
  precisely about being honest that an NTP-disciplined host (`Ntp`/locked) is a stronger reference
  than an assumed/undisciplined one (`System`/free-run). The overlay model already carries both
  variants; the selector must use them.
- **Treat `Holdover` as "lost" immediately (no coast).** *Rejected.* Holdover is the
  industry-standard graceful degradation (linuxptp, NDI Genlock fall-back); dropping the reference
  the instant a sample is late would chatter the badge and, under Layer B, the disciplined rate.
- **Mute the program / freeze output on reference loss.** *Rejected — violates invariant #1.* The
  local output free-runs and the program keeps running; AES67/ST 2110-30 themselves mandate *no*
  mute-on-unsync (ADR-0033 §8). Loss raises an alarm, never a stall.
- **Hard-code the PTP domain (e.g. `0` or `127`).** *Rejected.* SMPTE-2059 default-profile and
  AES67/Dante deployments use different domains; the domain MUST be explicit configuration matched
  to the grandmaster and to AES67-5's advertised `<domain>` (aes67-delivery brief).

## Consequences

- **No change to the commodity default and no data-plane code.** This ADR is policy over the
  existing pure value machines (`ptp.rs`, `sysref.rs`); the output clock, frame stores, and pacer are
  untouched. `cargo build --workspace` stays green (docs-only change).
- **`wallclock.rs`'s "deferred follow-up" now has a spec to close against.** The contract is: the
  measured (`ntp`-feature) path reports `Ntp`/locked when the kernel is disciplined, `System`/freerun
  otherwise; the assumed default remains honest about being an assumption. Wiring the selector to emit
  `Ntp` and surfacing `RefLoss` on signalled loss are the concrete follow-up edits this ADR authorises
  (separate, test-first PRs in `multiview-engine`/`multiview-cli`).
- **AES67-5 is unblocked.** The SDP `a=ts-refclk:ptp=IEEE1588-2008:<GMID>:<domain>` builder reads the
  selected PTP reference's profile (ST 2059-2) and configured domain; the audio media clock and SEND
  timestamp are PTP-anchored per this contract while the video output clock free-runs.
- **M12 (IP-broadcast clock) is unblocked.** The facility-clock capability surfaces the single
  `SelectedReference` + `AlarmKind::ReferenceLoss` rather than a parallel clock model.
- **Pro mode (feature `ptp`) requires real hardware to validate.** The `phc` module is compile-verified
  only (no PTP NIC in CI); lock / holdover / phase / re-lock behaviour MUST be validated on a real
  ST 2059 fabric (forced grandmaster drop) before any pro-mode phase-holdover promise. The servo and
  the lock-state machine are unit/property tested GPU-/NIC-free.
- **A new `AlarmKind::ReferenceLoss`** (non-breaking, `#[non_exhaustive]`) is the normative loss
  signal shared by the overlay, the control plane, AES67, and M12 — one alarm, one meaning.
- **Normative sources to track:** SMPTE ST 2059-1:2015 (SMPTE Epoch / frame-grid), ST 2059-2:2021
  (PTP profile, message intervals, 5 s fast-lock / 1 µs inter-slave), IEEE 1588-2008, RFC 7273
  (`ts-refclk`/`mediaclk`), RFC 5905 (NTP). The representative servo/lock thresholds in
  `ReferenceConfig` are deployment-tunable, not fixed by the standards.
