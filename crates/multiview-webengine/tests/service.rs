//! Queued capture service tests against fake producer executables (no browser).
//! Orders and counts rely on confirmed statuses; timeouts are hang guards only.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stderr,
    clippy::needless_pass_by_value,
    clippy::panic,
    clippy::unnecessary_debug_formatting,
    clippy::as_conversions
)]
use multiview_webengine::{
    artifact::{ArtifactStore, Limits},
    capture::{Background, Capture, Tools},
    fetch::{Codec, Fetch, FetchTools},
    service::{
        CancelOutcome, Checkpoint, Config, FetchRequest, JobId, Request, Service, State, Status,
        SubmitError, Submitted,
    },
};
use std::{
    io::{ErrorKind, Read},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::Mutex,
    time::{Duration, Instant},
};

// Serialises script-write + spawn so no fork inherits a writable script fd (ETXTBSY).
static SERIAL: Mutex<()> = Mutex::new(());
const DONE: &str = r#"printf '{"event":"capture_complete","frames":5}\n' >&2"#;
const SECRET: &str = "secret-document-marker";

fn exe(path: &Path, body: &str) {
    std::fs::write(path, body).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}
fn store_at(dir: &Path) -> ArtifactStore {
    let root = dir.join("s");
    std::fs::create_dir(&root).unwrap();
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
    ArtifactStore::open(
        &root,
        Limits {
            bytes: 100_000_000,
            count: 60,
        },
    )
    .unwrap()
}
struct Env {
    dir: tempfile::TempDir,
    work: PathBuf,
    service: Service,
}
fn env(script: &str, config: Config) -> Env {
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path().join("w");
    std::fs::create_dir(&work).unwrap();
    let node = work.join("node");
    exe(
        &node,
        &format!("#!/bin/sh\nW='{}'\n{script}\n", work.display()),
    );
    let stub = work.join("stub");
    exe(&stub, "#!/bin/sh\n");
    let tools = Tools {
        node,
        browser: stub.clone(),
        ffmpeg: stub,
    };
    let service = Service::start(tools, store_at(dir.path()), config).unwrap();
    Env { dir, work, service }
}
fn config(queue_capacity: usize, history: usize) -> Config {
    Config {
        queue_capacity,
        history,
        max_fills: 8,
    }
}
fn request(fill: &str) -> Request {
    Request {
        owner: "o".into(),
        fill: fill.into(),
        capture: Capture {
            width: 320,
            height: 180,
            fps: 5,
            frames: 5,
            windy: None,
            background: Background::Opaque,
            document: SECRET.into(),
        },
        maximum_bytes: 1000,
        timeout: Duration::from_secs(60),
    }
}
fn accepted(s: Submitted) -> JobId {
    match s {
        Submitted::Accepted(id) => id,
        Submitted::Coalesced(_) => panic!("expected accepted"),
    }
}
fn wait(service: &Service, id: JobId, done: impl Fn(&State) -> bool) -> Status {
    let end = Instant::now() + Duration::from_secs(60);
    loop {
        let status = service.status("o", id).expect("status retained");
        if done(&status.state) {
            return status;
        }
        assert!(Instant::now() < end, "hang guard: {status:?}");
        std::thread::sleep(Duration::from_millis(10));
    }
}
fn terminal(state: &State) -> bool {
    !matches!(state, State::Queued | State::Running)
}
fn wait_file(path: &Path) {
    let end = Instant::now() + Duration::from_secs(60);
    while !path.exists() {
        assert!(Instant::now() < end, "hang guard waiting for {path:?}");
        std::thread::sleep(Duration::from_millis(10));
    }
}
fn alive(pid: i32) -> bool {
    rustix::process::test_kill_process(rustix::process::Pid::from_raw(pid).unwrap()).is_ok()
}
fn pid_of(path: &Path) -> i32 {
    std::fs::read_to_string(path)
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}
fn artifact_files(e: &Env) -> usize {
    std::fs::read_dir(e.dir.path().join("s"))
        .unwrap()
        .filter(|f| {
            let n = f.as_ref().unwrap().file_name();
            Path::new(&n)
                .extension()
                .is_some_and(|x| x == "data" || x == "json")
        })
        .count()
}
/// Producer that records its start, optionally fails, then emits fixed bytes.
fn ok_script() -> String {
    format!(
        "cat >/dev/null\necho run >> \"$W/runs\"\nif [ -f \"$W/fail\" ]; then exit 3; fi\nprintf 'ftyp-moov-bytes'\n{DONE}"
    )
}
/// Producer that blocks until `release` exists, so queue states are observable.
fn gated_script() -> String {
    format!(
        "cat >/dev/null\necho run >> \"$W/runs\"\ntouch \"$W/started\"\nwhile [ ! -f \"$W/release\" ]; do sleep 0.02; done\nprintf 'ftyp-moov-bytes'\n{DONE}"
    )
}
fn runs(e: &Env) -> usize {
    std::fs::read_to_string(e.work.join("runs")).map_or(0, |s| s.lines().count())
}

#[test]
fn success_returns_durable_artifact_and_pins() {
    let _g = SERIAL.lock().unwrap();
    let e = env(&ok_script(), config(2, 8));
    let id = accepted(e.service.submit(request("f")).unwrap());
    let status = wait(&e.service, id, terminal);
    let State::Succeeded { ref artifact } = status.state else {
        panic!("{status:?}")
    };
    assert_eq!(status.owner, "o");
    assert!(status.finished_age.is_some());
    let mut lease = e.service.pin(artifact, "o").unwrap();
    let mut bytes = Vec::new();
    lease.read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes, b"ftyp-moov-bytes");
    let good = e.service.latest_good("o", "f").unwrap();
    assert_eq!(good.artifact, *artifact);
    assert_eq!(good.bytes, 15);
    assert_eq!(good.job, id);
    assert!(e.service.latest_good("o", "other").is_none());
    assert!(e.service.latest_good("x", "f").is_none());
    // No plaintext document anywhere in published metadata.
    let shown = format!("{status:?}{good:?}");
    assert!(!shown.contains(SECRET), "{shown}");
}

#[test]
fn failure_keeps_latest_good_and_reports_kind() {
    let _g = SERIAL.lock().unwrap();
    let e = env(&ok_script(), config(2, 8));
    let first = accepted(e.service.submit(request("f")).unwrap());
    let State::Succeeded { artifact } = wait(&e.service, first, terminal).state else {
        panic!("first must succeed")
    };
    std::fs::write(e.work.join("fail"), "").unwrap();
    let second = accepted(e.service.submit(request("f")).unwrap());
    let status = wait(&e.service, second, terminal);
    assert!(matches!(status.state, State::Failed { .. }), "{status:?}");
    let good = e.service.latest_good("o", "f").unwrap();
    assert_eq!(good.artifact, artifact);
    assert_eq!(good.job, first);
    assert!(e.service.pin(&artifact, "o").is_ok());
    assert_eq!(artifact_files(&e), 2, "only the good artifact remains");
}

