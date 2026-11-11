//! Deterministic runner tests against fake producer executables (no browser).
//! Success never depends on wall-clock timing; deadlines are only hang guards.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::items_after_statements,
    clippy::ignored_unit_patterns
)]
use multiview_webengine::{
    artifact::{ArtifactStore, Limits, Provenance},
    capture::{Background, Capture, Tools},
    runner::{run_capture, Control},
};
use std::{
    io::ErrorKind,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::{atomic::AtomicBool, Mutex, MutexGuard, PoisonError},
    time::{Duration, Instant},
};

// Serialises script-write + spawn so no fork inherits a writable script fd (ETXTBSY).
static SERIAL: Mutex<()> = Mutex::new(());
// One failing test must not poison the lock and fail every later test.
fn serial() -> MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(PoisonError::into_inner)
}
// Defines `escape CMD...`: exec CMD in a new session. `setsid(1)` is util-linux
// only; elsewhere (macOS) the stock perl does the same. `exec` keeps `$!` equal
// to the escaped pid, and a non-interactive `sh` background job is never a
// group leader, so `setsid()` succeeds.
const ESCAPE: &str = "escape() { if command -v setsid >/dev/null; then exec setsid \"$@\"; else exec perl -MPOSIX -e 'POSIX::setsid() or die; exec @ARGV' \"$@\"; fi; }";
const DONE: &str = r#"printf '{"event":"capture_complete","frames":5}\n' >&2"#;

struct Env {
    dir: tempfile::TempDir,
    store: ArtifactStore,
    tools: Tools,
}
fn exe(path: &Path, body: &str) {
    std::fs::write(path, body).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}
fn env(script: &str) -> Env {
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
    let store = ArtifactStore::open(
        &dir.path().join("s"),
        Limits {
            bytes: 100_000_000,
            count: 20,
        },
    )
    .unwrap_or_else(|_| {
        std::fs::create_dir(dir.path().join("s")).unwrap();
        std::fs::set_permissions(dir.path().join("s"), std::fs::Permissions::from_mode(0o700))
            .unwrap();
        ArtifactStore::open(
            &dir.path().join("s"),
            Limits {
                bytes: 100_000_000,
                count: 20,
            },
        )
        .unwrap()
    });
    Env {
        dir,
        store,
        tools: Tools {
            node,
            browser: stub.clone(),
            ffmpeg: stub,
        },
    }
}
fn capture(document: usize) -> Capture {
    Capture {
        width: 320,
        height: 180,
        fps: 5,
        frames: 5,
        windy: None,
        background: Background::Opaque,
        document: "x".repeat(document),
    }
}
fn prov() -> Provenance {
    Provenance {
        owner: "o".into(),
        fill: "f".into(),
        job: "j".into(),
        generation: 1,
    }
}
fn run(
    e: &mut Env,
    max: u64,
    doc: usize,
    cancel: &AtomicBool,
    secs: u64,
) -> std::io::Result<multiview_webengine::artifact::Artifact> {
    run_capture(
        &mut e.store,
        prov(),
        max,
        &e.tools,
        &capture(doc),
        Control {
            cancelled: cancel,
            deadline: Instant::now() + Duration::from_secs(secs),
        },
        |_| true,
    )
}
/// Cancels once the fake host has recorded its pid, so the test never depends
/// on how fast the OS starts the host (first exec of a new script can be slow,
/// e.g. while macOS assesses it).
fn cancel_once_started(flag: &'static AtomicBool, pid_file: PathBuf) {
    std::thread::spawn(move || {
        let give_up = Instant::now() + Duration::from_secs(30);
        while !pid_file.exists() && Instant::now() < give_up {
            std::thread::sleep(Duration::from_millis(10));
        }
        std::thread::sleep(Duration::from_millis(100));
        flag.store(true, std::sync::atomic::Ordering::Release);
    });
}
fn pid_of(path: &Path) -> i32 {
    std::fs::read_to_string(path)
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}
fn alive(pid: i32) -> bool {
    rustix::process::test_kill_process(rustix::process::Pid::from_raw(pid).unwrap()).is_ok()
}
fn no_artifacts(e: &Env) {
    let names: Vec<_> = std::fs::read_dir(e.dir.path().join("s"))
        .unwrap()
        .map(|f| f.unwrap().file_name().into_string().unwrap())
        .filter(|n| {
            Path::new(n)
                .extension()
                .is_some_and(|x| x == "data" || x == "json")
        })
        .collect();
    assert!(names.is_empty(), "{names:?}");
}
fn leaked_private_dirs() -> usize {
    std::fs::read_dir(std::env::temp_dir())
        .unwrap()
        .filter(|f| {
            f.as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("conspect-web-")
        })
        .count()
}

#[test]
fn host_never_reading_stdin_is_cancelled() {
    let _g = serial();
    static CANCEL: AtomicBool = AtomicBool::new(false);
    let mut e = env("echo $$ > \"$W/pid\"\nexec sleep 300");
    cancel_once_started(&CANCEL, e.dir.path().join("w/pid"));
    // Request far larger than a pipe buffer: the write cannot complete.
    let err = run(&mut e, 1000, 120_000, &CANCEL, 60).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Interrupted);
    assert!(!alive(pid_of(&e.dir.path().join("w/pid"))));
    no_artifacts(&e);
}

