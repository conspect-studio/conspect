//! The published accounting record and the sampler that produces it.
//!
//! [`ResourceUsage`] is the wire shape ADR-0099 decision 8 specifies: owner,
//! generation, phase, CPU cores, cumulative CPU and GPU seconds, memory and
//! peak memory, plus GPU utilisation and VRAM. GPU fields are `Option` because a
//! missing value reports **unknown**, never zero — the same rule the GPU-load
//! probe already follows.
//!
//! [`Sampler`] is deliberately clock- and I/O-free. It holds the previous
//! reading and the monotonic cumulative counters, and its
//! [`record`](Sampler::record) takes the clock reading as an argument. There is
//! no `SystemTime`, no `Instant::now` and no thread anywhere in this module, so
//! a test can drive a whole soak's worth of samples deterministically — and the
//! later 1 Hz task owns the actual cadence off the data plane.

use crate::cgroup::CgroupUsage;
use crate::error::{Error, Result};
use crate::fsroot::FsRoot;
use crate::job::JobPhase;
use crate::process_tree::{
    ratio_to_f32, sample_process_tree, ticks_to_seconds, ExpectedRoot, PreviousSweep,
    ProcessTreeSample, TreeSample, TypeAccountingArray, TypeSecondsArray, TYPE_COUNT,
};
use crate::scope::{Generation, ResourceScope, ScopeOwner, ScopeRegistry};

/// The cap a reported CPU figure saturates at, in cores.
///
/// The kernel lets a scope use as many cores as a host has; this crate bounds
/// the wire value so a garbled counter cannot produce an absurd number, and the
/// bound is high enough that no host this runs on reaches it.
pub const CPU_CORES_CAP: f32 = 64.0;

/// Whether a figure is kernel-exact or estimated.
///
/// `#[non_exhaustive]`: an `Unknown`/`mixed` accounting state is plausible as
/// the GPU slices land, so consumers carry a wildcard.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Accounting {
    /// Kernel-enforced cgroup accounting: nothing can be missed.
    Exact,
    /// A process-tree walk: a descendant that exited between two reads, or a
    /// GPU figure that could not be attributed, makes the result an estimate
    /// (ADR-0099 decision 8).
    ///
    /// The conservative default: a scope whose kind is not known to be a
    /// delegated cgroup is never reported as exact.
    #[default]
    Estimated,
}

/// One published resource sample.
///
/// Every field is either measured or honestly absent. `cpu_cores` is `None`
/// until two readings a known time apart exist, and the GPU fields are `None`
/// when the value is unknown.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct ResourceUsage {
    /// Who the resources are charged to.
    pub owner: ScopeOwner,
    /// The anti-misattribution generation.
    pub generation: Generation,
    /// The named phase the scope is in.
    pub phase: JobPhase,
    /// CPU in **cores** (`1.0` = one core fully used, `2.0` = two), when known.
    ///
    /// Saturates at [`CPU_CORES_CAP`].
    pub cpu_cores: Option<f32>,
    /// Cumulative CPU seconds charged to the scope (monotonic within a
    /// generation).
    pub cpu_seconds_total: f64,
    /// Current memory in bytes, when known.
    pub mem_bytes: Option<u64>,
    /// Lifetime peak memory in bytes, when known and available.
    pub mem_peak_bytes: Option<u64>,
    /// GPU utilisation fraction in `0.0..=1.0`, or `None` when unknown.
    pub gpu_util: Option<f32>,
    /// VRAM in bytes, or `None` when unknown.
    pub vram_bytes: Option<u64>,
    /// Cumulative GPU seconds, or `None` when no GPU attribution exists.
    pub gpu_seconds_total: Option<f64>,
    /// Whether the figures are exact or estimated.
    pub accounting: Accounting,
}

impl ResourceUsage {
    /// A sample with every optional GPU field unknown.
    ///
    /// Separate from the struct literal so a caller cannot accidentally build a
    /// "zero GPU" record: unknown is `None` here, by construction.
    #[must_use]
    pub fn unknown_gpu(
        owner: ScopeOwner,
        generation: Generation,
        phase: JobPhase,
        accounting: Accounting,
    ) -> Self {
        Self {
            owner,
            generation,
            phase,
            cpu_cores: None,
            cpu_seconds_total: 0.0,
            mem_bytes: None,
            mem_peak_bytes: None,
            gpu_util: None,
            vram_bytes: None,
            gpu_seconds_total: None,
            accounting,
        }
    }

