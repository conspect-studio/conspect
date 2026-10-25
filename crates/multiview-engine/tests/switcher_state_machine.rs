//! State-machine checks for the pure M/E switching value machine.
use multiview_core::switcher::{ArmedSet, FrameCount, Progress, SourceRef, TransitionDescriptor};
use multiview_core::time::MediaTime;
use multiview_engine::clock::Tick;
use multiview_engine::switcher::{SwitcherError, SwitcherState};
use proptest::prelude::*;
use proptest_state_machine::{prop_state_machine, ReferenceStateMachine, StateMachineTest};

fn tick(index: u64) -> Tick {
    Tick {
        index,
        pts: MediaTime::from_nanos(0),
    }
}
fn source(id: &str) -> SourceRef {
    SourceRef::new(id)
}
fn configured() -> SwitcherState {
    SwitcherState::new(source("a"), source("b"), vec![false, true])
}

#[test]
fn auto_uses_only_tick_index_and_completes_once() {
    let mut state = configured();
    state
        .set_transition(TransitionDescriptor::Mix {
            rate: FrameCount::try_from(4).unwrap(),
        })
        .unwrap();
    assert!(state.auto(tick(10)).unwrap().started);
    assert_eq!(
        state.sample(tick(12)).progress,
        Progress::new(2, FrameCount::try_from(4).unwrap())
    );
    assert!(state.sample(tick(14)).completed);
    assert!(!state.sample(tick(15)).completed);
    assert_eq!(state.program(), &source("b"));
    assert_eq!(state.preview(), &source("a"));
}

#[test]
fn manual_override_abort_and_midflight_cut() {
    let mut state = configured();
    state
        .set_transition(TransitionDescriptor::Mix {
            rate: FrameCount::try_from(8).unwrap(),
        })
        .unwrap();
    state.auto(tick(1)).unwrap();
    assert_eq!(
        state.tbar(u16::MAX, tick(2)).unwrap().progress,
        Progress::COMPLETE
    );
    assert_eq!(state.program(), &source("b"));
    assert!(!state.tbar(u16::MAX, tick(2)).unwrap().completed);
    assert_eq!(state.program(), &source("b"));
    state.auto(tick(3)).unwrap();
    let p = state.tbar(32768, tick(4)).unwrap().progress;
    assert_eq!(state.sample(tick(100)).progress, p);
    assert_eq!(state.tbar(32768, tick(101)).unwrap().progress, p);
    assert_eq!(state.abort().progress, Progress::ZERO);
    assert_eq!(state.program(), &source("b"));
    state.auto(tick(102)).unwrap();
    assert!(state.cut().unwrap().completed);
    assert_eq!(state.program(), &source("a"));
    assert!(!state.sample(tick(103)).completed);
}

#[test]
fn resting_tbar_does_not_block_auto() {
    let mut state = configured();
    state
        .set_transition(TransitionDescriptor::Mix {
            rate: FrameCount::try_from(8).unwrap(),
        })
        .unwrap();
    assert_eq!(state.tbar(0, tick(10)).unwrap().progress, Progress::ZERO);
    assert!(!state.is_in_flight());
    assert!(state.auto(tick(10)).unwrap().started);
    assert!(state.sample(tick(18)).completed);
    assert_eq!(state.program(), &source("b"));
}

#[test]
fn auto_resumes_manual_position_and_completes_at_proportional_tick() {
    let mut state = configured();
    state
        .set_transition(TransitionDescriptor::Mix {
            rate: FrameCount::try_from(10).unwrap(),
        })
        .unwrap();
    let manual = state.tbar(32768, tick(100)).unwrap();
    assert!(manual.started);
    let resumed = state.auto(tick(100)).unwrap();
    assert!(!resumed.started);
    assert!(!resumed.completed);
    assert_eq!(manual.progress.numerator(), 32768);
    assert_eq!(resumed.progress, manual.progress);
    assert!(!state.sample(tick(104)).completed);
    assert!(state.sample(tick(105)).completed);
    assert_eq!(state.completion_count(), 1);
    assert_eq!(state.program(), &source("b"));
    assert!(!state.sample(tick(106)).completed);
}

#[test]
fn manual_resume_at_tick_zero_keeps_exact_elapsed_offset() {
    let mut state = configured();
    state
        .set_transition(TransitionDescriptor::Mix {
            rate: FrameCount::try_from(10).unwrap(),
        })
        .unwrap();
    let manual = state.tbar(32768, tick(0)).unwrap();
    assert_eq!(state.auto(tick(0)).unwrap().progress, manual.progress);
    assert!(!state.sample(tick(4)).completed);
    assert!(state.sample(tick(5)).completed);
}

