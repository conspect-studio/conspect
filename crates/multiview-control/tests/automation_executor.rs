//! ADR-0183 automation executor state machine, driven by a fake break
//! provider and a fake command sink (no engine, no clock thread).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
use multiview_config::schema::{BreakActionKind, BreakStartAction, EventAutomationRule};
use multiview_control::automation::*;
use multiview_control::command::{Command, OperationId, RoutingLedger, SubmitError};
use multiview_control::source_events::{ActiveBreak, BreakStatus, SourceBreakProvider};
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct FakeBreaks {
    status: Mutex<Option<BreakStatus>>,
}
impl FakeBreaks {
    fn set(&self, active: Option<&str>) {
        *self.status.lock().unwrap() = Some(BreakStatus {
            epoch: 1,
            active: active.map(|id| ActiveBreak {
                event_id: id.into(),
                test: id.starts_with("test:"),
            }),
            degraded: false,
        });
    }
}
impl SourceBreakProvider for FakeBreaks {
    fn break_status(&self, id: &str) -> Option<BreakStatus> {
        (id == "news").then(|| self.status.lock().unwrap().clone()).flatten()
    }
}
#[derive(Default)]
struct FakeSink {
    ledger: RoutingLedger,
    sent: Mutex<Vec<Command>>,
    full: Mutex<bool>,
}
impl CommandSink for FakeSink {
    fn submit(&self, command: Command) -> Result<(), SubmitError> {
        if *self.full.lock().unwrap() {
            return Err(SubmitError::Full);
        }
        self.ledger.observe(&command);
        self.sent.lock().unwrap().push(command);
        Ok(())
    }
    fn ledger(&self) -> &RoutingLedger {
        &self.ledger
    }
}
impl FakeSink {
    fn swaps(&self) -> Vec<(String, String)> {
        self.sent
            .lock()
            .unwrap()
            .iter()
            .filter_map(|c| match c {
                Command::SwapSource { tile, source, .. } => Some((tile.clone(), source.clone())),
                _ => None,
            })
            .collect()
    }
    fn audio(&self) -> Vec<(String, String)> {
        self.sent
            .lock()
            .unwrap()
            .iter()
            .filter_map(|c| match c {
                Command::RouteAudio { target, source, .. } => {
                    Some((target.clone(), source.input_id.clone()))
                }
                _ => None,
            })
            .collect()
    }
}
struct FakeConfig(Mutex<ConfigView>);
impl ConfigProvider for FakeConfig {
    fn view(&self) -> ConfigView {
        self.0.lock().unwrap().clone()
    }
}
fn rule(armed: bool, target: &str) -> EventAutomationRule {
    EventAutomationRule {
        armed,
        on_break_start: BreakStartAction {
            action: BreakActionKind::SwapCell,
            target: target.into(),
            cells: None,
        },
        restore_on_end: true,
        max_duration_s: 10,
        webhook_url: Some("http://[2001:db8::1]/hook".into()),
    }
}
fn view(rule: Option<EventAutomationRule>) -> ConfigView {
    ConfigView {
        sources: vec![
            SourceView {
                id: "news".into(),
                bars: false,
                rule,
            },
            SourceView {
                id: "lineup".into(),
                bars: true,
                rule: None,
            },
            SourceView {
                id: "cam".into(),
                bars: false,
                rule: None,
            },
        ],
        cells: vec![
            ("c1".into(), Some("news".into())),
            ("c2".into(), Some("cam".into())),
            ("c3".into(), Some("news".into())),
        ],
    }
}
struct Rig {
    exec: AutomationExecutor,
    breaks: Arc<FakeBreaks>,
    sink: Arc<FakeSink>,
    config: Arc<FakeConfig>,
    hooks: Arc<Mutex<Vec<(String, serde_json::Value)>>>,
    audits: Arc<Mutex<Vec<String>>>,
}
fn rig(rule: Option<EventAutomationRule>) -> Rig {
    let breaks = Arc::new(FakeBreaks::default());
    breaks.set(None);
    let sink = Arc::new(FakeSink::default());
    let config = Arc::new(FakeConfig(Mutex::new(view(rule))));
    let hooks = Arc::new(Mutex::new(Vec::new()));
    let audits = Arc::new(Mutex::new(Vec::new()));
    let h = Arc::clone(&hooks);
    let a = Arc::clone(&audits);
    let exec = AutomationExecutor::new(
        breaks.clone(),
        sink.clone(),
        config.clone(),
        Arc::new(move |_id, detail| {
            a.lock()
                .unwrap()
                .push(detail["action"].as_str().unwrap_or_default().to_owned());
        }),
        Some(Arc::new(move |url, payload| {
            h.lock().unwrap().push((url.to_owned(), payload));
        })),
    );
    Rig {
        exec,
        breaks,
        sink,
        config,
        hooks,
        audits,
    }
}
const S: u64 = 1_000_000_000;

