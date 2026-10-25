//! State-machine checks for the pure M/E switching value machine.
use multiview_core::switcher::{ArmedSet, FrameCount, Progress, SourceRef, TransitionDescriptor};
use multiview_core::time::MediaTime;
use multiview_engine::clock::Tick;
use multiview_engine::switcher::{SwitcherState, SwitcherError};
use proptest::prelude::*;
use proptest_state_machine::{prop_state_machine, ReferenceStateMachine, StateMachineTest};

fn tick(index: u64) -> Tick {
    Tick { index, pts: MediaTime::from_nanos(0) }
}
fn source(id: &str) -> SourceRef { SourceRef::new(id) }
fn configured() -> SwitcherState {
    SwitcherState::new(source("a"), source("b"), vec![false, true])
}

#[test]
fn auto_uses_only_tick_index_and_completes_once() {
    let mut state = configured();
    state.set_transition(TransitionDescriptor::Mix { rate: FrameCount::try_from(4).unwrap() }).unwrap();
    assert!(state.auto(tick(10)).unwrap().started);
    assert_eq!(state.sample(tick(12)).progress, Progress::new(2, FrameCount::try_from(4).unwrap()));
    assert!(state.sample(tick(14)).completed);
    assert!(!state.sample(tick(15)).completed);
    assert_eq!(state.program(), &source("b"));
    assert_eq!(state.preview(), &source("a"));
}

#[test]
fn manual_override_abort_and_midflight_cut() {
    let mut state = configured();
    state.set_transition(TransitionDescriptor::Mix { rate: FrameCount::try_from(8).unwrap() }).unwrap();
    state.auto(tick(1)).unwrap();
    assert_eq!(state.tbar(u16::MAX, tick(2)).unwrap().progress, Progress::COMPLETE);
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
    assert_eq!(state.arm(ArmedSet::default()), Err(SwitcherError::NothingArmed));
}

#[derive(Clone, Debug)]
enum Cmd { Auto, Cut, Abort, Tbar(u16), Advance(u8), Arm(bool, bool), Punch(u8) }
#[derive(Clone, Debug)]
struct Model { tick: u64, completions: u64, pgm: SourceRef, pvw: SourceRef, last_progress: Progress, in_flight: bool }
struct Reference;
impl ReferenceStateMachine for Reference {
    type State = Model;
    type Transition = Cmd;
    fn init_state() -> BoxedStrategy<Model> {
        Just(Model { tick: 0, completions: 0, pgm: source("a"), pvw: source("b"), last_progress: Progress::ZERO, in_flight: false }).boxed()
    }
    fn transitions(_: &Model) -> BoxedStrategy<Cmd> {
        prop_oneof![Just(Cmd::Auto), Just(Cmd::Cut), Just(Cmd::Abort), any::<u16>().prop_map(Cmd::Tbar), (0u8..12).prop_map(Cmd::Advance), (any::<bool>(),any::<bool>()).prop_map(|(b,k)| Cmd::Arm(b,k)), (0u8..3).prop_map(Cmd::Punch)].boxed()
    }
    fn apply(mut state: Model, transition: &Cmd) -> Model {
        // The reference tracks *observed* completions and the last output projection;
        // its independent invariant is that a bus swap cannot occur without a take.
        if let Cmd::Advance(n) = transition { state.tick += u64::from(*n); }
        state
    }
}
struct Sut { machine: SwitcherState, tick: u64, completions: u64, previous: (SourceRef, SourceRef), previous_progress: Progress, was_in_flight: bool }
struct Test;
impl StateMachineTest for Test {
    type SystemUnderTest = Sut;
    type Reference = Reference;
    fn init_test(_: &Model) -> Sut {
        let mut machine = configured();
        machine.set_transition(TransitionDescriptor::Mix { rate: FrameCount::try_from(5).unwrap() }).unwrap();
        Sut { machine, tick: 0, completions: 0, previous: (source("a"),source("b")), previous_progress: Progress::ZERO, was_in_flight: false }
    }
    fn apply(mut sut: Sut, _: &Model, transition: Cmd) -> Sut {
        let result = match transition {
            Cmd::Auto => sut.machine.auto(tick(sut.tick)).ok(),
            Cmd::Cut => sut.machine.cut().ok(),
            Cmd::Abort => Some(sut.machine.abort()),
            Cmd::Tbar(position) => sut.machine.tbar(position, tick(sut.tick)).ok(),
            Cmd::Advance(n) => { sut.tick += u64::from(n); Some(sut.machine.sample(tick(sut.tick))) },
            Cmd::Arm(background,key) => { let mut set = ArmedSet::default(); if background { set = set.with_background(); } if key { set = set.with_key(1); } let _ = sut.machine.arm(set); None },
            Cmd::Punch(n) => { sut.machine.punch(source(&format!("p{n}"))); None },
        };
        if let Some(output) = result {
            assert!(output.progress.numerator() <= output.progress.denominator());
            if output.completed { sut.completions += 1; assert!(!sut.machine.is_in_flight()); }
            if !output.completed && !matches!(transition, Cmd::Punch(_)) {
                // A flip-flop is exclusively a completion effect.
                assert_eq!((sut.machine.program().clone(),sut.machine.preview().clone()), sut.previous);
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
