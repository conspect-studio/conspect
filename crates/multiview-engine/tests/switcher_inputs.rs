#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Numbered mixer inputs and the mixer patch (ADR-0110 §12.2/§12.8, #341).
use multiview_compositor::pipeline::Nv12Image;
use multiview_core::color::ColorInfo;
use multiview_core::layout::{Canvas, Cell, Layout};
use multiview_core::switcher::{FrameCount, SourceRef};
use multiview_core::time::MediaTime;
use multiview_engine::clock::Tick;
use multiview_engine::switcher::SwitcherState;
use multiview_engine::switcher_control::{
    PatchSet, ScenePair, SwitcherCommand, SwitcherControl, SwitcherTickDriver, SwitcherVerb,
    COMMAND_CAPACITY, MAX_COMMANDS_PER_TICK, REFUSE_NOT_AN_INPUT, REFUSE_NO_FRESH_FRAME,
    REFUSE_ON_AIR_PROTECTED, REFUSE_PATCH_DURING_AUTO, REFUSE_SAME_SOURCE, REFUSE_UNKNOWN_INPUT,
    TAKE_PRIME_DEADLINE_TICKS,
};
use multiview_engine::switcher_inputs::MixerInputs;
use multiview_framestore::{NoSignalPolicy, TileStore, TileThresholds};
use std::collections::HashMap;
use std::sync::Arc;

/// `a`, `b`, `c` are primed; `d` exists but never had a frame (cold).
fn stores() -> HashMap<String, Arc<TileStore<Nv12Image>>> {
    let thresholds = TileThresholds::from_millis(500, 2_000, 10_000).unwrap();
    ["a", "b", "c", "d"]
        .into_iter()
        .map(|id| {
            let store = Arc::new(TileStore::new(id, thresholds, NoSignalPolicy::Slate));
            if id != "d" {
                store.publish(
                    Nv12Image::solid(
                        64,
                        64,
                        128,
                        128,
                        128,
                        ColorInfo::default().resolve_defaults(64, 64),
                    )
                    .unwrap(),
                    MediaTime::ZERO,
                );
            }
            (id.to_owned(), store)
        })
        .collect()
}

fn tick(index: u64) -> Tick {
    Tick {
        index,
        pts: MediaTime::ZERO,
    }
}

fn scene(id: &str) -> Arc<Layout> {
    Arc::new(Layout {
        name: "test".into(),
        canvas: Canvas {
            width: 640,
            height: 360,
            fps_num: 25,
            fps_den: 1,
        },
        cells: vec![Cell {
            x: 0.0,
            y: 0.0,
            w: 1.0,
            h: 1.0,
            source: Some(id.to_owned()),
            ..Cell::default()
        }],
    })
}

fn command(op: impl std::fmt::Display, verb: SwitcherVerb) -> SwitcherCommand {
    SwitcherCommand {
        operation_id: op.to_string(),
        scene_version: 1,
        verb,
    }
}

fn patch(members: &[(u16, Option<&str>)], allow_on_air: bool) -> SwitcherVerb {
    SwitcherVerb::Patch(PatchSet {
        members: members
            .iter()
            .map(|(n, s)| (*n, s.map(SourceRef::new)))
            .collect(),
        allow_on_air,
    })
}

/// Inputs 1=a, 2=b, 3=c, 4 unpatched; PGM in:1, PVW in:3.
fn with_inputs() -> (SwitcherControl, SwitcherTickDriver) {
    let (handle, mut driver) = SwitcherControl::new(SwitcherState::new(
        SourceRef::input(1),
        SourceRef::input(3),
        vec![],
    ));
    driver.set_prime_stores(stores());
    driver.set_inputs(
        MixerInputs::new([
            (1, Some("Source 1".into()), Some(SourceRef::new("a"))),
            (2, None, Some(SourceRef::new("b"))),
            (3, None, Some(SourceRef::new("c"))),
            (4, None, None),
        ])
        .unwrap(),
    );
    handle
        .set_scenes(ScenePair {
            version: 1,
            program: scene("a"),
            preview: scene("c"),
        })
        .unwrap();
    (handle, driver)
}

fn run(driver: &mut SwitcherTickDriver, index: u64) {
    driver.drain_at_tick(tick(index));
    driver.commit_composed_tick(tick(index));
}

fn resolved(driver: &SwitcherTickDriver) -> (String, String) {
    let (p, v) = driver.resolved_selections();
    (p.as_str().to_owned(), v.as_str().to_owned())
}