#[test]
fn disarmed_rule_never_acts() {
    let r = rig(Some(rule(false, "bars")));
    r.exec.tick(S);
    r.breaks.set(Some("seg:1:0"));
    r.exec.tick(2 * S);
    assert!(r.sink.swaps().is_empty());
    assert_eq!(r.exec.status("news").0.phase, Phase::Disarmed);
}

#[test]
fn arm_start_take_end_restore() {
    let r = rig(Some(rule(true, "bars")));
    r.exec.tick(S);
    assert_eq!(r.exec.status("news").0.phase, Phase::Idle);
    r.breaks.set(Some("seg:1:0"));
    r.exec.tick(2 * S);
    assert_eq!(
        r.sink.swaps(),
        vec![("c1".into(), "lineup".into()), ("c3".into(), "lineup".into())]
    );
    let (status, log) = r.exec.status("news");
    assert_eq!(status.phase, Phase::InBreak);
    assert_eq!(status.replaced.len(), 2);
    assert_eq!(status.replaced[0].original, "news");
    assert!(status.since_unix_ms.is_some());
    assert!(log.iter().any(|e| e.action == "take"));
    // Same occurrence again: no duplicate take.
    r.exec.tick(3 * S);
    assert_eq!(r.sink.swaps().len(), 2);
    r.breaks.set(None);
    r.exec.tick(4 * S);
    assert_eq!(
        r.sink.swaps()[2..],
        [("c1".into(), "news".into()), ("c3".into(), "news".into())]
    );
    assert_eq!(r.exec.status("news").0.phase, Phase::Idle);
    let hooks = r.hooks.lock().unwrap();
    assert_eq!(hooks.len(), 2);
    assert_eq!(hooks[0].1["type"], "break_start");
    assert_eq!(hooks[1].1["type"], "break_end");
    let audits = r.audits.lock().unwrap();
    assert!(audits.contains(&"take".to_owned()) && audits.contains(&"restore".to_owned()));
}

#[test]
fn manual_override_during_break_cancels_auto_restore() {
    let r = rig(Some(rule(true, "cam")));
    r.exec.tick(S);
    r.breaks.set(Some("seg:1:0"));
    r.exec.tick(2 * S);
    // Operator swaps c1 by hand during the break (the shared ledger sees it).
    r.sink
        .submit(Command::SwapSource {
            op: OperationId::new(),
            tile: "c1".into(),
            source: "lineup".into(),
        })
        .unwrap();
    r.exec.tick(3 * S);
    assert_eq!(r.exec.status("news").0.phase, Phase::ManualOverride);
    let before = r.sink.swaps().len();
    r.breaks.set(None);
    r.exec.tick(4 * S);
    assert_eq!(r.sink.swaps().len(), before, "never overwrite a manual change");
    assert_eq!(r.exec.status("news").0.phase, Phase::Idle);
    assert_eq!(
        r.exec.manual_restore("news"),
        Err(ManualRestoreError::NothingToRestore)
    );
}

