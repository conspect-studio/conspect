//! The off-hot-path **placement controller** (ADR-0018 §4, the closed-loop
//! re-placement extension of invariant #9).
//!
//! Where [`multiview_hal::select_device`] is the *pure* admission/reconfig
//! placement decision (which single GPU hosts a pipeline's whole
//! `decode -> composite -> encode` island) and
//! [`multiview_hal::plan_split`](multiview_hal::split::plan_split) is the
//! deliberate last-resort cut, this module is the **live controller** that
//! senses sustained overload and *proposes* a corrective action — extending the
//! degradation control loop ([`crate::degrade`]) from *degradation* to
//! *placement + recovery*.
//!
//! It is a **pure value machine over injected [`DeviceLoad`] snapshots**
//! ([ADR-I001](../../../docs/decisions/ADR-I001.md)) — exactly the discipline of
//! the rest of the engine's control machinery (the alarm/tally/HA state
//! machines). [`PlacementController::observe`] takes one wait-free snapshot and
//! *returns* a [`PlacementProposal`]; it does **no** I/O, spawns no task, holds
//! no lock, and **never `.await`s** the engine or a client — so it is
//! structurally incapable of stalling the output clock or back-pressuring the
//! engine (invariants #1 + #10). The supervisor + make-before-break mechanism
//! ([ADR-R004](../../../docs/decisions/ADR-R004.md)) *execute* a proposal on the
//! control plane; the controller only proposes.
//!
//! The decision pipeline each control tick (ADR-0018 §4):
//!
//! 1. **Low-pass + detect sustained overload.** Each device's dominant-resource
//!    share is smoothed by an EWMA; the controlled pipeline's *current* device
//!    feeds the existing [`Hysteresis`]
//!    controller (reused verbatim — no new anti-flap math). A **transient** spike
//!    never trips it; only a smoothed value that crosses the hysteresis-high
//!    threshold and stays there for `>= dwell` ticks raises an overload.
//! 2. **A pin always wins; scanout affinity is just as absolute.** A pipeline
//!    pinned (by stable [`DeviceId`]) to its current device is never migrated off
//!    it — the operator override is absolute (ADR-0018 §2.1). A pipeline whose
//!    composite feeds a **local display sink** is likewise affinity-pinned to the
//!    connector-owning GPU ([ADR-0044](../../../docs/decisions/ADR-0044.md) §3):
//!    the scanout framebuffer must live on that GPU, so migrating or splitting
//!    composite off it would force the per-frame GPU→host→GPU copy ADR-0018
//!    forbids. Both may still *shed* locally.
//! 3. **SHED vs MIGRATE.** On a sustained overload the controller re-runs
//!    [`select_device`] over the *other* candidate
//!    GPUs. If a materially-better home exists (its score beats the current
//!    device's by `>= min_gain`) **and** the anti-storm gate allows, it proposes
//!    a make-before-break [`PlacementProposal::Migrate`]; otherwise the imbalance
//!    cannot be cured by moving (the whole host is hot), so it proposes the
//!    cheaper local [`PlacementProposal::Shed`].
//! 4. **Anti-storm damping.** Three independent damps bound migration frequency
//!    (ADR-0018 §4.6): a per-pipeline **cooldown** after any migration, a
//!    per-GPU **migration budget** over a rolling window, and the **min-gain**
//!    gate above. A marginal or too-frequent migration is suppressed (the
//!    controller holds or sheds instead).
//!
//! On a single-GPU host there is no other candidate to migrate to, so the
//! controller proposes only [`PlacementProposal::Hold`] or
//! [`PlacementProposal::Shed`] — i.e. **zero placement behaviour change** beyond
//! the degradation loop that already runs (ADR-0018 §consequences).

use std::collections::HashMap;

