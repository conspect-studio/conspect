//! Live overlay add / edit / remove on the running engine (ADR-W022).
//!
//! The command drain applies `UpsertOverlay`/`RemoveOverlay` at the frame
//! boundary as pure data mutation: it upserts/removes the document in the
//! working-config mirror and publishes the full set (with a bumped generation)
//! into the lock-free [`OverlayApplySlot`] the bake consumer re-derives from.
//! These tests prove the drain slice end-to-end on the software engine:
//!
//! * an upsert publishes the new set at the frame boundary (generation bump);
//! * an edit under the same id replaces the entry — never duplicates it;
//! * a remove drops the entry; removing an unknown id is a no-op publish-wise;
//! * a drain without the seam holds the command (warned, never a panic);
//! * a continuous upsert/remove churn flood cannot stall the clock or skip a
//!   frame (invariants #1 + #10) — the soak gate.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::sync::Arc;

use multiview_cli::control::{command_drain, command_drain_with_live_overlays};
use multiview_cli::live_overlays::overlay_apply_slot;
use multiview_cli::run::SoftwareEngine;
use multiview_compositor::blend::LinearRgba;
use multiview_compositor::pipeline::{CanvasColor, Nv12Image};
use multiview_config::MultiviewConfig;
use multiview_control::{command_bus, Command, EngineStateSnapshot, OperationId};
use multiview_engine::{CompositorDrive, EnginePublisher};
use multiview_events::Event;
use multiview_framestore::TileStore;

/// A 2-cell config with two declared bars sources bound to the cells.
fn two_cell_config() -> MultiviewConfig {
    let doc = r##"schema_version = 1
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
columns = ["1fr", "1fr"]
rows = ["1fr"]
areas = ["a b"]
[[sources]]
id = "in_a"
kind = "bars"
[[sources]]
id = "in_b"
kind = "bars"
[[cells]]
id = "cell_a"
area = "a"
[cells.source]
input_id = "in_a"
[[cells]]
id = "cell_b"
area = "b"
[cells.source]
input_id = "in_b"
[[outputs]]
kind = "hls"
path = "/tmp/live-overlay-apply.m3u8"
codec = "mpeg2video"
segment_ms = 1000
"##;
    MultiviewConfig::load_from_toml(doc).expect("parse two-cell config")
}

/// A real `CompositorDrive` over the config's solved layout (the drain's
/// frame-boundary target).
fn test_drive(config: &MultiviewConfig) -> CompositorDrive<Nv12Image> {
    let layout = config.solve_layout().expect("solve layout");
    let canvas_color = CanvasColor::default();
    let nosignal = Nv12Image::solid(
        config.canvas.width,
        config.canvas.height,
        16,
        128,
        128,
        canvas_color.output_tag(),
    )
    .expect("nosignal card");
    let mut stores = std::collections::HashMap::new();
    for source in &config.sources {
        stores.insert(
            source.id.clone(),
            Arc::new(TileStore::<Nv12Image>::with_defaults(source.id.clone())),
        );
    }
    CompositorDrive::new(
        Arc::new(layout),
        stores,
        nosignal,
        canvas_color,
        LinearRgba::opaque(0.0, 0.0, 0.0),
    )
    .expect("build drive")
}

/// Parse a validated `multiview_config::Overlay` from JSON (the same shape the
/// typed overlays route stores after ADR-W015 validation).
fn overlay_doc(json: serde_json::Value) -> multiview_config::Overlay {
    serde_json::from_value(json).expect("valid overlay document")
}

/// An analog wall-clock overlay document centred at (`x`, `y`).
fn analog_clock(id: &str, x: i64, y: i64) -> multiview_config::Overlay {
    overlay_doc(serde_json::json!({
        "id": id, "kind": "clock", "target": "canvas",
        "face": "analog", "x": x, "y": y, "radius": 16
    }))
}

/// Drain every queued `job.progress` event off `sub`, returning the phases in
/// arrival order (the drain's apply/held observability — ADR-W022 / MINOR-4).
fn job_phases(sub: &mut multiview_engine::EventSubscription<Event>) -> Vec<String> {
    let mut phases = Vec::new();
    while let Ok(envelope) = sub.try_recv() {
        if let Event::JobProgress(progress) = envelope.event.as_ref() {
            phases.push(progress.phase.clone());
        }
    }
    phases
}