    /// Whether any GPU figure is known.
    #[must_use]
    pub fn has_gpu(&self) -> bool {
        self.gpu_util.is_some() || self.vram_bytes.is_some() || self.gpu_seconds_total.is_some()
    }
}

/// CPU in **cores** from two cumulative readings a known time apart.
///
/// `1.0` means one core fully used; two CPU-seconds burned in one wall-clock
/// second is `2.0`, which is exactly what a multi-process browser tree can
/// produce and what a `0.0..=1.0` fraction hid.
///
/// This is the pure core of the sampler, and it is total by construction: it is
/// `pub` so a caller can unit-test the arithmetic without a `Sampler`.
///
/// * A non-positive or non-finite `elapsed_seconds` yields `None`: no interval
///   has passed, so there is nothing honest to divide by.
/// * A **counter reset** — `current < previous` — yields `Some(0.0)`: the scope
///   reported less CPU than before, which can only be a reset (or a wrapped
///   kernel counter), and reporting the drop as a negative share would be worse
///   than reporting no work in the interval.
/// * The result is saturated at [`CPU_CORES_CAP`] rather than panicking on an
///   over-range ratio (an infinite ratio saturates too, rather than reporting a
///   number the wire cannot carry).
#[must_use]
pub fn cpu_cores(previous_seconds: f64, current_seconds: f64, elapsed_seconds: f64) -> Option<f32> {
    if elapsed_seconds.is_nan() || elapsed_seconds <= 0.0 {
        return None;
    }
    let delta = if current_seconds >= previous_seconds {
        current_seconds - previous_seconds
    } else {
        // Counter reset (or a scope that moved generations): charge nothing.
        0.0
    };
    let ratio = delta / elapsed_seconds;
    if ratio.is_nan() || ratio <= 0.0 {
        return Some(0.0);
    }
    // An infinite ratio is a saturated scope, not an unknown: clamp it.
    Some(ratio_to_f32(ratio, CPU_CORES_CAP))
}

/// The monotonic value of `current`, given the `previous` reading.
///
/// Holds the previous value when the scope's counter **decreased** — a kernel
/// counter reset, or a sample taken across a scope replacement — so a reported
/// cumulative total never goes backwards.
///
/// The *total* is what must be monotonic; the CPU *cores* figure for the
/// interval is computed separately, by [`cpu_cores`], from the un-clamped
/// readings, so a reset does not suppress the next interval's measurement.
#[must_use]
pub fn monotonic_total(previous: f64, current: f64) -> f64 {
    if current >= previous {
        current
    } else {
        previous
    }
}

/// The sampler's state: the two things that make a delta meaningful.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Reading {
    /// The injected monotonic clock value, in seconds.
    at: f64,
    /// The scope's own counter at that instant, exactly as read.
    ///
    /// Kept raw (not clamped) so the *interval* after a reset is measured from
    /// the reset value while the *published total* is held separately in
    /// [`Reading::held`].
    counter: f64,
    /// The monotonic total published for the scope at that instant.
    held: f64,
}

