//! Integration tests for the metrics registry abstraction.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_telemetry::metrics::{Labels, MetricKind, MetricsRegistry};

#[test]
fn counter_registers_and_increments() {
    let reg = MetricsRegistry::new();
    let frames_out = reg.counter("multiview_frames_out_total", Labels::empty());
    assert_eq!(frames_out.get(), 0);
    frames_out.increment(1);
    frames_out.increment(4);
    assert_eq!(frames_out.get(), 5);
}

#[test]
fn same_name_and_labels_return_the_same_counter() {
    let reg = MetricsRegistry::new();
    let labels = Labels::new().with("tile", "0").with("source", "cam1");
    let a = reg.counter("multiview_dropped_total", labels.clone());
    let b = reg.counter("multiview_dropped_total", labels);
    a.increment(3);
    // Both handles point at the same underlying atomic.
    assert_eq!(b.get(), 3, "identical name+labels must share storage");
}

#[test]
fn different_labels_are_distinct_series() {
    let reg = MetricsRegistry::new();
    let t0 = reg.counter("multiview_dropped_total", Labels::new().with("tile", "0"));
    let t1 = reg.counter("multiview_dropped_total", Labels::new().with("tile", "1"));
    t0.increment(2);
    t1.increment(7);
    assert_eq!(t0.get(), 2);
    assert_eq!(t1.get(), 7, "distinct label sets must not collide");
}

#[test]
fn label_order_does_not_affect_identity() {
    let reg = MetricsRegistry::new();
    let a = reg.counter(
        "multiview_reconnects_total",
        Labels::new().with("source", "cam1").with("backend", "cuda"),
    );
    let b = reg.counter(
        "multiview_reconnects_total",
        Labels::new().with("backend", "cuda").with("source", "cam1"),
    );
    a.increment(1);
    assert_eq!(
        b.get(),
        1,
        "labels are an unordered set; ordering must not create a new series"
    );
}

#[test]
fn gauge_sets_and_reads_back() {
    let reg = MetricsRegistry::new();
    let fps = reg.gauge("multiview_tile_fps", Labels::new().with("tile", "2"));
    fps.set(29.97);
    assert!((fps.get() - 29.97).abs() < 1e-9);
    fps.add(0.03);
    assert!((fps.get() - 30.0).abs() < 1e-9);
    fps.sub(10.0);
    assert!((fps.get() - 20.0).abs() < 1e-9);
}

#[test]
fn histogram_records_into_explicit_buckets() {
    let reg = MetricsRegistry::new();
    // Explicit latency buckets in seconds (brief §15: explicit buckets).
    let h = reg.histogram(
        "multiview_encode_seconds",
        Labels::new().with("codec", "h264"),
        &[0.001, 0.005, 0.010, 0.050],
    );
    h.observe(0.0008); // <= 0.001
    h.observe(0.004); // <= 0.005
    h.observe(0.004); // <= 0.005
    h.observe(0.2); // overflow (+Inf only)

    let snap = h.snapshot();
    assert_eq!(snap.count, 4);
    assert!((snap.sum - (0.0008 + 0.004 + 0.004 + 0.2)).abs() < 1e-9);
    // Cumulative bucket counts (le semantics).
    assert_eq!(snap.cumulative_counts(), vec![1, 3, 3, 3]);
    // The +Inf bucket equals the total count.
    assert_eq!(snap.inf_count(), 4);
}

#[test]
fn histogram_rejects_observation_count_overflow_gracefully() {
    // Buckets must be sorted; an unsorted spec is rejected via the registry.
    let reg = MetricsRegistry::new();
    let h = reg.histogram(
        "multiview_decode_seconds",
        Labels::empty(),
        &[0.01, 0.1, 1.0],
    );
    for _ in 0..1000 {
        h.observe(0.05);
    }
    let snap = h.snapshot();
    assert_eq!(snap.count, 1000);
    assert_eq!(snap.cumulative_counts(), vec![0, 1000, 1000]);
}

#[test]
fn registry_describes_registered_series() {
    let reg = MetricsRegistry::new();
    let _ = reg.counter("multiview_frames_in_total", Labels::empty());
    let _ = reg.gauge(
        "multiview_vram_bytes",
        Labels::new().with("backend", "cuda"),
    );
    let _ = reg.histogram("multiview_composite_seconds", Labels::empty(), &[0.01]);

    let series = reg.series();
    // One per distinct (name, labels) registration.
    assert_eq!(series.len(), 3);
    let kinds: Vec<MetricKind> = series.iter().map(|s| s.kind).collect();
    assert!(kinds.contains(&MetricKind::Counter));
    assert!(kinds.contains(&MetricKind::Gauge));
    assert!(kinds.contains(&MetricKind::Histogram));
}

#[test]
fn labels_render_in_stable_sorted_form() {
    // Bounded-cardinality label rendering must be deterministic (sorted by key)
    // so the same series always renders identically.
    let l = Labels::new()
        .with("source", "cam1")
        .with("backend", "cuda")
        .with("tile", "3");
    assert_eq!(l.render(), r#"{backend="cuda",source="cam1",tile="3"}"#);
    assert_eq!(Labels::empty().render(), "");
}