#[test]
fn refresh_replaces_latest_good_and_preserves_superseded_for_library_release() {
    let _g = SERIAL.lock().unwrap();
    let e = env(&ok_script(), config(2, 8));
    let first = accepted(e.service.submit(request("f")).unwrap());
    let State::Succeeded { artifact: a1 } = wait(&e.service, first, terminal).state else {
        panic!()
    };
    let second = accepted(e.service.submit(request("f")).unwrap());
    let State::Succeeded { artifact: a2 } = wait(&e.service, second, terminal).state else {
        panic!()
    };
    assert_ne!(a1, a2);
    assert_eq!(e.service.latest_good("o", "f").unwrap().artifact, a2);
    assert!(e.service.pin(&a1, "o").is_ok());
    assert!(e.service.pin(&a2, "o").is_ok());
}

#[test]
fn capacity_rejects_and_same_fill_coalesces() {
    let _g = SERIAL.lock().unwrap();
    let e = env(&gated_script(), config(2, 8));
    let running = accepted(e.service.submit(request("a")).unwrap());
    wait_file(&e.work.join("started"));
    wait(&e.service, running, |s| matches!(s, State::Running));
    // Repeat trigger for the running fill coalesces; it does not occupy the queue.
    assert_eq!(
        e.service.submit(request("a")).unwrap(),
        Submitted::Coalesced(running)
    );
    let b = accepted(e.service.submit(request("b")).unwrap());
    assert_eq!(
        e.service.submit(request("b")).unwrap(),
        Submitted::Coalesced(b)
    );
    let c = accepted(e.service.submit(request("c")).unwrap());
    assert_eq!(
        e.service.submit(request("d")).unwrap_err(),
        SubmitError::Full
    );
    assert_eq!(e.service.status("o", b).unwrap().state, State::Queued);
    std::fs::write(e.work.join("release"), "").unwrap();
    let ids: Vec<String> = [running, b, c]
        .into_iter()
        .map(|id| match wait(&e.service, id, terminal).state {
            State::Succeeded { artifact } => artifact.as_str().to_owned(),
            other => panic!("{other:?}"),
        })
        .collect();
    // Artifact ids are allocated in run order: FIFO execution.
    let mut sorted = ids.clone();
    sorted.sort();
    assert_eq!(ids, sorted);
    assert_eq!(runs(&e), 3, "coalesced triggers ran once");
    // Capacity is available again.
    assert!(e.service.submit(request("d")).is_ok());
}

#[test]
fn cancel_queued_never_runs() {
    let _g = SERIAL.lock().unwrap();
    let e = env(&gated_script(), config(2, 8));
    let running = accepted(e.service.submit(request("a")).unwrap());
    wait_file(&e.work.join("started"));
    let queued = accepted(e.service.submit(request("b")).unwrap());
    assert_eq!(e.service.cancel("o", queued), CancelOutcome::Cancelled);
    assert_eq!(
        e.service.status("o", queued).unwrap().state,
        State::Cancelled
    );
    assert_eq!(e.service.cancel("o", queued), CancelOutcome::Finished);
    assert_eq!(
        e.service.cancel("o", JobId::from_raw(9999)),
        CancelOutcome::Unknown
    );
    std::fs::write(e.work.join("release"), "").unwrap();
    assert!(matches!(
        wait(&e.service, running, terminal).state,
        State::Succeeded { .. }
    ));
    assert_eq!(runs(&e), 1);
    // The freed slot and fill can be reused.
    assert!(e.service.submit(request("b")).is_ok());
}

#[test]
fn cancel_running_stops_producer_and_publishes_nothing() {
    let _g = SERIAL.lock().unwrap();
    let e = env(
        "cat >/dev/null\necho $$ > \"$W/pid\"\nexec sleep 300",
        config(2, 8),
    );
    let id = accepted(e.service.submit(request("a")).unwrap());
    wait_file(&e.work.join("pid"));
    wait(&e.service, id, |s| matches!(s, State::Running));
    assert_eq!(e.service.cancel("o", id), CancelOutcome::Cancelling);
    assert_eq!(wait(&e.service, id, terminal).state, State::Cancelled);
    assert!(!alive(pid_of(&e.work.join("pid"))));
    assert_eq!(artifact_files(&e), 0);
    assert!(e.service.latest_good("o", "a").is_none());
}

#[test]
fn shutdown_cancels_active_and_queued_and_joins() {
    let _g = SERIAL.lock().unwrap();
    let e = env(
        "cat >/dev/null\necho $$ > \"$W/pid\"\nexec sleep 300",
        config(2, 8),
    );
    let running = accepted(e.service.submit(request("a")).unwrap());
    wait_file(&e.work.join("pid"));
    let queued = accepted(e.service.submit(request("b")).unwrap());
    e.service.shutdown();
    // Joined: producer is dead and states are final without waiting.
    assert!(!alive(pid_of(&e.work.join("pid"))));
    assert_eq!(
        e.service.status("o", running).unwrap().state,
        State::Cancelled
    );
    assert_eq!(
        e.service.status("o", queued).unwrap().state,
        State::Cancelled
    );
    assert_eq!(
        e.service.submit(request("c")).unwrap_err(),
        SubmitError::ShuttingDown
    );
    assert!(e.service.pin(&"a0000000000000000".parse_id(), "o").is_err());
    e.service.shutdown(); // idempotent
}

trait ParseId {
    fn parse_id(&self) -> multiview_webengine::artifact::ArtifactId;
}
impl ParseId for str {
    fn parse_id(&self) -> multiview_webengine::artifact::ArtifactId {
        multiview_webengine::artifact::ArtifactId::parse(self).unwrap()
    }
}

#[test]
fn per_job_timeout_fails_and_invalid_requests_are_rejected() {
    let _g = SERIAL.lock().unwrap();
    let e = env("cat >/dev/null\nexec sleep 300", config(2, 8));
    let mut slow = request("a");
    slow.timeout = Duration::from_secs(1);
    let id = accepted(e.service.submit(slow).unwrap());
    assert_eq!(
        wait(&e.service, id, terminal).state,
        State::Failed {
            kind: ErrorKind::TimedOut
        }
    );
    for mutate in [
        (|r: &mut Request| r.timeout = Duration::ZERO) as fn(&mut Request),
        |r| r.timeout = Duration::from_secs(121),
        |r| r.maximum_bytes = 0,
        |r| r.fill = String::new(),
        |r| r.owner = "x".repeat(129),
        |r| r.capture.frames = 1,
    ] {
        let mut bad = request("v");
        mutate(&mut bad);
        assert_eq!(e.service.submit(bad).unwrap_err(), SubmitError::Invalid);
    }
}

#[test]
fn history_is_bounded_and_oldest_finished_is_forgotten() {
    let _g = SERIAL.lock().unwrap();
    let e = env(&ok_script(), config(2, 2));
    let mut ids = Vec::new();
    for i in 0..4 {
        let id = accepted(e.service.submit(request(&format!("f{i}"))).unwrap());
        wait(&e.service, id, terminal);
        ids.push(id);
    }
    assert!(e.service.status("o", ids[0]).is_none());
    assert!(e.service.status("o", ids[1]).is_none());
    assert!(e.service.status("o", ids[2]).is_some());
    assert!(e.service.status("o", ids[3]).is_some());
    // Latest-good metadata outlives status history.
    assert!(e.service.latest_good("o", "f0").is_some());
}