#[tokio::test]
async fn upsert_overlay_publishes_the_new_set_at_the_frame_boundary() {
    let config = two_cell_config();
    let publisher = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(16));
    let (sender, command_rx) = command_bus(16);
    let slot = overlay_apply_slot(config.overlays.clone());
    let mut drain = command_drain_with_live_overlays(
        command_rx,
        config.clone(),
        Arc::clone(&publisher),
        Arc::clone(&slot),
    );
    let mut drive = test_drive(&config);
    let mut sub = publisher.subscribe();

    assert_eq!(
        slot.load().generation(),
        0,
        "the seeded slot is generation 0"
    );
    assert!(
        slot.load().overlays().is_empty(),
        "boot config has no overlays"
    );

    sender
        .try_submit(Command::UpsertOverlay {
            op: OperationId::new(),
            overlay: Box::new(analog_clock("clk", 200, 120)),
        })
        .expect("submit upsert");
    drain(&mut drive);

    let set = slot.load();
    assert_eq!(set.generation(), 1, "the apply bumps the generation");
    assert_eq!(set.overlays().len(), 1);
    assert_eq!(set.overlays()[0].id, "clk");
    assert_eq!(set.overlays()[0].kind, "clock");
    assert_eq!(
        job_phases(&mut sub),
        vec!["apply_overlay".to_owned()],
        "the applied upsert is observable as a job.progress outcome"
    );

    // EDIT: an upsert under the same id REPLACES the entry (never duplicates).
    sender
        .try_submit(Command::UpsertOverlay {
            op: OperationId::new(),
            overlay: Box::new(analog_clock("clk", 64, 64)),
        })
        .expect("submit edit");
    drain(&mut drive);

    let set = slot.load();
    assert_eq!(set.generation(), 2);
    assert_eq!(set.overlays().len(), 1, "an edit must not duplicate the id");
    assert_eq!(
        set.overlays()[0].params.get("x"),
        Some(&serde_json::json!(64)),
        "the published set carries the NEW params"
    );
}

/// Task #130 (live re-blend): a pure equal-`z` REORDER must re-sequence the
/// engine's live overlay working set at the frame boundary so the composite
/// draw order actually changes — WITHOUT a restart. Two equal-z overlays blend
/// in working-set order (the bake consumer's `analog_clocks_from_config` keeps
/// input order, fed to the compositor's STABLE `sort_by_key(|l| l.z)`), so the
/// published `OverlaySet.overlays()` order IS the equal-z draw-order tie-break.
/// Before the fix, `UpsertOverlay` edited the mirror IN PLACE by id, so
/// re-submitting upserts was a no-op for order; `ReorderOverlays` re-sequences
/// it. This is Class-1 (a generation bump, one lock-free publish, no restart).
#[tokio::test]
async fn reorder_overlays_reblends_the_live_draw_order_at_the_frame_boundary() {
    let config = two_cell_config();
    let publisher = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(16));
    let (sender, command_rx) = command_bus(16);
    let slot = overlay_apply_slot(config.overlays.clone());
    let mut drain = command_drain_with_live_overlays(
        command_rx,
        config.clone(),
        Arc::clone(&publisher),
        Arc::clone(&slot),
    );
    let mut drive = test_drive(&config);
    let mut sub = publisher.subscribe();

    // Two analog clocks, both default z=0 → equal-z, so working-set order is the
    // ONLY thing deciding which blends on top. Upsert clk_a then clk_b.
    sender
        .try_submit(Command::UpsertOverlay {
            op: OperationId::new(),
            overlay: Box::new(analog_clock("clk_a", 10, 10)),
        })
        .expect("submit clk_a");
    sender
        .try_submit(Command::UpsertOverlay {
            op: OperationId::new(),
            overlay: Box::new(analog_clock("clk_b", 50, 50)),
        })
        .expect("submit clk_b");
    drain(&mut drive);

    let set = slot.load();
    assert_eq!(
        set.generation(),
        2,
        "two upserts bumped the generation twice"
    );
    let ids_before: Vec<&str> = set.overlays().iter().map(|o| o.id.as_str()).collect();
    assert_eq!(
        ids_before,
        vec!["clk_a", "clk_b"],
        "declaration/insertion order before the reorder"
    );
    let _ = job_phases(&mut sub); // drain the two apply_overlay phases

    // REORDER: ask for [clk_b, clk_a] — same ids, same documents, only the
    // draw order swapped. Against the pre-fix engine this is an unhandled
    // command (skipped), so the order would NOT change.
    sender
        .try_submit(Command::ReorderOverlays {
            op: OperationId::new(),
            order: vec!["clk_b".to_owned(), "clk_a".to_owned()],
        })
        .expect("submit reorder");
    drain(&mut drive);

    let set = slot.load();
    let ids_after: Vec<&str> = set.overlays().iter().map(|o| o.id.as_str()).collect();
    assert_eq!(
        ids_after,
        vec!["clk_b", "clk_a"],
        "the reorder re-sequenced the live working set (the top overlay flipped) \
         — the equal-z draw order actually changed live"
    );
    assert_eq!(
        set.generation(),
        3,
        "the reorder is a frame-boundary apply (a generation bump), not a restart"
    );
    // The set still carries BOTH documents, unchanged — a pure permutation, not
    // an add/remove/edit.
    assert_eq!(set.overlays().len(), 2, "no overlay added or removed");
    assert!(
        set.overlays().iter().any(|o| o.id == "clk_a")
            && set.overlays().iter().any(|o| o.id == "clk_b"),
        "both overlays survive the reorder"
    );
    assert_eq!(
        job_phases(&mut sub),
        vec!["apply_overlay".to_owned()],
        "the reorder is observable as a job.progress outcome (Class-1, no restart)"
    );
}