#[test]
fn fractional_tbar_handoff_rounds_half_up_without_backstepping() {
    for (rate, position, elapsed) in [(10, 39000, 6), (2, 30000, 1), (2, 40000, 1), (10, 34500, 5)]
    {
        let mut state = configured();
        let rate = FrameCount::try_from(rate).unwrap();
        state
            .set_transition(TransitionDescriptor::Mix { rate })
            .unwrap();
        let manual = state.tbar(position, tick(100)).unwrap().progress;
        let resumed = state.auto(tick(100)).unwrap();
        assert!(
            resumed.progress >= manual,
            "rate={rate:?}, position={position}"
        );
        assert!(!resumed.completed);
        if (u64::from(elapsed) * u64::from(u16::MAX)) >= u64::from(position) * u64::from(rate.get())
        {
            assert_eq!(resumed.progress, Progress::new(elapsed, rate));
        } else {
            assert_eq!(resumed.progress, manual);
        }
        let finish = 100 + u64::from(rate.get() - elapsed);
        assert!(!state.sample(tick(finish - 1)).completed);
        assert!(state.sample(tick(finish)).completed);
        assert_eq!(state.completion_count(), 1);
    }
}

proptest! {
    #[test]
    fn auto_from_tbar_never_backsteps_and_finishes_on_rounded_tick(
        rate in 1u32..=u32::MAX,
        position in 1u16..u16::MAX,
    ) {
        let mut state = configured();
        let rate = FrameCount::try_from(rate).unwrap();
        state.set_transition(TransitionDescriptor::Mix { rate }).unwrap();
        let start = 100u64;
        let manual = state.tbar(position, tick(start)).unwrap().progress;
        let elapsed = (u64::from(position) * u64::from(rate.get())
            + u64::from(u16::MAX) / 2) / u64::from(u16::MAX);
        let finish = start + u64::from(rate.get()) - elapsed;
        let resumed = state.auto(tick(start)).unwrap();
        prop_assert!(resumed.progress >= manual);
        if finish > start {
            prop_assert!(!resumed.completed);
            prop_assert!(!state.sample(tick(finish - 1)).completed);
            prop_assert!(state.sample(tick(finish)).completed);
        } else {
            prop_assert!(resumed.completed);
        }
        prop_assert_eq!(state.completion_count(), 1);
        prop_assert!(!state.sample(tick(finish + 1)).completed);
    }
}

#[test]
fn repeated_terminal_tbar_completes_and_flips_exactly_once() {
    let mut state = configured();
    let first = state.tbar(u16::MAX, tick(1)).unwrap();
    let second = state.tbar(u16::MAX, tick(2)).unwrap();
    assert!(first.started && first.completed);
    assert!(!second.started && !second.completed);
    assert_eq!(state.completion_count(), 1);
    assert_eq!(state.program(), &source("b"));
    assert_eq!(state.preview(), &source("a"));
}

#[test]
fn terminal_tbar_completes_manual_take_once() {
    let mut state = configured();
    state.tbar(12345, tick(2)).unwrap();
    let completed = state.tbar(u16::MAX, tick(3)).unwrap();
    assert!(completed.completed);
    assert!(!completed.started);
    assert!(!state.is_in_flight());
    assert_eq!(state.completion_count(), 1);
    assert_eq!(state.program(), &source("b"));
    assert!(!state.tbar(u16::MAX, tick(4)).unwrap().completed);
}

#[test]
fn configuration_is_rejected_during_auto_and_applies_after_completion() {
    let mut state = configured();
    let initial = TransitionDescriptor::Mix {
        rate: FrameCount::try_from(4).unwrap(),
    };
    let next = TransitionDescriptor::Mix {
        rate: FrameCount::try_from(7).unwrap(),
    };
    state.set_transition(initial.clone()).unwrap();
    state.auto(tick(10)).unwrap();
    let keys = ArmedSet::default().with_key(1);
    assert_eq!(state.arm(keys.clone()), Err(SwitcherError::InFlight));
    assert_eq!(
        state.set_transition(next.clone()),
        Err(SwitcherError::InFlight)
    );
    assert_eq!(state.set_preview(source("c")), Err(SwitcherError::InFlight));
    assert_eq!(state.armed(), &ArmedSet::default().with_background());
    assert_eq!(state.transition(), &initial);
    assert_eq!(state.preview(), &source("b"));
    assert!(state.sample(tick(14)).completed);
    assert_eq!(state.arm(keys.clone()), Ok(()));
    assert_eq!(state.set_transition(next.clone()), Ok(()));
    assert_eq!(state.set_preview(source("c")), Ok(()));
    assert_eq!(state.armed(), &keys);
    assert_eq!(state.transition(), &next);
    assert_eq!(state.preview(), &source("c"));
    assert!(state.auto(tick(20)).unwrap().started);
    assert!(state.sample(tick(27)).completed);
    assert_eq!(state.keys_on_air(), &[true, true]);
}

