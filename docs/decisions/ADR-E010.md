# ADR-E010: Wire the failure-learning ledger into the placement scorer + engine slow tick

- **Status:** Proposed
- **Area:** Efficiency / HAL / Engine
- **Date:** 2026-07-11
- **Source:** agent session (task #36 HAL-1 design-decision pass; the rule-6 modeled-but-not-wired sweep)
- **Builds on:** [ADR-0018](ADR-0018.md) (adaptive affinity-first placement — the `select`
  decision engine + closed-loop re-placement controller), [ADR-0035](ADR-0035.md) (self-aware
  placement SENSE → DETECT → WARN → PLAN → APPLY — which built the slow control tick this ledger
  folds into), [ADR-0017](ADR-0017.md) (monitoring + the wait-free `DeviceLoad` snapshot),
  [ADR-E008](ADR-E008.md) (cost-model-driven planner — the placement cost the penalty is a term of).
  `crates/multiview-hal/src/failure.rs` cites ADR-0035.
- **Relates to:** invariant #1 (output clock — placement runs off the clock thread), #2 (last-good
  held across a failure-driven re-select), #9 (closed-loop degradation / hysteresis — the penalty
  decay **is** the hysteresis here), #10 (isolation — the producer only pushes onto a bounded
  drop-oldest channel and the slow tick drains it; neither can back-pressure the data plane).

## Context

`multiview-hal::failure` (`FailureLedger` + `FailureSignal` + `HardwareId`) is a built,
unit-tested, **pure, deterministic, wait-free-readable** data structure: a decaying
per-`(Stage, HardwareId)` penalty so a placement that keeps **flapping** on a piece of hardware is
*avoided* on the next selection — *"a decode that keeps flapping on a GPU → stop trying to decode
it **there**"* — without a permanent ban. The penalty decays exponentially by a configured
half-life (self-healing once failures stop) and crosses `is_excluded` only at an extreme
hard-avoid threshold. The module carries no clock; every call takes `now_ns: i64`, so behaviour is
reproducible in a unit test. Its own module doc names the exact producer/consumer seams.

**It is wired at neither end** — a rule-6 modeled-but-not-wired defect surfaced by the task #36
sweep. Verified on main `3842b275`:

- `rg 'FailureSignal::' crates/` → **0 non-test hits**: no data-plane seam raises a signal.
- `FailureLedger::penalty_for` / `::is_excluded` have **zero non-test callers**: the scorer never
  consults it.
- `FailureLedger::record` has **no producer**: nothing folds a signal into the ledger.

The two **host sites** the wiring folds into, however, **exist and run today** — they were built by
ADR-0018 (the planner) and ADR-0035 (the slow control tick), since landed:

- **Consumer** — the placement scorer `select_device` (`crates/multiview-hal/src/select.rs:452`) →
  `score_candidate` (`:697`, the blended DRF-share + Tetris-fit cost) + `passes_hard_gates`
  (`:633`, the candidate-set gate). `select_device` has **6 runtime callers**:
  `cli/pipeline.rs:626/795/1006`, `cli/placement.rs:271`, `engine/placement.rs:479`,
  `engine/migration.rs:318`.
- **Record / drain seam** — the engine slow control tick `cli/placement.rs::run` (`:326-350`), a
  ~1 Hz `CONTROL_PERIOD` loop (poll `DeviceLoad` → publish snapshot →
  `coordinator.observe_only()`), spawned live from `cli/main.rs:719` on multi-GPU hosts — the
  natural single-writer that will drain the signal channel and fold each into the ledger.

The **producer proper** — `FailureSignal` emission at the `multiview-ffmpeg` fault seams — **does
not exist yet** (the 0-non-test-emitter gap above); it is the new machinery the Decision's point 1
builds. So the ledger has two live host sites and one seam still to raise the signal.