#[test]
fn hung_host_hits_deadline_and_is_reaped() {
    let _g = serial();
    let mut e = env("echo $$ > \"$W/pid\"\nsleep 300 & echo $! > \"$W/child\"\nwait");
    let err = run(&mut e, 1000, 10, &AtomicBool::new(false), 1).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::TimedOut);
    assert!(!alive(pid_of(&e.dir.path().join("w/pid"))));
    assert!(!alive(pid_of(&e.dir.path().join("w/child"))));
    no_artifacts(&e);
}

#[test]
fn excess_output_hits_quota_and_publishes_nothing() {
    let _g = serial();
    let mut e = env(&format!(
        "cat >/dev/null &\nhead -c 3000000 /dev/zero\n{DONE}"
    ));
    let err = run(&mut e, 1000, 10, &AtomicBool::new(false), 60).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::StorageFull);
    no_artifacts(&e);
}

#[test]
fn stderr_flood_is_drained_and_final_line_decides() {
    let _g = serial();
    let mut e = env(&format!(
        "cat >/dev/null\nprintf 'ftyp-moov-bytes'\nhead -c 2000000 /dev/zero | tr '\\0' 'n' >&2\nprintf '\\nnoise\\n' >&2\n{DONE}\nprintf '\\n\\n' >&2"
    ));
    let art = run(&mut e, 1000, 10, &AtomicBool::new(false), 60).unwrap();
    assert_eq!(art.bytes, 15);
}

#[test]
fn stderr_flood_without_completion_fails() {
    let _g = serial();
    let mut e = env("cat >/dev/null\nprintf 'bytes'\nhead -c 2000000 /dev/zero | tr '\\0' 'n' >&2");
    assert!(run(&mut e, 1000, 10, &AtomicBool::new(false), 60).is_err());
    no_artifacts(&e);
}

#[test]
fn failed_exit_with_completion_line_is_rejected() {
    let _g = serial();
    let mut e = env(&format!("cat >/dev/null\nprintf 'bytes'\n{DONE}\nexit 3"));
    assert!(run(&mut e, 1000, 10, &AtomicBool::new(false), 60).is_err());
    no_artifacts(&e);
}

#[test]
fn exit_leaving_descendants_returns_and_kills_group() {
    let _g = serial();
    // `grouped` shares the group (swept); `escaped` setsid-escapes while holding
    // both output pipes open, which must not stall the runner.
    let mut e = env(&format!(
        "cat >/dev/null\nsleep 300 >/dev/null 2>&1 & echo $! > \"$W/grouped\"\n{ESCAPE}\nescape sleep 300 &\necho $! > \"$W/escaped\"\nprintf 'bytes'\n{DONE}"
    ));
    let result = run(&mut e, 1000, 10, &AtomicBool::new(false), 60);
    let escaped = pid_of(&e.dir.path().join("w/escaped"));
    let grouped = pid_of(&e.dir.path().join("w/grouped"));
    // Clean up the deliberately escaped process regardless of outcome.
    let _ = rustix::process::kill_process(
        rustix::process::Pid::from_raw(escaped).unwrap(),
        rustix::process::Signal::KILL,
    );
    assert_eq!(result.unwrap().bytes, 5);
    assert!(!alive(grouped));
}

#[test]
fn environment_is_cleared_and_private_dirs_are_removed() {
    let _g = serial();
    std::env::set_var("NODE_OPTIONS", "--require=/nonexistent");
    std::env::set_var("LD_PRELOAD", "/nonexistent.so");
    std::env::set_var("CONSPECT_TEST_SECRET", "hunter2");
    let mut e = env(&format!(
        "env > \"$W/env\"\npwd > \"$W/cwd\"\ncat > \"$W/req\"\nprintf 'bytes'\n{DONE}"
    ));
    let before = leaked_private_dirs();
    run(&mut e, 1000, 10, &AtomicBool::new(false), 60).unwrap();
    let w = e.dir.path().join("w");
    let env_dump = std::fs::read_to_string(w.join("env")).unwrap();
    for banned in [
        "NODE_OPTIONS",
        "LD_PRELOAD",
        "CONSPECT_TEST_SECRET",
        "hunter2",
    ] {
        assert!(!env_dump.contains(banned), "{banned} leaked");
    }
    let vars: std::collections::BTreeMap<_, _> =
        env_dump.lines().filter_map(|l| l.split_once('=')).collect();
    assert_eq!(vars["PATH"], "/usr/bin:/bin");
    let cwd = std::fs::read_to_string(w.join("cwd")).unwrap();
    for key in ["HOME", "TMPDIR"] {
        assert!(vars[key].contains("conspect-web-"), "{key}");
    }
    assert!(cwd.contains("conspect-web-"));
    let request: serde_json::Value =
        serde_json::from_slice(&std::fs::read(w.join("req")).unwrap()).unwrap();
    let profile = PathBuf::from(request["profile"].as_str().unwrap());
    assert!(profile.starts_with(vars["HOME"].rsplit_once('/').unwrap().0));
    assert!(!profile.exists(), "profile retained");
    assert!(!PathBuf::from(cwd.trim()).exists(), "private root retained");
    assert_eq!(leaked_private_dirs(), before);
}

#[test]
fn relative_or_missing_tool_paths_are_refused_before_spawn() {
    let mut e = env("exit 0");
    e.tools.node = PathBuf::from("node");
    let err = run(&mut e, 1000, 10, &AtomicBool::new(false), 5).unwrap_err();
    assert!(err.to_string().contains("tool path"));
    no_artifacts(&e);
}