use multiview_hal::degradation::{Hysteresis, HysteresisConfig, LadderMove};
use multiview_hal::select::{
    select_device, GpuCandidate, Pins, PipelineDemand, PlacementPolicy, RejectReason, Selection,
};
use multiview_hal::split::{plan_split, SplitOutcome, SplitPlan, SplitPolicy};
use multiview_hal::{DeviceId, DeviceLoad, Stage};

/// Why the controller proposed a shed rather than a migration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ShedReason {
    /// The pipeline is pinned to its (overloaded) device, so it may not migrate;
    /// the cheap local lever is the only available relief.
    Pinned,
    /// The pipeline feeds a **local display sink** whose framebuffer must live on
    /// the connector-owning GPU (ADR-0044 §3): migrating or splitting composite
    /// off that GPU would force the per-frame GPU→host→GPU copy ADR-0018 forbids,
    /// so the scanout affinity is absolute and the only relief is a local shed.
    DisplayBound,
    /// No other candidate GPU is a materially-better home (the imbalance cannot
    /// be cured by moving — the whole host is loaded), so shed locally.
    NoBetterHome,
    /// A better home exists but the anti-storm gate (cooldown / per-GPU budget)
    /// currently forbids migrating, so shed locally this tick.
    AntiStorm,
}

/// A make-before-break migration the controller proposes (ADR-0018 §4.4,
/// [ADR-R004](../../../docs/decisions/ADR-R004.md) Class-2).
///
/// The controller *proposes*; the supervisor + scene-swap machinery execute the
/// parallel spin-up + IDR-aligned cutover + teardown. [`Self::idr_aligned`] is
/// always `true`: the cutover lands on an IDR/GOP boundary by construction
/// (Multiview pins output GOP + drives `forceIDR`), so the output clock drops no
/// frame (invariant #1).
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct MigrationPlan {
    /// The overloaded device the pipeline migrates **off**.
    pub from: DeviceId,
    /// The chosen better home the pipeline migrates **to**.
    pub to: DeviceId,
    /// The modelled score improvement (`current_score - new_score`, lower score
    /// is better) the migration buys — `>= min_gain` by the gate.
    pub gain: f32,
    /// Always `true`: the cutover is IDR/GOP-aligned make-before-break, so no
    /// output frame is dropped.
    pub idr_aligned: bool,
}

impl MigrationPlan {
    /// Build a migration plan from `from` → `to` with the modelled `gain`.
    ///
    /// `idr_aligned` is always `true` by construction (Multiview pins the output
    /// GOP + drives `forceIDR`, so the cutover boundary is an IDR boundary). The
    /// controller builds plans internally; this constructor lets the
    /// make-before-break executor ([`crate::migration`]) and tests build one
    /// without the controller (the struct is `#[non_exhaustive]`, so an
    /// out-of-module struct literal is impossible).
    #[must_use]
    pub const fn new(from: DeviceId, to: DeviceId, gain: f32) -> Self {
        Self {
            from,
            to,
            gain,
            idr_aligned: true,
        }
    }
}

/// The action the placement controller proposes for one control tick.
///
/// The controller never executes anything — it returns one of these for the
/// control plane to act on (or ignore). `Hold` is the overwhelmingly common
/// case (no sustained overload).
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum PlacementProposal {
    /// No sustained overload (or a transient that the EWMA/dwell absorbed):
    /// leave the placement untouched.
    Hold,
    /// A sustained overload that migrating cannot cure (or must not, per pin /
    /// anti-storm): relieve it with the cheaper local degradation ladder.
    Shed {
        /// Why a shed was proposed instead of a migration.
        reason: ShedReason,
    },
    /// A sustained overload that a materially-better home can cure: migrate the
    /// whole island make-before-break.
    Migrate(MigrationPlan),
    /// No single GPU can host the pipeline even after degrade-to-fit, but a
    /// deliberate cost-accounted split clears its gain gate: cut the island
    /// across two GPUs (the last resort, ADR-0018 §20).
    Split(SplitPlan),
}

