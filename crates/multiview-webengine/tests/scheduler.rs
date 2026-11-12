#![cfg(unix)]
//! Scheduler tests against the real queued service with fake producers.
//! A manual clock drives all timing; counts come from the producer's run log.
//! The only real-time use is a hang guard.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::many_single_char_names,
    clippy::unnecessary_wraps,
    clippy::unchecked_time_subtraction
)]
use multiview_webengine::{
    artifact::{ArtifactStore, Limits},
    capture::{Background, Capture, Tools},
    scheduler::{
        Config as SchedConfig, FillKey, Refresh, ScheduleError, Scheduler, TriggerError, View,
    },
    service::{Config, Request, Service, State, SubmitError, Submitted},
};
use std::{
    cell::RefCell,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::Mutex,
    time::{Duration, Instant},
};

static SERIAL: Mutex<()> = Mutex::new(());
const MIN: Duration = Duration::from_secs(60);

fn exe(path: &Path, body: &str) {
    std::fs::write(path, body).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}
struct Env {
    _dir: tempfile::TempDir,
    work: PathBuf,
    service: Service,
}
fn env(gated: bool, queue: usize) -> Env {
    let _g = SERIAL.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path().join("w");
    std::fs::create_dir(&work).unwrap();
    let gate = if gated {
        "while [ ! -f \"$W/release\" ]; do sleep 0.02; done\n"
    } else {
        ""
    };
    let node = work.join("node");
    exe(
        &node,
        &format!(
            "#!/bin/sh\nW='{}'\ncat >/dev/null\necho run >> \"$W/runs\"\n{gate}printf 'ftyp'\nprintf '{{\"event\":\"capture_complete\",\"frames\":5}}\\n' >&2\n",
            work.display()
        ),
    );
    let stub = work.join("stub");
    exe(&stub, "#!/bin/sh\n");
    let root = dir.path().join("s");
    std::fs::create_dir(&root).unwrap();
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
    let store = ArtifactStore::open(
        &root,
        Limits {
            bytes: 100_000_000,
            count: 60,
        },
    )
    .unwrap();
    let tools = Tools {
        node,
        browser: stub.clone(),
        ffmpeg: stub,
    };
    let service = Service::start(
        tools,
        store,
        Config {
            queue_capacity: queue,
            history: 64,
            max_fills: 16,
        },
    )
    .unwrap();
    Env {
        _dir: dir,
        work,
        service,
    }
}
fn key(fill: &str) -> FillKey {
    FillKey {
        owner: "o".into(),
        fill: fill.into(),
    }
}
fn view(fill: &str, refresh: Refresh) -> View {
    View {
        key: key(fill),
        refresh,
    }
}
fn req(k: &FillKey) -> Result<Request, ()> {
    Ok(Request {
        owner: k.owner.clone(),
        fill: k.fill.clone(),
        capture: Capture {
            width: 320,
            height: 180,
            fps: 5,
            frames: 5,
            windy: None,
            background: Background::Opaque,
            document: "doc".into(),
        },
        maximum_bytes: 1000,
        timeout: Duration::from_secs(60),
    })
}
fn sched(max: usize, per_tick: usize) -> Scheduler {
    Scheduler::new(SchedConfig {
        max_schedules: max,
        max_due_per_tick: per_tick,
    })
}
fn runs(e: &Env) -> usize {
    std::fs::read_to_string(e.work.join("runs")).map_or(0, |s| s.lines().count())
}
fn wait_runs(e: &Env, n: usize) {
    let end = Instant::now() + Duration::from_secs(60);
    while runs(e) < n {
        assert!(Instant::now() < end, "hang guard runs");
        std::thread::sleep(Duration::from_millis(10));
    }
}
fn wait_done(e: &Env, id: multiview_webengine::service::JobId) {
    let end = Instant::now() + Duration::from_secs(60);
    loop {
        let s = e.service.status("o", id).unwrap();
        if matches!(s.state, State::Succeeded { .. } | State::Failed { .. }) {
            return;
        }
        assert!(Instant::now() < end, "hang guard {s:?}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn definitions_are_validated_and_copied() {
    let mut s = sched(2, 4);
    let t0 = Duration::ZERO;
    for bad in [0, 1441] {
        assert_eq!(
            s.upsert(t0, &view("a", Refresh::Interval { minutes: bad })),
            Err(ScheduleError::Invalid)
        );
    }
    let mut v = view("a", Refresh::Interval { minutes: 5 });
    s.upsert(t0, &v).unwrap();
    v.refresh = Refresh::OneShot; // caller mutation must not leak in
    assert_eq!(
        s.definition(&key("a")).unwrap().refresh,
        Refresh::Interval { minutes: 5 }
    );
    s.upsert(t0, &view("b", Refresh::OneShot)).unwrap();
    assert_eq!(
        s.upsert(t0, &view("c", Refresh::OneShot)),
        Err(ScheduleError::Full)
    );
    // Replacing an existing key is not blocked by the bound.
    s.upsert(t0, &view("b", Refresh::Interval { minutes: 1 }))
        .unwrap();
    assert!(s.remove(&key("b")));
    assert!(!s.remove(&key("b")));
    s.upsert(t0, &view("c", Refresh::OneShot)).unwrap();
}

#[test]
fn oneshot_never_runs_on_boot_or_tick_only_on_trigger() {
    let e = env(false, 4);
    let mut s = sched(4, 4);
    s.upsert(Duration::ZERO, &view("a", Refresh::OneShot))
        .unwrap();
    assert_eq!(s.next_due(&key("a")), None);
    for h in [0, 1, 1000] {
        let r = s.tick(Duration::from_secs(h * 3600), &e.service, req);
        assert_eq!(r.accepted + r.coalesced + r.refused + r.deferred, 0);
        assert_eq!(r.next_due, None);
    }
    assert_eq!(runs(&e), 0);
    assert_eq!(
        s.trigger(&e.service, &key("zz"), req),
        Err(TriggerError::Unknown)
    );
    let Submitted::Accepted(id) = s.trigger(&e.service, &key("a"), req).unwrap() else {
        panic!("accepted")
    };
    wait_done(&e, id);
    assert_eq!(runs(&e), 1);
}

#[test]
fn interval_first_due_is_strictly_future_within_one_period_and_stable() {
    let mut s = sched(8, 8);
    for (i, now) in [Duration::ZERO, Duration::from_secs(12_345)]
        .into_iter()
        .enumerate()
    {
        let f = format!("f{i}");
        s.upsert(now, &view(&f, Refresh::Interval { minutes: 10 }))
            .unwrap();
        let d = s.next_due(&key(&f)).unwrap();
        assert!(d > now && d <= now + 10 * MIN, "{d:?}");
    }
    // Phase is a stable function of the key: same key, later registration lands
    // on the same slot modulo the period.
    s.upsert(
        Duration::ZERO,
        &view("p", Refresh::Interval { minutes: 10 }),
    )
    .unwrap();
    let a = s.next_due(&key("p")).unwrap();
    s.upsert(
        Duration::ZERO,
        &view("p", Refresh::Interval { minutes: 10 }),
    )
    .unwrap();
    assert_eq!(s.next_due(&key("p")), Some(a), "unchanged policy keeps due");
    s.remove(&key("p"));
    s.upsert(3 * 10 * MIN, &view("p", Refresh::Interval { minutes: 10 }))
        .unwrap();
    let b = s.next_due(&key("p")).unwrap();
    assert_eq!(
        b.as_nanos() % (10 * MIN).as_nanos(),
        a.as_nanos() % (10 * MIN).as_nanos()
    );
    // Distinct keys are staggered (not all on one slot).
    let mut slots = std::collections::BTreeSet::new();
    for i in 0..16 {
        let f = format!("s{i}");
        let mut t = Scheduler::new(SchedConfig {
            max_schedules: 1,
            max_due_per_tick: 1,
        });
        t.upsert(Duration::ZERO, &view(&f, Refresh::Interval { minutes: 60 }))
            .unwrap();
        slots.insert(t.next_due(&key(&f)).unwrap());
    }
    assert!(slots.len() >= 8, "staggered: {}", slots.len());
}

#[test]
fn due_interval_runs_once_skips_missed_periods_and_does_not_storm() {
    let e = env(false, 8);
    let mut s = sched(4, 4);
    s.upsert(Duration::ZERO, &view("a", Refresh::Interval { minutes: 1 }))
        .unwrap();
    let first = s.next_due(&key("a")).unwrap();
    let r = s.tick(first - Duration::from_nanos(1), &e.service, req);
    assert_eq!(r.accepted, 0);
    assert_eq!(r.next_due, Some(first));
    // Caller stalled for 50 periods: exactly one job, next slot strictly future.
    let late = first + 50 * MIN + Duration::from_secs(3);
    let r = s.tick(late, &e.service, req);
    assert_eq!(r.accepted, 1);
    let next = r.next_due.unwrap();
    assert!(next > late && next <= late + MIN, "{next:?}");
    // Repeated ticks before next slot do nothing.
    let r = s.tick(late, &e.service, req);
    assert_eq!((r.accepted, r.coalesced, r.refused), (0, 0, 0));
    wait_runs(&e, 1);
    assert_eq!(runs(&e), 1);
}

#[test]
fn overlapping_due_coalesces_and_refusal_advances_without_hot_retry() {
    let e = env(true, 1);
    let mut s = sched(4, 4);
    s.upsert(Duration::ZERO, &view("a", Refresh::Interval { minutes: 1 }))
        .unwrap();
    let t = s.next_due(&key("a")).unwrap();
    assert_eq!(s.tick(t, &e.service, req).accepted, 1);
    wait_runs(&e, 1); // "a" is now running, gated
    let t2 = s.next_due(&key("a")).unwrap();
    let r = s.tick(t2, &e.service, req);
    assert_eq!((r.accepted, r.coalesced), (0, 1), "no overlapping job");
    // Fill the single queue slot with another fill, then a due "b" is refused.
    assert!(matches!(
        e.service.submit(req(&key("q")).unwrap()),
        Ok(Submitted::Accepted(_))
    ));
    s.upsert(t2, &view("b", Refresh::Interval { minutes: 1 }))
        .unwrap();
    let tb = s.next_due(&key("b")).unwrap();
    let calls = RefCell::new(0);
    let r = s.tick(tb, &e.service, |k| {
        *calls.borrow_mut() += 1;
        req(k)
    });
    assert_eq!(r.refused, 1);
    assert!(s.next_due(&key("b")).unwrap() > tb);
    let r = s.tick(tb, &e.service, |k| {
        *calls.borrow_mut() += 1;
        req(k)
    });
    assert_eq!(r.refused, 0);
    assert!(*calls.borrow() >= 1 && *calls.borrow() <= 2);
    // Trigger surfaces the refusal explicitly.
    assert_eq!(
        s.trigger(&e.service, &key("b"), req),
        Err(TriggerError::Submit(SubmitError::Full))
    );
    std::fs::write(e.work.join("release"), "").unwrap();
}

#[test]
fn per_tick_bound_defers_earliest_first_and_factory_failure_advances() {
    let e = env(false, 16);
    let mut s = sched(8, 2);
    for f in ["a", "b", "c", "d"] {
        s.upsert(Duration::ZERO, &view(f, Refresh::Interval { minutes: 1 }))
            .unwrap();
    }
    let now = Duration::from_secs(60);
    let order = RefCell::new(Vec::new());
    let r = s.tick(now, &e.service, |k| {
        order.borrow_mut().push(k.clone());
        req(k)
    });
    assert_eq!((r.accepted, r.deferred), (2, 2));
    // Earliest-due first.
    let dues: Vec<Duration> = ["a", "b", "c", "d"]
        .iter()
        .map(|f| s.next_due(&key(f)).unwrap())
        .collect();
    let fired = order.borrow().clone();
    assert_eq!(fired.len(), 2);
    for k in &fired {
        assert!(s.next_due(k).unwrap() > now);
    }
    let waiting = dues.iter().filter(|d| **d <= now).count();
    assert_eq!(waiting, 2);
    // Factory failure advances and is counted, never retried hot.
    let r = s.tick(now, &e.service, |_| Err::<Request, _>("no secrets"));
    assert_eq!((r.factory_failed, r.deferred), (2, 0));
    let r = s.tick(now, &e.service, |_| Err::<Request, _>("no secrets"));
    assert_eq!(r.factory_failed, 0);
    // A request naming a different owner/fill than the trusted key is refused.
    let mut s2 = sched(1, 1);
    s2.upsert(Duration::ZERO, &view("m", Refresh::OneShot))
        .unwrap();
    assert_eq!(
        s2.trigger(&e.service, &key("m"), |_| req(&key("other"))),
        Err(TriggerError::Factory)
    );
    wait_runs(&e, 2);
}

fn counting(n: &std::cell::Cell<usize>) -> impl FnMut(&FillKey) -> Result<Request, ()> + '_ {
    move |k| {
        n.set(n.get() + 1);
        req(k)
    }
}

#[test]
fn unrepresentable_next_due_retires_interval_without_hot_loop() {
    let e = env(false, 16);
    let calls = std::cell::Cell::new(0);
    for now in [Duration::MAX, Duration::MAX - Duration::from_secs(30)] {
        let mut s = sched(4, 4);
        s.upsert(now, &view("z", Refresh::Interval { minutes: 1440 }))
            .unwrap();
        // Either representable (strictly future) or retired; never <= now.
        if let Some(d) = s.next_due(&key("z")) {
            assert!(d > now);
        }
        for _ in 0..5 {
            let r = s.tick(now, &e.service, counting(&calls));
            assert_eq!(r.accepted + r.coalesced + r.refused + r.factory_failed, 0);
            assert!(r.next_due.is_none_or(|d| d >= now));
        }
        assert_eq!(calls.get(), 0, "nothing was due, factory never called");
    }
    // Force a due-then-retire at the extreme: due at an earlier time, fire at MAX.
    let mut s = sched(4, 4);
    s.upsert(Duration::ZERO, &view("z", Refresh::Interval { minutes: 1 }))
        .unwrap();
    let r = s.tick(Duration::MAX, &e.service, counting(&calls));
    assert_eq!(r.accepted, 1);
    assert_eq!(calls.get(), 1);
    assert_eq!(s.next_due(&key("z")), None, "retired, not <= now");
    assert_eq!(r.next_due, None);
    for _ in 0..5 {
        let r = s.tick(Duration::MAX, &e.service, counting(&calls));
        assert_eq!(r.accepted, 0);
    }
    assert_eq!(calls.get(), 1, "no hot loop");
    wait_runs(&e, 1);
}

#[test]
fn deferred_report_is_bounded_and_not_in_the_past() {
    let e = env(false, 64);
    let mut s = sched(64, 3);
    for i in 0..20 {
        s.upsert(
            Duration::ZERO,
            &view(&format!("f{i}"), Refresh::Interval { minutes: 1 }),
        )
        .unwrap();
    }
    let now = 2 * MIN;
    let calls = std::cell::Cell::new(0);
    let r = s.tick(now, &e.service, counting(&calls));
    assert_eq!(calls.get(), 3, "only the bound reaches the factory");
    assert_eq!((r.accepted, r.deferred), (3, 17));
    assert_eq!(r.next_due, Some(now + Duration::from_secs(1)));
    // Draining later ticks never exceeds the bound and ends with a future due.
    let mut total = 3;
    loop {
        let r = s.tick(now, &e.service, counting(&calls));
        assert!(r.accepted + r.coalesced + r.refused <= 3);
        total += r.accepted + r.coalesced + r.refused;
        assert!(r.next_due.is_none_or(|d| d >= now));
        if r.deferred == 0 {
            assert!(r.next_due.unwrap() > now);
            break;
        }
    }
    assert_eq!(total, 20);
    assert_eq!(calls.get(), 20, "one factory call per actual submission");
    wait_runs(&e, 1);
}