fn table(handle: &SwitcherControl) -> Vec<(u16, Option<String>)> {
    handle
        .inputs()
        .unwrap()
        .inputs
        .iter()
        .map(|i| (i.number, i.source.as_ref().map(|s| s.as_str().to_owned())))
        .collect()
}

#[test]
fn without_a_table_selections_are_sources_and_input_fields_stay_empty() {
    let (handle, mut driver) = SwitcherControl::new(SwitcherState::new(
        SourceRef::new("a"),
        SourceRef::new("b"),
        vec![],
    ));
    driver.set_prime_stores(stores());
    handle
        .set_scenes(ScenePair {
            version: 1,
            program: scene("a"),
            preview: scene("b"),
        })
        .unwrap();
    run(&mut driver, 0);
    assert_eq!(driver.selections(), driver.resolved_selections());
    let snapshot = handle.snapshot().unwrap();
    assert_eq!(
        (
            snapshot.program_input,
            snapshot.preview_input,
            snapshot.inputs_version
        ),
        (None, None, 0)
    );
    assert!(handle.inputs().is_none());
    let mut outcomes = handle.outcomes();
    handle
        .try_send(command(1, patch(&[(1, Some("c"))], true)))
        .unwrap();
    run(&mut driver, 1);
    assert!(outcomes.try_recv().unwrap().event.error.is_some());
}

#[test]
fn select_input_then_cut_resolves_program_and_flip_flops_inputs() {
    let (handle, mut driver) = with_inputs();
    let mut outcomes = handle.outcomes();
    run(&mut driver, 0);
    let snapshot = handle.snapshot().unwrap();
    assert_eq!(snapshot.program.as_str(), "a");
    assert_eq!(
        (snapshot.program_input, snapshot.preview_input),
        (Some(1), Some(3))
    );
    handle
        .try_send(command(1, SwitcherVerb::SelectPreview(SourceRef::input(2))))
        .unwrap();
    handle.try_send(command(2, SwitcherVerb::Cut)).unwrap();
    run(&mut driver, 1);
    assert!(outcomes.try_recv().unwrap().event.error.is_none());
    assert!(outcomes.try_recv().unwrap().event.error.is_none());
    let snapshot = handle.snapshot().unwrap();
    assert_eq!(
        snapshot.program.as_str(),
        "b",
        "Program shows the source behind input 2"
    );
    assert_eq!(snapshot.preview.as_str(), "a");
    assert_eq!(
        (snapshot.program_input, snapshot.preview_input),
        (Some(2), Some(1))
    );
    assert_eq!(driver.selections().0, &SourceRef::input(2));
    // A raw source, an unpatched and an undeclared input are refused.
    for (op, target, error) in [
        (3, SourceRef::new("c"), REFUSE_NOT_AN_INPUT),
        (4, SourceRef::input(4), REFUSE_UNKNOWN_INPUT),
        (5, SourceRef::input(9), REFUSE_UNKNOWN_INPUT),
    ] {
        handle
            .try_send(command(op, SwitcherVerb::SelectProgram(target)))
            .unwrap();
        run(&mut driver, 1 + op);
        assert_eq!(
            outcomes.try_recv().unwrap().event.error.as_deref(),
            Some(error)
        );
    }
    assert_eq!(resolved(&driver), ("b".into(), "a".into()));
}