#[test]
fn max_fills_bounds_queued_distinct_fills_and_release_on_cancel() {
    let _g = SERIAL.lock().unwrap();
    let mut cfg = config(4, 8);
    cfg.max_fills = 2;
    let e = env(&gated_script(), cfg);
    let a = accepted(e.service.submit(request("a")).unwrap());
    wait_file(&e.work.join("started"));
    let b = accepted(e.service.submit(request("b")).unwrap());
    // Two distinct first fills are reserved (running a + queued b): no third.
    assert_eq!(
        e.service.submit(request("c")).unwrap_err(),
        SubmitError::Full
    );
    // Cancelling a never-good queued fill releases its reservation.
    assert_eq!(e.service.cancel("o", b), CancelOutcome::Cancelled);
    let c = accepted(e.service.submit(request("c")).unwrap());
    std::fs::write(e.work.join("release"), "").unwrap();
    for id in [a, c] {
        assert!(matches!(
            wait(&e.service, id, terminal).state,
            State::Succeeded { .. }
        ));
    }
    // Completion never exceeded the cap: exactly two latest-good fills.
    assert_eq!(e.service.checkpoint().latest_good.len(), 2);
    assert_eq!(
        e.service.submit(request("d")).unwrap_err(),
        SubmitError::Full
    );
    // An existing good fill can still refresh.
    assert!(e.service.submit(request("a")).is_ok());
}

#[test]
fn other_owner_cannot_see_or_cancel_jobs() {
    let _g = SERIAL.lock().unwrap();
    let e = env(&gated_script(), config(2, 8));
    let running = accepted(e.service.submit(request("a")).unwrap());
    wait_file(&e.work.join("started"));
    let queued = accepted(e.service.submit(request("b")).unwrap());
    for id in [running, queued] {
        assert!(e.service.status("x", id).is_none());
        assert_eq!(e.service.cancel("x", id), CancelOutcome::Unknown);
    }
    assert_eq!(e.service.status("o", queued).unwrap().state, State::Queued);
    assert_eq!(
        e.service.status("o", running).unwrap().state,
        State::Running
    );
}

#[test]
fn shutdown_settles_running_even_when_history_is_one() {
    let _g = SERIAL.lock().unwrap();
    let e = env(
        "cat >/dev/null\necho $$ > \"$W/pid\"\nexec sleep 300",
        config(2, 1),
    );
    let running = accepted(e.service.submit(request("a")).unwrap());
    wait_file(&e.work.join("pid"));
    let queued = accepted(e.service.submit(request("b")).unwrap());
    assert_eq!(e.service.cancel("o", queued), CancelOutcome::Cancelled);
    // Cancelling the running job twice coalesces into one Cancelling state.
    assert_eq!(e.service.cancel("o", running), CancelOutcome::Cancelling);
    assert_eq!(e.service.cancel("o", running), CancelOutcome::Cancelling);
    e.service.shutdown();
    // The active job settled as Cancelled; history 1 forgot the older queued one.
    assert_eq!(
        e.service
            .status("o", running)
            .expect("running retained")
            .state,
        State::Cancelled
    );
    assert!(e.service.status("o", queued).is_none());
}

#[test]
fn checkpoint_restart_continues_ids_and_generations() {
    let _g = SERIAL.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path().join("w");
    std::fs::create_dir(&work).unwrap();
    let node = work.join("node");
    exe(
        &node,
        &format!("#!/bin/sh\nW='{}'\n{}\n", work.display(), ok_script()),
    );
    let stub = work.join("stub");
    exe(&stub, "#!/bin/sh\n");
    let tools = || Tools {
        node: node.clone(),
        browser: stub.clone(),
        ffmpeg: stub.clone(),
    };
    let store = store_at(dir.path());
    let service = Service::start(tools(), store, config(2, 8)).unwrap();
    let first = accepted(service.submit(request("f")).unwrap());
    wait(&service, first, terminal);
    service.shutdown();
    let cp = service.checkpoint();
    assert!(cp.next_job > first.raw());
    assert_eq!(cp.latest_good.len(), 1);
    drop(service);
    let reopen = || {
        ArtifactStore::open(
            &dir.path().join("s"),
            Limits {
                bytes: 100_000_000,
                count: 60,
            },
        )
        .unwrap()
    };
    // Stale counters (generation not above the stored one) fail closed.
    let stale = Checkpoint {
        next_generation: 1,
        ..cp.clone()
    };
    assert!(Service::start_with_checkpoint(tools(), reopen(), config(2, 8), stale).is_err());
    let zero = Checkpoint {
        next_job: 0,
        ..cp.clone()
    };
    assert!(Service::start_with_checkpoint(tools(), reopen(), config(2, 8), zero).is_err());
    let service =
        Service::start_with_checkpoint(tools(), reopen(), config(2, 8), cp.clone()).unwrap();
    let good = service.latest_good("o", "f").unwrap();
    assert_eq!(good.job, first);
    let next = accepted(service.submit(request("f")).unwrap());
    assert!(next.raw() >= cp.next_job);
    assert!(matches!(
        wait(&service, next, terminal).state,
        State::Succeeded { .. }
    ));
}

#[test]
fn unknown_status_is_none() {
    let _g = SERIAL.lock().unwrap();
    let e = env(&ok_script(), config(1, 1));
    assert!(e.service.status("o", JobId::from_raw(1)).is_none());
}

/// Real-tool integration: one authored HTML document through the service into a
/// pinned store artifact. Skips loudly unless `CONSPECT_REQUIRE_REAL_BROWSER=1`.
#[test]
fn actual_browser_capture_through_service_is_pinnable() {
    let _g = SERIAL.lock().unwrap();
    let var = |k: &str| std::env::var_os(k).map(PathBuf::from);
    let which = |name: &str| {
        std::env::split_paths(&std::env::var_os("PATH")?)
            .map(|d| d.join(name))
            .find(|p| p.is_file())
    };
    let browser = || {
        let root = PathBuf::from(std::env::var_os("HOME")?).join(".cache/ms-playwright");
        let mut dirs: Vec<_> = std::fs::read_dir(root)
            .ok()?
            .flatten()
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("chromium_headless_shell-")
            })
            .map(|e| e.path())
            .collect();
        dirs.sort();
        dirs.into_iter()
            .rev()
            .map(|d| d.join("chrome-headless-shell-linux64/chrome-headless-shell"))
            .find(|p| p.is_file())
    };
    let tools = (|| {
        Some(Tools {
            node: var("CONSPECT_WEB_NODE").or_else(|| which("node"))?,
            browser: var("CONSPECT_WEB_BROWSER").or_else(browser)?,
            ffmpeg: var("CONSPECT_WEB_FFMPEG").or_else(|| which("ffmpeg"))?,
        })
    })();
    let Some(tools) = tools else {
        assert!(
            std::env::var("CONSPECT_REQUIRE_REAL_BROWSER").as_deref() != Ok("1"),
            "real browser tools required but not found"
        );
        eprintln!("SKIPPED real-browser service test: node/browser/ffmpeg not found");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let service = Service::start(tools, store_at(dir.path()), config(2, 4)).unwrap();
    let mut req = request("radar");
    req.maximum_bytes = 1_000_000;
    req.timeout = Duration::from_secs(60);
    req.capture.document = "<body style='margin:0;background:black'><canvas id='c' width='320' height='180'></canvas><script>window.mvCaptureFrame=n=>{const x=c.getContext('2d');x.fillStyle=n%2?'red':'blue';x.fillRect(0,0,320,180)}</script>".into();
    let id = accepted(service.submit(req).unwrap());
    let State::Succeeded { artifact } = wait(&service, id, terminal).state else {
        panic!("real capture failed")
    };
    let mut lease = service.pin(&artifact, "o").unwrap();
    let mut bytes = Vec::new();
    lease.read_to_end(&mut bytes).unwrap();
    assert!(bytes.windows(4).any(|s| s == b"ftyp"));
    assert!(bytes.windows(4).any(|s| s == b"moov"));
}