#[test]
fn watchdog_restores_after_max_duration() {
    let r = rig(Some(rule(true, "bars")));
    r.exec.tick(S);
    r.breaks.set(Some("seg:1:0"));
    r.exec.tick(2 * S);
    r.exec.tick(11 * S);
    assert_eq!(r.exec.status("news").0.phase, Phase::InBreak);
    r.exec.tick(12 * S);
    assert_eq!(r.exec.status("news").0.phase, Phase::Idle);
    assert_eq!(r.sink.swaps().len(), 4);
    // The break is still signalled: the same occurrence never re-takes.
    r.exec.tick(13 * S);
    assert_eq!(r.sink.swaps().len(), 4);
}

#[test]
fn disarm_during_break_restores() {
    let r = rig(Some(rule(true, "bars")));
    r.exec.tick(S);
    r.breaks.set(Some("seg:1:0"));
    r.exec.tick(2 * S);
    *r.config.0.lock().unwrap() = view(Some(rule(false, "bars")));
    r.exec.tick(3 * S);
    assert_eq!(r.sink.swaps().len(), 4);
    assert_eq!(r.exec.status("news").0.phase, Phase::Disarmed);
}

#[test]
fn rule_deletion_during_break_restores() {
    let r = rig(Some(rule(true, "bars")));
    r.exec.tick(S);
    r.breaks.set(Some("seg:1:0"));
    r.exec.tick(2 * S);
    *r.config.0.lock().unwrap() = view(None);
    r.exec.tick(3 * S);
    assert_eq!(r.sink.swaps()[2..].len(), 2, "F1: deleted rule still restores");
}

#[test]
fn manual_restore_now_and_bars_tone_audio() {
    let r = rig(Some(rule(true, TARGET_BARS_TONE)));
    r.exec.tick(S);
    r.breaks.set(Some("test:break:1"));
    r.exec.tick(2 * S);
    assert_eq!(r.sink.audio(), vec![("news".into(), "lineup".into())]);
    let status = r.exec.status("news").0;
    assert!(status.test);
    assert_eq!(status.audio_replaced.as_deref(), Some("lineup"));
    r.exec.manual_restore("news").unwrap();
    assert_eq!(r.sink.audio()[1], ("news".into(), "news".into()));
    assert_eq!(r.exec.status("news").0.phase, Phase::Idle);
}

#[test]
fn busy_bus_restore_retries_then_finishes() {
    let r = rig(Some(rule(true, "bars")));
    r.exec.tick(S);
    r.breaks.set(Some("seg:1:0"));
    r.exec.tick(2 * S);
    *r.sink.full.lock().unwrap() = true;
    r.breaks.set(None);
    r.exec.tick(3 * S);
    assert_eq!(r.exec.status("news").0.phase, Phase::Restoring);
    *r.sink.full.lock().unwrap() = false;
    r.exec.tick(4 * S);
    assert_eq!(r.exec.status("news").0.phase, Phase::Idle);
    assert_eq!(r.sink.swaps().len(), 4);
}

#[test]
fn missing_bars_source_is_refused_and_logged() {
    let r = rig(Some(rule(true, "bars")));
    r.config.0.lock().unwrap().sources[1].bars = false;
    r.exec.tick(S);
    r.breaks.set(Some("seg:1:0"));
    r.exec.tick(2 * S);
    assert!(r.sink.swaps().is_empty());
    let status = r.exec.status("news").0;
    assert!(status.last_error.unwrap().contains("bars"));
}

#[test]
fn degraded_evidence_never_starts_a_take() {
    let r = rig(Some(rule(true, "bars")));
    r.exec.tick(S);
    *r.breaks.status.lock().unwrap() = Some(BreakStatus {
        epoch: 1,
        active: Some(ActiveBreak {
            event_id: "seg:1:0".into(),
            test: false,
        }),
        degraded: true,
    });
    r.exec.tick(2 * S);
    assert!(r.sink.swaps().is_empty());
}