#[test]
fn a_three_member_patch_lands_on_one_tick_and_a_bad_member_changes_nothing() {
    let (handle, mut driver) = with_inputs();
    let mut outcomes = handle.outcomes();
    run(&mut driver, 0);
    let before = table(&handle);
    // One bad member (undeclared input 9): nothing changes.
    handle
        .try_send(command(
            "bad",
            patch(&[(4, Some("a")), (9, Some("b"))], false),
        ))
        .unwrap();
    // Unknown source: nothing changes.
    handle
        .try_send(command("ghost", patch(&[(4, Some("zzz"))], false)))
        .unwrap();
    // Duplicate destination: nothing changes.
    handle
        .try_send(command(
            "dup",
            patch(&[(4, Some("a")), (4, Some("b"))], false),
        ))
        .unwrap();
    // Unpatching the PVW input: refused.
    handle
        .try_send(command("unpatch", patch(&[(3, None)], false)))
        .unwrap();
    run(&mut driver, 1);
    for _ in 0..4 {
        assert!(outcomes.try_recv().unwrap().event.error.is_some());
    }
    assert_eq!(table(&handle), before);
    assert_eq!(handle.snapshot().unwrap().inputs_version, 0);

    // Inputs 2 and 4 are off-air; 3 is on PVW (protect only guards PGM).
    handle
        .try_send(command(
            "three",
            patch(&[(2, Some("c")), (3, Some("b")), (4, Some("a"))], false),
        ))
        .unwrap();
    run(&mut driver, 7);
    let outcome = outcomes.try_recv().unwrap().event;
    assert!(outcome.completed && outcome.error.is_none(), "{outcome:?}");
    let snapshot = handle.snapshot().unwrap();
    assert_eq!(outcome.tick, 7);
    assert_eq!(
        snapshot.tick, outcome.tick,
        "snapshot and outcome on one tick"
    );
    assert_eq!(snapshot.inputs_version, 1);
    assert_eq!(
        snapshot.preview.as_str(),
        "b",
        "PVW follows its input's new source"
    );
    let readback = handle.inputs().unwrap();
    assert_eq!((readback.tick, readback.version), (7, 1));
    assert_eq!(
        table(&handle),
        vec![
            (1, Some("a".into())),
            (2, Some("c".into())),
            (3, Some("b".into())),
            (4, Some("a".into())),
        ]
    );
    assert_eq!(readback.inputs[0].label.as_deref(), Some("Source 1"));
}

#[test]
fn repatching_the_program_input_needs_allow_and_a_fresh_source() {
    let (handle, mut driver) = with_inputs();
    let mut outcomes = handle.outcomes();
    run(&mut driver, 0);
    handle
        .try_send(command("protect", patch(&[(1, Some("b"))], false)))
        .unwrap();
    run(&mut driver, 1);
    assert_eq!(
        outcomes.try_recv().unwrap().event.error.as_deref(),
        Some(REFUSE_ON_AIR_PROTECTED)
    );
    assert_eq!(resolved(&driver).0, "a");

    handle
        .try_send(command("allow", patch(&[(1, Some("b"))], true)))
        .unwrap();
    driver.drain_at_tick(tick(2));
    assert_eq!(resolved(&driver).0, "b", "Program cuts on the patch tick");
    driver.commit_composed_tick(tick(2));
    let outcome = outcomes.try_recv().unwrap().event;
    assert!(outcome.completed);
    let snapshot = handle.snapshot().unwrap();
    assert_eq!((snapshot.tick, snapshot.program.as_str()), (2, "b"));
    assert_eq!(snapshot.program_input, Some(1));

    // Cold source: held (single slot), then refused at the deadline.
    handle
        .try_send(command(
            "cold",
            patch(&[(1, Some("d")), (4, Some("c"))], true),
        ))
        .unwrap();
    run(&mut driver, 3);
    assert!(outcomes.try_recv().is_err(), "held, no outcome yet");
    handle
        .try_send(command("queued", SwitcherVerb::Cut))
        .unwrap();
    run(&mut driver, 4);
    assert!(
        outcomes.try_recv().unwrap().event.error.is_some(),
        "one held take at a time"
    );
    for index in 5..(3 + TAKE_PRIME_DEADLINE_TICKS) {
        run(&mut driver, index);
        assert!(outcomes.try_recv().is_err());
    }
    run(&mut driver, 3 + TAKE_PRIME_DEADLINE_TICKS);
    let outcome = outcomes.try_recv().unwrap().event;
    assert_eq!(outcome.error.as_deref(), Some(REFUSE_NO_FRESH_FRAME));
    assert_eq!(outcome.tick, 3 + TAKE_PRIME_DEADLINE_TICKS);
    assert_eq!(table(&handle)[0], (1, Some("b".into())));
    assert_eq!(table(&handle)[3], (4, None), "no member of the set applied");
    assert_eq!(handle.snapshot().unwrap().inputs_version, 1);
}

#[test]
fn a_patch_on_the_program_or_preview_input_is_refused_during_auto() {
    let (handle, mut driver) = with_inputs();
    let mut outcomes = handle.outcomes();
    run(&mut driver, 0);
    handle
        .try_send(command(
            "auto",
            SwitcherVerb::Auto(FrameCount::try_from(25).unwrap()),
        ))
        .unwrap();
    run(&mut driver, 1);
    for (op, number) in [("pgm", 1), ("pvw", 3)] {
        handle
            .try_send(command(op, patch(&[(number, Some("b"))], true)))
            .unwrap();
    }
    handle
        .try_send(command("offair", patch(&[(4, Some("b"))], false)))
        .unwrap();
    run(&mut driver, 2);
    assert_eq!(
        outcomes.try_recv().unwrap().event.error.as_deref(),
        Some(REFUSE_PATCH_DURING_AUTO)
    );
    assert_eq!(
        outcomes.try_recv().unwrap().event.error.as_deref(),
        Some(REFUSE_PATCH_DURING_AUTO)
    );
    assert!(
        outcomes.try_recv().unwrap().event.error.is_none(),
        "off-air input patches"
    );
    assert_eq!(resolved(&driver), ("a".into(), "c".into()));
}