// ---- durable journal ----

use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

struct Durable {
    dir: tempfile::TempDir,
    work: PathBuf,
    node: PathBuf,
    stub: PathBuf,
}
impl Durable {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let work = dir.path().join("w");
        std::fs::create_dir(&work).unwrap();
        let node = work.join("node");
        exe(
            &node,
            &format!("#!/bin/sh\nW='{}'\n{}\n", work.display(), ok_script()),
        );
        let stub = work.join("stub");
        exe(&stub, "#!/bin/sh\n");
        let _ = store_at(dir.path());
        Self {
            dir,
            work,
            node,
            stub,
        }
    }
    fn tools(&self) -> Tools {
        Tools {
            node: self.node.clone(),
            browser: self.stub.clone(),
            ffmpeg: self.stub.clone(),
        }
    }
    fn store(&self) -> ArtifactStore {
        ArtifactStore::open(
            &self.dir.path().join("s"),
            Limits {
                bytes: 100_000_000,
                count: 60,
            },
        )
        .unwrap()
    }
    fn journal(&self) -> PathBuf {
        self.dir.path().join("journal")
    }
    fn start(&self) -> std::io::Result<Service> {
        Service::start_durable(self.tools(), self.store(), config(2, 8), &self.journal())
    }
    fn set_fail(&self, on: bool) {
        let f = self.work.join("fail");
        if on {
            std::fs::write(f, "").unwrap();
        } else {
            let _ = std::fs::remove_file(f);
        }
    }
}

#[test]
fn durable_restart_recovers_latest_good_and_counters_across_failure_and_cancel() {
    let _g = SERIAL.lock().unwrap();
    let d = Durable::new();
    let service = d.start().unwrap();
    let first = accepted(service.submit(request("f")).unwrap());
    wait(&service, first, terminal);
    d.set_fail(true);
    let failed = accepted(service.submit(request("f")).unwrap());
    assert!(matches!(
        wait(&service, failed, terminal).state,
        State::Failed { .. }
    ));
    d.set_fail(false);
    let cancelled = accepted(service.submit(request("g")).unwrap());
    let _ = service.cancel("o", cancelled);
    wait(&service, cancelled, terminal);
    let good = service.latest_good("o", "f").unwrap();
    assert_eq!(good.job, first);
    service.shutdown();
    drop(service);
    let service = d.start().unwrap();
    let again = service.latest_good("o", "f").unwrap();
    assert_eq!(again.job, first);
    assert_eq!(again.artifact, good.artifact);
    assert!(matches!(
        service.status("o", failed).unwrap().state,
        State::Failed { .. }
    ));
    assert_eq!(
        service.status("o", cancelled).unwrap().state,
        State::Cancelled
    );
    let next = accepted(service.submit(request("f")).unwrap());
    assert!(next.raw() > failed.raw().max(cancelled.raw()));
    let done = wait(&service, next, terminal);
    assert!(matches!(done.state, State::Succeeded { .. }));
    let new_good = service.latest_good("o", "f").unwrap();
    assert_ne!(new_good.artifact, good.artifact);
    // Journal never holds the capture document.
    let raw = std::fs::read(d.journal()).unwrap();
    assert!(!String::from_utf8_lossy(&raw).contains(SECRET));
}

#[test]
fn durable_counters_exceed_history_even_after_pruning() {
    let _g = SERIAL.lock().unwrap();
    let d = Durable::new();
    d.set_fail(true);
    let service = Service::start_durable(d.tools(), d.store(), config(2, 1), &d.journal()).unwrap();
    let mut last = 0;
    for _ in 0..4 {
        let id = accepted(service.submit(request("f")).unwrap());
        wait(&service, id, terminal);
        last = id.raw();
    }
    service.shutdown();
    drop(service);
    let service = Service::start_durable(d.tools(), d.store(), config(2, 1), &d.journal()).unwrap();
    let id = accepted(service.submit(request("f")).unwrap());
    assert!(id.raw() > last);
    // Journal size is bounded by history, not by jobs ever run.
    assert!(std::fs::metadata(d.journal()).unwrap().len() < 2000);
}

#[test]
fn durable_corrupt_stale_or_missing_journal_fails_closed() {
    let _g = SERIAL.lock().unwrap();
    let d = Durable::new();
    let service = d.start().unwrap();
    let a = accepted(service.submit(request("f")).unwrap());
    wait(&service, a, terminal);
    service.shutdown();
    let old = std::fs::read(d.journal()).unwrap();
    drop(service);
    let service = d.start().unwrap();
    let b = accepted(service.submit(request("f")).unwrap());
    wait(&service, b, terminal);
    service.shutdown();
    let new = std::fs::read(d.journal()).unwrap();
    drop(service);
    // Stale: store holds an artifact above the old high-waters.
    std::fs::write(d.journal(), &old).unwrap();
    assert!(d.start().is_err());
    // Corrupt: flipped byte breaks the checksum; truncation and garbage too.
    let mut bad = new.clone();
    let last = bad.len() - 3;
    bad[last] ^= 1;
    std::fs::write(d.journal(), &bad).unwrap();
    assert!(d.start().is_err());
    std::fs::write(d.journal(), &new[..new.len() / 2]).unwrap();
    assert!(d.start().is_err());
    std::fs::write(d.journal(), b"garbage").unwrap();
    assert!(d.start().is_err());
    // Missing journal with a non-empty store.
    std::fs::remove_file(d.journal()).unwrap();
    assert!(d.start().is_err());
    // The genuine journal still recovers, and nothing was deleted.
    std::fs::write(d.journal(), &new).unwrap();
    let service = d.start().unwrap();
    assert!(service.latest_good("o", "f").is_some());
}

#[test]
fn durable_unfinished_jobs_are_not_silently_successful_at_boot() {
    let _g = SERIAL.lock().unwrap();
    let d = Durable::new();
    let service = d.start().unwrap();
    let a = accepted(service.submit(request("f")).unwrap());
    wait(&service, a, terminal);
    service.shutdown();
    drop(service);
    // Forge a crash image: job 1 still `running` (its artifact exists and is
    // recoverable by trusted provenance), plus a queued and an orphan running job.
    let text = std::fs::read_to_string(d.journal()).unwrap();
    let body = text.split_once('\n').unwrap().1;
    let mut doc: serde_json::Value = serde_json::from_str(body).unwrap();
    let hw = doc["job_high"].as_u64().unwrap();
    let jobs = doc["jobs"].as_array_mut().unwrap();
    jobs[0]["state"] = "running".into();
    jobs[0]["artifact"] = serde_json::Value::Null;
    jobs[0].as_object_mut().unwrap().remove("artifact");
    for (i, st) in ["queued", "running"].iter().enumerate() {
        jobs.push(
            serde_json::json!({"id": hw + i as u64, "owner": "o", "fill": "h",
            "generation": 0, "state": st}),
        );
    }
    doc["job_high"] = (hw + 2).into();
    doc["good"] = serde_json::json!([]);
    let body = serde_json::to_vec(&doc).unwrap();
    let mut out = format!("{:x}\n", Sha256::digest(&body)).into_bytes();
    out.extend(body);
    std::fs::write(d.journal(), out).unwrap();
    let service = d.start().unwrap();
    // The committed artifact is recovered by its trusted reserved identity.
    assert!(matches!(
        service.status("o", a).unwrap().state,
        State::Succeeded { .. }
    ));
    assert!(service.latest_good("o", "f").is_some());
    assert_eq!(
        service.status("o", JobId::from_raw(hw)).unwrap().state,
        State::Uncertain
    );
    assert!(matches!(
        service.status("o", JobId::from_raw(hw + 1)).unwrap().state,
        State::Failed { .. }
    ));
    assert!(service.latest_good("o", "h").is_none());
    let next = accepted(service.submit(request("h")).unwrap());
    assert!(next.raw() >= hw + 2);
}