/// Turns successive scope readings into [`ResourceUsage`] records.
///
/// The sampler owns no clock and performs no I/O: [`record`](Self::record) takes
/// the monotonic time as an argument. It is not `Sync` and holds no thread — the
/// later 1 Hz task calls it from one place, off the data plane.
///
/// # One sampler per `{owner, generation}`
///
/// A sampler's retained sweep state and cumulative CPU total belong to **one**
/// process tree. When a tree walk reports
/// [`RootGone`](TreeSample::RootGone) the sampler **clears** that state, latches
/// itself finished, and refuses every further
/// [`sample_tree`](Self::sample_tree) with
/// [`Error::ScopeEnded`](crate::Error::ScopeEnded). The caller retires the scope
/// and registers a new generation, which gets a new sampler — so a generation
/// boundary can never carry the old tree's identity map or held CPU total into
/// the next browser, and there is no way to "reuse" a finished sampler by
/// mistake. [`is_finished`](Self::is_finished) reports the latch, and
/// [`finish`](Self::finish) applies the same clearing for a scope the caller
/// retires for any other reason.
///
/// The sampler carries the owner and generation it was built for purely so the
/// refusal names them; the latch is per-sampler, and generation keying is the
/// caller's, via [`ScopeRegistry`].
#[derive(Clone, Debug)]
pub struct Sampler {
    previous: Option<Reading>,
    /// The per-pid map and running totals from the last tree walk. Only this
    /// compact state is kept, never the whole previous [`ProcessTreeSample`],
    /// so a sweep does not clone every record it just produced.
    previous_sweep: Option<PreviousSweep>,
    accounting: Accounting,
    /// The identity a process tree must still match, when this sampler drives a
    /// walk. `None` for a sampler that only records cgroup readings.
    expected: Option<ExpectedRoot>,
    /// The owner this sampler's figures are charged to, for diagnostics.
    owner: Option<ScopeOwner>,
    /// The generation this sampler belongs to, for diagnostics.
    generation: Generation,
    /// Set once the scope has ended; every further sample is refused.
    finished: bool,
}

impl Default for Sampler {
    fn default() -> Self {
        Self {
            previous: None,
            previous_sweep: None,
            accounting: Accounting::default(),
            expected: None,
            owner: None,
            generation: Generation::FIRST,
            finished: false,
        }
    }
}

impl Sampler {
    /// A sampler for a scope whose figures are `accounting`.
    ///
    /// Use this for a cgroup-backed scope, which is sampled with
    /// [`record`](Self::record) alone and has no root identity to check.
    #[must_use]
    pub fn new(accounting: Accounting) -> Self {
        Self {
            accounting,
            ..Self::default()
        }
    }

    /// A sampler for the process-tree scope `{owner, generation}` rooted at
    /// `expected`.
    ///
    /// `expected` is the identity read at registration with
    /// [`ExpectedRoot::read`](crate::process_tree::ExpectedRoot::read), so the
    /// **first** sample is already checked: a pid the kernel recycled between
    /// the spawn and the first sweep reports
    /// [`RootGone`](TreeSample::RootGone) instead of being charged to this
    /// owner.
    #[must_use]
    pub fn for_scope(
        owner: ScopeOwner,
        generation: Generation,
        expected: ExpectedRoot,
        accounting: Accounting,
    ) -> Self {
        Self {
            accounting,
            expected: Some(expected),
            owner: Some(owner),
            generation,
            ..Self::default()
        }
    }

    /// A sampler for a registered [`ResourceScope`].
    ///
    /// The scope's own [`ScopeKind`] supplies the root pid and the
    /// registration-time `starttime`, so the two cannot drift apart at the call
    /// site. A cgroup scope has no walk to guard and yields a sampler that only
    /// accepts [`record`](Self::record); a process-tree scope (including a pool
    /// backed by one) checks that root's identity on every
    /// [`sample_scope`](Self::sample_scope).
    #[must_use]
    pub fn for_resource_scope(scope: &ResourceScope, accounting: Accounting) -> Self {
        // A cgroup scope has no walk, so `process_tree_root` is already `None`
        // for it and for a pool backed by a cgroup.
        let expected = scope.kind.process_tree_root().map(|pid| ExpectedRoot {
            pid,
            starttime: scope.kind.process_tree_root_starttime().unwrap_or(0),
        });
        Self {
            accounting,
            expected,
            owner: Some(scope.owner.clone()),
            generation: scope.generation,
            ..Self::default()
        }
    }

    /// The owner this sampler was built for, when it was named.
    #[must_use]
    pub fn owner(&self) -> Option<&ScopeOwner> {
        self.owner.as_ref()
    }

    /// The generation this sampler belongs to.
    #[must_use]
    pub const fn generation(&self) -> Generation {
        self.generation
    }

    /// Whether the scope has ended and every further sample will be refused.
    #[must_use]
    pub const fn is_finished(&self) -> bool {
        self.finished
    }

    /// The retained per-pid state of the last sweep, for a cost measurement.
    ///
    /// This is the state a sweep keeps *between* calls — the growth of its map
    /// against a fixed tree is what the per-sweep allocation test measures.
    #[must_use]
    pub fn previous_sweep(&self) -> Option<&PreviousSweep> {
        self.previous_sweep.as_ref()
    }

