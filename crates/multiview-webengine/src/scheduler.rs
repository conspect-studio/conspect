//! Bounded per-view refresh schedule driving [`Service`] submissions.
//!
//! Pure state plus an explicit caller-supplied clock: no thread, no wall clock,
//! no I/O of its own. The controller calls [`Scheduler::tick`] (about 1 Hz) with a
//! monotonic `Duration` and a request factory. The scheduler stores only the
//! view's identity and refresh policy; capture documents and credentials are
//! resolved by the factory at the moment a job is due, never held here.
//!
//! This is the scheduling core only: it does not finish any HTTP feature.
use crate::service::{Request, Service, SubmitError, Submitted};
use std::{
    collections::{BTreeMap, BinaryHeap},
    time::Duration,
};

/// Shortest and longest accepted interval, in minutes.
pub const MIN_MINUTES: u32 = 1;
/// See [`MIN_MINUTES`].
pub const MAX_MINUTES: u32 = 1440;

/// How a view refreshes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refresh {
    /// Runs only on an explicit [`Scheduler::trigger`]; never at boot or on tick.
    OneShot,
    /// Runs every `minutes` (1..=1440) on a stable per-fill phase.
    Interval {
        /// Period in minutes.
        minutes: u32,
    },
}

/// Trusted schedule key: an already-authorized owner and fill.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FillKey {
    /// Owner object.
    pub owner: String,
    /// Fill object.
    pub fill: String,
}

/// Independent copy of one view's refresh definition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct View {
    /// Schedule key.
    pub key: FillKey,
    /// Refresh policy.
    pub refresh: Refresh,
}

/// Bounds.
#[derive(Clone, Copy, Debug)]
pub struct Config {
    /// Maximum registered schedules; at least 1.
    pub max_schedules: usize,
    /// Maximum factory calls/submissions per tick; at least 1.
    pub max_due_per_tick: usize,
}

/// Why an upsert was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScheduleError {
    /// Empty identity or interval outside 1..=1440 minutes.
    Invalid,
    /// Registered-schedule table is full.
    Full,
}

/// Why an explicit trigger did not submit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TriggerError {
    /// No such schedule.
    Unknown,
    /// The factory failed or returned a request for a different key.
    Factory,
    /// The service refused the request.
    Submit(SubmitError),
}

/// What one tick did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TickReport {
    /// Jobs newly queued.
    pub accepted: usize,
    /// Due views whose fill already had an active job.
    pub coalesced: usize,
    /// Due views the service refused (their next-due still advanced).
    pub refused: usize,
    /// Due views whose factory failed or mismatched the key (also advanced).
    pub factory_failed: usize,
    /// Views still due but deferred by the per-tick bound.
    pub deferred: usize,
    /// Earliest future due instant, if any. Always strictly after `now` when
    /// nothing was deferred; when views were deferred it is `now + 1s`
    /// (saturating) so a 1 Hz caller never busy-spins. Never earlier than `now`.
    pub next_due: Option<Duration>,
}

struct Entry {
    refresh: Refresh,
    next_due: Option<Duration>,
}

/// Registered per-view schedules.
pub struct Scheduler {
    config: Config,
    entries: BTreeMap<FillKey, Entry>,
}

fn period(minutes: u32) -> u128 {
    u128::from(minutes) * 60_000_000_000
}

/// Stable FNV-1a phase in `0..period` for a key.
fn phase(key: &FillKey, period_ns: u128) -> u128 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in key
        .owner
        .bytes()
        .chain(std::iter::once(0))
        .chain(key.fill.bytes())
    {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    u128::from(h) % period_ns
}

/// Smallest instant strictly after `now` congruent to the key's phase, or
/// `None` when that instant is not representable as a `Duration` (the schedule
/// is then retired rather than re-fired every tick).
fn next_after(key: &FillKey, minutes: u32, now: Duration) -> Option<Duration> {
    let p = period(minutes);
    let ph = phase(key, p);
    let n = now.as_nanos();
    let k = if n < ph { 0 } else { (n - ph) / p + 1 };
    let t = ph + k * p;
    let secs = u64::try_from(t / 1_000_000_000).ok()?;
    let nanos = u32::try_from(t % 1_000_000_000).ok()?;
    Some(Duration::new(secs, nanos))
}

impl Scheduler {
    /// Empty scheduler (bounds are clamped to at least 1).
    #[must_use]
    pub fn new(config: Config) -> Self {
        Self {
            config: Config {
                max_schedules: config.max_schedules.max(1),
                max_due_per_tick: config.max_due_per_tick.max(1),
            },
            entries: BTreeMap::new(),
        }
    }