static SYNCS: AtomicUsize = AtomicUsize::new(0);
static FAIL_AT: AtomicUsize = AtomicUsize::new(usize::MAX);
fn flaky(b: multiview_webengine::service_journal::JournalBoundary) -> std::io::Result<()> {
    use multiview_webengine::service_journal::JournalBoundary;
    if b == JournalBoundary::Sync
        && SYNCS.fetch_add(1, AtomicOrdering::SeqCst) == FAIL_AT.load(AtomicOrdering::SeqCst)
    {
        return Err(std::io::Error::other("injected fsync failure"));
    }
    Ok(())
}

#[test]
fn durable_outcome_persist_failure_is_uncertain_keeps_artifact_not_latest_good() {
    let _g = SERIAL.lock().unwrap();
    let d = Durable::new();
    // Syncs: 0 boot, 1 submit, 2 launch, 3 reserved id, 4 outcome.
    SYNCS.store(0, AtomicOrdering::SeqCst);
    FAIL_AT.store(4, AtomicOrdering::SeqCst);
    let service =
        Service::start_durable_with_hook(d.tools(), d.store(), config(2, 8), &d.journal(), flaky)
            .unwrap();
    let id = accepted(service.submit(request("f")).unwrap());
    let st = wait(&service, id, terminal);
    FAIL_AT.store(usize::MAX, AtomicOrdering::SeqCst);
    assert_eq!(st.state, State::Uncertain);
    assert!(service.latest_good("o", "f").is_none());
    service.shutdown();
    drop(service);
    // The artifact was not deleted, and boot resolves the job from the store.
    let kept = std::fs::read_dir(d.dir.path().join("s"))
        .unwrap()
        .filter(|f| {
            Path::new(&f.as_ref().unwrap().file_name())
                .extension()
                .is_some_and(|x| x == "data")
        })
        .count();
    assert_eq!(kept, 1);
    let service = d.start().unwrap();
    // Strict: the verified committed artifact is recovered as Succeeded.
    assert!(matches!(
        service.status("o", id).unwrap().state,
        State::Succeeded { .. }
    ));
    assert!(service.latest_good("o", "f").is_some());
}

#[test]
fn durable_reservation_persist_failure_refuses_submit_without_exposing_id() {
    let _g = SERIAL.lock().unwrap();
    let d = Durable::new();
    SYNCS.store(0, AtomicOrdering::SeqCst);
    FAIL_AT.store(1, AtomicOrdering::SeqCst);
    let service =
        Service::start_durable_with_hook(d.tools(), d.store(), config(2, 8), &d.journal(), flaky)
            .unwrap();
    assert_eq!(
        service.submit(request("f")).err(),
        Some(SubmitError::Persistence)
    );
    FAIL_AT.store(usize::MAX, AtomicOrdering::SeqCst);
    assert_eq!(runs_in(&d.work), 0);
    let ok = accepted(service.submit(request("f")).unwrap());
    wait(&service, ok, terminal);
}
fn runs_in(work: &Path) -> usize {
    std::fs::read_to_string(work.join("runs")).map_or(0, |s| s.lines().count())
}

fn forge(d: &Durable, edit: impl FnOnce(&mut serde_json::Value)) {
    let text = std::fs::read_to_string(d.journal()).unwrap();
    let mut doc: serde_json::Value =
        serde_json::from_str(text.split_once('\n').unwrap().1).unwrap();
    edit(&mut doc);
    let body = serde_json::to_vec(&doc).unwrap();
    let mut out = format!("{:x}\n", Sha256::digest(&body)).into_bytes();
    out.extend(body);
    std::fs::write(d.journal(), out).unwrap();
}

static ART_FAIL: AtomicUsize = AtomicUsize::new(0);
fn art_hook(b: multiview_webengine::artifact::Boundary) -> std::io::Result<()> {
    use multiview_webengine::artifact::Boundary;
    let want = match ART_FAIL.load(AtomicOrdering::SeqCst) {
        1 => Boundary::CommitDirectorySync,
        2 => Boundary::BytesDirectorySync,
        _ => return Ok(()),
    };
    if b == want {
        Err(std::io::Error::other("injected artifact boundary failure"))
    } else {
        Ok(())
    }
}

#[test]
fn artifact_failure_after_manifest_rename_is_uncertain_then_succeeded_at_boot() {
    let _g = SERIAL.lock().unwrap();
    let d = Durable::new();
    let mut store = d.store();
    store.set_boundary_hook(art_hook);
    ART_FAIL.store(1, AtomicOrdering::SeqCst);
    let service = Service::start_durable(d.tools(), store, config(2, 8), &d.journal()).unwrap();
    let id = accepted(service.submit(request("f")).unwrap());
    let st = wait(&service, id, terminal);
    ART_FAIL.store(0, AtomicOrdering::SeqCst);
    // The commit may be durable: never Failed or Cancelled.
    assert_eq!(st.state, State::Uncertain);
    assert!(service.latest_good("o", "f").is_none());
    service.shutdown();
    drop(service);
    let service = d.start().unwrap();
    assert!(matches!(
        service.status("o", id).unwrap().state,
        State::Succeeded { .. }
    ));
    assert!(service.latest_good("o", "f").is_some());
}

#[test]
fn artifact_failure_after_content_rename_is_uncertain_even_across_boot() {
    let _g = SERIAL.lock().unwrap();
    let d = Durable::new();
    let mut store = d.store();
    store.set_boundary_hook(art_hook);
    ART_FAIL.store(2, AtomicOrdering::SeqCst);
    let service = Service::start_durable(d.tools(), store, config(2, 8), &d.journal()).unwrap();
    let id = accepted(service.submit(request("f")).unwrap());
    assert_eq!(wait(&service, id, terminal).state, State::Uncertain);
    ART_FAIL.store(0, AtomicOrdering::SeqCst);
    service.shutdown();
    drop(service);
    // Content exists, manifest does not: the store cannot see an artifact.
    let service = d.start().unwrap();
    assert_eq!(service.status("o", id).unwrap().state, State::Uncertain);
    assert!(service.latest_good("o", "f").is_none());
}

static DIR_SYNCS: AtomicUsize = AtomicUsize::new(0);
static DIR_FAIL: AtomicUsize = AtomicUsize::new(usize::MAX);
fn dir_flaky(b: multiview_webengine::service_journal::JournalBoundary) -> std::io::Result<()> {
    use multiview_webengine::service_journal::JournalBoundary;
    if b == JournalBoundary::DirectorySync
        && DIR_SYNCS.fetch_add(1, AtomicOrdering::SeqCst) == DIR_FAIL.load(AtomicOrdering::SeqCst)
    {
        return Err(std::io::Error::other("injected directory fsync failure"));
    }
    Ok(())
}

