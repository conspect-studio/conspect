//! Scheduler state-machine tests: injected unix-second clock, fake backend.
use super::*;
use std::collections::HashMap;

/// (player, load key, job, artifact, then).
type LoadCall = (String, String, u64, String, LoadThen);

#[derive(Default)]
struct Fake {
    /// Every submit: (principal key id, idempotency key, fill id).
    submits: Mutex<Vec<(String, String, String)>>,
    /// Idempotency key -> job id (replay returns the same job).
    keys: Mutex<HashMap<String, u64>>,
    jobs: Mutex<HashMap<u64, JobPoll>>,
    /// Every load: (player, load key, job, artifact, then).
    loads: Mutex<Vec<LoadCall>>,
    load_result: Mutex<Option<LoadPoll>>,
    ops: Mutex<HashMap<u64, LoadPoll>>,
    submit_error: Mutex<Option<String>>,
}
impl Fake {
    fn set_job(&self, job: u64, state: JobPoll) {
        self.jobs.lock().unwrap().insert(job, state);
    }
    fn submits(&self) -> Vec<(String, String, String)> {
        self.submits.lock().unwrap().clone()
    }
    fn loads(&self) -> Vec<LoadCall> {
        self.loads.lock().unwrap().clone()
    }
}
impl RefreshBackend for Fake {
    fn submit(&self, p: &Principal, key: &str, body: &CaptureBody) -> ControlResult<u64> {
        if let Some(e) = self.submit_error.lock().unwrap().clone() {
            return Err(ControlError::Conflict(e));
        }
        self.submits
            .lock()
            .unwrap()
            .push((p.key_id.clone(), key.to_owned(), body.fill_id.clone()));
        let mut keys = self.keys.lock().unwrap();
        let next = u64::try_from(keys.len()).unwrap() + 1;
        let job = *keys.entry(key.to_owned()).or_insert(next);
        self.jobs
            .lock()
            .unwrap()
            .entry(job)
            .or_insert(JobPoll::Pending);
        Ok(job)
    }
    fn job(&self, _: &Principal, job: u64) -> ControlResult<JobPoll> {
        self.jobs
            .lock()
            .unwrap()
            .get(&job)
            .cloned()
            .ok_or(ControlError::EngineBusy)
    }
    fn load(
        &self,
        _: &Principal,
        player: &str,
        key: &str,
        job: u64,
        artifact: &str,
        then: LoadThen,
    ) -> ControlResult<(u64, LoadPoll)> {
        self.loads.lock().unwrap().push((
            player.to_owned(),
            key.to_owned(),
            job,
            artifact.to_owned(),
            then,
        ));
        let state = self
            .load_result
            .lock()
            .unwrap()
            .clone()
            .unwrap_or(LoadPoll::Pending);
        self.ops.lock().unwrap().insert(job, state.clone());
        Ok((job, state))
    }
    fn load_state(&self, _: &Principal, _: &str, op: u64) -> ControlResult<LoadPoll> {
        self.ops
            .lock()
            .unwrap()
            .get(&op)
            .cloned()
            .ok_or(ControlError::EngineBusy)
    }
}

fn body(fill: &str, player: &str) -> CaptureBody {
    serde_json::from_value(serde_json::json!({
        "fill_id": fill, "target_player": player,
        "view": {"lat":-33.9,"lon":150.4,"zoom":8,"overlay":"radar","width":1280,"height":720,"fps":10,"frames":60,"public":true},
        "terms": {"accepted": true, "version": WINDY_TERMS_VERSION}
    }))
    .unwrap()
}
fn spec(fill: &str, player: &str, minutes: u32) -> FillSpec {
    FillSpec {
        fill_id: fill.to_owned(),
        player: player.to_owned(),
        minutes,
        then: LoadThen::Resume,
        body: body(fill, player),
        owner: FillOwner::Declared,
        source: FillSource::Config,
    }
}
fn no_keys() -> PrincipalResolver {
    Arc::new(|_| None)
}
fn entry<'a>(s: &'a Scheduler, fill: &str) -> &'a Entry {
    &s.entries[fill]
}
const T0: u64 = 1_800_000_000;

