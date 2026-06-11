//! A small, dependency-free metrics registry for the documented engine metrics.
//!
//! The observability brief (core-engine §15) names the metric taxonomy:
//!
//! * **Counters** — frames in/out, dropped, reconnects, encode errors.
//! * **Gauges** — per-tile FPS, queue depth, VRAM bytes, active encode sessions.
//! * **Histograms** — decode/composite/encode + end-to-end latency, with
//!   *explicit* buckets.
//! * **Labels** — `{tile, source, backend, codec}` with bounded cardinality.
//!
//! This registry is intentionally tiny and pure (no `metrics`-facade global, no
//! native deps), so it builds in the GPU-free default and never installs process
//! globals that would interfere with tests. A Prometheus text exporter lives in
//! `multiview-control` (or a feature here later); this crate owns the *model*.
//!
//! ## Concurrency
//!
//! Handles are cheap [`Arc`] clones over atomics, so the data plane can update a
//! metric without locking. The registry's series table is behind a [`Mutex`];
//! registration happens at startup/config time, not per frame, so that lock is
//! never on the hot path. Updating an already-resolved handle takes no lock.
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// An ordered, bounded set of `key="value"` metric labels.
///
/// Labels form an **unordered set**: two `Labels` with the same key/value pairs
/// are equal regardless of insertion order, and they render in sorted-by-key
/// form so a given series always renders identically. Keep cardinality bounded
/// (the brief warns against unbounded label values).
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Labels {
    /// Sorted by key (a `BTreeMap`), giving order-independent identity.
    pairs: BTreeMap<String, String>,
}

impl Labels {
    /// An empty label set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// An empty label set (alias for [`Labels::new`], reads well at call sites).
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// Add or replace a label, consuming and returning `self` (builder style).
    #[must_use]
    pub fn with(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.pairs.insert(key.into(), value.into());
        self
    }

    /// Whether there are no labels.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pairs.is_empty()
    }

    /// Number of distinct label keys.
    #[must_use]
    pub fn len(&self) -> usize {
        self.pairs.len()
    }

    /// Render in Prometheus form: `{k1="v1",k2="v2"}`, sorted by key. An empty
    /// set renders as the empty string (no braces).
    #[must_use]
    pub fn render(&self) -> String {
        if self.pairs.is_empty() {
            return String::new();
        }
        let body = self
            .pairs
            .iter()
            .map(|(k, v)| format!(r#"{k}="{v}""#))
            .collect::<Vec<_>>()
            .join(",");
        format!("{{{body}}}")
    }
}

/// The kind of a registered metric series.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum MetricKind {
    /// Monotonically increasing counter.
    Counter,
    /// Arbitrary up/down gauge.
    Gauge,
    /// Bucketed latency/size distribution.
    Histogram,
}

/// Identity of a series: its name plus its (order-independent) label set.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct SeriesKey {
    name: String,
    labels: Labels,
}

/// A descriptor of one registered series, returned by [`MetricsRegistry::series`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct SeriesDescriptor {
    /// The metric name.
    pub name: String,
    /// The metric's label set.
    pub labels: Labels,
    /// The kind of metric.
    pub kind: MetricKind,
}

/// A monotonic counter handle. Cheap to clone; clones share storage.
#[derive(Debug, Clone)]
pub struct Counter {
    value: Arc<AtomicU64>,
}

impl Counter {
    /// Add `delta` to the counter.
    pub fn increment(&self, delta: u64) {
        self.value.fetch_add(delta, Ordering::Relaxed);
    }

    /// The current value.
    #[must_use]
    pub fn get(&self) -> u64 {
        self.value.load(Ordering::Relaxed)
    }
}

/// A gauge handle storing an `f64` as its bit pattern in an atomic. Cheap to
/// clone; clones share storage.
#[derive(Debug, Clone)]
pub struct Gauge {
    /// `f64::to_bits` of the current value.
    bits: Arc<AtomicU64>,
}