#[test]
fn outcome_dir_sync_failure_then_retry_cannot_erase_success() {
    let _g = SERIAL.lock().unwrap();
    let d = Durable::new();
    // Dir syncs: 0 boot, 1 submit, 2 launch, 3 reserved id, 4 outcome.
    DIR_SYNCS.store(0, AtomicOrdering::SeqCst);
    DIR_FAIL.store(4, AtomicOrdering::SeqCst);
    let service = Service::start_durable_with_hook(
        d.tools(),
        d.store(),
        config(2, 8),
        &d.journal(),
        dir_flaky,
    )
    .unwrap();
    let id = accepted(service.submit(request("f")).unwrap());
    assert_eq!(wait(&service, id, terminal).state, State::Uncertain);
    DIR_FAIL.store(usize::MAX, AtomicOrdering::SeqCst);
    // A later successful write (a retry from the old image) must keep it.
    let other = accepted(service.submit(request("g")).unwrap());
    wait(&service, other, terminal);
    service.shutdown();
    drop(service);
    let service = d.start().unwrap();
    assert!(matches!(
        service.status("o", id).unwrap().state,
        State::Succeeded { .. }
    ));
    assert!(service.latest_good("o", "f").is_some());
    assert!(service.latest_good("o", "g").is_some());
}

#[test]
fn reservation_persist_failure_runs_no_producer_and_leaves_no_part_file() {
    let _g = SERIAL.lock().unwrap();
    let d = Durable::new();
    SYNCS.store(0, AtomicOrdering::SeqCst);
    FAIL_AT.store(3, AtomicOrdering::SeqCst);
    let service =
        Service::start_durable_with_hook(d.tools(), d.store(), config(2, 8), &d.journal(), flaky)
            .unwrap();
    let id = accepted(service.submit(request("f")).unwrap());
    let st = wait(&service, id, terminal);
    FAIL_AT.store(usize::MAX, AtomicOrdering::SeqCst);
    assert!(matches!(st.state, State::Failed { .. }));
    assert_eq!(runs_in(&d.work), 0);
    let parts = std::fs::read_dir(d.dir.path().join("s"))
        .unwrap()
        .filter(|f| {
            f.as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".part")
        })
        .count();
    assert_eq!(parts, 0);
}

#[test]
fn crash_with_reserved_id_and_no_manifest_is_uncertain_not_failed() {
    let _g = SERIAL.lock().unwrap();
    let d = Durable::new();
    let service = d.start().unwrap();
    let a = accepted(service.submit(request("f")).unwrap());
    wait(&service, a, terminal);
    service.shutdown();
    drop(service);
    forge(&d, |doc| {
        let hw = doc["job_high"].as_u64().unwrap();
        let gh = doc["generation_high"].as_u64().unwrap();
        doc["jobs"].as_array_mut().unwrap().push(serde_json::json!({
            "id": hw, "owner": "o", "fill": "h", "generation": gh,
            "reserved": "a0000000000000fff", "state": "running"}));
        doc["job_high"] = (hw + 1).into();
        doc["generation_high"] = (gh + 1).into();
    });
    let service = d.start().unwrap();
    let hw_job = service
        .checkpoint()
        .next_job
        .checked_sub(1)
        .map(JobId::from_raw)
        .unwrap();
    assert_eq!(service.status("o", hw_job).unwrap().state, State::Uncertain);
    assert!(service.latest_good("o", "h").is_none());
}

#[test]
fn prior_failed_cancelled_or_uncertain_with_verified_artifact_is_succeeded_at_boot() {
    let _g = SERIAL.lock().unwrap();
    for forged in ["failed", "cancelled", "uncertain"] {
        let d = Durable::new();
        let service = d.start().unwrap();
        let a = accepted(service.submit(request("f")).unwrap());
        wait(&service, a, terminal);
        service.shutdown();
        drop(service);
        forge(&d, |doc| {
            let job = &mut doc["jobs"][0];
            job["state"] = forged.into();
            if forged == "failed" {
                job["kind"] = "Other".into();
            }
            job.as_object_mut().unwrap().remove("artifact");
            doc["good"] = serde_json::json!([]);
        });
        let service = d.start().unwrap();
        assert!(matches!(
            service.status("o", a).unwrap().state,
            State::Succeeded { .. }
        ));
        assert!(service.latest_good("o", "f").is_some());
    }
}

#[test]
fn shrinking_max_fills_never_turns_verified_success_into_failure() {
    let _g = SERIAL.lock().unwrap();
    let d = Durable::new();
    let service = d.start().unwrap();
    let a = accepted(service.submit(request("f")).unwrap());
    wait(&service, a, terminal);
    let b = accepted(service.submit(request("g")).unwrap());
    wait(&service, b, terminal);
    service.shutdown();
    drop(service);
    forge(&d, |doc| {
        for j in doc["jobs"].as_array_mut().unwrap() {
            j["state"] = "running".into();
            j.as_object_mut().unwrap().remove("artifact");
        }
        doc["good"] = serde_json::json!([]);
    });
    let small = Config {
        queue_capacity: 2,
        history: 8,
        max_fills: 1,
    };
    let service = Service::start_durable(d.tools(), d.store(), small, &d.journal()).unwrap();
    for id in [a, b] {
        assert!(matches!(
            service.status("o", id).unwrap().state,
            State::Succeeded { .. }
        ));
    }
    let tabled = ["f", "g"]
        .iter()
        .filter(|f| service.latest_good("o", f).is_some())
        .count();
    assert_eq!(tabled, 1);
}

// ---- durable queued fetch (real embedded fetch host under real node) ----

const FETCH_TOKEN: &str = "test-token-0123456789";
const FETCH_URL: &str = "https://www.youtube.com/watch?v=secret-url-marker";
const FETCH_YTDLP: &str = r#"
case "$*" in
 *" -J "*) printf '{"title":"SecretTitle","uploader":"SecretUploader","duration":12.5}'; exit 0;;
esac
echo "$$" > "$W/pid"
if [ -f "$W/payload" ]; then cat "$W/payload"; exit 0; fi
if [ -f "$W/slow" ]; then printf 'MVPROG 1 10\n' >&2; printf 'partial'; sleep 60; fi
printf 'MVPROG 5 10\n' >&2
printf 'hello-bytes'
[ -f "$W/fail" ] && exit 3
exit 0
"#;