#[test]
fn due_times_sit_on_a_stable_staggered_slot_grid() {
    let mut s = Scheduler::default();
    s.upsert(spec("radar", "vt-1", 5), T0, FirstDue::CurrentSlot)
        .unwrap();
    let due = entry(&s, "radar").next_due;
    assert!(
        due <= T0 && T0 - due < 300,
        "current slot starts at or before now"
    );
    assert_eq!(
        due % 300,
        offset_secs("radar", 300),
        "stable per-fill stagger"
    );
    let mut next = Scheduler::default();
    next.upsert(spec("radar", "vt-1", 5), T0, FirstDue::NextSlot)
        .unwrap();
    assert_eq!(entry(&next, "radar").next_due, due + 300);
    // The same slot is computed from anywhere inside it.
    assert_eq!(slot_at("radar", 5, due).0, slot_at("radar", 5, due + 299).0);
    assert_eq!(
        slot_at("radar", 5, due).0 + 1,
        slot_at("radar", 5, due + 300).0
    );
}

#[test]
fn upsert_rejects_out_of_range_intervals_and_foreign_bodies() {
    let mut s = Scheduler::default();
    assert!(s
        .upsert(spec("a", "vt-1", 0), T0, FirstDue::CurrentSlot)
        .is_err());
    assert!(s
        .upsert(spec("a", "vt-1", 1441), T0, FirstDue::CurrentSlot)
        .is_err());
    let mut wrong = spec("a", "vt-1", 5);
    wrong.body = body("other", "vt-1");
    assert!(s.upsert(wrong, T0, FirstDue::CurrentSlot).is_err());
    let mut wrong_player = spec("a", "vt-1", 5);
    wrong_player.body = body("a", "vt-2");
    assert!(s.upsert(wrong_player, T0, FirstDue::CurrentSlot).is_err());
}

#[test]
fn success_loads_the_new_artifact_and_schedules_the_next_slot() {
    let fake = Fake::default();
    let mut s = Scheduler::default();
    s.upsert(spec("radar", "vt-1", 5), T0, FirstDue::CurrentSlot)
        .unwrap();
    let slot_start = entry(&s, "radar").next_due;
    s.tick(T0, &fake, &no_keys());
    assert_eq!(fake.submits().len(), 1);
    assert_eq!(fake.submits()[0].0, "media-fill:radar");
    assert_eq!(
        fake.loads(),
        Vec::<LoadCall>::new(),
        "nothing touches the player before capture succeeds"
    );
    fake.set_job(
        1,
        JobPoll::Succeeded {
            artifact: "art-1".into(),
        },
    );
    s.tick(T0 + 5, &fake, &no_keys());
    let loads = fake.loads();
    assert_eq!(loads.len(), 1);
    assert_eq!(loads[0].0, "vt-1");
    assert_eq!(loads[0].2, 1);
    assert_eq!(loads[0].3, "art-1");
    assert_eq!(loads[0].4, LoadThen::Resume);
    assert_eq!(
        entry(&s, "radar").phase,
        Phase::Loading {
            job: 1,
            artifact: "art-1".into(),
            op: 1,
            deadline: T0 + 5 + LOAD_DEADLINE_SECS
        }
    );
    assert!(
        entry(&s, "radar").last_artifact_id.is_none(),
        "success is the native ACK, not admission"
    );
    fake.ops.lock().unwrap().insert(1, LoadPoll::Completed);
    s.tick(T0 + 10, &fake, &no_keys());
    let e = entry(&s, "radar");
    assert_eq!(e.phase, Phase::Idle);
    assert_eq!(e.last_artifact_id.as_deref(), Some("art-1"));
    assert_eq!(e.last_success_at, Some(T0 + 10));
    // Next due is the slot after the one the run completed in (never in the past).
    assert_eq!(e.next_due, slot_at("radar", 5, T0 + 10).1 + 300);
    assert!(e.next_due > T0 + 10 && e.next_due >= slot_start + 300);
    let st = &s.status()[0];
    assert!(!st.in_flight);
    assert_eq!(st.last_artifact_id.as_deref(), Some("art-1"));
    assert!(st.last_error.is_none());
}

