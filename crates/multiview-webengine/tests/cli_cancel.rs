#![cfg(unix)]
//! Confirm termination is converted to cancellation, with process cleanup.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use std::{
    io::{Read, Write},
    os::unix::fs::PermissionsExt,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

#[test]
fn cli_sigterm_reaps_active_producer_without_committing() {
    let directory = tempfile::tempdir().unwrap();
    let script = directory.path().join("producer");
    let ready = directory.path().join("ready");
    std::fs::write(
        &script,
        format!(
            // Publish the pid atomically: `ready` must never be seen empty.
            "#!/bin/sh\necho $$ > '{0}.tmp' && mv '{0}.tmp' '{0}'\ncat >/dev/null\nexec sleep 60\n",
            ready.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let store = directory.path().join("store");
    std::fs::create_dir(&store).unwrap();
    let request = serde_json::json!({"root":store,"tools":{"node":script,"browser":"/bin/sh","ffmpeg":"/bin/sh"},"capture":{"width":64,"height":64,"fps":1,"frames":2,"document":"x"},"provenance":{"owner":"owner","fill":"fill","job":"job","generation":1},"maximum_bytes":10_000,"store_bytes":100_000,"store_files":20,"timeout_seconds":60});
    let mut worker = Command::new(env!("CARGO_BIN_EXE_conspect-capture"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut input = worker.stdin.take().unwrap();
    input
        .write_all(&serde_json::to_vec(&request).unwrap())
        .unwrap();
    drop(input);
    let hang_guard = Instant::now() + Duration::from_secs(10);
    while !ready.exists() {
        if Instant::now() >= hang_guard {
            let _ = worker.kill();
            let _ = worker.wait();
            panic!("producer never became ready");
        }
        thread::sleep(Duration::from_millis(5));
    }
    let producer = std::fs::read_to_string(&ready)
        .unwrap()
        .trim()
        .parse::<u32>()
        .unwrap();
    let pid = rustix::process::Pid::from_raw(i32::try_from(worker.id()).unwrap()).unwrap();
    rustix::process::kill_process(pid, rustix::process::Signal::TERM).unwrap();
    let status = loop {
        if let Some(status) = worker.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= hang_guard {
            let _ = worker.kill();
            let _ = worker.wait();
            panic!("worker ignored cancellation");
        }
        thread::sleep(Duration::from_millis(5));
    };
    assert!(!status.success());
    let mut output = Vec::new();
    worker
        .stdout
        .take()
        .unwrap()
        .read_to_end(&mut output)
        .unwrap();
    assert_eq!(output, Vec::<u8>::new());
    assert!(
        !std::path::Path::new(&format!("/proc/{producer}")).exists(),
        "producer was not reaped"
    );
    assert!(!std::fs::read_dir(store)
        .unwrap()
        .filter_map(Result::ok)
        .any(|entry| matches!(
            entry.path().extension().and_then(|s| s.to_str()),
            Some("data" | "json")
        )));
}
