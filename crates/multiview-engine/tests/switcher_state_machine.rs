//! State-machine checks for the pure M/E switching value machine.
use multiview_core::switcher::{ArmedSet, FrameCount, Progress, SourceRef, TransitionDescriptor};
use multiview_core::time::MediaTime;
use multiview_engine::clock::Tick;
use multiview_engine::switcher::{SwitcherError, SwitcherProjection, SwitcherState};
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
    state.tbar(0, tick(3)).unwrap();
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
fn terminal_lever_ignores_edits_and_jitter_until_returned_to_bottom() {
    for edit in 0..3 {
        let mut state = configured();
        let first = state.tbar(u16::MAX, tick(1)).unwrap();
        assert!(first.started && first.completed);
        match edit {
            0 => state.set_preview(source("c")).unwrap(),
            1 => state.arm(ArmedSet::default().with_key(1)).unwrap(),
            _ => state
                .set_transition(TransitionDescriptor::Mix {
                    rate: FrameCount::try_from(5).unwrap(),
                })
                .unwrap(),
        }
        let program = state.program().clone();
        let preview = state.preview().clone();
        let keys = state.keys_on_air().to_vec();
        for position in [64000, u16::MAX, 64000] {
            let output = state.tbar(position, tick(2)).unwrap();
            assert_eq!(output.progress, Progress::ZERO);
            assert!(!output.started && !output.completed);
            assert!(!state.is_in_flight());
            assert_eq!(state.completion_count(), 1);
            assert_eq!(state.program(), &program);
            assert_eq!(state.preview(), &preview);
            assert_eq!(state.keys_on_air(), keys);
        }
        assert_eq!(state.tbar(0, tick(3)).unwrap().progress, Progress::ZERO);
        let next = state.tbar(30000, tick(4)).unwrap();
        assert!(next.started && !next.completed);
        assert!(state.is_in_flight());
    }
}

#[test]
fn lever_requires_bottom_after_every_manual_take_termination() {
    for ending in ["cut", "abort", "auto", "max"] {
        let mut state = configured();
        state
            .set_transition(TransitionDescriptor::Mix {
                rate: FrameCount::try_from(10).unwrap(),
            })
            .unwrap();
        assert!(state.tbar(30000, tick(100)).unwrap().started);
        match ending {
            "cut" => assert!(state.cut().unwrap().completed),
            "abort" => assert_eq!(state.abort().progress, Progress::ZERO),
            "auto" => {
                assert!(!state.auto(tick(100)).unwrap().completed);
                assert!(state.sample(tick(106)).completed);
            }
            _ => assert!(state.tbar(u16::MAX, tick(101)).unwrap().completed),
        }
        let completed = state.completion_count();
        let program = state.program().clone();
        for position in [30000, 31000, u16::MAX] {
            let output = state.tbar(position, tick(110)).unwrap();
            assert_eq!(
                output,
                SwitcherProjection {
                    progress: Progress::ZERO,
                    started: false,
                    completed: false,
                },
                "ending={ending}, position={position}"
            );
            assert!(!state.is_in_flight());
            assert_eq!(state.completion_count(), completed);
            assert_eq!(state.program(), &program);
        }
        assert_eq!(state.tbar(0, tick(111)).unwrap().progress, Progress::ZERO);
        assert!(state.tbar(30000, tick(112)).unwrap().started);
    }
}

#[test]
fn parked_lever_returning_to_bottom_does_not_abort_an_independent_auto() {
    let mut state = configured();
    state
        .set_transition(TransitionDescriptor::Mix {
            rate: FrameCount::try_from(5).unwrap(),
        })
        .unwrap();
    assert!(state.tbar(u16::MAX, tick(1)).unwrap().completed);
    assert_eq!(state.program(), &source("b"));
    assert!(state.auto(tick(10)).unwrap().started);
    assert_eq!(
        state.tbar(40000, tick(11)).unwrap().progress,
        Progress::ZERO
    );
    let returned = state.tbar(0, tick(12)).unwrap();
    assert_eq!(
        returned.progress,
        Progress::new(2, FrameCount::try_from(5).unwrap())
    );
    assert!(!returned.started && !returned.completed);
    assert!(state.is_in_flight());
    assert_eq!(state.program(), &source("b"));
    assert!(state.sample(tick(15)).completed);
    assert_eq!(state.program(), &source("a"));
    assert_eq!(state.completion_count(), 2);
}

#[test]
fn duplicated_tbar_zero_does_not_abort_an_independent_auto() {
    let mut state = configured();
    state
        .set_transition(TransitionDescriptor::Mix {
            rate: FrameCount::try_from(5).unwrap(),
        })
        .unwrap();
    assert!(state.tbar(u16::MAX, tick(1)).unwrap().completed);
    assert!(state.auto(tick(10)).unwrap().started);
    state.tbar(0, tick(11)).unwrap();
    let again = state.tbar(0, tick(12)).unwrap();
    assert_eq!(
        again.progress,
        Progress::new(2, FrameCount::try_from(5).unwrap())
    );
    assert!(state.is_in_flight());
    assert!(state.sample(tick(15)).completed);
    assert_eq!(state.completion_count(), 2);
}