fn real_node() -> Option<PathBuf> {
    let found = std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|d| d.join("node"))
        .find(|p| p.is_file());
    if found.is_none() {
        assert!(
            std::env::var("CONSPECT_REQUIRE_REAL_BROWSER").as_deref() != Ok("1"),
            "node required"
        );
        eprintln!("SKIPPED queued fetch test: node not found");
    }
    found
}
struct FetchEnv {
    d: Durable,
    node: PathBuf,
    real_validator: Option<PathBuf>,
}
impl FetchEnv {
    fn new() -> Option<Self> {
        let node = real_node()?;
        let d = Durable::new();
        let w = &d.work;
        exe(
            &w.join("ytdlp"),
            &format!("#!/bin/sh\nW='{}'\n{FETCH_YTDLP}\n", w.display()),
        );
        exe(&w.join("wrapper"), &format!(
            "#!/bin/sh\nif [ \"$1\" = --probe ]; then echo 'multiview-egress-boundary/1 {FETCH_TOKEN}'; exit 0; fi\n[ \"$1\" = -- ] || exit 9\nshift\nexec \"$@\"\n"
        ));
        Some(Self {
            d,
            node,
            real_validator: None,
        })
    }
    /// `stub` is an EXPLICIT FAKE validator; `real_validator` swaps in real ffmpeg.
    fn ftools(&self) -> FetchTools {
        FetchTools {
            node: self.node.clone(),
            ytdlp: self.d.work.join("ytdlp"),
            ffmpeg: match self.real_validator.clone() {
                Some(f) => f,
                None => self.d.stub.clone(),
            },
            egress_wrapper: self.d.work.join("wrapper"),
            egress_token: FETCH_TOKEN.into(),
        }
    }
    fn start(&self) -> Service {
        Service::start_durable_with_fetch(
            self.d.tools(),
            self.ftools(),
            self.d.store(),
            config(2, 8),
            &self.d.journal(),
        )
        .unwrap()
    }
}
fn freq(fill: &str) -> FetchRequest {
    FetchRequest {
        owner: "o".into(),
        fill: fill.into(),
        fetch: Fetch {
            url: FETCH_URL.into(),
            host_allowlist: Fetch::youtube_allowlist(),
            codec: Codec::default(),
            max_height: 720,
            max_duration_secs: 600,
            extract_metadata: true,
        },
        maximum_bytes: 1_000_000,
        timeout: Duration::from_secs(60),
    }
}

#[test]
fn queued_fetch_success_pins_artifact_and_hides_sideband_by_default() {
    let _g = SERIAL.lock().unwrap();
    let Some(e) = FetchEnv::new() else { return };
    let service = e.start();
    let id = accepted(service.submit_fetch(freq("f")).unwrap());
    let st = wait(&service, id, terminal);
    let State::Succeeded { artifact } = st.state.clone() else {
        panic!("{st:?}")
    };
    let dbg = format!("{st:?}");
    for secret in ["SecretTitle", "SecretUploader", "secret-url-marker"] {
        assert!(!dbg.contains(secret));
    }
    let mut lease = service.pin(&artifact, "o").unwrap();
    let mut body = String::new();
    lease.read_to_string(&mut body).unwrap();
    assert_eq!(body, "hello-bytes");
    assert!(service.pin(&artifact, "other").is_err());
    // Explicit opt-in API only; cross-owner is indistinguishable from unknown.
    let meta = service.fetch_meta("o", id).unwrap();
    assert_eq!(meta.title.as_deref(), Some("SecretTitle"));
    assert!(service.fetch_meta("x", id).is_none());
    assert_eq!(service.latest_good("o", "f").unwrap().bytes, 11);
    let journal = std::fs::read(e.d.journal()).unwrap();
    let text = String::from_utf8_lossy(&journal);
    for secret in ["SecretTitle", "secret-url-marker"] {
        assert!(!text.contains(secret));
    }
    // Capture still works on the same service.
    let cap = accepted(service.submit(request("c")).unwrap());
    assert!(matches!(
        wait(&service, cap, terminal).state,
        State::Succeeded { .. }
    ));
}

#[test]
fn fetch_submit_without_tools_or_with_bad_url_is_invalid() {
    let _g = SERIAL.lock().unwrap();
    let d = Durable::new();
    let service = d.start().unwrap();
    assert_eq!(
        service.submit_fetch(freq("f")).unwrap_err(),
        SubmitError::Invalid
    );
    drop(service);
    let Some(e) = FetchEnv::new() else { return };
    let service = e.start();
    let mut bad = freq("f");
    bad.fetch.url = "http://www.youtube.com/x".into();
    assert_eq!(service.submit_fetch(bad).unwrap_err(), SubmitError::Invalid);
}

#[test]
fn queued_fetch_cancellation_kills_group_and_keeps_latest_good() {
    let _g = SERIAL.lock().unwrap();
    let Some(e) = FetchEnv::new() else { return };
    let service = e.start();
    let first = accepted(service.submit_fetch(freq("f")).unwrap());
    wait(&service, first, terminal);
    let good = service.latest_good("o", "f").unwrap();
    std::fs::write(e.d.work.join("slow"), "").unwrap();
    let _ = std::fs::remove_file(e.d.work.join("pid"));
    let id = accepted(service.submit_fetch(freq("f")).unwrap());
    wait_file(&e.d.work.join("pid"));
    let pid = pid_of(&e.d.work.join("pid"));
    assert_eq!(service.cancel("o", id), CancelOutcome::Cancelling);
    assert_eq!(wait(&service, id, terminal).state, State::Cancelled);
    let end = Instant::now() + Duration::from_secs(30);
    while alive(pid) {
        assert!(Instant::now() < end, "downloader survived cancel");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        service.latest_good("o", "f").unwrap().artifact,
        good.artifact
    );
}

#[test]
fn queued_fetch_failed_download_keeps_latest_good_and_survives_restart() {
    let _g = SERIAL.lock().unwrap();
    let Some(e) = FetchEnv::new() else { return };
    let service = e.start();
    let first = accepted(service.submit_fetch(freq("f")).unwrap());
    wait(&service, first, terminal);
    let good = service.latest_good("o", "f").unwrap();
    std::fs::write(e.d.work.join("fail"), "").unwrap();
    let failed = accepted(service.submit_fetch(freq("f")).unwrap());
    assert!(matches!(
        wait(&service, failed, terminal).state,
        State::Failed { .. }
    ));
    assert_eq!(
        service.latest_good("o", "f").unwrap().artifact,
        good.artifact
    );
    service.shutdown();
    drop(service);

    let service = e.start();
    let again = service.latest_good("o", "f").unwrap();
    assert_eq!(again.artifact, good.artifact);
    assert_eq!(again.job, first);
    assert!(matches!(
        service.status("o", failed).unwrap().state,
        State::Failed { .. }
    ));
    let mut lease = service.pin(&good.artifact, "o").unwrap();
    let mut body = String::new();
    lease.read_to_string(&mut body).unwrap();
    assert_eq!(body, "hello-bytes");
    // Counters moved on; sideband meta is not persisted.
    let _ = std::fs::remove_file(e.d.work.join("fail"));
    let next = accepted(service.submit_fetch(freq("f")).unwrap());
    assert!(next.raw() > failed.raw());
    assert!(matches!(
        wait(&service, next, terminal).state,
        State::Succeeded { .. }
    ));
    assert!(service.fetch_meta("o", first).is_none());
}

#[test]
fn queued_fetch_unfinished_at_crash_is_never_success_after_restart() {
    let _g = SERIAL.lock().unwrap();
    let Some(e) = FetchEnv::new() else { return };
    std::fs::write(e.d.work.join("slow"), "").unwrap();
    let service = e.start();
    let id = accepted(service.submit_fetch(freq("f")).unwrap());
    wait_file(&e.d.work.join("pid"));
    // Simulate a crash: leak the service so no outcome is journaled by shutdown.
    // Instead, shut down (cancels, outcome recorded) then restart: still never success.
    service.shutdown();
    drop(service);
    let service = e.start();
    let st = service.status("o", id).unwrap();
    assert!(matches!(
        st.state,
        State::Cancelled | State::Failed { .. } | State::Uncertain
    ));
    assert!(service.latest_good("o", "f").is_none());
}

