//! End-to-end (ADR-0183): operator test injection → real source-event service
//! → `RuntimeReader` break status → automation executor → command sink.
use crate::source_event_runtime::Runtime;
use multiview_config::schema::{BreakActionKind, BreakStartAction, EventAutomationRule};
use multiview_control::automation::{
    AutomationExecutor, CommandSink, ConfigProvider, ConfigView, Phase, SourceView,
};
use multiview_control::command::{Command, RoutingLedger, SubmitError};
use multiview_control::source_events::{
    SourceEventTestInjector, TestEventBoundary, TestEventOutcome,
};
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct Sink {
    ledger: RoutingLedger,
    swaps: Mutex<Vec<(String, String)>>,
}
impl CommandSink for Sink {
    fn submit(&self, command: Command) -> Result<(), SubmitError> {
        self.ledger.observe(&command);
        if let Command::SwapSource { tile, source, .. } = command {
            self.swaps.lock().unwrap().push((tile, source));
        }
        Ok(())
    }
    fn ledger(&self) -> &RoutingLedger {
        &self.ledger
    }
}
struct Config;
impl ConfigProvider for Config {
    fn view(&self) -> ConfigView {
        ConfigView {
            sources: vec![
                SourceView {
                    id: "news".into(),
                    bars: false,
                    rule: Some(EventAutomationRule {
                        armed: true,
                        on_break_start: BreakStartAction {
                            action: BreakActionKind::SwapCell,
                            target: "bars".into(),
                            cells: None,
                        },
                        restore_on_end: true,
                        max_duration_s: 600,
                        webhook_url: None,
                    }),
                },
                SourceView {
                    id: "lineup".into(),
                    bars: true,
                    rule: None,
                },
            ],
            cells: vec![("main".into(), Some("news".into()))],
        }
    }
}

fn until(exec: &AutomationExecutor, phase: Phase) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut now = 1_000_000_000u64;
    while exec.status("news").0.phase != phase {
        assert!(std::time::Instant::now() < deadline, "never reached {phase:?}");
        now += 250_000_000;
        exec.tick(now);
        std::thread::yield_now();
    }
}

#[test]
fn test_injection_drives_take_and_restore_end_to_end() {
    let runtime = Runtime::new().unwrap();
    let reader = runtime.reader();
    let lease = runtime.activate("news", 1).unwrap();
    let (_context, _producer) = lease.connect().unwrap().register(1).unwrap();
    let sink = Arc::new(Sink::default());
    let exec = AutomationExecutor::new(
        Arc::new(reader.clone()),
        sink.clone(),
        Arc::new(Config),
        Arc::new(|_, _| {}),
        None,
    );
    until(&exec, Phase::Idle);
    for round in 1..=2 {
        assert_eq!(
            reader.inject_test_event(
                "news",
                TestEventBoundary::Start {
                    duration_ns: Some(30_000_000_000)
                }
            ),
            TestEventOutcome::Queued
        );
        until(&exec, Phase::InBreak);
        let status = exec.status("news").0;
        assert!(status.test);
        assert_eq!(
            status.event_id.as_deref(),
            Some(format!("test:break:{round}").as_str())
        );
        assert_eq!(
            reader.inject_test_event("news", TestEventBoundary::End),
            TestEventOutcome::Queued
        );
        until(&exec, Phase::Idle);
    }
    assert_eq!(
        *sink.swaps.lock().unwrap(),
        vec![
            ("main".to_owned(), "lineup".to_owned()),
            ("main".to_owned(), "news".to_owned()),
            ("main".to_owned(), "lineup".to_owned()),
            ("main".to_owned(), "news".to_owned()),
        ]
    );
    runtime.shutdown().unwrap();
}