/// A reorder request whose id sequence already matches the working set is a
/// no-op: no generation bump, no spurious re-derivation (idempotent — a shed
/// retry re-submitting the same order cannot thrash the bake consumer).
#[tokio::test]
async fn an_already_ordered_reorder_is_a_noop() {
    let config = two_cell_config();
    let publisher = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(16));
    let (sender, command_rx) = command_bus(16);
    let slot = overlay_apply_slot(config.overlays.clone());
    let mut drain = command_drain_with_live_overlays(
        command_rx,
        config.clone(),
        Arc::clone(&publisher),
        Arc::clone(&slot),
    );
    let mut drive = test_drive(&config);
    let mut sub = publisher.subscribe();

    sender
        .try_submit(Command::UpsertOverlay {
            op: OperationId::new(),
            overlay: Box::new(analog_clock("clk_a", 10, 10)),
        })
        .expect("submit clk_a");
    sender
        .try_submit(Command::UpsertOverlay {
            op: OperationId::new(),
            overlay: Box::new(analog_clock("clk_b", 50, 50)),
        })
        .expect("submit clk_b");
    drain(&mut drive);
    let gen_before = slot.load().generation();
    let _ = job_phases(&mut sub);

    // Same order as the working set → nothing to do.
    sender
        .try_submit(Command::ReorderOverlays {
            op: OperationId::new(),
            order: vec!["clk_a".to_owned(), "clk_b".to_owned()],
        })
        .expect("submit no-op reorder");
    drain(&mut drive);

    assert_eq!(
        slot.load().generation(),
        gen_before,
        "an already-ordered reorder publishes nothing (no generation bump)"
    );
    assert!(
        job_phases(&mut sub).is_empty(),
        "a no-op reorder emits no apply outcome"
    );
}