    /// Retire the scope: drop the retained sweep state and the held CPU reading,
    /// and refuse every further sample.
    ///
    /// Called automatically when a walk reports
    /// [`RootGone`](TreeSample::RootGone); a caller retiring a scope for any
    /// other reason (a job cancelled by the operator) calls it explicitly rather
    /// than dropping the sampler and hoping.
    pub fn finish(&mut self) {
        self.previous = None;
        self.previous_sweep = None;
        // The registered identity belongs to a process that is gone; keeping it
        // would invite a "just carry on" call that the latch refuses anyway.
        self.expected = None;
        self.finished = true;
    }

    /// Record one reading at monotonic `now_seconds`, returning the CPU cores
    /// for the interval since the previous reading.
    ///
    /// `None` until a second reading exists, and for a non-advancing clock. The
    /// scope's cumulative figure is held by the sampler (see
    /// [`cpu_seconds_total`](Self::cpu_seconds_total)), so a kernel counter reset
    /// is absorbed once and growth continues from the reset value.
    pub fn record(&mut self, now_seconds: f64, cpu_seconds_total: f64) -> Option<f32> {
        let cores = self.previous.as_ref().and_then(|previous| {
            cpu_cores(
                previous.counter,
                cpu_seconds_total,
                now_seconds - previous.at,
            )
        });
        let held = self
            .previous
            .map_or(cpu_seconds_total, |previous| previous.held);
        self.previous = Some(Reading {
            at: now_seconds,
            counter: cpu_seconds_total,
            held: monotonic_total(held, cpu_seconds_total),
        });
        cores
    }

    /// The monotonic cumulative CPU seconds last published.
    ///
    /// This is the scope's lifetime figure as this sampler has observed it: the
    /// largest reading seen, so it never goes backwards — including across a
    /// kernel counter reset, where the difference between the pre-reset value
    /// and the (unknown) work done across the reset is silently lost. Two CPU
    /// readings of the same process tree cannot resolve that work; a delegated
    /// cgroup can, which is why exact accounting is the preferred mode.
    #[must_use]
    pub fn cpu_seconds_total(&self) -> f64 {
        self.previous.map_or(0.0, |previous| previous.held)
    }

    /// A usage record for `{owner, generation, phase}` reflecting the last
    /// [`record`](Self::record) call.
    #[must_use]
    pub fn release(
        &self,
        owner: ScopeOwner,
        generation: Generation,
        phase: JobPhase,
        cpu_cores: Option<f32>,
        mem_bytes: Option<u64>,
        mem_peak_bytes: Option<u64>,
    ) -> ResourceUsage {
        ResourceUsage {
            owner,
            generation,
            phase,
            cpu_cores,
            cpu_seconds_total: self.cpu_seconds_total(),
            mem_bytes,
            mem_peak_bytes,
            // No GPU attribution exists in this slice (ADR-0099 decision 8's
            // multi-PID NVML query is a later task): unknown, never zero.
            gpu_util: None,
            vram_bytes: None,
            gpu_seconds_total: None,
            accounting: self.accounting,
        }
    }