#[test]
fn queued_fetch_real_ffmpeg_rejects_non_media_and_accepts_real_mp4() {
    let _g = SERIAL.lock().unwrap();
    let Some(ff) = ["/usr/bin/ffmpeg", "/usr/local/bin/ffmpeg"]
        .iter()
        .map(PathBuf::from)
        .find(|p| p.is_file())
    else {
        assert!(
            std::env::var("CONSPECT_REQUIRE_REAL_BROWSER").as_deref() != Ok("1"),
            "ffmpeg required"
        );
        return;
    };
    let Some(mut e) = FetchEnv::new() else { return };
    e.real_validator = Some(ff.clone());
    let service = e.start();
    // HTML masquerading as media: the REAL validator must refuse, nothing published.
    std::fs::write(
        e.d.work.join("payload"),
        "<html><body>blocked</body></html>",
    )
    .unwrap();
    let bad = accepted(service.submit_fetch(freq("f")).unwrap());
    assert!(matches!(
        wait(&service, bad, terminal).state,
        State::Failed { .. }
    ));
    assert!(service.latest_good("o", "f").is_none());
    // Real local MP4 (generated by real ffmpeg, no network) succeeds.
    let st = std::process::Command::new(&ff)
        .args(["-nostdin", "-v", "error", "-y", "-f", "lavfi", "-i"])
        .arg("testsrc=size=64x64:rate=5:duration=1")
        .args(["-pix_fmt", "yuv420p", "-c:v", "mpeg4", "-f", "mp4"])
        .arg(e.d.work.join("payload"))
        .status()
        .unwrap();
    assert!(st.success());
    let want = std::fs::read(e.d.work.join("payload")).unwrap();
    let ok = accepted(service.submit_fetch(freq("f")).unwrap());
    let State::Succeeded { artifact } = wait(&service, ok, terminal).state else {
        panic!("real mp4 must validate")
    };
    let mut lease = service.pin(&artifact, "o").unwrap();
    let mut got = Vec::new();
    lease.read_to_end(&mut got).unwrap();
    assert_eq!(got, want);
}

static ADMISSION_PAUSE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);
fn pause_admitted(
    boundary: multiview_webengine::service_journal::JournalBoundary,
) -> std::io::Result<()> {
    use std::sync::atomic::Ordering;
    if boundary == multiview_webengine::service_journal::JournalBoundary::DirectorySync
        && ADMISSION_PAUSE
            .compare_exchange(1, 2, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    {
        while ADMISSION_PAUSE.load(Ordering::SeqCst) == 2 {
            std::thread::yield_now();
        }
    }
    Ok(())
}
#[test]
fn shutdown_after_durable_intent_is_uncertain_not_definite_refusal() {
    use std::sync::{atomic::Ordering, Arc};
    let _serial = SERIAL.lock().unwrap();
    let durable = Durable::new();
    let service = Arc::new(
        Service::start_durable_with_hook(
            durable.tools(),
            durable.store(),
            config(2, 8),
            &durable.journal(),
            pause_admitted,
        )
        .unwrap(),
    );
    ADMISSION_PAUSE.store(1, Ordering::SeqCst);
    let submitter = Arc::clone(&service);
    let result = std::thread::spawn(move || submitter.submit(request("admission-race")));
    let guard = Instant::now() + Duration::from_secs(10);
    while ADMISSION_PAUSE.load(Ordering::SeqCst) != 2 {
        assert!(Instant::now() < guard);
        std::thread::yield_now();
    }
    // No queued job is yet exposed in memory. Shutdown sets the flag while
    // the durable-intent hook holds the admission transaction before exposure.
    service.shutdown();
    ADMISSION_PAUSE.store(3, Ordering::SeqCst);
    assert!(matches!(
        result.join().unwrap(),
        Err(SubmitError::Persistence)
    ));
    drop(service);
    let restarted = durable.start().unwrap();
    assert!(matches!(
        restarted.status("o", JobId::from_raw(1)).unwrap().state,
        State::Cancelled | State::Uncertain
    ));
    assert!(restarted.latest_good("o", "admission-race").is_none());
}

const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

#[test]
fn correlated_submit_is_idempotent_across_finish_and_restart_without_payload_in_journal() {
    let _g = SERIAL.lock().unwrap();
    let d = Durable::new();
    let service = d.start().unwrap();
    let id = accepted(service.submit_correlated(request("f"), TOKEN).unwrap());
    wait(&service, id, terminal);
    assert_eq!(
        service.submit_correlated(request("f"), TOKEN).unwrap(),
        Submitted::Coalesced(id)
    );
    assert_eq!(service.lookup_correlated("o", "f", TOKEN).unwrap().0, id);
    assert!(service.lookup_correlated("other", "f", TOKEN).is_none());
    assert!(service.lookup_correlated("o", "g", TOKEN).is_none());
    assert!(service.lookup_correlated("o", "f", "bad").is_none());
    assert_eq!(
        service.submit_correlated(request("f"), "bad").err(),
        Some(SubmitError::Invalid)
    );
    service.shutdown();
    drop(service);
    let journal = std::fs::read_to_string(d.journal()).unwrap();
    assert!(journal.contains(TOKEN));
    assert!(!journal.contains(SECRET));
    let service = d.start().unwrap();
    assert_eq!(
        service.submit_correlated(request("f"), TOKEN).unwrap(),
        Submitted::Coalesced(id)
    );
    assert!(service.lookup_correlated("other", "f", TOKEN).is_none());
    assert_eq!(runs_in(&d.work), 1);
}

#[test]
fn correlation_survives_pause_after_queued_persist_and_restart_never_reruns() {
    let _g = SERIAL.lock().unwrap();
    let d = Durable::new();
    // Syncs: 0 boot, 1 queued intent (correlated); fail the launch persist (2)
    // so the job never reaches a producer. Then restart.
    SYNCS.store(0, AtomicOrdering::SeqCst);
    FAIL_AT.store(2, AtomicOrdering::SeqCst);
    let service =
        Service::start_durable_with_hook(d.tools(), d.store(), config(2, 8), &d.journal(), flaky)
            .unwrap();
    let id = accepted(service.submit_correlated(request("f"), TOKEN).unwrap());
    wait(&service, id, terminal);
    FAIL_AT.store(usize::MAX, AtomicOrdering::SeqCst);
    service.shutdown();
    drop(service);
    let service = d.start().unwrap();
    let (found, _state) = service.lookup_correlated("o", "f", TOKEN).unwrap();
    assert_eq!(found, id);
    assert_eq!(
        service.submit_correlated(request("f"), TOKEN).unwrap(),
        Submitted::Coalesced(id)
    );
    assert_eq!(runs_in(&d.work), 0);
    assert!(service.lookup_correlated("o2", "f", TOKEN).is_none());
}

#[test]
fn forged_duplicate_or_malformed_correlation_fails_closed_at_boot() {
    let _g = SERIAL.lock().unwrap();
    let d = Durable::new();
    let service = d.start().unwrap();
    let id = accepted(service.submit_correlated(request("f"), TOKEN).unwrap());
    wait(&service, id, terminal);
    service.shutdown();
    drop(service);
    forge(&d, |doc| {
        doc["jobs"][0]["correlation"] = serde_json::json!("NOT-HEX");
    });
    assert!(d.start().is_err());
}