/// Tuning for the placement controller (ADR-0018 §4.6 / §consequences — policy
/// as data, conservative defaults).
///
/// `overload` is the [`Hysteresis`] band the EWMA'd dominant-resource share is
/// tested against (reused verbatim from the degradation ladder); `ewma_alpha` is
/// the low-pass smoothing factor (`0.0..=1.0`, higher = less smoothing);
/// `dwell_ticks` is how many consecutive sustained-high ticks must pass before
/// an overload is raised (transients below this never migrate);
/// `migration_cooldown_ticks` is the per-pipeline dwell after any migration;
/// `per_gpu_budget` over `budget_window_ticks` caps migrations touching one GPU;
/// `min_gain` is the minimum score improvement a migration must buy.
///
/// (`PartialEq` is intentionally not derived: the embedded
/// [`PlacementPolicy`] does not
/// implement it, and a controller config is never compared for equality.)
#[derive(Debug, Clone, Copy)]
pub struct PlacementControllerConfig {
    /// The overload-detection hysteresis band (EWMA'd dominant share vs
    /// high/low). Reused verbatim from `multiview-hal`.
    pub overload: HysteresisConfig,
    /// EWMA smoothing factor (`0.0..=1.0`); the share each tick is
    /// `alpha * sample + (1 - alpha) * previous`. Lower = smoother (more
    /// transient rejection).
    pub ewma_alpha: f32,
    /// Consecutive sustained-high control ticks required before an overload is
    /// raised. `0` collapses to "act the first tick the band is crossed".
    pub dwell_ticks: u32,
    /// Per-pipeline cooldown (control ticks) after any migration before another
    /// is permitted.
    pub migration_cooldown_ticks: u32,
    /// The maximum number of migrations touching one GPU (as source or
    /// destination) within `budget_window_ticks`.
    pub per_gpu_budget: u32,
    /// The rolling window (control ticks) over which `per_gpu_budget` is counted.
    pub budget_window_ticks: u32,
    /// The minimum score improvement (`current - candidate`, lower is better) a
    /// migration must buy to be proposed.
    pub min_gain: f32,
    /// The pure placement policy passed to [`select_device`] on re-selection.
    pub select_policy: PlacementPolicy,
    /// The deliberate-split policy passed to [`plan_split`] as the last resort.
    pub split_policy: SplitPolicy,
}

impl PlacementControllerConfig {
    /// The ADR-0018 conservative defaults: smooth heavily (`alpha = 0.3`), a
    /// 3-tick sustained dwell, a 10-tick migration cooldown, a 2-per-GPU budget
    /// over a 60-tick window, and a `min_gain` of `0.1` (a migration must beat
    /// the current home by at least 10 dominant-share points).
    #[must_use]
    pub fn new_default() -> Self {
        Self {
            overload: HysteresisConfig::new_default(),
            ewma_alpha: 0.3,
            dwell_ticks: 3,
            migration_cooldown_ticks: 10,
            per_gpu_budget: 2,
            budget_window_ticks: 60,
            min_gain: 0.1,
            select_policy: PlacementPolicy::new_default(),
            split_policy: SplitPolicy::new_default(),
        }
    }

    /// The clamped EWMA factor in `0.0..=1.0` (a non-finite / out-of-range value
    /// falls back to `1.0` = no smoothing, the conservative "never hide a real
    /// reading" choice).
    fn clamped_alpha(&self) -> f32 {
        if self.ewma_alpha.is_finite() && (0.0..=1.0).contains(&self.ewma_alpha) {
            self.ewma_alpha
        } else {
            1.0
        }
    }
}

impl Default for PlacementControllerConfig {
    fn default() -> Self {
        Self::new_default()
    }
}