#[test]
fn single_flight_no_second_job_while_one_runs() {
    let fake = Fake::default();
    let mut s = Scheduler::default();
    s.upsert(spec("radar", "vt-1", 1), T0, FirstDue::CurrentSlot)
        .unwrap();
    s.tick(T0, &fake, &no_keys());
    // Several intervals pass while the job is still pending (inside the deadline).
    for dt in [60, 120, 180, 240] {
        s.tick(T0 + dt, &fake, &no_keys());
    }
    assert_eq!(
        fake.submits().len(),
        1,
        "a running fill is never started again"
    );
    assert!(s.status()[0].in_flight);
}

#[test]
fn failure_keeps_old_clip_records_error_and_backs_off_bounded() {
    let fake = Fake::default();
    let mut s = Scheduler::default();
    s.upsert(spec("radar", "vt-1", 2), T0, FirstDue::CurrentSlot)
        .unwrap();
    // First a good run establishes the "old clip".
    s.tick(T0, &fake, &no_keys());
    *fake.load_result.lock().unwrap() = Some(LoadPoll::Completed);
    fake.set_job(
        1,
        JobPoll::Succeeded {
            artifact: "old".into(),
        },
    );
    s.tick(T0 + 1, &fake, &no_keys());
    assert_eq!(entry(&s, "radar").last_artifact_id.as_deref(), Some("old"));
    let mut now = entry(&s, "radar").next_due;
    // Now every capture fails.
    let mut backoffs = Vec::new();
    for _ in 0..4 {
        s.tick(now, &fake, &no_keys());
        let job = entry(&s, "radar").last_job.unwrap();
        fake.set_job(job, JobPoll::Failed("capture failed".into()));
        now += 1;
        s.tick(now, &fake, &no_keys());
        let e = entry(&s, "radar");
        backoffs.push(e.next_due - now);
        now = e.next_due;
    }
    assert_eq!(fake.loads().len(), 1, "a failed run never loads anything");
    let e = entry(&s, "radar");
    assert_eq!(e.last_artifact_id.as_deref(), Some("old"), "old clip kept");
    assert_eq!(e.last_error.as_deref(), Some("capture failed"));
    assert_eq!(e.failures, 4);
    assert_eq!(
        backoffs,
        vec![30, 60, 120, 120],
        "exponential, capped at the interval"
    );
}

#[test]
fn submit_refusal_and_deadlines_fail_the_run_without_loading() {
    let fake = Fake::default();
    let mut s = Scheduler::default();
    s.upsert(spec("radar", "vt-1", 10), T0, FirstDue::CurrentSlot)
        .unwrap();
    *fake.submit_error.lock().unwrap() = Some("queue full".into());
    s.tick(T0, &fake, &no_keys());
    assert_eq!(entry(&s, "radar").phase, Phase::Idle);
    assert!(entry(&s, "radar")
        .last_error
        .as_deref()
        .unwrap()
        .contains("queue full"));
    *fake.submit_error.lock().unwrap() = None;
    let now = entry(&s, "radar").next_due;
    s.tick(now, &fake, &no_keys());
    s.tick(now + CAPTURE_DEADLINE_SECS, &fake, &no_keys());
    assert_eq!(
        entry(&s, "radar").last_error.as_deref(),
        Some("capture deadline exceeded")
    );
    assert_eq!(fake.loads(), Vec::<LoadCall>::new());
}

