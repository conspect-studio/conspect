#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
use multiview_core::switcher::{FrameCount, SourceRef};
use multiview_core::time::MediaTime;
use multiview_engine::clock::Tick;
use multiview_engine::switcher::SwitcherState;
use multiview_engine::switcher_control::{
    SwitcherCommand, SwitcherControl, SwitcherVerb, COMMAND_CAPACITY, MAX_COMMANDS_PER_TICK,
};

fn tick(index: u64) -> Tick {
    Tick {
        index,
        pts: MediaTime::ZERO,
    }
}
fn source(id: &str) -> SourceRef {
    SourceRef::new(id)
}
fn command(op: usize, verb: SwitcherVerb) -> SwitcherCommand {
    SwitcherCommand {
        operation_id: op.to_string(),
        verb,
    }
}

#[test]
fn commands_apply_only_at_exact_tick_and_reject_overload_before_202() {
    let (handle, mut driver) =
        SwitcherControl::new(SwitcherState::new(source("a"), source("b"), vec![]));
    assert!(handle.snapshot().is_none());
    for i in 0..COMMAND_CAPACITY {
        handle.try_send(command(i, SwitcherVerb::Cut)).unwrap();
    }
    assert!(handle
        .try_send(command(COMMAND_CAPACITY, SwitcherVerb::Cut))
        .is_err());
    driver.drain_at_tick(tick(5));
    assert_eq!(handle.snapshot().unwrap().tick, 5);
    assert_eq!(
        handle.snapshot().unwrap().completion_count,
        MAX_COMMANDS_PER_TICK as u64
    );
    driver.drain_at_tick(tick(6));
    assert_eq!(
        handle.snapshot().unwrap().completion_count,
        (MAX_COMMANDS_PER_TICK * 2) as u64
    );
}

#[test]
fn rejected_command_has_correlated_outcome_without_bus_mutation() {
    let (handle, mut driver) =
        SwitcherControl::new(SwitcherState::new(source("a"), source("b"), vec![]));
    let mut outcomes = handle.outcomes();
    let rate = FrameCount::try_from(25).unwrap();
    handle
        .try_send(command(1, SwitcherVerb::Auto(rate)))
        .unwrap();
    driver.drain_at_tick(tick(0));
    handle
        .try_send(command(2, SwitcherVerb::SelectPreview(source("c"))))
        .unwrap();
    driver.drain_at_tick(tick(1));
    let first = outcomes.try_recv().unwrap();
    assert_eq!(first.event.operation_id, "1");
    assert_eq!(first.event.tick, 0);
    assert!(first.event.error.is_none());
    let second = outcomes.try_recv().unwrap();
    assert_eq!(second.event.operation_id, "2");
    assert_eq!(second.event.tick, 1);
    assert!(second.event.error.as_ref().unwrap().contains("active take"));
    assert_eq!(handle.snapshot().unwrap().preview.as_str(), "b");
    driver.drain_at_tick(tick(25));
    assert_eq!(handle.snapshot().unwrap().program.as_str(), "b");
}