#[test]
fn duplicated_nonzero_tbar_does_not_override_auto_resumed_from_it() {
    let mut state = configured();
    state
        .set_transition(TransitionDescriptor::Mix {
            rate: FrameCount::try_from(10).unwrap(),
        })
        .unwrap();
    state.tbar(30000, tick(1)).unwrap();
    assert!(state.auto(tick(2)).unwrap().progress > Progress::ZERO);
    state.tbar(30000, tick(3)).unwrap();
    assert!(state.is_in_flight());
    assert!(state.sample(tick(100)).completed);
    assert_eq!(state.completion_count(), 1);
}

proptest! {
    #[test]
    fn resending_the_last_tbar_value_is_a_no_op(
        ops in proptest::collection::vec((0u8..6, any::<u16>(), 0u8..6), 0..24),
        last in prop_oneof![Just(0u16), Just(u16::MAX), any::<u16>()],
    ) {
        let mut state = configured();
        state
            .set_transition(TransitionDescriptor::Mix {
                rate: FrameCount::try_from(5).unwrap(),
            })
            .unwrap();
        let mut now = 0u64;
        for (kind, value, advance) in ops {
            now += u64::from(advance);
            let _ = match kind {
                0 => state.auto(tick(now)),
                1 => state.cut(),
                2 => Ok(state.abort()),
                3 => state.tbar(value, tick(now)),
                4 => Ok(state.punch(source("c"))),
                _ => Ok(state.sample(tick(now))),
            };
        }
        let _ = state.tbar(last, tick(now));
        let _ = state.sample(tick(now));
        let before = format!("{state:?}");
        let _ = state.tbar(last, tick(now));
        prop_assert_eq!(format!("{state:?}"), before);
    }
}