#[test]
fn a_rejected_composed_tick_restores_the_input_table() {
    let (handle, mut driver) = with_inputs();
    let mut outcomes = handle.outcomes();
    run(&mut driver, 0);
    let before = table(&handle);
    handle
        .try_send(command("x", patch(&[(1, Some("b")), (4, Some("c"))], true)))
        .unwrap();
    driver.drain_at_tick(tick(1));
    assert_eq!(resolved(&driver).0, "b");
    driver.reject_composed_tick(tick(1), "render failed");
    assert!(outcomes.try_recv().unwrap().event.error.is_some());
    assert_eq!(resolved(&driver).0, "a", "table restored with the machine");
    run(&mut driver, 2);
    assert_eq!(table(&handle), before);
    assert_eq!(handle.snapshot().unwrap().inputs_version, 0);
    // The restored table is the live one: a later patch starts from it.
    handle
        .try_send(command("y", patch(&[(4, Some("a"))], false)))
        .unwrap();
    run(&mut driver, 3);
    assert!(outcomes.try_recv().unwrap().event.completed);
    assert_eq!(table(&handle)[0], (1, Some("a".into())));
    assert_eq!(handle.snapshot().unwrap().inputs_version, 1);
}

#[test]
fn a_patch_flood_is_bounded_per_tick_and_by_the_mailbox() {
    let (handle, mut driver) = with_inputs();
    let mut outcomes = handle.outcomes();
    run(&mut driver, 0);
    let mut accepted = 0;
    for i in 0..1_000_u32 {
        let source = if i % 2 == 0 { "a" } else { "b" };
        if handle
            .try_send(command(i, patch(&[(4, Some(source))], false)))
            .is_ok()
        {
            accepted += 1;
        }
    }
    assert_eq!(accepted, COMMAND_CAPACITY);
    let mut index = 1;
    let mut total = 0;
    while total < COMMAND_CAPACITY {
        run(&mut driver, index);
        let mut this_tick = 0;
        while let Ok(item) = outcomes.try_recv() {
            assert!(item.event.completed);
            this_tick += 1;
        }
        assert!(this_tick <= MAX_COMMANDS_PER_TICK);
        total += this_tick;
        index += 1;
    }
    assert_eq!(
        index - 1,
        u64::try_from(COMMAND_CAPACITY / MAX_COMMANDS_PER_TICK).unwrap()
    );
    assert_eq!(
        handle.snapshot().unwrap().inputs_version,
        u64::try_from(COMMAND_CAPACITY).unwrap()
    );
}

/// Review A-1: punching the Preview input, or another input carrying the
/// Program source, is refused, and both buses stay distinct.
#[test]
fn punching_the_preview_input_or_the_program_source_is_refused() {
    let (handle, mut driver) = with_inputs();
    let mut outcomes = handle.outcomes();
    run(&mut driver, 0);
    // Input 3 is on Preview.
    handle
        .try_send(command(1, SwitcherVerb::SelectProgram(SourceRef::input(3))))
        .unwrap();
    run(&mut driver, 1);
    assert_eq!(
        outcomes.try_recv().unwrap().event.error.as_deref(),
        Some(REFUSE_SAME_SOURCE)
    );
    // Input 4 now carries `a`, the Program source.
    handle
        .try_send(command(2, patch(&[(4, Some("a"))], false)))
        .unwrap();
    run(&mut driver, 2);
    assert!(outcomes.try_recv().unwrap().event.error.is_none());
    handle
        .try_send(command(3, SwitcherVerb::SelectProgram(SourceRef::input(4))))
        .unwrap();
    run(&mut driver, 3);
    assert!(outcomes.try_recv().unwrap().event.error.is_some());
    assert_eq!(resolved(&driver), ("a".into(), "c".into()));
    let snapshot = handle.snapshot().unwrap();
    assert_eq!(
        (snapshot.program_input, snapshot.preview_input),
        (Some(1), Some(3))
    );
}