    /// Sample the process tree rooted at `expected` through `fs` and return the
    /// walk.
    ///
    /// The returned sample's `exited_cpu_ticks` carries the CPU time of
    /// processes that exited since the previous walk — identified by
    /// `(pid, starttime)`, so a reused pid is a new process and a live process
    /// whose counter dropped is not booked as exited — so a per-type breakdown
    /// stays truthful while the cumulative figure stays monotonic.
    ///
    /// The **root's** identity is checked on every sweep, the first included:
    /// `expected` is the registration-time identity (see
    /// [`ExpectedRoot`](crate::process_tree::ExpectedRoot)) and a sweep that
    /// finds a different process behind the pid returns
    /// [`TreeSample::RootGone`]. A `RootGone` **finishes** the sampler: it drops
    /// the retained per-pid state and the held CPU total and refuses every
    /// further sample with [`Error::ScopeEnded`], so the impostor's figures can
    /// never be folded into the old owner's — see the type-level one-sampler-per
    /// `{owner, generation}` contract on [`Sampler`].
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::ScopeEnded`] once the scope has ended, and
    /// [`crate::error::Error::Io`] or [`crate::error::Error::Parse`] when the
    /// root process's own `/proc` entry cannot be read.
    pub fn sample_tree<F: FsRoot + ?Sized>(
        &mut self,
        fs: &F,
        expected: ExpectedRoot,
    ) -> Result<TreeSample> {
        if self.finished {
            return Err(self.scope_ended("the root of this scope was already reported gone"));
        }
        // Defence in depth: a sampler built with a registered identity
        // (`for_scope` / `for_resource_scope`) refuses to be pointed at a
        // different process than the one its generation was opened for.
        if let Some(registered) = self.expected {
            if registered != expected {
                return Err(self.scope_ended(
                    "the identity handed to sample_tree is not the one this \
                     sampler's scope was registered with",
                ));
            }
        }
        let sample = sample_process_tree(
            fs,
            expected.pid,
            Some(&expected),
            self.previous_sweep.as_ref(),
        )?;
        match &sample {
            // Keep only the compact per-pid state, never a clone of the sample.
            TreeSample::Sampled(reading) => {
                self.previous_sweep = Some(PreviousSweep::from_sample(reading));
            }
            TreeSample::RootGone => self.finish(),
        }
        Ok(sample)
    }

    /// Sample the process tree of a registered process-tree [`ResourceScope`].
    ///
    /// The scope's root pid and registration-time `starttime` are the identity
    /// the walk is checked against, so a sample can only ever be attributed to
    /// the process the scope was registered for.
    ///
    /// # Errors
    ///
    /// [`Error::ScopeEnded`] when this sampler has finished (including a scope
    /// whose root was already reported gone), and the errors of
    /// [`sample_tree`](Self::sample_tree) otherwise.
    pub fn sample_scope<F: FsRoot + ?Sized>(
        &mut self,
        fs: &F,
        scope: &ResourceScope,
    ) -> Result<TreeSample> {
        let expected = match scope.kind.process_tree_root() {
            Some(pid) => ExpectedRoot {
                pid,
                starttime: scope.kind.process_tree_root_starttime().unwrap_or(0),
            },
            None => return Err(Error::parse(
                scope.owner.as_str(),
                "scope is cgroup-backed: sample it with record() and release(), not a tree walk",
            )),
        };
        self.owner = Some(scope.owner.clone());
        self.generation = scope.generation;
        self.sample_tree(fs, expected)
    }

    /// The refusal a finished sampler returns.
    fn scope_ended(&self, detail: &str) -> Error {
        Error::ScopeEnded {
            owner: self
                .owner
                .as_ref()
                .map_or_else(|| "unknown".to_owned(), |owner| owner.as_str().to_owned()),
            generation: self.generation.get(),
            detail: detail.to_owned(),
        }
    }

    /// CPU seconds for a process-tree walk.
    #[must_use]
    pub fn tree_cpu_seconds(sample: &ProcessTreeSample) -> f64 {
        ticks_to_seconds(sample.cpu_ticks)
    }

    /// CPU seconds for a cgroup reading.
    #[must_use]
    pub fn cgroup_cpu_seconds(usage: &CgroupUsage) -> f64 {
        usage.cpu_seconds_total()
    }
}

/// Build the exact-accounting record for a cgroup reading.
///
/// `memory.peak_bytes` is carried straight from `memory.peak`, which this crate
/// only ever reads — so the field is the cgroup's lifetime peak, not a peak of
/// our samples.
#[must_use]
pub fn usage_from_cgroup(
    owner: ScopeOwner,
    generation: Generation,
    phase: JobPhase,
    cpu_cores: Option<f32>,
    usage: &CgroupUsage,
) -> ResourceUsage {
    let mut record = ResourceUsage::unknown_gpu(owner, generation, phase, Accounting::Exact);
    record.cpu_cores = cpu_cores;
    record.cpu_seconds_total = usage.cpu_seconds_total();
    record.mem_bytes = usage.memory_current_bytes;
    record.mem_peak_bytes = usage.memory_peak_bytes;
    record
}

/// The per-type CPU split of a walk, in seconds.
///
/// This is the diagnostics view ADR-0099's measured bug needs: it shows the GPU
/// process carrying the bulk of the CPU, so a regression to "one process" is
/// visible without a second data structure. The result is a fixed array indexed
/// by [`ChromiumProcessType::index`], so no map is allocated per sweep.
#[must_use]
pub fn type_cpu_seconds(by_type: &TypeAccountingArray) -> TypeSecondsArray {
    let mut seconds: TypeSecondsArray = [0.0; TYPE_COUNT];
    for (index, entry) in by_type.iter().enumerate() {
        if let Some(slot) = seconds.get_mut(index) {
            *slot = ticks_to_seconds(entry.cpu_ticks);
        }
    }
    seconds
}

/// Attribute a sampler's reading to `{owner, generation}`, dropping it when the
/// scope is stale.
///
/// Returns `None` when `registry` no longer holds that generation — the drop is
/// the correct outcome for a late sample (ADR-0099 decision 8), never a charge
/// to whatever now owns the name.
#[must_use]
pub fn attribute(
    registry: &ScopeRegistry,
    owner: &ScopeOwner,
    generation: Generation,
) -> Option<Generation> {
    registry.accepts(owner, generation).then_some(generation)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_cores_needs_a_positive_interval() {
        assert_eq!(cpu_cores(0.0, 1.0, 0.0), None);
        assert_eq!(cpu_cores(0.0, 1.0, -1.0), None);
        assert_eq!(cpu_cores(0.0, 1.0, f64::NAN), None);
    }

    #[test]
    fn cpu_cores_reports_cores_not_a_unit_fraction() {
        // Two CPU-seconds burned in one wall-clock second is TWO cores, not a
        // saturating 1.0 fraction.
        assert_eq!(cpu_cores(0.0, 2.0, 1.0), Some(2.0_f32));
        // Half a core is still 0.5.
        assert_eq!(cpu_cores(0.0, 1.0, 2.0), Some(0.5_f32));
    }

    #[test]
    fn cpu_cores_saturates_at_the_documented_cap_instead_of_panicking() {
        assert_eq!(cpu_cores(0.0, 65.0, 1.0), Some(CPU_CORES_CAP));
        // A finite but absurd ratio, and an infinite one, both saturate.
        assert_eq!(cpu_cores(0.0, 1.0e300, 1.0e-300), Some(CPU_CORES_CAP));
        assert_eq!(cpu_cores(0.0, f64::INFINITY, 1.0), Some(CPU_CORES_CAP));
        assert!(cpu_cores(0.0, 1.0e9, 1.0).is_some_and(|cores| cores <= CPU_CORES_CAP));
    }

    #[test]
    fn cpu_cores_reports_zero_on_a_counter_reset() {
        // The scope reports LESS CPU than the previous reading: a reset, not
        // negative work.
        assert_eq!(cpu_cores(100.0, 50.0, 1.0), Some(0.0_f32));
    }

    #[test]
    fn monotonic_total_holds_the_previous_value_on_a_reset() {
        assert!((monotonic_total(100.0, 50.0) - 100.0).abs() < 1e-9);
        assert!((monotonic_total(100.0, 150.0) - 150.0).abs() < 1e-9);
    }

    #[test]
    fn the_sampler_grows_again_after_a_reset() {
        let mut sampler = Sampler::new(Accounting::Estimated);
        sampler.record(0.0, 100.0);
        sampler.record(1.0, 50.0);
        assert!(
            (sampler.cpu_seconds_total() - 100.0).abs() < 1e-9,
            "the held total covers the reset interval"
        );
        sampler.record(2.0, 150.0);
        assert!(
            (sampler.cpu_seconds_total() - 150.0).abs() < 1e-9,
            "the total resumes growing from the reset value, never jumping ahead of it"
        );
        sampler.record(3.0, 175.0);
        assert!(
            (sampler.cpu_seconds_total() - 175.0).abs() < 1e-9,
            "and keeps growing"
        );
    }

    #[test]
    fn unknown_gpu_fields_stay_none() {
        let usage = ResourceUsage::unknown_gpu(
            ScopeOwner::new("vt-1"),
            Generation::new(0),
            JobPhase::new("browser_start"),
            Accounting::Estimated,
        );
        assert_eq!(usage.gpu_util, None);
        assert_eq!(usage.vram_bytes, None);
        assert_eq!(usage.gpu_seconds_total, None);
        assert!(!usage.has_gpu());
    }
}