#[test]
fn configuration_is_rejected_during_manual_and_applies_after_completion() {
    let mut state = configured();
    state.tbar(12000, tick(1)).unwrap();
    let keys = ArmedSet::default().with_key(1);
    let next = TransitionDescriptor::Mix {
        rate: FrameCount::try_from(3).unwrap(),
    };
    assert_eq!(state.arm(keys.clone()), Err(SwitcherError::InFlight));
    assert_eq!(
        state.set_transition(next.clone()),
        Err(SwitcherError::InFlight)
    );
    assert_eq!(state.set_preview(source("c")), Err(SwitcherError::InFlight));
    assert!(state.tbar(u16::MAX, tick(2)).unwrap().completed);
    assert_eq!(state.arm(keys.clone()), Ok(()));
    assert_eq!(state.set_transition(next.clone()), Ok(()));
    assert_eq!(state.set_preview(source("c")), Ok(()));
    assert_eq!(state.armed(), &keys);
    assert_eq!(state.transition(), &next);
    assert_eq!(state.preview(), &source("c"));
}

#[test]
fn key_only_and_flip_flop_off_and_punch() {
    let mut state = configured();
    state.arm(ArmedSet::default().with_key(1)).unwrap();
    assert!(state.cut().unwrap().completed);
    assert_eq!(state.program(), &source("a"));
    assert_eq!(state.preview(), &source("b"));
    assert_eq!(state.keys_on_air(), &[true, true]);
    state.set_flip_flop(false);
    state.arm(ArmedSet::default().with_background()).unwrap();
    state.cut().unwrap();
    assert_eq!(state.program(), &source("b"));
    assert_eq!(state.preview(), &source("b"));
    state.punch(source("c"));
    assert_eq!(state.program(), &source("c"));
    assert_eq!(state.preview(), &source("b"));
    assert_eq!(
        state.arm(ArmedSet::default()),
        Err(SwitcherError::NothingArmed)
    );
}

