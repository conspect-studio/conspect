#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! The mixer half of an atomic multi-crosspoint set (ADR-0110 §12.8, #308):
//! the switcher validates and primes an armed patch, applies it only at the
//! tick the router proposes through the shared `SetArm`, and never holds the
//! single take slot while it waits.
use multiview_compositor::pipeline::Nv12Image;
use multiview_core::color::ColorInfo;
use multiview_core::layout::{Canvas, Cell, Layout};
use multiview_core::switcher::SourceRef;
use multiview_core::time::MediaTime;
use multiview_engine::clock::Tick;
use multiview_engine::patch_set::{ArmState, SetArm, SetRefusal, SET_ADOPT_DEADLINE_TICKS};
use multiview_engine::switcher::SwitcherState;
use multiview_engine::switcher_control::{
    PatchSet, ScenePair, SwitcherCommand, SwitcherControl, SwitcherTickDriver, SwitcherVerb,
    REFUSE_NO_FRESH_FRAME, REFUSE_ON_AIR_PROTECTED, REFUSE_SET_ARMED,
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

fn command(op: &str, verb: SwitcherVerb) -> SwitcherCommand {
    SwitcherCommand {
        operation_id: op.to_owned(),
        scene_version: 1,
        verb,
        expect: None,
    }
}

fn armed(members: &[(u16, &str)], allow_on_air: bool) -> (SwitcherVerb, Arc<SetArm>) {
    let arm = Arc::new(SetArm::new());
    (
        SwitcherVerb::PatchArmed {
            set: PatchSet {
                members: members
                    .iter()
                    .map(|(n, s)| (*n, Some(SourceRef::new(*s))))
                    .collect(),
                allow_on_air,
            },
            arm: Arc::clone(&arm),
        },
        arm,
    )
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
            (1, None, Some(SourceRef::new("a"))),
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

fn input(handle: &SwitcherControl, number: u16) -> Option<String> {
    handle
        .inputs()
        .unwrap()
        .inputs
        .iter()
        .find(|i| i.number == number)
        .and_then(|i| i.source.as_ref().map(|s| s.as_str().to_owned()))
}

#[test]
fn an_armed_patch_lands_only_at_the_proposed_tick_and_holds_no_take_slot() {
    let (handle, mut driver) = with_inputs();
    let mut outcomes = handle.outcomes();
    run(&mut driver, 0);
    let (verb, arm) = armed(&[(4, "a"), (2, "c")], false);
    handle.try_send(command("set", verb)).unwrap();
    run(&mut driver, 1);
    assert_eq!(arm.state(), ArmState::SwitcherReady, "validated and primed");
    assert_eq!(input(&handle, 4), None, "nothing applied before T");
    // A Cut while the set is armed applies (no held take slot).
    handle.try_send(command("cut", SwitcherVerb::Cut)).unwrap();
    run(&mut driver, 2);
    let cut = outcomes.try_recv().unwrap().event;
    assert!(cut.completed && cut.error.is_none(), "{cut:?}");
    assert_eq!(handle.snapshot().unwrap().program_input, Some(3));
    // The router proposes T = 6.
    assert!(arm.propose(6));
    for t in 3..6 {
        run(&mut driver, t);
        assert_eq!(input(&handle, 4), None, "tick {t}: still before T");
    }
    run(&mut driver, 6);
    assert_eq!(arm.state(), ArmState::Committed);
    assert_eq!(arm.tick(), 6);
    let readback = handle.inputs().unwrap();
    assert_eq!(readback.tick, 6, "the mixer table changes at T");
    assert_eq!(input(&handle, 4).as_deref(), Some("a"));
    assert_eq!(input(&handle, 2).as_deref(), Some("c"));
    let done = outcomes.try_recv().unwrap().event;
    assert_eq!((done.operation_id.as_str(), done.tick), ("set", 6));
    assert!(done.completed && done.error.is_none());
    // The slot is free again: a second armed set is admitted.
    let (verb, second) = armed(&[(4, "b")], false);
    handle.try_send(command("second", verb)).unwrap();
    run(&mut driver, 7);
    assert_eq!(second.state(), ArmState::SwitcherReady);
}

#[test]
fn a_missed_rejected_or_repeated_t_goes_back_to_the_router_and_never_commits() {
    let (handle, mut driver) = with_inputs();
    let mut outcomes = handle.outcomes();
    run(&mut driver, 0);
    let (verb, arm) = armed(&[(4, "b")], false);
    handle.try_send(command("set", verb)).unwrap();
    run(&mut driver, 1);
    // Missed: proposed for a tick the clock already passed.
    assert!(arm.propose(1));
    run(&mut driver, 2);
    assert_eq!(
        arm.state(),
        ArmState::SwitcherReady,
        "missed T re-proposable"
    );
    // Repeated: the clock repeats tick 3 (stale picture), never composes it.
    assert!(arm.propose(3));
    driver.repeat_tick(tick(3));
    assert_eq!(
        arm.state(),
        ArmState::SwitcherReady,
        "a repeat tick never commits"
    );
    // Rejected: the frame of T does not composite; the table is restored.
    assert!(arm.propose(4));
    driver.drain_at_tick(tick(4));
    assert_eq!(arm.state(), ArmState::Applying);
    driver.reject_composed_tick(tick(4), "render failed");
    assert_eq!(
        arm.state(),
        ArmState::SwitcherReady,
        "rejected T re-proposable"
    );
    assert_eq!(handle.snapshot().unwrap().inputs_version, 0);
    assert!(
        outcomes.try_recv().is_err(),
        "no outcome for an unresolved set: {:?}",
        outcomes.try_recv()
    );
    // A re-proposal lands.
    assert!(arm.propose(6));
    run(&mut driver, 5);
    run(&mut driver, 6);
    assert_eq!((arm.state(), arm.tick()), (ArmState::Committed, 6));
    assert_eq!(input(&handle, 4).as_deref(), Some("b"));
    assert!(outcomes.try_recv().unwrap().event.completed);
}

#[test]
fn revalidation_at_t_refuses_on_air_protected_after_a_cut_moved_program() {
    let (handle, mut driver) = with_inputs();
    let mut outcomes = handle.outcomes();
    run(&mut driver, 0);
    // Input 3 is on Preview, so `protect` admits re-patching it.
    let (verb, arm) = armed(&[(3, "b")], false);
    handle.try_send(command("set", verb)).unwrap();
    run(&mut driver, 1);
    assert_eq!(arm.state(), ArmState::SwitcherReady);
    assert!(arm.propose(3));
    // A Cut puts input 3 on Program before T.
    handle.try_send(command("cut", SwitcherVerb::Cut)).unwrap();
    run(&mut driver, 2);
    assert!(outcomes.try_recv().unwrap().event.completed);
    run(&mut driver, 3);
    assert_eq!(arm.state(), ArmState::Refused);
    assert_eq!(arm.reason(), Some(SetRefusal::OnAirProtected));
    let refused = outcomes.try_recv().unwrap().event;
    assert_eq!(refused.error.as_deref(), Some(REFUSE_ON_AIR_PROTECTED));
    assert_eq!(input(&handle, 3).as_deref(), Some("c"), "table unchanged");
}

#[test]
fn a_cold_program_source_is_refused_at_the_deadline_and_a_second_set_is_busy() {
    let (handle, mut driver) = with_inputs();
    let mut outcomes = handle.outcomes();
    run(&mut driver, 0);
    // Re-patch the Program input onto the cold source `d` (allowed on air).
    let (verb, arm) = armed(&[(1, "d")], true);
    handle.try_send(command("cold", verb)).unwrap();
    let (verb2, second) = armed(&[(4, "a")], false);
    handle.try_send(command("second", verb2)).unwrap();
    run(&mut driver, 1);
    let busy = outcomes.try_recv().unwrap().event;
    assert_eq!(busy.operation_id, "second");
    assert_eq!(busy.error.as_deref(), Some(REFUSE_SET_ARMED));
    assert_eq!(second.reason(), Some(SetRefusal::Busy));
    for t in 2..=SET_ADOPT_DEADLINE_TICKS {
        run(&mut driver, t);
        assert_eq!(arm.state(), ArmState::Pending, "never ready while cold");
    }
    run(&mut driver, 1 + SET_ADOPT_DEADLINE_TICKS);
    assert_eq!(arm.reason(), Some(SetRefusal::FeedNotFresh));
    let refused = outcomes.try_recv().unwrap().event;
    assert_eq!(refused.error.as_deref(), Some(REFUSE_NO_FRESH_FRAME));
    assert_eq!(refused.tick, 1 + SET_ADOPT_DEADLINE_TICKS);
    assert_eq!(input(&handle, 1).as_deref(), Some("a"), "Program unchanged");
}

#[test]
fn a_router_refusal_frees_the_slot_and_is_reported_once() {
    let (handle, mut driver) = with_inputs();
    let mut outcomes = handle.outcomes();
    run(&mut driver, 0);
    let (verb, arm) = armed(&[(4, "a")], false);
    handle.try_send(command("set", verb)).unwrap();
    run(&mut driver, 1);
    assert!(arm.propose(5));
    // The router refuses it (an output member's encoder never opened).
    assert!(arm.refuse(SetRefusal::EncoderNotReady));
    run(&mut driver, 2);
    let refused = outcomes.try_recv().unwrap().event;
    assert_eq!(refused.error.as_deref(), Some("refused:encoder-not-ready"));
    for t in 3..8 {
        run(&mut driver, t);
    }
    assert!(outcomes.try_recv().is_err(), "exactly one outcome");
    assert_eq!(handle.snapshot().unwrap().inputs_version, 0);
    let (verb, next) = armed(&[(4, "b")], false);
    handle.try_send(command("next", verb)).unwrap();
    run(&mut driver, 8);
    assert_eq!(next.state(), ArmState::SwitcherReady, "slot freed");
}

/// Review C-2: with the router gone and the clock only repeating ticks (no
/// fresh drain), the armed set still reaches a terminal state within the
/// switcher backstop, and a set the clock is applying is never touched.
#[test]
fn sustained_repeat_ticks_still_expire_an_armed_set_but_never_an_applying_one() {
    use multiview_engine::switcher_control::SET_SWITCHER_GRACE_TICKS;
    let backstop = 1 + SET_ADOPT_DEADLINE_TICKS + SET_SWITCHER_GRACE_TICKS;
    // SwitcherReady, proposed once, then the router vanishes.
    let (handle, mut driver) = with_inputs();
    let mut outcomes = handle.outcomes();
    run(&mut driver, 0);
    let (verb, arm) = armed(&[(4, "b")], false);
    handle.try_send(command("set", verb)).unwrap();
    run(&mut driver, 1);
    assert_eq!(arm.state(), ArmState::SwitcherReady);
    assert!(arm.propose(5));
    let mut ended = None;
    for t in 2..=backstop + 1 {
        driver.repeat_tick(tick(t));
        if arm.state().is_terminal() {
            ended = Some(t);
            break;
        }
    }
    assert_eq!(ended, Some(backstop), "terminal exactly at the backstop");
    assert_eq!(arm.reason(), Some(SetRefusal::Deadline));
    let refused = outcomes.try_recv().unwrap().event;
    assert!(!refused.completed);
    assert!(outcomes.try_recv().is_err(), "exactly one outcome");
    assert_eq!(input(&handle, 4), None, "table unchanged");

    // Pending (cold Program source) under repeats: feed-not-fresh at 150.
    let (handle, mut driver) = with_inputs();
    run(&mut driver, 0);
    let (verb, cold) = armed(&[(1, "d")], true);
    handle.try_send(command("cold", verb)).unwrap();
    run(&mut driver, 1);
    let mut ended = None;
    for t in 2..=backstop {
        driver.repeat_tick(tick(t));
        if cold.state().is_terminal() {
            ended = Some(t);
            break;
        }
    }
    assert_eq!(ended, Some(1 + SET_ADOPT_DEADLINE_TICKS));
    assert_eq!(cold.reason(), Some(SetRefusal::FeedNotFresh));

    // Applying is the clock's: repeats past every deadline never move it.
    let (handle, mut driver) = with_inputs();
    run(&mut driver, 0);
    let (verb, held) = armed(&[(4, "a")], false);
    handle.try_send(command("held", verb)).unwrap();
    run(&mut driver, 1);
    assert!(held.propose(2));
    driver.drain_at_tick(tick(2));
    assert_eq!(held.state(), ArmState::Applying);
    for t in 3..=backstop + 4 {
        driver.repeat_tick(tick(t));
        assert_eq!(held.state(), ArmState::Applying, "tick {t}");
    }
    driver.commit_composed_tick(tick(2));
    assert_eq!((held.state(), held.tick()), (ArmState::Committed, 2));
}

/// #307 PR 2: a member of an armed set is a reference (the frame hook never
/// removes a layout store an armed salvo will patch), until the set settles.
#[test]
fn an_armed_set_member_is_a_source_reference_until_it_settles() {
    let (handle, mut driver) = with_inputs();
    run(&mut driver, 1);
    assert!(!driver.references_source("d"));
    let (verb, _arm) = armed(&[(4, "d")], false);
    handle.try_send(command("set", verb)).unwrap();
    run(&mut driver, 2);
    assert!(
        driver.references_source("d"),
        "an armed set's member must count as referenced"
    );
}