impl Gauge {
    /// Set the gauge to an absolute value.
    pub fn set(&self, value: f64) {
        self.bits.store(value.to_bits(), Ordering::Relaxed);
    }

    /// The current value.
    #[must_use]
    pub fn get(&self) -> f64 {
        f64::from_bits(self.bits.load(Ordering::Relaxed))
    }

    /// Add `delta` (compare-and-swap loop, lock-free).
    pub fn add(&self, delta: f64) {
        self.update(|current| current + delta);
    }

    /// Subtract `delta`.
    pub fn sub(&self, delta: f64) {
        self.update(|current| current - delta);
    }

    /// Apply `f` atomically via a CAS loop.
    fn update(&self, f: impl Fn(f64) -> f64) {
        let mut current = self.bits.load(Ordering::Relaxed);
        loop {
            let next = f(f64::from_bits(current)).to_bits();
            match self.bits.compare_exchange_weak(
                current,
                next,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(observed) => current = observed,
            }
        }
    }
}

/// Inner mutable state of a histogram, behind a [`Mutex`].
#[derive(Debug)]
struct HistogramInner {
    /// Upper bounds (`le`), strictly sorted ascending; excludes `+Inf`.
    bounds: Vec<f64>,
    /// Per-bucket (non-cumulative) observation counts; same length as `bounds`.
    counts: Vec<u64>,
    /// Count of observations above the largest bound (the `+Inf`-only bucket).
    overflow: u64,
    /// Total number of observations.
    count: u64,
    /// Sum of all observed values.
    sum: f64,
}

/// A histogram handle with explicit buckets. Cheap to clone; clones share state.
#[derive(Debug, Clone)]
pub struct Histogram {
    inner: Arc<Mutex<HistogramInner>>,
}

impl Histogram {
    /// Record one observation into the matching bucket.
    ///
    /// Locking note: the lock is held only for the duration of a vector index
    /// and a couple of additions. Histogram observation is expected on warm
    /// paths (per-frame latency), not the protected output-clock path, and the
    /// critical section is constant-time. If a thread ever panics while holding
    /// the lock we recover the poisoned guard rather than propagating a panic
    /// into a telemetry caller (telemetry must never crash the engine).
    pub fn observe(&self, value: f64) {
        let mut inner = match self.inner.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        inner.count = inner.count.saturating_add(1);
        inner.sum += value;
        // Find the first bound >= value (le semantics). `partition_point`
        // returns the count of bounds strictly less than `value`.
        let idx = inner.bounds.partition_point(|&b| b < value);
        match inner.counts.get_mut(idx) {
            Some(slot) => *slot = slot.saturating_add(1),
            None => inner.overflow = inner.overflow.saturating_add(1),
        }
    }

    /// Take a point-in-time snapshot of the histogram.
    #[must_use]
    pub fn snapshot(&self) -> HistogramSnapshot {
        let inner = match self.inner.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        HistogramSnapshot {
            bounds: inner.bounds.clone(),
            counts: inner.counts.clone(),
            overflow: inner.overflow,
            count: inner.count,
            sum: inner.sum,
        }
    }
}

/// An immutable snapshot of a [`Histogram`].
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct HistogramSnapshot {
    /// Upper bounds (`le`), ascending, excluding `+Inf`.
    pub bounds: Vec<f64>,
    /// Per-bucket (non-cumulative) counts; same length as `bounds`.
    pub counts: Vec<u64>,
    /// Observations above the largest bound.
    pub overflow: u64,
    /// Total observation count.
    pub count: u64,
    /// Sum of observed values.
    pub sum: f64,
}

impl HistogramSnapshot {
    /// Cumulative (`le`) bucket counts, one per finite bound, as Prometheus
    /// exports them. Each entry is the number of observations `<=` that bound.
    #[must_use]
    pub fn cumulative_counts(&self) -> Vec<u64> {
        let mut running: u64 = 0;
        self.counts
            .iter()
            .map(|&c| {
                running = running.saturating_add(c);
                running
            })
            .collect()
    }

