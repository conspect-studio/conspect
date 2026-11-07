use multiview_control::auth::{Principal, Role};
use multiview_control::event_automation::*;

struct Adapter {
    principal: Principal,
    ready: bool,
}
impl Adapter {
    fn new() -> Self {
        Self {
            principal: Principal {
                key_id: "authenticated-test".into(),
                role: Role::Admin,
                scoped_object_ids: None,
                scoped_output_ids: None,
                scoped_discovery_domains: None,
            },
            ready: true,
        }
    }
}
impl Authorization for Adapter {
    fn principal(&self) -> &Principal {
        &self.principal
    }
    fn resource_exists(&self, _: &Resource) -> bool {
        true
    }
    fn vt_ready(&self, _: &str) -> bool {
        self.ready
    }
}
fn rule() -> Rule {
    Rule {
        id: "rule".into(),
        source: "source".into(),
        action: Action::Route {
            scope: Scope::Output("out".into()),
            source: "replacement".into(),
        },
        allow_unpresented: false,
        max_lateness_ns: 10,
        trigger: BoundaryKind::Start,
    }
}
fn event() -> Boundary {
    Boundary {
        event: "event".into(),
        source: "source".into(),
        epoch: 1,
        boundary: BoundaryKind::Start,
        reliable: true,
        current: true,
        presented: true,
        effective_ns: 100,
        position_ns: 100,
        monotonic_ns: 100,
    }
}
#[test]
fn disarmed_dry_run_and_future_are_not_reservations() {
    let mut model = SafetyModel::new("source".into(), 2, 2, 1, 0).unwrap();
    let auth = Adapter::new();
    assert_eq!(
        model.reserve(&rule(), &event(), &auth),
        Err(Rejection::Disarmed)
    );
    model.set_mode(Mode::DryRun);
    assert!(matches!(
        model.reserve(&rule(), &event(), &auth),
        Ok(Decision::DryRun(_))
    ));
    model.set_mode(Mode::Armed);
    let mut future = event();
    future.effective_ns = 101;
    assert_eq!(
        model.reserve(&rule(), &future, &auth),
        Err(Rejection::NotDue)
    );
    assert!(matches!(
        model.reserve(&rule(), &event(), &auth),
        Ok(Decision::Reserved { .. })
    ));
}
#[test]
fn reject_vendor_stale_epoch_and_replay() {
    let mut m = SafetyModel::new("source".into(), 2, 2, 1, 90).unwrap();
    m.set_mode(Mode::Armed);
    let a = Adapter::new();
    let mut e = event();
    e.reliable = false;
    assert_eq!(m.reserve(&rule(), &e, &a), Err(Rejection::Unreliable));
    e = event();
    e.epoch = 0;
    assert_eq!(m.reserve(&rule(), &e, &a), Err(Rejection::Stale));
    e = event();
    e.effective_ns = 90;
    assert_eq!(m.reserve(&rule(), &e, &a), Err(Rejection::Replay));
    e = event();
    e.presented = false;
    assert_eq!(m.reserve(&rule(), &e, &a), Err(Rejection::NotPresented));
}
#[test]
fn duplicate_failure_and_capacity_fail_closed() {
    let mut m = SafetyModel::new("source".into(), 1, 1, 1, 0).unwrap();
    m.set_mode(Mode::Armed);
    let a = Adapter::new();
    let Decision::Reserved { id, .. } = m.reserve(&rule(), &event(), &a).unwrap() else {
        panic!()
    };
    m.acknowledge(id, Outcome::Accepted).unwrap();
    assert_eq!(m.outcome(id), Some(Outcome::Accepted));
    m.acknowledge(id, Outcome::Failed).unwrap();
    assert_eq!(m.reserve(&rule(), &event(), &a), Err(Rejection::Duplicate));
    let mut e = event();
    e.event = "second".into();
    assert_eq!(m.reserve(&rule(), &e, &a), Err(Rejection::Capacity));
    assert_eq!(
        m.acknowledge(id, Outcome::Succeeded),
        Err(Rejection::InvalidOutcome)
    );
}
#[test]
fn vt_readiness_and_authorization_are_not_acceptance() {
    let mut m = SafetyModel::new("source".into(), 4, 2, 1, 0).unwrap();
    m.set_mode(Mode::Armed);
    let mut a = Adapter::new();
    let mut r = rule();
    r.action = Action::Vt {
        player: "vt".into(),
        verb: VtVerb::Play,
    };
    a.ready = false;
    assert_eq!(m.reserve(&r, &event(), &a), Err(Rejection::NotReady));
    a.ready = true;
    a.principal.scoped_object_ids = Some(vec!["source".into()]);
    assert_eq!(m.reserve(&r, &event(), &a), Err(Rejection::Unauthorized));
    a.principal.scoped_object_ids = None;
    a.principal.scoped_output_ids = Some(vec!["other".into()]);
    assert_eq!(
        m.reserve(&rule(), &event(), &a),
        Err(Rejection::Unauthorized)
    );
}
fn replacement() -> Rule {
    let mut r = rule();
    r.action = Action::Replace {
        scope: Scope::Output("out".into()),
        video: Replacement::Bars,
        audio: AudioPolicy::Silent,
        expected_generation: 7,
        max_hold_ns: 50,
    };
    r
}
#[test]
fn lease_collision_manual_override_and_safe_restore() {
    let mut m = SafetyModel::new("source".into(), 8, 2, 1, 0).unwrap();
    m.set_mode(Mode::Armed);
    let a = Adapter::new();
    let Decision::Reserved { id, .. } = m.reserve(&replacement(), &event(), &a).unwrap() else {
        panic!()
    };
    let mut e = event();
    e.event = "second".into();
    assert_eq!(
        m.reserve(&replacement(), &e, &a),
        Err(Rejection::ScopeConflict)
    );
    m.acknowledge(id, Outcome::Accepted).unwrap();
    assert_eq!(m.restore(id, 8, &a), Err(Rejection::NotApplied));
    m.acknowledge(id, Outcome::Applied { generation: 8 })
        .unwrap();
    m.set_mode(Mode::Disarmed);
    assert_eq!(m.restore(id, 9, &a), Err(Rejection::GenerationConflict));
    assert_eq!(m.restore(id, 8, &a), Err(Rejection::GenerationConflict));
    let mut m = SafetyModel::new("source".into(), 8, 2, 1, 0).unwrap();
    m.set_mode(Mode::Armed);
    let Decision::Reserved { id, .. } = m.reserve(&replacement(), &event(), &a).unwrap() else {
        panic!()
    };
    m.acknowledge(id, Outcome::Applied { generation: 8 })
        .unwrap();
    m.set_mode(Mode::Disarmed);
    let plan = m.restore(id, 8, &a).unwrap();
    assert!(matches!(
        plan.action,
        Action::Restore {
            lease: _,
            expected_generation: 8,
            ..
        }
    ));
    m.finish_restore(id, true).unwrap();
    assert_eq!(m.restore(id, 8, &a), Err(Rejection::NotApplied));
}
#[test]
fn recovery_epoch_and_expiry_do_not_replay() {
    let a = Adapter::new();
    let mut m = SafetyModel::new("source".into(), 8, 2, 1, 0).unwrap();
    m.set_mode(Mode::Armed);
    let Decision::Reserved { id, .. } = m.reserve(&replacement(), &event(), &a).unwrap() else {
        panic!()
    };
    assert_eq!(m.expired_leases(149), Vec::<u64>::new());
    assert_eq!(m.expired_leases(150), vec![id]);
    m.acknowledge(id, Outcome::Applied { generation: 8 })
        .unwrap();
    m.restore(id, 8, &a).unwrap();
    m.finish_restore(id, false).unwrap();
    assert_eq!(m.restore(id, 8, &a), Err(Rejection::GenerationConflict));
    m.advance_epoch(2, 100).unwrap();
    assert_eq!(m.reserve(&rule(), &event(), &a), Err(Rejection::Disarmed));
    m.set_mode(Mode::Armed);
    assert_eq!(m.reserve(&rule(), &event(), &a), Err(Rejection::Stale));
    let mut e = event();
    e.epoch = 2;
    assert_eq!(m.reserve(&rule(), &e, &a), Err(Rejection::Replay));
    let mut restarted = SafetyModel::new("source".into(), 8, 2, 2, 100).unwrap();
    restarted.set_mode(Mode::Armed);
    assert_eq!(restarted.reserve(&rule(), &e, &a), Err(Rejection::Replay));
}
#[test]
fn source_scope_late_lifecycle_and_boundary_fail_closed() {
    let mut m = SafetyModel::new("source".into(), 8, 2, 1, 0).unwrap();
    m.set_mode(Mode::Armed);
    let mut a = Adapter::new();
    a.principal.scoped_object_ids = Some(vec!["rule".into(), "out".into(), "replacement".into()]);
    assert_eq!(
        m.reserve(&rule(), &event(), &a),
        Err(Rejection::Unauthorized)
    );
    a.principal.scoped_object_ids = None;
    let mut e = event();
    e.current = false;
    assert_eq!(m.reserve(&rule(), &e, &a), Err(Rejection::Stale));
    e = event();
    e.boundary = BoundaryKind::End;
    assert_eq!(m.reserve(&rule(), &e, &a), Err(Rejection::Invalid));
    e = event();
    e.position_ns = 111;
    assert_eq!(m.reserve(&rule(), &e, &a), Err(Rejection::Late));
}
#[test]
fn monotonic_watchdog_survives_frozen_media_and_epoch_change() {
    let a = Adapter::new();
    let mut m = SafetyModel::new("source".into(), 8, 2, 1, 0).unwrap();
    m.set_mode(Mode::Armed);
    let mut e = event();
    e.monotonic_ns = 10_000;
    let Decision::Reserved { id, plan } = m.reserve(&replacement(), &e, &a).unwrap() else {
        panic!()
    };
    assert_eq!(plan.evidence, e);
    m.acknowledge(id, Outcome::Applied { generation: 8 })
        .unwrap();
    m.advance_epoch(2, 0).unwrap(); // disarms, source clock may now be frozen/reset
    assert_eq!(m.expired_leases(10_049), Vec::<u64>::new());
    assert_eq!(m.expired_leases(10_050), vec![id]);
    assert!(m.restore(id, 8, &a).is_ok());
}
#[test]
fn source_models_have_independent_epoch_fences_and_reject_other_sources() {
    let a = Adapter::new();
    let mut m = SafetyModel::new("source".into(), 8, 2, 1, 0).unwrap();
    m.set_mode(Mode::Armed);
    let mut other = SafetyModel::new("other".into(), 8, 2, 99, 0).unwrap();
    other.set_mode(Mode::Armed);
    let mut r = rule();
    r.source = "other".into();
    let mut e = event();
    e.source = "other".into();
    e.epoch = 99;
    assert_eq!(m.reserve(&r, &e, &a), Err(Rejection::Invalid));
    assert!(other.reserve(&r, &e, &a).is_ok());
    assert!(m.reserve(&rule(), &event(), &a).is_ok());
}
#[test]
fn manual_generation_revocation_frees_scope_but_retains_replay_tombstone() {
    let a = Adapter::new();
    let mut m = SafetyModel::new("source".into(), 8, 1, 1, 0).unwrap();
    m.set_mode(Mode::Armed);
    let Decision::Reserved { id, .. } = m.reserve(&replacement(), &event(), &a).unwrap() else {
        panic!()
    };
    m.acknowledge(id, Outcome::Applied { generation: 8 })
        .unwrap();
    assert_eq!(m.restore(id, 9, &a), Err(Rejection::GenerationConflict));
    assert_eq!(
        m.reserve(&replacement(), &event(), &a),
        Err(Rejection::Duplicate)
    );
    let mut e = event();
    e.event = "new".into();
    assert!(m.reserve(&replacement(), &e, &a).is_ok());
    assert_eq!(m.restore(id, 8, &a), Err(Rejection::GenerationConflict));
}
#[test]
fn bounds_and_tone_are_validated() {
    assert!(SafetyModel::new("source".into(), 0, 1, 1, 0).is_err());
    let mut m = SafetyModel::new("source".into(), 8, 2, 1, 0).unwrap();
    m.set_mode(Mode::Armed);
    let a = Adapter::new();
    let mut r = replacement();
    if let Action::Replace { audio, .. } = &mut r.action {
        *audio = AudioPolicy::Tone {
            channel_mask: 0,
            level_millidbfs: -20000,
        };
    }
    assert_eq!(m.reserve(&r, &event(), &a), Err(Rejection::Invalid));
    r = rule();
    r.id = "x".repeat(257);
    assert_eq!(m.reserve(&r, &event(), &a), Err(Rejection::Invalid));
}