/// Task #130 (GENUINE render-level proof): a pure equal-`z` overlay reorder
/// changes the **rendered** draw order — not merely the published slot.
///
/// This drives the EXACT bake-consumer derivation `StreamBaker::refresh_overlays`
/// uses (`crate::pipeline::analog_clocks_from_config(set.overlays(), w, h)` →
/// `OverlayBaker::set_analog_clocks` → `OverlayBaker::draw_list`) and asserts the
/// order of the rendered clock-face **`Ring` primitives** (back-to-front draw
/// order) flips with the reorder. Two analog clocks at distinct centres make each
/// face identifiable by its `Ring` centre. Pre-fix (`UpsertOverlay` edits the
/// mirror in place by id) the published set order — and therefore the rendered
/// order — does NOT change; post-fix `ReorderOverlays` re-sequences it. Requires
/// `ffmpeg` (the bake module home) + `overlay` (the renderer) — the real
/// deployment posture; run with `--features ffmpeg,overlay`.
#[cfg(all(feature = "ffmpeg", feature = "overlay"))]
#[tokio::test]
async fn reorder_overlays_changes_the_rendered_draw_order() {
    use multiview_cli::overlays::OverlayBaker;
    use multiview_cli::pipeline::analog_clocks_from_config;
    use multiview_compositor::overlay::OverlayPrimitive;
    use multiview_core::time::MediaTime;
    use multiview_overlay::geometry::PixelRect;

    // The canvas of `two_cell_config()` (64×64) as f32 literals — avoids a
    // u32→f32 `as` cast in this integration test (where `as_conversions` is not
    // relaxed). The faces are placed at whole-pixel centres, so `Ring.cx/cy` are
    // exact integer-valued f32 (the config placement rounds to i32 then widens).
    const CANVAS_W: u32 = 64;
    const CANVAS_H: u32 = 64;
    const CANVAS_WF: f32 = 64.0;
    const CANVAS_HF: f32 = 64.0;

    // The rendered draw order of the analog faces, identified by `Ring` centre,
    // derived through the REAL bake path from the live slot's published set.
    fn rendered_ring_centres(
        slot: &multiview_cli::live_overlays::OverlayApplySlot,
    ) -> Vec<(f32, f32)> {
        let set = slot.load();
        // EXACTLY what `refresh_overlays` does: derive the ordered face specs
        // from the published set, in slot order.
        let specs = analog_clocks_from_config(set.overlays(), CANVAS_W, CANVAS_H);
        let tile = multiview_cli::overlays::TileSpec::new(
            "t",
            "T",
            PixelRect {
                x: 0.0,
                y: 0.0,
                width: CANVAS_WF,
                height: CANVAS_HF,
            },
        );
        let mut baker = OverlayBaker::new(
            vec![tile],
            multiview_cli::wallclock::WallClockSource::system(),
        )
        .expect("build baker");
        baker.set_analog_clocks(specs);
        let list = baker
            .draw_list(
                MediaTime::ZERO,
                &std::collections::HashMap::new(),
                &std::collections::HashMap::new(),
                &std::collections::HashMap::new(),
            )
            .expect("draw list");
        // The clock-face bezel `Ring`s, in back-to-front draw order, by centre.
        list.primitives
            .iter()
            .filter_map(|p| match p {
                OverlayPrimitive::Ring { cx, cy, .. } => Some((*cx, *cy)),
                _ => None,
            })
            .collect()
    }

    let config = two_cell_config();
    let publisher = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(16));
    let (sender, command_rx) = command_bus(16);
    let slot = overlay_apply_slot(config.overlays.clone());
    let mut drain = command_drain_with_live_overlays(
        command_rx,
        config.clone(),
        Arc::clone(&publisher),
        Arc::clone(&slot),
    );
    let mut drive = test_drive(&config);

    // Two analog clocks at distinct centres, both equal z=0. (Radius 16 fits the
    // 64×64 canvas so the placement is not clamped.)
    sender
        .try_submit(Command::UpsertOverlay {
            op: OperationId::new(),
            overlay: Box::new(analog_clock("clk_a", 10, 10)),
        })
        .expect("submit clk_a");
    sender
        .try_submit(Command::UpsertOverlay {
            op: OperationId::new(),
            overlay: Box::new(analog_clock("clk_b", 50, 50)),
        })
        .expect("submit clk_b");
    drain(&mut drive);

    let before = rendered_ring_centres(&slot);
    assert_eq!(
        before,
        vec![(10.0, 10.0), (50.0, 50.0)],
        "the RENDERED face draw order before the reorder follows insertion order"
    );

    // The reorder must flip the RENDERED order, not just the slot Vec.
    sender
        .try_submit(Command::ReorderOverlays {
            op: OperationId::new(),
            order: vec!["clk_b".to_owned(), "clk_a".to_owned()],
        })
        .expect("submit reorder");
    drain(&mut drive);

    let after = rendered_ring_centres(&slot);
    assert_eq!(
        after,
        vec![(50.0, 50.0), (10.0, 10.0)],
        "the reorder changed the RENDERED clock-face draw order (clk_b now draws \
         first/under, clk_a last/on-top) — proven through the real bake path, not \
         just the published slot"
    );
}

#[tokio::test]
async fn remove_overlay_drops_the_entry_from_the_published_set() {
    let config = two_cell_config();
    let publisher = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(16));
    let (sender, command_rx) = command_bus(16);
    let slot = overlay_apply_slot(config.overlays.clone());
    let mut drain = command_drain_with_live_overlays(
        command_rx,
        config.clone(),
        Arc::clone(&publisher),
        Arc::clone(&slot),
    );
    let mut drive = test_drive(&config);

    sender
        .try_submit(Command::UpsertOverlay {
            op: OperationId::new(),
            overlay: Box::new(analog_clock("clk", 200, 120)),
        })
        .expect("submit upsert");
    drain(&mut drive);
    assert_eq!(slot.load().overlays().len(), 1);

    sender
        .try_submit(Command::RemoveOverlay {
            op: OperationId::new(),
            id: "clk".to_owned(),
        })
        .expect("submit remove");
    drain(&mut drive);

    let set = slot.load();
    assert_eq!(set.generation(), 2, "the remove publishes a new generation");
    assert!(set.overlays().is_empty(), "the entry is gone from the set");

    // Removing an UNKNOWN id publishes no new set — but it is still surfaced
    // (warned + a held outcome event), never a silent drop.
    let mut sub = publisher.subscribe();
    sender
        .try_submit(Command::RemoveOverlay {
            op: OperationId::new(),
            id: "ghost".to_owned(),
        })
        .expect("submit ghost remove");
    drain(&mut drive);
    assert_eq!(
        slot.load().generation(),
        2,
        "an unknown-id remove publishes nothing (no spurious re-derive)"
    );
    assert_eq!(
        job_phases(&mut sub),
        vec!["apply_overlay_held".to_owned()],
        "an unknown-id remove is observable as a held job.progress outcome"
    );
}

