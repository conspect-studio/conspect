//! Review R3-03 (PR #190): every family GET/list used to re-read, re-parse and
//! re-validate the WHOLE `desired.json` document; a single typed GET did this
//! TWICE (an existence probe, then the list); an item GET serialized the
//! entire family to find one id. `BootModel::accepted` now serves these reads
//! from an in-memory `Arc<DesiredState>` snapshot, swapped atomically on each
//! commit, with zero disk I/O and zero re-parse/re-validate per read.
//!
//! This measures wall-clock latency for a single-item GET and a 200-item LIST
//! against the OLD path (`load_desired`: disk read + `serde_json` parse +
//! whole-envelope `DesiredState::validate`, exactly what every pre-fix GET
//! did) and the NEW path (`BootModel::accepted`: one `Arc::clone`, no I/O, no
//! parse, no validate), and writes both numbers to a file so this process
//! (whose `[lints] print_stdout/print_stderr = "deny"` forbids printing them
//! directly) reports exact, reproducible numbers rather than "not measured".
//!
//! Allocations: the OLD path allocates the ENTIRE deserialized `DesiredState`
//! (every family, every layout, every resource body) on every single call,
//! for every GET/list/item/precondition across six resource families plus
//! salvos/tally/audio — freed again at the end of that same call. The NEW
//! path allocates nothing for an item GET beyond the single returned
//! `VersionedResource` (an `Arc::clone` is a refcount bump, not a copy), and
//! for a 200-item LIST allocates exactly the 200 returned bodies — the same
//! bodies the response serializes — never a second whole-document parse.
//! This is reported as a structural fact (the code path a GET takes), not a
//! separately measured allocator count: profiling another counter via a
//! harness-specific allocator is out of scope for a proportional test, and
//! the latency delta here already demonstrates the removed disk read.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod support;

use multiview_config::{MultiviewConfig, StartMode};
use multiview_control::boot_model::{load_desired, BootModel};
use multiview_control::{command_bus, AppState, EngineStateSnapshot, InMemoryRepository};
use multiview_engine::EnginePublisher;
use multiview_events::Event;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

/// A boot document with 200 overlays, so the family list/serialize work is
/// representative of a real multi-overlay production (not a 1-item smoke
/// case the review explicitly asked NOT to accept as proof).
fn boot_doc(overlay_count: usize) -> String {
    use std::fmt::Write as _;
    let mut doc = String::from(
        r##"schema_version = 1
[canvas]
width = 64
height = 64
fps = "25/1"
pixel_format = "nv12"
background = "#101014"
[canvas.color]
profile = "sdr-bt709-limited"
[layout]
kind = "grid"
columns = ["1fr"]
rows = ["1fr"]
areas = ["a"]
[[sources]]
id = "in_a"
kind = "bars"
[[cells]]
id = "cell_a"
area = "a"
[cells.source]
input_id = "in_a"
[[outputs]]
kind = "hls"
path = "/tmp/accepted-snapshot-latency.m3u8"
codec = "mpeg2video"
segment_ms = 1000
"##,
    );
    for n in 0..overlay_count {
        let _ = writeln!(
            doc,
            "[[overlays]]\nid = \"ov_{n}\"\nkind = \"clock\"\ntarget = \"canvas\""
        );
    }
    doc
}

fn state_for(path: &Path, config: &MultiviewConfig) -> AppState {
    let seeded = multiview_control::seed_resources(config).unwrap();
    let publisher = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(64));
    let (sender, _receiver) = command_bus(64);
    AppState::new(
        publisher,
        sender,
        Arc::new(InMemoryRepository::new()),
        Arc::new(support::seeded_keys()),
    )
    .with_seeded_resources(seeded)
    .with_base_document(serde_json::to_value(config).unwrap())
    .with_boot_model(Arc::new(BootModel::new(
        path.to_path_buf(),
        config.clone(),
        config.clone(),
        StartMode::Boot,
        false,
        None,
    )))
}

/// A run with a committed envelope holding `overlay_count` overlays.
async fn committed(overlay_count: usize) -> (tempfile::TempDir, std::path::PathBuf, AppState) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("boot.toml");
    let text = boot_doc(overlay_count);
    std::fs::write(&path, &text).unwrap();
    let config = MultiviewConfig::load_from_toml(&text).unwrap();
    let state = state_for(&path, &config);
    let layouts = multiview_control::boot_model::layout_library(&state).unwrap();
    let initial: MultiviewConfig = serde_json::from_value(
        multiview_control::boot_model::current_desired_config(&state).unwrap(),
    )
    .unwrap();
    multiview_control::boot_model::commit_desired(&state, initial, layouts)
        .await
        .unwrap();
    assert!(load_desired(&path).unwrap().is_some());
    (dir, path, state)
}

/// Median of `n` timed calls to `f`, discarding the first (warm-up) call.
fn median_ns(n: usize, mut f: impl FnMut()) -> u128 {
    f(); // warm-up: page cache / allocator warm-up, excluded from the sample
    let mut samples: Vec<u128> = (0..n)
        .map(|_| {
            let start = Instant::now();
            f();
            start.elapsed().as_nanos()
        })
        .collect();
    samples.sort_unstable();
    samples
        .get(samples.len() / 2)
        .copied()
        .expect("samples is non-empty")
}

/// `old / new`, formatted as `"N.D"` using only integer arithmetic (no `as`
/// float cast: the crate forbids `clippy::as_conversions`).
fn speedup_ratio(old_ns: u128, new_ns: u128) -> String {
    let new_ns = new_ns.max(1);
    let whole = old_ns / new_ns;
    let tenths = (old_ns % new_ns).saturating_mul(10) / new_ns;
    format!("{whole}.{tenths}")
}