    /// Register or replace a view definition (copied). An unchanged policy keeps
    /// its next-due; a changed one restarts strictly after `now`. `OneShot` never
    /// becomes due on its own, including at registration.
    /// # Errors
    /// `Invalid` or `Full`.
    pub fn upsert(&mut self, now: Duration, view: &View) -> Result<(), ScheduleError> {
        let valid = !view.key.owner.is_empty()
            && !view.key.fill.is_empty()
            && match view.refresh {
                Refresh::OneShot => true,
                Refresh::Interval { minutes } => (MIN_MINUTES..=MAX_MINUTES).contains(&minutes),
            };
        if !valid {
            return Err(ScheduleError::Invalid);
        }
        match self.entries.get(&view.key) {
            Some(e) if e.refresh == view.refresh => return Ok(()),
            None if self.entries.len() >= self.config.max_schedules => {
                return Err(ScheduleError::Full);
            }
            Some(_) | None => {}
        }
        let next_due = match view.refresh {
            Refresh::OneShot => None,
            Refresh::Interval { minutes } => next_after(&view.key, minutes, now),
        };
        self.entries.insert(
            view.key.clone(),
            Entry {
                refresh: view.refresh,
                next_due,
            },
        );
        Ok(())
    }

    /// Drop a schedule; true if it existed.
    pub fn remove(&mut self, key: &FillKey) -> bool {
        self.entries.remove(key).is_some()
    }

    /// Independent copy of the stored definition.
    #[must_use]
    pub fn definition(&self, key: &FillKey) -> Option<View> {
        self.entries.get(key).map(|e| View {
            key: key.clone(),
            refresh: e.refresh,
        })
    }

    /// Registered schedule count.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True when nothing is registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Next due instant of one schedule (`None` for one-shot or unknown).
    #[must_use]
    pub fn next_due(&self, key: &FillKey) -> Option<Duration> {
        self.entries.get(key).and_then(|e| e.next_due)
    }

    /// Earliest due instant across all interval schedules.
    #[must_use]
    pub fn earliest_due(&self) -> Option<Duration> {
        self.entries.values().filter_map(|e| e.next_due).min()
    }

    /// Explicitly run one view now through the factory (works for `OneShot`);
    /// an interval schedule's cadence is unchanged.
    /// # Errors
    /// `Unknown`, `Factory` or `Submit`.
    pub fn trigger<E>(
        &self,
        service: &Service,
        key: &FillKey,
        factory: impl FnOnce(&FillKey) -> Result<Request, E>,
    ) -> Result<Submitted, TriggerError> {
        if !self.entries.contains_key(key) {
            return Err(TriggerError::Unknown);
        }
        submit_one(service, key, factory)
    }

    /// Submit every due interval view (earliest first, at most
    /// `max_due_per_tick`). Each fired view moves to its next phase slot strictly
    /// after `now`, so missed intervals run once and refusals never hot-retry.
    pub fn tick<E>(
        &mut self,
        now: Duration,
        service: &Service,
        mut factory: impl FnMut(&FillKey) -> Result<Request, E>,
    ) -> TickReport {
        // Bounded O(n) selection of the `max_due_per_tick` earliest due views:
        // a max-heap capped at the bound, so no allocation scales with `n`
        // beyond that bound and only selected keys are cloned.
        let cap = self.config.max_due_per_tick.min(self.entries.len());
        let mut heap: BinaryHeap<(Duration, &FillKey)> = BinaryHeap::with_capacity(cap + 1);
        let mut total_due = 0usize;
        for (k, e) in &self.entries {
            let Some(d) = e.next_due.filter(|d| *d <= now) else {
                continue;
            };
            total_due += 1;
            if heap.len() < cap {
                heap.push((d, k));
            } else if heap.peek().is_some_and(|top| (d, k) < *top) {
                heap.pop();
                heap.push((d, k));
            }
        }
        let due: Vec<(Duration, FillKey)> = heap
            .into_sorted_vec()
            .into_iter()
            .map(|(d, k)| (d, k.clone()))
            .collect();
        let mut report = TickReport::default();
        let take = due.len();
        report.deferred = total_due - take;
        for (_, key) in due.into_iter().take(take) {
            match submit_one(service, &key, &mut factory) {
                Ok(Submitted::Accepted(_)) => report.accepted += 1,
                Ok(Submitted::Coalesced(_)) => report.coalesced += 1,
                Err(TriggerError::Submit(_)) => report.refused += 1,
                Err(_) => report.factory_failed += 1,
            }
            if let Some(e) = self.entries.get_mut(&key) {
                if let Refresh::Interval { minutes } = e.refresh {
                    e.next_due = next_after(&key, minutes, now);
                }
            }
        }
        report.next_due = if report.deferred > 0 {
            Some(now.saturating_add(Duration::from_secs(1)))
        } else {
            self.earliest_due().map(|d| d.max(now))
        };
        report
    }
}

fn submit_one<E>(
    service: &Service,
    key: &FillKey,
    factory: impl FnOnce(&FillKey) -> Result<Request, E>,
) -> Result<Submitted, TriggerError> {
    let request = factory(key).map_err(|_| TriggerError::Factory)?;
    if request.owner != key.owner || request.fill != key.fill {
        return Err(TriggerError::Factory);
    }
    service.submit(request).map_err(TriggerError::Submit)
}