#[tokio::test]
async fn overlay_commands_without_a_seam_are_held_without_panicking() {
    // The plain drain (software path without the seam wired) holds the
    // commands — warned, surfaced as held job.progress outcomes, never a
    // panic — and the clock-side apply still runs.
    let config = two_cell_config();
    let publisher = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(16));
    let (sender, command_rx) = command_bus(16);
    let mut drain = command_drain(command_rx, config.clone(), Arc::clone(&publisher));
    let mut drive = test_drive(&config);
    let mut sub = publisher.subscribe();

    sender
        .try_submit(Command::UpsertOverlay {
            op: OperationId::new(),
            overlay: Box::new(analog_clock("clk", 200, 120)),
        })
        .expect("submit upsert");
    sender
        .try_submit(Command::RemoveOverlay {
            op: OperationId::new(),
            id: "clk".to_owned(),
        })
        .expect("submit remove");
    drain(&mut drive);
    assert_eq!(
        job_phases(&mut sub),
        vec![
            "apply_overlay_held".to_owned(),
            "apply_overlay_held".to_owned()
        ],
        "BOTH seam-less holds are observable as held job.progress outcomes"
    );
}

/// SOAK (invariants #1 + #10): a continuous full-speed flood of live overlay
/// churn — upserts at varying placements and removes — must never stall the
/// output clock or skip a frame: exactly N frames for N ticks, never faltered.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_overlay_churn_flood_never_falters_the_output_clock() {
    use multiview_engine::{CooperativePacer, ManualTimeSource};
    use std::sync::atomic::{AtomicBool, Ordering};

    const TICKS: u64 = 240;

    let cfg = two_cell_config();
    let mut engine = SoftwareEngine::build(&cfg).expect("build software engine");
    let (tx, rx) = command_bus(64);
    let slot = overlay_apply_slot(cfg.overlays.clone());

    // Background flooder: live overlay churn at full speed for the whole run.
    // A full bus just sheds the submit (inv #10).
    let stop_flood = Arc::new(AtomicBool::new(false));
    let flooder = {
        let stop_flood = Arc::clone(&stop_flood);
        std::thread::spawn(move || {
            let mut n: i64 = 0;
            while !stop_flood.load(Ordering::Relaxed) {
                let id = format!("churn{}", n % 4);
                let _ = tx.try_submit(Command::UpsertOverlay {
                    op: OperationId::new(),
                    overlay: Box::new(
                        serde_json::from_value(serde_json::json!({
                            "id": id, "kind": "clock", "target": "canvas",
                            "face": "analog", "x": 8 + (n % 48), "y": 8 + (n % 48),
                            "radius": 8
                        }))
                        .expect("valid churn overlay"),
                    ),
                });
                let _ = tx.try_submit(Command::RemoveOverlay {
                    op: OperationId::new(),
                    id,
                });
                n = n.wrapping_add(1);
            }
        })
    };

    let publisher = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(8));
    let drain = command_drain_with_live_overlays(
        rx,
        cfg.clone(),
        Arc::clone(&publisher),
        Arc::clone(&slot),
    );

    let time = Arc::new(ManualTimeSource::new());
    // Bounded so a drive-loop regression that parks/spins the output clock under
    // this jumped-clock bounded run (the ADR-T018 cadence-hold `skip_to` vs
    // `max_ticks` desync that hung CI ~37 min) fails FAST here instead of hanging.
    // The honest sleep-free run finishes in well under a second.
    let report = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        engine.run_for_with_control(Arc::clone(&time), CooperativePacer, TICKS, drain),
    )
    .await
    .expect("output clock stalled under live-overlay churn (bounded run never completed)")
    .expect("software run under live-overlay churn");

    stop_flood.store(true, Ordering::Relaxed);
    let _ = flooder.join();

    assert_eq!(
        report.frames, TICKS,
        "live-overlay churn must still produce exactly N frames for N ticks"
    );
    assert!(
        !report.faltered,
        "live-overlay churn must never falter the output clock (invariants #1 + #10)"
    );
    assert!(
        slot.load().generation() > 0,
        "the churn actually applied (the slot advanced past the boot set)"
    );
}