#[test]
fn restart_inside_a_slot_replays_the_same_capture_key() {
    let first = Fake::default();
    let mut s = Scheduler::default();
    s.upsert(spec("radar", "vt-1", 5), T0, FirstDue::CurrentSlot)
        .unwrap();
    s.tick(T0, &first, &no_keys());
    // "Restart": a fresh scheduler, at the last second of the same slot.
    let restart_at = slot_at("radar", 5, T0).1 + 299;
    assert!(restart_at >= T0);
    let mut restarted = Scheduler::default();
    restarted
        .upsert(spec("radar", "vt-1", 5), restart_at, FirstDue::CurrentSlot)
        .unwrap();
    restarted.tick(restart_at, &first, &no_keys());
    let submits = first.submits();
    assert_eq!(submits.len(), 2);
    assert_eq!(
        submits[0].1, submits[1].1,
        "same (fill, slot) => same idempotency key"
    );
    assert_eq!(
        entry(&restarted, "radar").last_job,
        entry(&s, "radar").last_job,
        "the ledger replays the same job"
    );
    // The next slot uses a different key; a changed body uses a different key.
    let sp = spec("radar", "vt-1", 5);
    let (slot, _) = slot_at("radar", 5, T0);
    assert_ne!(capture_key(&sp, slot), capture_key(&sp, slot + 1));
    let mut changed = sp.clone();
    changed.body.view.zoom = 9;
    assert_ne!(capture_key(&sp, slot), capture_key(&changed, slot));
    assert_eq!(load_key(&sp, 7), load_key(&sp, 7));
}

#[test]
fn global_concurrency_is_bounded() {
    let fake = Fake::default();
    let mut s = Scheduler::new(1);
    for i in 0..5 {
        s.upsert(
            spec(&format!("f{i}"), &format!("vt-{i}"), 1),
            T0,
            FirstDue::CurrentSlot,
        )
        .unwrap();
    }
    s.tick(T0 + 60, &fake, &no_keys());
    assert_eq!(fake.submits().len(), 1);
    assert_eq!(s.in_flight(), 1);
    s.tick(T0 + 65, &fake, &no_keys());
    assert_eq!(
        fake.submits().len(),
        1,
        "no second fill starts while the cap is used"
    );
    // Finish it; the next earliest-due fill starts on the following tick.
    *fake.load_result.lock().unwrap() = Some(LoadPoll::Completed);
    fake.set_job(
        1,
        JobPoll::Succeeded {
            artifact: "a".into(),
        },
    );
    s.tick(T0 + 70, &fake, &no_keys());
    assert_eq!(fake.submits().len(), 2);
    assert_eq!(s.in_flight(), 1);
    let mut wide = Scheduler::new(2);
    for i in 0..5 {
        wide.upsert(
            spec(&format!("f{i}"), &format!("vt-{i}"), 1),
            T0,
            FirstDue::CurrentSlot,
        )
        .unwrap();
    }
    let fake2 = Fake::default();
    wide.tick(T0 + 60, &fake2, &no_keys());
    assert_eq!(fake2.submits().len(), 2);
}

#[test]
fn fill_count_is_bounded() {
    let mut s = Scheduler::default();
    for i in 0..MAX_FILLS {
        s.upsert(
            spec(&format!("f{i}"), &format!("vt-{i}"), 5),
            T0,
            FirstDue::NextSlot,
        )
        .unwrap();
    }
    assert!(s
        .upsert(spec("extra", "vt-x", 5), T0, FirstDue::NextSlot)
        .is_err());
    // Replacing an existing fill is still allowed at capacity.
    assert!(s
        .upsert(spec("f0", "vt-0", 7), T0, FirstDue::NextSlot)
        .is_ok());
}