/// A per-GPU rolling-window migration ledger (ADR-0018 §4.6 anti-storm budget).
///
/// Records the control-tick at which each migration touched a GPU (as source or
/// destination); a GPU is over budget when `per_gpu_budget` migrations fall
/// within the trailing `budget_window_ticks`. Mirrors the supervisor's
/// `max_restarts`-over-a-window pattern.
#[derive(Debug, Clone, Default)]
struct MigrationLedger {
    /// For each touched GPU, the ticks at which it was migrated to/from (kept
    /// pruned to the rolling window on each query).
    events: HashMap<DeviceId, Vec<u64>>,
}

impl MigrationLedger {
    /// How many migrations touched `device` within the trailing `window` ticks
    /// ending at `now`.
    fn count_in_window(&self, device: &DeviceId, now: u64, window: u32) -> u32 {
        let horizon = now.saturating_sub(u64::from(window));
        self.events.get(device).map_or(0, |ticks| {
            let n = ticks.iter().filter(|&&t| t >= horizon).count();
            u32::try_from(n).unwrap_or(u32::MAX)
        })
    }

    /// Record that a migration touched both `from` and `to` at tick `now`.
    fn record(&mut self, from: &DeviceId, to: &DeviceId, now: u64) {
        self.events.entry(from.clone()).or_default().push(now);
        self.events.entry(to.clone()).or_default().push(now);
    }
}

/// The live, off-hot-path placement controller for one pipeline (ADR-0018 §4).
///
/// Constructed at admission with the pipeline's demand, its candidate GPUs, its
/// current home, and any operator pins. Driven once per **slow control tick**
/// (not the per-frame output clock) by [`PlacementController::observe`] with the
/// latest wait-free [`DeviceLoad`] snapshot; it returns a [`PlacementProposal`].
///
/// It owns only value state (EWMA registers, the overload [`Hysteresis`], a
/// cooldown counter, the migration ledger, a tick counter) — no thread, no
/// channel, no lock. The engine owns the snapshot publication and the control
/// thread; this type is the pure brain it calls.
#[derive(Debug, Clone)]
pub struct PlacementController {
    config: PlacementControllerConfig,
    demand: PipelineDemand,
    candidates: Vec<GpuCandidate>,
    pins: Pins,
    /// The device currently hosting the pipeline island.
    current: DeviceId,
    /// Per-device EWMA of the dominant-resource share (the low-pass).
    ewma: HashMap<DeviceId, f32>,
    /// The reused anti-flap overload detector over the current device's EWMA.
    overload: Hysteresis,
    /// Consecutive sustained-high ticks seen so far (the dwell counter).
    sustained_ticks: u32,
    /// Per-pipeline migration cooldown remaining (ticks).
    cooldown_remaining: u32,
    /// The rolling per-GPU migration budget ledger.
    ledger: MigrationLedger,
    /// The monotonic control-tick counter (drives the rolling window + cooldown).
    tick: u64,
}

impl PlacementController {
    /// Construct a controller for a pipeline currently hosted on `current`.
    ///
    /// `demand` is the pipeline's work shape (the same the planner admits);
    /// `candidates` are the GPUs that *could* host it; `pins` carries any
    /// operator override; `config` is the policy/tuning.
    #[must_use]
    pub fn new(
        config: PlacementControllerConfig,
        demand: PipelineDemand,
        candidates: Vec<GpuCandidate>,
        pins: Pins,
        current: DeviceId,
    ) -> Self {
        // Precondition the scanout-affinity refusal rests on: if the pipeline
        // declares a sink locality, the device it is *currently* placed on must be
        // one of those connector-owning GPUs (the admission path placed it there).
        // Were `current` outside the locality, `is_display_bound_here` would read
        // false and the controller could migrate composite off the scanout GPU —
        // the exact thing the affinity forbids. Validated in debug builds; an empty
        // locality (no display sink) imposes no constraint.
        debug_assert!(
            demand.sink_localities().is_empty() || demand.sink_localities().contains(&current),
            "a display-bound pipeline's current device must be in its sink locality"
        );
        let overload = Hysteresis::new(config.overload);
        Self {
            config,
            demand,
            candidates,
            pins,
            current,
            ewma: HashMap::new(),
            overload,
            sustained_ticks: 0,
            cooldown_remaining: 0,
            tick: 0,
            ledger: MigrationLedger::default(),
        }
    }