So the shape is **mostly a connect** — wiring the two existing host sites is not new machinery —
with **one** genuinely new piece: the producer emission (Decision point 1) — raising `FailureSignal`
at the **existing** `multiview-ffmpeg` fault seams, which emit nothing today. Even as connect-plus-one-producer
it is **size L** and touches an AES67-locked file.

## Decision

**WIRE** — not REMOVE; the design verdict is WIRE, **with implementation blocked on #256** (a
file-territory lock, not an unbuilt dependency — the task #185 HAL-1 design-pass verdict, concurred
2026-07-11). This ADR is the committed design; **IMPL is sequenced post-#256 and is GPU-validated**
(see Consequences), and ships **rule-6-complete** — producer + record + consumer land in one PR,
never a partial. Three connect points:

1. **Producer — raise `FailureSignal` at the data-plane fault seams** (`multiview-ffmpeg`,
   feature-gated). The seams the module doc already names: the `*_cuvid`/hwaccel HW→SW fallback
   site (`decode_init_failed`); the supervisor's consecutive-fault debounce crossing its threshold
   (`decode_flapping` — a single corrupt packet must **not** penalise the GPU; bad inputs are the
   product); hwframe-pool / device OOM (`gpu_out_of_memory`); an NVENC session-exhausted open
   (`nvenc_session_exhausted`, distinct from a generic open error); any backend device-lost / reset
   (`device_lost`). Each is keyed to the faulting `(Stage, HardwareId)` via the stable `DeviceId`
   (never the enumeration index). Signals are pushed onto a **bounded drop-oldest** `FailureSignal`
   channel (inv #10) — the data plane never blocks on the send.

2. **Record — drain the channel on the slow tick** (`cli/placement.rs::run`). Before
   `observe_only()`, drain the bounded channel and fold each signal via
   `FailureLedger::record(signal, now_ns)`. The ledger is owned by the slow-tick task (single
   writer); `now_ns` is that tick's monotonic clock. Never on the output-clock thread (inv #1).

3. **Consume — score against the ledger** (`multiview-hal::select`). `select_device` takes
   `&FailureLedger` + `now_ns`; `score_candidate` adds `weight * penalty_for(stage, hw, now_ns)` to
   the blended placement cost, and `passes_hard_gates` drops any candidate for which
   `is_excluded(stage, hw, now_ns)` is true. Because the penalty decays, the exclusion lifts on its
   own once the failures stop.

The `select_device` signature grows `(+ &FailureLedger, + now_ns)`, threaded through all 6 callers.

## Rationale

- **Not REMOVE.** Failure-history exclusion is unique to this module. `passes_hard_gates` today
  gates only on capability / cost-budget / free-VRAM / NVENC-ceiling — present **capacity**, not
  **failure history**; the "flapping"/anti-flap logic in `select`, `split`, `degradation`, and the
  planner is the degradation-**ladder** hysteresis, unrelated to per-hardware fault history.
  Deleting the ledger drops the only *"stop retrying a flapping placement"* mechanism, an
  ADR-0018/0035 resilience requirement.
- **Not BLOCKED on design.** Unlike the schema.rs REMOVE cluster, no unbuilt epic gates the
  connect: both host sites are built and running. The *implementation* is blocked on #256 (the
  file-territory lock) and gated on hardware validation — a **timing** gate on shared-file
  contention, not a design gate.
- **One PR, both ends (rule 6).** A producer with no consumer — or a consumer reading a ledger
  nothing writes — is itself modeled-but-not-wired and unfalsifiable in production. The ledger only
  earns its keep when a real signal raises a real penalty that a real selection reads.
- **Clean layering.** ADR-0018 built `select`; ADR-0035 built the slow control tick
  (`cli/placement.rs::run`) this ledger folds into; ADR-E008 defines the placement cost the penalty
  is a term of. This ADR is the final hop that ADR-0035's PLAN/APPLY left as a documented seam.

## Alternatives considered