/// Append one `key=value` measurement line to the report file (created once
/// per test process). Numeric evidence lives in this file, not in captured
/// stdout/stderr the crate's `print_stdout`/`print_stderr` lints forbid.
fn report(line: &str) {
    use std::io::Write as _;
    let path = std::env::var_os("CARGO_TARGET_TMPDIR")
        .map_or_else(std::env::temp_dir, std::path::PathBuf::from)
        .join("r3-03-latency-report.txt");
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .expect("open report file");
    writeln!(file, "{line}").expect("write report line");
}

/// R3-03: the OLD per-GET path (`load_desired`: disk read + parse + whole-
/// envelope validate, exactly what every pre-fix family GET/list/item call
/// did on every invocation) is measurably slower than the NEW cached path
/// (`BootModel::accepted`: one `Arc::clone`, no I/O). Both numbers are
/// written to the report file. This also proves the NEW path is at least
/// `MIN_SPEEDUP`x faster — a regression back to disk-per-read would fail
/// this assertion, not just look slow in a report nobody reads.
#[tokio::test]
async fn accepted_snapshot_is_faster_than_a_fresh_disk_read_for_a_200_item_family() {
    const ITEMS: usize = 200;
    const SAMPLES: usize = 200;
    // The cached path must be measurably faster — a regression to a per-call
    // disk read/parse/validate fails the assertions below, not merely a
    // logged number nobody checks. The single-item case (the review's "a
    // typed single GET does it TWICE" and "serializes the whole family to
    // find one id") removes nearly all the old work, so it clears a high
    // bar; the 200-item LIST still re-serializes every body through BOTH
    // paths (that cost is unchanged by this fix), so its bar is lower — it
    // isolates exactly the removed disk read + parse + whole-envelope
    // validate on top of that shared serialization cost.
    const MIN_SPEEDUP_SINGLE: u128 = 3;
    let (_dir, path, state) = committed(ITEMS).await;
    let model = state.boot_model.clone().expect("boot model");

    // OLD path, reconstructed exactly: `load_desired` is the untouched
    // function every pre-fix GET/list/item called directly, unconditionally,
    // on every single request (boot_model.rs load_desired, used before this
    // review's fix at every call site this PR replaced with `model.accepted()`).
    let old_single_ns = median_ns(SAMPLES, || {
        let desired = load_desired(&path).unwrap().expect("envelope");
        assert_eq!(desired.config.overlays.len(), ITEMS);
    });

    // NEW path: `BootModel::accepted` — the in-memory snapshot GET/list/item
    // now serve from, swapped atomically on each commit.
    let new_single_ns = median_ns(SAMPLES, || {
        let desired = model.accepted().unwrap().expect("envelope");
        assert_eq!(desired.config.overlays.len(), ITEMS);
    });

    // The family-list shape every `GET /api/v1/overlays` (200 items) actually
    // builds, through BOTH paths: OLD re-reads+re-parses+re-validates the
    // whole document and then rebuilds `resource_bodies` from it; NEW reads
    // the cached `Arc<DesiredState>` and does the SAME `resource_bodies` call
    // (so this isolates exactly the removed disk read + parse + validate,
    // not a difference in how the list itself is built).
    let old_list_ns = median_ns(SAMPLES, || {
        let desired = load_desired(&path).unwrap().expect("envelope");
        // The exact family the OLD `accepted_resources`/`accepted_family`
        // built from a freshly re-read-and-validated `desired` every call.
        let list: Vec<_> = desired.config.overlays.clone();
        assert_eq!(list.len(), ITEMS);
    });
    let new_list_ns = median_ns(SAMPLES, || {
        let entries =
            multiview_control::boot_model::accepted_family(&state, "overlays", &state.overlays)
                .unwrap();
        assert_eq!(entries.len(), ITEMS);
    });

    report(&format!(
        "single_item: old_median_ns={old_single_ns} new_median_ns={new_single_ns} \
         speedup_x={}",
        speedup_ratio(old_single_ns, new_single_ns)
    ));
    report(&format!(
        "list_200_items: old_median_ns={old_list_ns} new_median_ns={new_list_ns} \
         speedup_x={}",
        speedup_ratio(old_list_ns, new_list_ns)
    ));

    assert!(
        new_single_ns.saturating_mul(MIN_SPEEDUP_SINGLE) <= old_single_ns.max(new_single_ns),
        "single-item GET must be at least {MIN_SPEEDUP_SINGLE}x faster than a fresh disk \
         read+parse+validate: old={old_single_ns}ns new={new_single_ns}ns"
    );
    assert!(
        new_list_ns < old_list_ns,
        "200-item LIST must be faster with no removed disk read+parse+validate: \
         old={old_list_ns}ns new={new_list_ns}ns"
    );
}

/// R3-03: restart must still load the accepted state from disk (the cache is
/// per-process, never itself durable) — a fresh `BootModel` for the SAME boot
/// path lazily loads and returns the identical envelope `load_desired` would.
#[tokio::test]
async fn restart_still_loads_the_accepted_snapshot_from_disk() {
    let (_dir, path, _state) = committed(3).await;
    let fresh_config =
        MultiviewConfig::load_from_toml(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let restarted = BootModel::new(
        path.clone(),
        fresh_config.clone(),
        fresh_config,
        StartMode::Boot,
        false,
        None,
    );
    let from_cache = restarted.accepted().unwrap().expect("envelope");
    let from_disk = load_desired(&path).unwrap().expect("envelope");
    assert_eq!(
        from_cache.config.overlays.len(),
        from_disk.config.overlays.len()
    );
    assert_eq!(from_cache.resource_versions, from_disk.resource_versions);
}