    /// The device currently hosting the pipeline island.
    #[must_use]
    pub const fn current_device(&self) -> &DeviceId {
        &self.current
    }

    /// The smoothed (EWMA) dominant-resource share of the current device, if it
    /// has been observed at least once. Exposed for telemetry/testing.
    #[must_use]
    pub fn current_share(&self) -> Option<f32> {
        self.ewma.get(&self.current).copied()
    }

    /// Whether the pipeline is pinned to its current device (so it may never be
    /// migrated off it).
    #[must_use]
    pub fn is_pinned_here(&self) -> bool {
        self.pins.pipeline() == Some(&self.current)
    }

    /// Whether the pipeline feeds a local display sink anchored to its current
    /// device — i.e. the current device is in the demand's scanout-locality set
    /// (ADR-0044 §3), so composite may never be migrated/split off it.
    ///
    /// A pipeline with no display sink (empty locality) is never display-bound.
    /// The check is by membership, so on a multi-GPU host the affinity binds
    /// composite to whichever connector-owning GPU currently hosts it.
    #[must_use]
    pub fn is_display_bound_here(&self) -> bool {
        let localities = self.demand.sink_localities();
        !localities.is_empty() && localities.contains(&self.current)
    }

    /// Observe one wait-free [`DeviceLoad`] snapshot and propose an action.
    ///
    /// This is the entire control step. It is pure and synchronous: it updates
    /// the per-device EWMA registers, drives the overload detector over the
    /// current device's smoothed share, and — only on a **sustained** overload —
    /// decides SHED vs MIGRATE (vs the last-resort split) under the anti-storm
    /// gate. It never blocks, never awaits, never performs I/O.
    pub fn observe(&mut self, loads: &[DeviceLoad]) -> PlacementProposal {
        self.tick = self.tick.saturating_add(1);
        self.cooldown_remaining = self.cooldown_remaining.saturating_sub(1);

        // 1. Low-pass every visible device's dominant-resource share.
        for load in loads {
            let sample = dominant_share(load);
            self.update_ewma(&load.device_id, sample);
        }

        // 2. Drive the overload detector over the *current* device's smoothed
        //    share. A device with no observed share yet reads as idle (0.0): no
        //    overload can be raised before we have evidence.
        let share = self.ewma.get(&self.current).copied().unwrap_or(0.0);
        let mv = self.overload.observe(f64::from(share));
        let sustained = self.note_sustained(mv);

        if !sustained {
            return PlacementProposal::Hold;
        }

        // 3. A pinned pipeline never migrates off its device — shed locally.
        if self.is_pinned_here() {
            return PlacementProposal::Shed {
                reason: ShedReason::Pinned,
            };
        }

        // 3b. A display-bound pipeline (its composite feeds a local KMS scanout
        //     sink) is affinity-pinned to the connector-owning GPU (ADR-0044 §3):
        //     migrating or splitting composite off it would force the per-frame
        //     GPU→host→GPU copy ADR-0018 forbids. So the scanout affinity is a
        //     HARD constraint, never a soft weight — the only relief is a local
        //     shed. On a single-GPU display host this is trivially satisfied
        //     (current IS the only/locality device); the type-level machinery
        //     still models it for the multi-GPU host.
        if self.is_display_bound_here() {
            return PlacementProposal::Shed {
                reason: ShedReason::DisplayBound,
            };
        }

        // 4. SHED vs MIGRATE: re-select over the *other* candidates and compare.
        self.decide_relief(loads, share)
    }