#[test]
fn parked_lever_return_after_cut_or_punch_preserves_independent_auto() {
    for ending in ["cut", "punch"] {
        let mut state = configured();
        state
            .set_transition(TransitionDescriptor::Mix {
                rate: FrameCount::try_from(5).unwrap(),
            })
            .unwrap();
        state.tbar(30000, tick(1)).unwrap();
        if ending == "cut" {
            assert!(state.cut().unwrap().completed);
        } else {
            assert!(state.punch(source("c")).completed);
        }
        let program = state.program().clone();
        assert!(state.auto(tick(10)).unwrap().started);
        assert_eq!(
            state.tbar(0, tick(12)).unwrap().progress,
            Progress::new(2, FrameCount::try_from(5).unwrap())
        );
        assert!(state.is_in_flight(), "ending={ending}");
        assert_eq!(state.program(), &program);
        assert!(state.sample(tick(15)).completed);
        assert_eq!(state.completion_count(), 2);
        assert_ne!(state.program(), &program);
    }
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
#[derive(Clone, Debug, PartialEq, Eq)]
enum ModelFlight {
    Manual,
    Auto { end_tick: u64 },
}
#[derive(Clone, Debug)]
struct Model {
    tick: u64,
    last_manual_position: u16,
    lever_out_of_sync: bool,
    flight: Option<ModelFlight>,
    rate: u32,
}
struct Reference;
impl ReferenceStateMachine for Reference {
    type State = Model;
    type Transition = Cmd;
    fn init_state() -> BoxedStrategy<Model> {
        Just(Model {
            tick: 0,
            last_manual_position: 0,
            lever_out_of_sync: false,
            flight: None,
            rate: 5,
        })
        .boxed()
    }
    fn transitions(_: &Model) -> BoxedStrategy<Cmd> {
        prop_oneof![
            Just(Cmd::Auto),
            Just(Cmd::Cut),
            Just(Cmd::Abort),
            any::<u16>().prop_map(Cmd::Tbar),
            Just(Cmd::RepeatTerminalTbar),
            Just(Cmd::Tbar(0)),
            Just(Cmd::Tbar(u16::MAX)),
            Just(Cmd::Tbar(64000)),
            Just(Cmd::Tbar(30000)),
            Just(Cmd::Tbar(31000)),
            (0u8..12).prop_map(Cmd::Advance),
            (any::<bool>(), any::<bool>()).prop_map(|(b, k)| Cmd::Arm(b, k)),
            (0u8..3).prop_map(Cmd::SetPreview),
            (2u8..12).prop_map(Cmd::SetTransition),
            (0u8..3).prop_map(Cmd::Punch)
        ]
        .boxed()
    }
    fn apply(mut state: Model, transition: &Cmd) -> Model {
        match transition {
            Cmd::Tbar(0) => {
                let parked = state.lever_out_of_sync;
                state.last_manual_position = 0;
                state.lever_out_of_sync = false;
                if !parked
                    || matches!(state.flight, Some(ModelFlight::Auto { end_tick }) if state.tick >= end_tick)
                {
                    state.flight = None;
                }
            }
            Cmd::Tbar(position) if !state.lever_out_of_sync => {
                state.last_manual_position = *position;
                state.flight = if *position == u16::MAX {
                    state.lever_out_of_sync = true;
                    None
                } else {
                    Some(ModelFlight::Manual)
                };
            }
            Cmd::RepeatTerminalTbar if !state.lever_out_of_sync => {
                state.last_manual_position = u16::MAX;
                state.lever_out_of_sync = true;
                state.flight = None;
            }
            Cmd::Cut | Cmd::Abort | Cmd::Punch(_) => {
                state.flight = None;
                state.lever_out_of_sync = state.last_manual_position != 0;
            }
            Cmd::Auto => {
                if let Some(ModelFlight::Manual) = state.flight {
                    let elapsed = (u64::from(state.last_manual_position) * u64::from(state.rate)
                        + u64::from(u16::MAX) / 2)
                        / u64::from(u16::MAX);
                    let remaining = u64::from(state.rate) - elapsed;
                    if remaining == 0 {
                        state.flight = None;
                        state.lever_out_of_sync = state.last_manual_position != 0;
                    } else {
                        state.flight = Some(ModelFlight::Auto {
                            end_tick: state.tick + remaining,
                        });
                    }
                } else if state.flight.is_none() {
                    state.flight = Some(ModelFlight::Auto {
                        end_tick: state.tick + u64::from(state.rate),
                    });
                }
            }
            Cmd::Advance(n) => {
                state.tick += u64::from(*n);
                if matches!(state.flight, Some(ModelFlight::Auto { end_tick }) if state.tick >= end_tick)
                {
                    state.flight = None;
                    state.lever_out_of_sync = state.last_manual_position != 0;
                }
            }
            Cmd::SetTransition(rate) if state.flight.is_none() => {
                state.rate = u32::from(*rate);
            }
            _ => {}
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
    lever_out_of_sync: bool,
    last_manual_position: u16,
}
impl Sut {
    fn check_projection(&mut self, transition: &Cmd, output: SwitcherProjection, parked: bool) {
        if parked {
            assert!(
                !output.started,
                "started while lever out of sync: {transition:?}"
            );
            assert_eq!(
                output.progress,
                Progress::ZERO,
                "parked lever moved: {transition:?}"
            );
            assert!(
                !output.started && !output.completed,
                "parked lever triggered take: {transition:?}"
            );
            assert_eq!(self.machine.is_in_flight(), self.was_in_flight);
        }
        assert!(output.progress.numerator() <= output.progress.denominator());
        if output.completed {
            assert!(
                self.was_in_flight || output.started,
                "a completion must close an active or newly started take: {transition:?}"
            );
            self.completions += 1;
            assert!(!self.machine.is_in_flight());
        }
        if !output.completed && !matches!(transition, Cmd::Punch(_)) {
            // A flip-flop is exclusively a completion effect.
            assert_eq!(
                (
                    self.machine.program().clone(),
                    self.machine.preview().clone()
                ),
                self.previous
            );
        }
        self.previous_progress = output.progress;
    }

    fn track_lever(&mut self, transition: &Cmd, has_output: bool, parked: bool) {
        if matches!(transition, Cmd::Tbar(0)) {
            self.last_manual_position = 0;
            self.lever_out_of_sync = false;
        } else if !parked {
            if let Cmd::Tbar(position) = transition {
                self.last_manual_position = *position;
            } else if matches!(transition, Cmd::RepeatTerminalTbar) && !self.lever_out_of_sync {
                self.last_manual_position = u16::MAX;
            }
            if has_output && !self.machine.is_in_flight() {
                self.lever_out_of_sync = self.last_manual_position != 0;
            }
        }
        if matches!(
            transition,
            Cmd::Advance(_) | Cmd::Auto | Cmd::Cut | Cmd::Abort | Cmd::Punch(_)
        ) && !self.machine.is_in_flight()
        {
            self.lever_out_of_sync = self.last_manual_position != 0;
        }
    }
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
            lever_out_of_sync: false,
            last_manual_position: 0,
        }
    }
    fn apply(mut sut: Sut, _model: &Model, transition: Cmd) -> Sut {
        let parked_manual =
            sut.lever_out_of_sync && matches!(transition, Cmd::Tbar(position) if position != 0);
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
            sut.check_projection(&transition, output, parked_manual);
        }
        sut.previous = (sut.machine.program().clone(), sut.machine.preview().clone());
        sut.was_in_flight = sut.machine.is_in_flight();
        sut.track_lever(&transition, result.is_some(), parked_manual);
        sut
    }
    fn check_invariants(sut: &Sut, model: &Model) {
        assert_eq!(sut.tick, model.tick);
        assert_eq!(sut.machine.is_in_flight(), model.flight.is_some());
        assert_eq!(sut.lever_out_of_sync, model.lever_out_of_sync);
        assert_eq!(sut.last_manual_position, model.last_manual_position);
        assert!(sut.previous_progress.numerator() <= sut.previous_progress.denominator());
        assert_eq!(sut.completions, sut.machine.completion_count());
    }
}
prop_state_machine! {
    #[test]
    fn arbitrary_switcher_verb_sequences(sequential 1..80 => Test);
}