#[test]
fn key_owned_fill_runs_as_its_key_and_stops_when_revoked_or_rescoped() {
    let fake = Fake::default();
    let mut s = Scheduler::default();
    let mut sp = spec("radar", "vt-1", 5);
    sp.owner = FillOwner::Key("op".into());
    sp.source = FillSource::Api;
    s.upsert(sp, T0, FirstDue::CurrentSlot).unwrap();
    let scoped = Arc::new(Mutex::new(Some(vec![
        "radar".to_owned(),
        "vt-1".to_owned(),
    ])));
    let scope = Arc::clone(&scoped);
    let resolve: PrincipalResolver = Arc::new(move |key| {
        (key == "op").then(|| Principal {
            key_id: "op".into(),
            role: Role::Operator,
            scoped_object_ids: scope.lock().unwrap().clone(),
            scoped_output_ids: None,
            scoped_discovery_domains: None,
        })
    });
    s.tick(T0, &fake, &resolve);
    assert_eq!(
        fake.submits()[0].0,
        "op",
        "runs as the registering key, not escalated"
    );
    // Re-scope away from the player: the in-flight run fails closed, no load.
    *scoped.lock().unwrap() = Some(vec!["radar".to_owned()]);
    fake.set_job(
        1,
        JobPoll::Succeeded {
            artifact: "a".into(),
        },
    );
    s.tick(T0 + 5, &fake, &resolve);
    assert_eq!(fake.loads(), Vec::<LoadCall>::new());
    assert!(entry(&s, "radar")
        .last_error
        .as_deref()
        .unwrap()
        .contains("forbidden"));
    // A revoked key never submits.
    let none = no_keys();
    let now = entry(&s, "radar").next_due;
    s.tick(now, &fake, &none);
    assert_eq!(fake.submits().len(), 1);
}

#[test]
fn declared_principal_is_least_privilege() {
    let p = declared_principal("radar", "vt-1");
    assert_eq!(p.role, Role::Operator);
    assert!(authorize_object(&p, "radar").is_ok());
    assert!(authorize_object(&p, "vt-1").is_ok());
    assert!(authorize_object(&p, "vt-2").is_err());
    assert!(crate::auth::authorize_output(&p, "program").is_err());
}

#[test]
fn removed_fill_never_loads_its_in_flight_capture() {
    let fake = Fake::default();
    let mut s = Scheduler::default();
    s.upsert(spec("radar", "vt-1", 5), T0, FirstDue::CurrentSlot)
        .unwrap();
    s.tick(T0, &fake, &no_keys());
    assert!(s.remove("radar"));
    fake.set_job(
        1,
        JobPoll::Succeeded {
            artifact: "a".into(),
        },
    );
    s.tick(T0 + 5, &fake, &no_keys());
    assert_eq!(fake.loads(), Vec::<LoadCall>::new());
    assert!(s.status().is_empty());
}

#[test]
fn config_fill_maps_to_a_capture_body() {
    let toml = r#"
id = "syd-radar"
player = "vt-1"
kind = "web_capture"
template = "windy"
refresh = { mode = "interval", minutes = 5 }
[view]
width = 1920
height = 1080
[view.params]
overlay = "radar"
lat = -33.946
lon = 150.370
zoom = 10
[terms]
user = "operator@example.com"
accepted_at = "2026-11-07T00:00:00Z"
template = "windy"
terms_version = "2023-09-01"
"#;
    let fill: multiview_config::MediaFill = toml::from_str(toml).unwrap();
    let sp = spec_from_config(&fill).unwrap();
    assert_eq!(sp.minutes, 5);
    assert_eq!(sp.owner, FillOwner::Declared);
    assert_eq!(sp.body.fill_id, "syd-radar");
    assert_eq!(sp.body.target_player.as_deref(), Some("vt-1"));
    assert_eq!(sp.body.view.zoom, 10);
    assert_eq!(sp.body.view.width, 1920);
    assert_eq!(sp.body.view.capture, Some(CaptureMode::Steps));
    let mut one_shot = fill.clone();
    one_shot.refresh = multiview_config::FillRefresh::OneShot;
    assert!(spec_from_config(&one_shot).is_err());
}