    /// Update the EWMA register for `device` with a fresh `sample` share.
    fn update_ewma(&mut self, device: &DeviceId, sample: f32) {
        let alpha = self.config.clamped_alpha();
        let next = match self.ewma.get(device) {
            Some(&previous) => alpha.mul_add(sample, (1.0 - alpha) * previous),
            // First observation seeds the register with the raw sample.
            None => sample,
        };
        self.ewma.insert(device.clone(), next.clamp(0.0, 1.0));
    }

    /// Fold one overload-detector move into the sustained-dwell counter,
    /// returning whether a **sustained** overload is now raised.
    ///
    /// A [`LadderMove::Down`] (the band's high threshold crossed) increments the
    /// dwell; anything else resets it. An overload is raised only once the dwell
    /// has reached `dwell_ticks` consecutive sustained-high ticks — so a single
    /// transient spike (one `Down` then a relax) never raises it.
    fn note_sustained(&mut self, mv: LadderMove) -> bool {
        match mv {
            LadderMove::Down => {
                self.sustained_ticks = self.sustained_ticks.saturating_add(1);
            }
            LadderMove::Up | LadderMove::Hold => {
                // Still over the high band? The detector saturates at MAX_LEVEL
                // and then returns Hold, so a continued overload must keep the
                // dwell satisfied rather than reset it. We only reset when the
                // smoothed share has dropped back below the high threshold.
                let share = self.ewma.get(&self.current).copied().unwrap_or(0.0);
                if f64::from(share) <= self.config.overload.high {
                    self.sustained_ticks = 0;
                }
            }
        }
        self.sustained_ticks > self.config.dwell_ticks
    }

    /// Decide the relief action for a confirmed sustained overload: migrate to a
    /// materially-better home if one exists and the anti-storm gate allows, else
    /// shed locally (or, as a last resort, split).
    fn decide_relief(&mut self, loads: &[DeviceLoad], current_share: f32) -> PlacementProposal {
        // Re-run the pure selection, excluding the overloaded current device, to
        // find the best alternative home (whole-island, affinity-gated).
        let others: Vec<GpuCandidate> = self
            .candidates
            .iter()
            .filter(|c| c.device_id != self.current)
            .cloned()
            .collect();

        match select_device(
            &others,
            &self.demand,
            loads,
            &Pins::none(),
            self.config.select_policy,
        ) {
            Ok(selection) => self.consider_migration(selection, current_share),
            // No single *other* GPU can host the whole pipeline: the move that
            // would cure the imbalance does not exist. Try the deliberate split
            // as the last resort; otherwise shed locally.
            Err(reason) => self.consider_split_or_shed(loads, &reason),
        }
    }

    /// Decide whether a re-selected better home clears the min-gain + anti-storm
    /// gates; propose the migration if so, else shed.
    fn consider_migration(
        &mut self,
        selection: Selection,
        current_share: f32,
    ) -> PlacementProposal {
        let gain = current_share - selection.score;
        if gain < self.config.min_gain {
            // The alternative is not materially better: moving would not cure the
            // imbalance (and would risk ping-pong). Shed locally instead.
            return PlacementProposal::Shed {
                reason: ShedReason::NoBetterHome,
            };
        }

        if !self.anti_storm_allows(&selection.device) {
            // A better home exists but the cooldown / per-GPU budget forbids a
            // move this tick: hold quality by shedding rather than storming.
            return PlacementProposal::Shed {
                reason: ShedReason::AntiStorm,
            };
        }

        // Commit the migration: record it against the budget + arm the cooldown,
        // then propose it. The controller only proposes; the control plane
        // executes the make-before-break cutover.
        self.ledger
            .record(&self.current, &selection.device, self.tick);
        self.cooldown_remaining = self.config.migration_cooldown_ticks;
        // Reset the dwell so the post-migration device starts fresh (it should
        // not inherit the old home's sustained-overload count).
        self.sustained_ticks = 0;
        let plan = MigrationPlan {
            from: self.current.clone(),
            to: selection.device.clone(),
            gain,
            idr_aligned: true,
        };
        self.current = selection.device;
        self.overload = Hysteresis::new(self.config.overload);
        PlacementProposal::Migrate(plan)
    }