| Alternative | Rejected because |
| ----------- | ---------------- |
| **REMOVE** the `failure` module as dead code | It is the *only* failure-history exclusion path; nothing supersedes it (contrast I1 / `negotiate_answer`, genuinely superseded by `multiview-webrtc`). Removing it drops an ADR-0018/0035 resilience requirement. |
| **BLOCKED on an unbuilt placement dependency** — defer to a future epic | The *design* has no unbuilt dependency: both host sites exist and run today. (The *implementation* is sequenced behind the #256 file-territory lock — a timing gate on shared-file contention, not a missing dependency.) |
| Ship the **consumer only** (scorer reads a ledger nothing writes) | Rule 6 — a penalty that is always 0 changes no behaviour and is unfalsifiable; modeled-but-not-wired by another name. |
| Ship the **producer only** (raise signals nothing reads) | Same failure — signals into a ledger no selection consults change no placement. |
| Raise `FailureSignal` **synchronously on the output-clock / data-plane thread** | Violates inv #1/#10 — the data plane must never touch the ledger or block on a channel send. It pushes onto a bounded drop-oldest channel; the slow tick folds it. |
| Fold signals in a **new dedicated task** rather than the existing slow tick | Adds a second control loop for no gain — the ADR-0035 slow tick already runs at ~1 Hz off-thread, owns the placement view, and is the natural single writer of the ledger. |

## Consequences

**Sequencing — why this is not startable-now (the IMPL constraints):**

- **pipeline.rs is #256-locked.** The `select_device(+ &FailureLedger, + now_ns)` signature change
  hits all 6 callers **including** `cli/pipeline.rs:626/795/1006` — and `pipeline.rs` is LANE-CORE,
  owned by the AES67 #256 lane. IMPL is therefore **BLOCKED-on-#256** (the same file-territory gate
  as the schema.rs REMOVE cluster), not blocked-on-design.
- **GPU validation (rule 26).** The producer signals fire from real FFI / hardware fault paths
  (device-lost, NVENC-exhausted, HW→SW `cuvid` fallback, hwframe OOM) that emit nothing today.
  Raising and validating them requires a **real-GPU runner leg**, not software CI — bulletproofing
  the bad/contended-input paths is part of "done," not a follow-up.

**Scope / cost.** Size **L**, 4 crates: `multiview-ffmpeg` (producers, feature-gated),
`multiview-hal` (scorer signature), `multiview-engine` + `multiview-cli` (channel ownership +
slow-tick drain + ledger lifetime). The public `select_device` signature change is the blast
radius; enabling a hardware feature must not change any *default* public API — the ledger param
threads through existing callers, and the default pure-Rust build still places nothing.

**Invariants.** #1 preserved (all ledger I/O off the clock thread); #10 preserved (bounded
drop-oldest producer channel, slow-tick drain, `observe_only` still only *proposes*); #9 extended
(the penalty decay is placement-level hysteresis layered on the degradation ladder); #2 unaffected
(last-good holds across any re-select a penalty triggers).

**Residual risk (for the IMPL).** The penalty is only as good as its attribution. Every producer
seam must recover the **exact faulting `(Stage, HardwareId)`** at the async FFI / supervisor
boundary where the fault surfaces — a boundary where the originating stage and device are easy to
lose (a `get_format` callback on a decoder thread, a supervisor that already unwound the actor, a
pooled hwframe whose device context is one hop removed). A **misattributed** signal penalises — and
at the hard threshold *excludes* — a **healthy** device, forcing exactly the needless migration off
good hardware the ledger exists to prevent. The IMPL must key each signal to the stable `DeviceId`
of the device that actually faulted (never the enumeration index, never a neighbouring stage's), and
the producer's efficiency/correctness review must treat attribution accuracy as a first-class
correctness property, not a detail.

**Follow-through (rule 27).** On IMPL merge, flip `failure.rs`'s *"wiring is NOT in this crate"*
module doc to point at the landed producer / record / consumer sites, and advance ADR-0035's
PLAN/APPLY status accordingly. Until IMPL lands, `failure.rs` stays honestly documented as the
pure ledger with its seams named but unwired.