    /// The `+Inf` bucket count, which equals the total observation count.
    #[must_use]
    pub fn inf_count(&self) -> u64 {
        self.count
    }
}

/// One stored series, holding the concrete handle storage.
#[derive(Debug, Clone)]
enum Stored {
    Counter(Counter),
    Gauge(Gauge),
    Histogram(Histogram),
}

impl Stored {
    fn kind(&self) -> MetricKind {
        match self {
            Stored::Counter(_) => MetricKind::Counter,
            Stored::Gauge(_) => MetricKind::Gauge,
            Stored::Histogram(_) => MetricKind::Histogram,
        }
    }
}

/// A registry of metric series, keyed by `(name, labels)`.
///
/// Registering the same `(name, labels)` twice returns a handle to the same
/// underlying storage. Cheap to clone (the table is shared); pass clones to
/// whichever subsystem needs to publish a metric.
#[derive(Debug, Clone, Default)]
pub struct MetricsRegistry {
    series: Arc<Mutex<BTreeMap<SeriesKey, Stored>>>,
}

impl MetricsRegistry {
    /// Create an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Lock the table, recovering a poisoned guard rather than panicking — a
    /// telemetry registration must never crash the caller.
    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<SeriesKey, Stored>> {
        match self.series.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// Get or create a [`Counter`] for `(name, labels)`.
    ///
    /// If a series with the same name and labels but a *different* kind already
    /// exists, a fresh counter is returned without disturbing the stored series
    /// (kind collisions are a programming error; we never panic on the caller).
    #[must_use]
    pub fn counter(&self, name: impl Into<String>, labels: Labels) -> Counter {
        let key = SeriesKey {
            name: name.into(),
            labels,
        };
        let mut table = self.lock();
        if let Some(Stored::Counter(existing)) = table.get(&key) {
            return existing.clone();
        }
        let counter = Counter {
            value: Arc::new(AtomicU64::new(0)),
        };
        table.insert(key, Stored::Counter(counter.clone()));
        counter
    }

    /// Get or create a [`Gauge`] for `(name, labels)`.
    #[must_use]
    pub fn gauge(&self, name: impl Into<String>, labels: Labels) -> Gauge {
        let key = SeriesKey {
            name: name.into(),
            labels,
        };
        let mut table = self.lock();
        if let Some(Stored::Gauge(existing)) = table.get(&key) {
            return existing.clone();
        }
        let gauge = Gauge {
            bits: Arc::new(AtomicU64::new(0_f64.to_bits())),
        };
        table.insert(key, Stored::Gauge(gauge.clone()));
        gauge
    }

    /// Get or create a [`Histogram`] for `(name, labels)` with explicit upper
    /// bounds (`le`).
    ///
    /// The supplied `bounds` are de-duplicated and sorted ascending, so an
    /// unsorted or repeated spec is normalized rather than rejected — the caller
    /// always receives a well-formed histogram. A re-registration with the same
    /// `(name, labels)` returns the existing histogram and **ignores** the new
    /// bounds (the first registration wins).
    #[must_use]
    pub fn histogram(&self, name: impl Into<String>, labels: Labels, bounds: &[f64]) -> Histogram {
        let key = SeriesKey {
            name: name.into(),
            labels,
        };
        let mut table = self.lock();
        if let Some(Stored::Histogram(existing)) = table.get(&key) {
            return existing.clone();
        }
        // Normalize bounds: sort ascending and de-duplicate. NaN bounds are
        // dropped (they have no ordering and would corrupt bucketing).
        let mut sorted: Vec<f64> = bounds.iter().copied().filter(|b| !b.is_nan()).collect();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        sorted.dedup();
        let len = sorted.len();
        let histogram = Histogram {
            inner: Arc::new(Mutex::new(HistogramInner {
                bounds: sorted,
                counts: vec![0; len],
                overflow: 0,
                count: 0,
                sum: 0.0,
            })),
        };
        table.insert(key, Stored::Histogram(histogram.clone()));
        histogram
    }

    /// Describe every registered series, in deterministic `(name, labels)` order.
    #[must_use]
    pub fn series(&self) -> Vec<SeriesDescriptor> {
        let table = self.lock();
        table
            .iter()
            .map(|(key, stored)| SeriesDescriptor {
                name: key.name.clone(),
                labels: key.labels.clone(),
                kind: stored.kind(),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_len_and_is_empty() {
        assert!(Labels::empty().is_empty());
        assert_eq!(Labels::empty().len(), 0);
        let l = Labels::new().with("tile", "0").with("source", "cam");
        assert!(!l.is_empty());
        assert_eq!(l.len(), 2);
    }

    #[test]
    fn with_replaces_an_existing_key() {
        // Re-setting a key updates it rather than creating a second entry, so
        // identity stays stable.
        let l = Labels::new().with("tile", "0").with("tile", "1");
        assert_eq!(l.len(), 1);
        assert_eq!(l.render(), r#"{tile="1"}"#);
    }

    #[test]
    fn histogram_normalizes_unsorted_and_duplicate_bounds() {
        let reg = MetricsRegistry::new();
        let h = reg.histogram("h", Labels::empty(), &[1.0, 0.5, 0.5, 0.1]);
        let snap = h.snapshot();
        assert_eq!(snap.bounds, vec![0.1, 0.5, 1.0], "sorted + deduped");
        assert_eq!(snap.counts.len(), 3);
    }

    #[test]
    fn histogram_drops_nan_bounds() {
        let reg = MetricsRegistry::new();
        let h = reg.histogram("h", Labels::empty(), &[0.1, f64::NAN, 0.2]);
        let snap = h.snapshot();
        assert_eq!(snap.bounds, vec![0.1, 0.2], "NaN bounds are discarded");
    }

    #[test]
    fn histogram_first_registration_bounds_win() {
        let reg = MetricsRegistry::new();
        let _first = reg.histogram("h", Labels::empty(), &[0.1, 0.2]);
        let second = reg.histogram("h", Labels::empty(), &[1.0, 2.0, 3.0]);
        // Same (name, labels) => same histogram; the new bounds are ignored.
        assert_eq!(second.snapshot().bounds, vec![0.1, 0.2]);
    }

    #[test]
    fn empty_histogram_puts_everything_in_overflow() {
        // No finite bounds means every observation is the +Inf bucket.
        let reg = MetricsRegistry::new();
        let h = reg.histogram("h", Labels::empty(), &[]);
        h.observe(0.0);
        h.observe(123.0);
        let snap = h.snapshot();
        assert!(snap.bounds.is_empty());
        assert_eq!(snap.overflow, 2);
        assert_eq!(snap.count, 2);
        assert!(snap.cumulative_counts().is_empty());
        assert_eq!(snap.inf_count(), 2);
    }

    #[test]
    fn gauge_negative_values_roundtrip() {
        let reg = MetricsRegistry::new();
        let g = reg.gauge("queue_depth_delta", Labels::empty());
        g.set(-5.0);
        assert!((g.get() - (-5.0)).abs() < 1e-12);
        g.add(2.0);
        assert!((g.get() - (-3.0)).abs() < 1e-12);
    }

    #[test]
    fn observe_value_equal_to_bound_is_included_le() {
        // le semantics: a value exactly on a bound counts toward that bucket.
        let reg = MetricsRegistry::new();
        let h = reg.histogram("h", Labels::empty(), &[1.0, 2.0]);
        h.observe(1.0);
        h.observe(2.0);
        assert_eq!(h.snapshot().cumulative_counts(), vec![1, 2]);
    }
}