    /// On a no-fit re-selection, try the deliberate last-resort split; if even
    /// that is not justified, shed locally.
    fn consider_split_or_shed(
        &self,
        loads: &[DeviceLoad],
        reason: &RejectReason,
    ) -> PlacementProposal {
        // Only attempt a split when the no-fit cause is the whole pipeline not
        // fitting (degrade-to-fit's territory). An unsatisfiable pin, an empty
        // candidate set, or any future reject is never a split (ADR-0018 §20 — a
        // split needs two GPUs and a real bottleneck); those shed locally.
        let splittable = matches!(
            reason,
            &RejectReason::NoCandidateFitsWholePipeline | &RejectReason::AllOverHeadroomCeiling
        );
        if !splittable {
            return PlacementProposal::Shed {
                reason: ShedReason::NoBetterHome,
            };
        }
        let bottleneck = self.infer_bottleneck_stage();

        let devices: Vec<DeviceId> = self
            .candidates
            .iter()
            .map(|c| c.device_id.clone())
            .collect();
        let outcome: SplitOutcome = plan_split(
            &self.demand,
            &devices,
            loads,
            bottleneck,
            self.config.split_policy,
        );
        match outcome {
            Ok(plan) => PlacementProposal::Split(plan),
            // A split that needs two devices we don't have, has no bottleneck, or
            // does not clear its gain gate is not taken: shed locally instead so
            // the program degrades gracefully but never stalls (inv #1).
            // `SplitReject` is `#[non_exhaustive]`, so a wildcard covers any
            // future reject the same conservative way.
            Err(_) => PlacementProposal::Shed {
                reason: ShedReason::NoBetterHome,
            },
        }
    }

    /// Infer which splittable stage (decode vs encode) is the heavier load on the
    /// controlled pipeline, used as the split hint. `composite` is never split.
    fn infer_bottleneck_stage(&self) -> Option<Stage> {
        let decode = self.demand.stage_load_mpps(Stage::Decode);
        let encode = self.demand.stage_load_mpps(Stage::Encode);
        if decode <= 0.0 && encode <= 0.0 {
            return None;
        }
        if encode >= decode {
            Some(Stage::Encode)
        } else {
            Some(Stage::Decode)
        }
    }

    /// Whether the anti-storm gate currently permits migrating to `target`: the
    /// per-pipeline cooldown must have elapsed, and neither the source nor the
    /// target GPU may be over its per-GPU rolling-window migration budget.
    fn anti_storm_allows(&self, target: &DeviceId) -> bool {
        if self.cooldown_remaining > 0 {
            return false;
        }
        let window = self.config.budget_window_ticks;
        let budget = self.config.per_gpu_budget;
        let source_count = self
            .ledger
            .count_in_window(&self.current, self.tick, window);
        let target_count = self.ledger.count_in_window(target, self.tick, window);
        source_count < budget && target_count < budget
    }
}

/// The dominant-resource share of one device's load (the DRF `max` over its
/// known busy fractions), used as the overload signal.
///
/// VRAM used-fraction, encoder util, decoder util, and the compositor-pressure
/// busy fraction are each considered where known; the largest is the dominant
/// share. A device with *no* known signal reads as `0.0` (idle) — an unknown
/// device must never *fabricate* an overload, exactly as the selector never
/// fabricates a metric.
fn dominant_share(load: &DeviceLoad) -> f32 {
    let mut dominant = 0.0_f32;
    let mut consider = |value: Option<f32>| {
        if let Some(v) = value {
            if v.is_finite() {
                dominant = dominant.max(v.clamp(0.0, 1.0));
            }
        }
    };
    consider(load.vram_used_frac());
    consider(load.enc_util_frac);
    consider(load.dec_util_frac);
    consider(load.effective_compute_frac());
    dominant
}