#[derive(Clone, Debug)]
enum Cmd {
    Auto,
    Cut,
    Abort,
    Tbar(u16),
    RepeatTerminalTbar,
    Advance(u8),
    Arm(bool, bool),
    SetPreview(u8),
    SetTransition(u8),
    Punch(u8),
}
#[derive(Clone, Debug)]
struct Model {
    tick: u64,
}
struct Reference;
impl ReferenceStateMachine for Reference {
    type State = Model;
    type Transition = Cmd;
    fn init_state() -> BoxedStrategy<Model> {
        Just(Model { tick: 0 }).boxed()
    }
    fn transitions(_: &Model) -> BoxedStrategy<Cmd> {
        prop_oneof![
            Just(Cmd::Auto),
            Just(Cmd::Cut),
            Just(Cmd::Abort),
            any::<u16>().prop_map(Cmd::Tbar),
            Just(Cmd::RepeatTerminalTbar),
            (0u8..12).prop_map(Cmd::Advance),
            (any::<bool>(), any::<bool>()).prop_map(|(b, k)| Cmd::Arm(b, k)),
            (0u8..3).prop_map(Cmd::SetPreview),
            (2u8..12).prop_map(Cmd::SetTransition),
            (0u8..3).prop_map(Cmd::Punch)
        ]
        .boxed()
    }
    fn apply(mut state: Model, transition: &Cmd) -> Model {
        // The reference tracks *observed* completions and the last output projection;
        // its independent invariant is that a bus swap cannot occur without a take.
        if let Cmd::Advance(n) = transition {
            state.tick += u64::from(*n);
        }
        state
    }
}
struct Sut {
    machine: SwitcherState,
    tick: u64,
    completions: u64,
    previous: (SourceRef, SourceRef),
    previous_progress: Progress,
    was_in_flight: bool,
}
struct Test;
impl StateMachineTest for Test {
    type SystemUnderTest = Sut;
    type Reference = Reference;
    fn init_test(_: &Model) -> Sut {
        let mut machine = configured();
        assert_eq!(
            machine.set_transition(TransitionDescriptor::Mix {
                rate: FrameCount::try_from(5).unwrap_or(FrameCount::ONE),
            }),
            Ok(())
        );
        Sut {
            machine,
            tick: 0,
            completions: 0,
            previous: (source("a"), source("b")),
            previous_progress: Progress::ZERO,
            was_in_flight: false,
        }
    }
    fn apply(mut sut: Sut, _: &Model, transition: Cmd) -> Sut {
        let result = match transition {
            Cmd::Auto => sut.machine.auto(tick(sut.tick)).ok(),
            Cmd::Cut => sut.machine.cut().ok(),
            Cmd::Abort => Some(sut.machine.abort()),
            Cmd::Tbar(position) => sut.machine.tbar(position, tick(sut.tick)).ok(),
            Cmd::RepeatTerminalTbar => {
                let first = sut.machine.tbar(u16::MAX, tick(sut.tick)).ok();
                if first.is_some() {
                    let second = sut.machine.tbar(u16::MAX, tick(sut.tick));
                    assert!(
                        matches!(second, Ok(projection) if !projection.completed),
                        "repeated terminal T-bar completed twice or returned an error"
                    );
                }
                first
            }
            Cmd::Advance(n) => {
                sut.tick += u64::from(n);
                Some(sut.machine.sample(tick(sut.tick)))
            }
            Cmd::Arm(background, key) => {
                let mut set = ArmedSet::default();
                if background {
                    set = set.with_background();
                }
                if key {
                    set = set.with_key(1);
                }
                let expected = if sut.machine.is_in_flight() {
                    Err(SwitcherError::InFlight)
                } else if !background && !key {
                    Err(SwitcherError::NothingArmed)
                } else {
                    Ok(())
                };
                assert_eq!(sut.machine.arm(set.clone()), expected);
                if expected.is_ok() {
                    assert_eq!(sut.machine.armed(), &set);
                }
                None
            }
            Cmd::SetPreview(n) => {
                let scene = source(&format!("v{n}"));
                let before = sut.machine.preview().clone();
                let expected = if sut.machine.is_in_flight() {
                    Err(SwitcherError::InFlight)
                } else {
                    Ok(())
                };
                assert_eq!(sut.machine.set_preview(scene.clone()), expected);
                assert_eq!(
                    sut.machine.preview(),
                    if expected.is_ok() { &scene } else { &before }
                );
                None
            }
            Cmd::SetTransition(n) => {
                let next = TransitionDescriptor::Mix {
                    rate: FrameCount::try_from(u32::from(n)).unwrap_or(FrameCount::ONE),
                };
                let before = sut.machine.transition().clone();
                let expected = if sut.machine.is_in_flight() {
                    Err(SwitcherError::InFlight)
                } else {
                    Ok(())
                };
                assert_eq!(sut.machine.set_transition(next.clone()), expected);
                assert_eq!(
                    sut.machine.transition(),
                    if expected.is_ok() { &next } else { &before }
                );
                None
            }
            Cmd::Punch(n) => Some(sut.machine.punch(source(&format!("p{n}")))),
        };
        if let Some(output) = result {
            assert!(output.progress.numerator() <= output.progress.denominator());
            if output.completed {
                assert!(
                    sut.was_in_flight || output.started,
                    "a completion must close an active or newly started take: {transition:?}"
                );
                sut.completions += 1;
                assert!(!sut.machine.is_in_flight());
            }
            if !output.completed && !matches!(transition, Cmd::Punch(_)) {
                // A flip-flop is exclusively a completion effect.
                assert_eq!(
                    (sut.machine.program().clone(), sut.machine.preview().clone()),
                    sut.previous
                );
            }
            sut.previous_progress = output.progress;
        }
        sut.previous = (sut.machine.program().clone(), sut.machine.preview().clone());
        sut.was_in_flight = sut.machine.is_in_flight();
        sut
    }
    fn check_invariants(sut: &Sut, model: &Model) {
        assert_eq!(sut.tick, model.tick);
        assert!(sut.previous_progress.numerator() <= sut.previous_progress.denominator());
        assert_eq!(sut.completions, sut.machine.completion_count());
    }
}
prop_state_machine! {
    #[test]
    fn arbitrary_switcher_verb_sequences(sequential 1..80 => Test);
}
