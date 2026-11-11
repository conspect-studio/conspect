#![cfg(unix)]
//! Process-tree resource accounting: declared procfs fixtures plus a real fake
//! producer whose descendant burns CPU. No success depends on wall-clock timing;
//! deadlines are hang guards only.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::similar_names,
    clippy::panic
)]
use multiview_webengine::{
    artifact::{ArtifactStore, Limits, Provenance},
    capture::{Background, Capture, Tools},
    resource::{MemoryBasis, Tracker},
    runner::{run_capture_observed, Control},
    service::{Config, Request, Service, Submitted},
};
use std::{
    os::unix::fs::PermissionsExt,
    path::Path,
    sync::{atomic::AtomicBool, atomic::Ordering, Mutex},
    time::{Duration, Instant},
};

static SERIAL: Mutex<()> = Mutex::new(());

fn proc_entry(root: &Path, pid: u32, ppid: u32, pgrp: u32, start: u64, ticks: (u64, u64)) {
    let d = root.join(pid.to_string());
    std::fs::create_dir_all(&d).unwrap();
    // comm deliberately contains spaces and a parenthesis.
    std::fs::write(
        d.join("stat"),
        format!(
            "{pid} (we ) b) S {ppid} {pgrp} 1 0 -1 0 0 0 0 0 {} {} 0 0 20 0 2 0 {start} 0 100 0\n",
            ticks.0, ticks.1
        ),
    )
    .unwrap();
}
fn io(root: &Path, pid: u32, r: u64, w: u64) {
    std::fs::write(
        root.join(pid.to_string()).join("io"),
        format!("rchar: 1\nread_bytes: {r}\nwrite_bytes: {w}\n"),
    )
    .unwrap();
}
fn pss(root: &Path, pid: u32, kb: u64) {
    std::fs::write(
        root.join(pid.to_string()).join("smaps_rollup"),
        format!("Rss: 999 kB\nPss:           {kb} kB\n"),
    )
    .unwrap();
}

#[test]
fn six_process_kinds_are_attributed_and_cpu_is_summed() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path();
    proc_entry(p, 1000, 1, 1000, 50, (100, 0)); // root
    proc_entry(p, 1001, 1000, 1000, 51, (200, 50)); // child
    proc_entry(p, 1002, 1001, 1000, 52, (300, 0)); // grandchild
    proc_entry(p, 1003, 1, 1000, 53, (400, 0)); // reparented group member
    proc_entry(p, 2000, 1, 2000, 60, (9000, 0)); // unrelated
    proc_entry(p, 1004, 1000, 1000, 54, (1000, 0)); // child without pss/io
    io(p, 1000, 10, 20);
    io(p, 1001, 1, 2);
    for pid in [1002, 1003, 1004] {
        io(p, pid, 0, 0);
    }
    pss(p, 1000, 100);
    pss(p, 1001, 50);
    let mut t = Tracker::with_proc_root(p, 1000).unwrap();
    let t0 = Instant::now();
    let a = t.sample(t0);
    assert_eq!(a.processes, 5, "root+child+grandchild+group member+1004");
    assert_eq!(a.threads, Some(10));
    assert!(a.gpu.is_none());
    assert!(a.cpu_cores.is_none());
    // Ticks 100+250+300+400+1000 = 2050; the unrelated 9000 is excluded.
    assert_eq!(a.memory_basis, MemoryBasis::PssWithRssFallback);
    assert_eq!(a.read_bytes, Some(11));
    assert_eq!(a.write_bytes, Some(22));

    // 1001 exits: its CPU stays counted; 1000 gains 100 ticks (1 s at 100 Hz).
    std::fs::remove_dir_all(p.join("1001")).unwrap();
    proc_entry(p, 1000, 1, 1000, 50, (200, 0));
    let b = t.sample(t0 + Duration::from_secs(1));
    assert_eq!(b.processes, 4, "root, 1002 (known), 1003, 1004");
    assert!(b.cpu_seconds >= a.cpu_seconds, "exited CPU is retained");
    assert_eq!(b.read_bytes, Some(11), "exited IO retained");
    let cores = b.cpu_cores.unwrap();
    let expected = (b.cpu_seconds - a.cpu_seconds) / 1.0;
    assert!((cores - expected).abs() < 1e-9);
    // Exact tick ratio, independent of CLK_TCK: (2050 + 100) / 2050.
    assert!((b.cpu_seconds / a.cpu_seconds - 2150.0 / 2050.0).abs() < 1e-9);
    assert!(cores > 0.0);
}

#[test]
fn pid_reuse_and_missing_counters_are_not_zero() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path();
    proc_entry(p, 10, 1, 10, 5, (0, 0));
    let mut t = Tracker::with_proc_root(p, 10).unwrap();
    let a = t.sample(Instant::now());
    assert_eq!(a.memory_basis, MemoryBasis::Rss);
    assert_eq!(a.read_bytes, None, "no io file means unknown, not 0");
    // Same pid, different starttime: not our root.
    proc_entry(p, 10, 1, 10, 6, (5, 0));
    let b = t.sample(Instant::now());
    assert!(b.root_lost);
    assert_eq!(b.processes, 0);
    assert!(Tracker::with_proc_root(p, 999).is_none());
}

fn exe(path: &Path, body: &str) {
    std::fs::write(path, body).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[cfg(target_os = "linux")]
#[test]
fn real_producer_descendant_cpu_reaches_observer() {
    let _g = SERIAL.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path().join("w");
    std::fs::create_dir(&work).unwrap();
    // Root sleeps; a descendant burns CPU, so root-only accounting would see ~0.
    let node = work.join("node");
    exe(
        &node,
        "#!/bin/sh\n( while :; do :; done ) &\n( while :; do :; done ) &\nwait\n",
    );
    let stub = work.join("stub");
    exe(&stub, "#!/bin/sh\n");
    let sdir = dir.path().join("s");
    std::fs::create_dir(&sdir).unwrap();
    std::fs::set_permissions(&sdir, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut store = ArtifactStore::open(
        &sdir,
        Limits {
            bytes: 1_000_000,
            count: 4,
        },
    )
    .unwrap();
    let tools = Tools {
        node,
        browser: stub.clone(),
        ffmpeg: stub,
    };
    let capture = Capture {
        width: 320,
        height: 180,
        fps: 5,
        frames: 5,
        windy: None,
        background: Background::Opaque,
        document: "x".into(),
    };
    let cancel = AtomicBool::new(false);
    let mut best: Option<multiview_webengine::resource::ResourceSample> = None;
    let mut seen = 0usize;
    let err = run_capture_observed(
        &mut store,
        Provenance {
            owner: "o".into(),
            fill: "f".into(),
            job: "j".into(),
            generation: 1,
        },
        1000,
        &tools,
        &capture,
        Control {
            cancelled: &cancel,
            deadline: Instant::now() + Duration::from_secs(60), // hang guard only
        },
        |_| true,
        |_| Ok(()),
        &mut |s| {
            seen += 1;
            if s.processes >= 3 && s.cpu_seconds > 0.0 && s.cpu_cores.is_some() {
                cancel.store(true, Ordering::Release);
            }
            best = Some(s);
        },
    )
    .unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::Interrupted);
    let s = best.unwrap();
    assert!(seen >= 2);
    assert!(s.processes >= 3, "root + two burners: {s:?}");
    assert!(s.cpu_seconds > 0.0);
    assert!(s.gpu.is_none());
}

#[cfg(target_os = "linux")]
#[test]
fn service_status_exposes_descendant_sample_while_running() {
    let _g = SERIAL.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path().join("w");
    std::fs::create_dir(&work).unwrap();
    let node = work.join("node");
    exe(
        &node,
        "#!/bin/sh\n( while :; do :; done ) &\n( while :; do :; done ) &\nwait\n",
    );
    let stub = work.join("stub");
    exe(&stub, "#!/bin/sh\n");
    let sdir = dir.path().join("s");
    std::fs::create_dir(&sdir).unwrap();
    std::fs::set_permissions(&sdir, std::fs::Permissions::from_mode(0o700)).unwrap();
    let store = ArtifactStore::open(
        &sdir,
        Limits {
            bytes: 1_000_000,
            count: 4,
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
            queue_capacity: 2,
            history: 4,
            max_fills: 2,
        },
    )
    .unwrap();
    let Submitted::Accepted(id) = service
        .submit(Request {
            owner: "o".into(),
            fill: "f".into(),
            capture: Capture {
                width: 320,
                height: 180,
                fps: 5,
                frames: 5,
                windy: None,
                background: Background::Opaque,
                document: "x".into(),
            },
            maximum_bytes: 1000,
            timeout: Duration::from_secs(60),
        })
        .unwrap()
    else {
        panic!("accepted")
    };
    let end = Instant::now() + Duration::from_secs(60); // hang guard
    let sample = loop {
        let st = service.status("o", id).unwrap();
        if let Some(r) = st
            .resource
            .filter(|r| r.processes >= 3 && r.cpu_seconds > 0.0)
        {
            break r;
        }
        assert!(Instant::now() < end, "hang guard");
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(sample.gpu.is_none());
    let _ = service.cancel("o", id);
}

#[test]
fn truncated_scan_keeps_root_and_does_not_retire_hidden_members() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path();
    proc_entry(p, 1000, 1, 1000, 50, (100, 0));
    proc_entry(p, 1001, 1000, 1000, 51, (200, 0));
    io(p, 1000, 10, 10);
    io(p, 1001, 5, 5);
    let mut t = Tracker::with_proc_root(p, 1000).unwrap();
    let t0 = Instant::now();
    let a = t.sample(t0);
    assert!(!a.truncated);
    assert_eq!(a.processes, 2);
    // Many unrelated entries and a scan cap of 1: the scan cannot see everything.
    for pid in 3000..3020 {
        proc_entry(p, pid, 1, pid, 70 + u64::from(pid), (1, 0));
    }
    t.set_caps(1, 100);
    let b = t.sample(t0 + Duration::from_secs(1));
    assert!(b.truncated, "capped scan must be visible");
    assert!(!b.root_lost, "root is read directly, not lost");
    assert_eq!(b.processes, 2, "known members read directly");
    assert!(
        (b.cpu_seconds - a.cpu_seconds).abs() < 1e-9,
        "no double count"
    );
    assert_eq!(b.read_bytes, None, "truncated: IO totals are unknown");
    assert!(!b.io_complete);
}

#[test]
fn truncated_tracked_set_is_flagged() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path();
    proc_entry(p, 1000, 1, 1000, 50, (1, 0));
    for pid in 1001..1006 {
        proc_entry(p, pid, 1000, 1000, u64::from(pid), (1, 0));
    }
    let mut t = Tracker::with_proc_root(p, 1000).unwrap();
    t.set_caps(100, 3);
    let a = t.sample(Instant::now());
    assert!(a.truncated);
    assert_eq!(a.processes, 3);
}

#[test]
fn missing_io_of_any_live_member_makes_aggregate_unknown() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path();
    proc_entry(p, 1000, 1, 1000, 50, (1, 0));
    proc_entry(p, 1001, 1000, 1000, 51, (1, 0));
    io(p, 1000, 10, 20);
    let mut t = Tracker::with_proc_root(p, 1000).unwrap();
    let a = t.sample(Instant::now());
    assert!(!a.io_complete);
    assert_eq!(a.read_bytes, None);
    assert_eq!(a.write_bytes, None);
    io(p, 1001, 1, 2);
    let b = t.sample(Instant::now());
    assert!(b.io_complete);
    assert_eq!(b.read_bytes, Some(11));
}

#[test]
fn new_descendant_of_reparented_known_member_is_attributed() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path();
    proc_entry(p, 1000, 1, 1000, 50, (1, 0));
    proc_entry(p, 1001, 1000, 1000, 51, (1, 0));
    proc_entry(p, 1002, 1001, 1000, 52, (1, 0));
    let mut t = Tracker::with_proc_root(p, 1000).unwrap();
    let t0 = Instant::now();
    assert_eq!(t.sample(t0).processes, 3);
    // 1001 exits; 1002 is reparented to init and has left the group, then forks 1005.
    std::fs::remove_dir_all(p.join("1001")).unwrap();
    proc_entry(p, 1002, 1, 1002, 52, (1, 0));
    proc_entry(p, 1005, 1002, 1002, 90, (500, 0));
    // Root is also gone: only known identities seed the walk.
    std::fs::remove_dir_all(p.join("1000")).unwrap();
    let b = t.sample(t0 + Duration::from_secs(1));
    assert!(b.root_lost);
    assert_eq!(b.processes, 2, "1002 (known) and its new child 1005");
    assert!(b.cpu_rate_estimated, "members exited between samples");
}

fn children(root: &Path, pid: u32, kids: &str) {
    let d = root
        .join(pid.to_string())
        .join("task")
        .join(pid.to_string());
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(d.join("children"), kids).unwrap();
}

#[test]
fn capped_scan_with_unseen_new_descendant_flags_rates_unknown() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path();
    proc_entry(p, 1000, 1, 1000, 50, (100, 0));
    proc_entry(p, 1001, 1000, 1000, 51, (100, 0));
    io(p, 1000, 10, 10);
    io(p, 1001, 5, 5);
    let mut t = Tracker::with_proc_root(p, 1000).unwrap();
    let t0 = Instant::now();
    let a = t.sample(t0);
    assert!(a.cpu_cores.is_none() && a.io_complete);
    for pid in 3000..3020 {
        proc_entry(p, pid, 1, pid, 70 + u64::from(pid), (1, 0));
    }
    // New descendant with a huge lifetime counter and no members IO visible.
    proc_entry(p, 1002, 1001, 1000, 90, (90_000, 0));
    t.set_caps(1, 100);
    let b = t.sample(t0 + Duration::from_secs(1));
    assert!(b.truncated);
    assert!(!b.io_complete, "unseen members: IO is not complete");
    assert_eq!(b.cpu_cores, None);
    assert_eq!(b.read_bytes_per_sec, None);
    assert_eq!(b.write_bytes_per_sec, None);
    assert_eq!(b.read_bytes, None);
}

#[test]
fn direct_children_chain_is_found_when_ppid_scan_is_capped() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path();
    proc_entry(p, 1000, 1, 1000, 50, (1, 0));
    proc_entry(p, 1001, 1000, 1001, 51, (1, 0));
    proc_entry(p, 1002, 1001, 1002, 52, (1, 0));
    proc_entry(p, 1003, 1002, 1003, 53, (1, 0));
    children(p, 1000, "1001 ");
    children(p, 1001, "1002 ");
    children(p, 1002, "1003 ");
    for pid in 3000..3020 {
        proc_entry(p, pid, 1, pid, 70 + u64::from(pid), (1, 0));
    }
    let mut t = Tracker::with_proc_root(p, 1000).unwrap();
    t.set_caps(1, 100);
    let a = t.sample(Instant::now());
    assert!(a.truncated);
    assert_eq!(a.processes, 4, "chain found via task children files");
}

#[test]
fn direct_children_discovery_respects_tracked_cap() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path();
    proc_entry(p, 1000, 1, 1000, 50, (1, 0));
    let mut kids = String::new();
    for pid in 1001..1020 {
        proc_entry(p, pid, 1000, 1001, u64::from(pid), (1, 0));
        kids += &pid.to_string();
        kids.push(' ');
    }
    children(p, 1000, &kids);
    let mut t = Tracker::with_proc_root(p, 1000).unwrap();
    t.set_caps(1, 5);
    let a = t.sample(Instant::now());
    assert!(a.truncated);
    assert!(a.processes <= 5);
}

#[test]
fn uncapped_sample_after_truncation_does_not_spike_from_lifetime_totals() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path();
    proc_entry(p, 1000, 1, 1000, 50, (100, 0));
    io(p, 1000, 10, 10);
    let mut t = Tracker::with_proc_root(p, 1000).unwrap();
    let t0 = Instant::now();
    t.sample(t0);
    for pid in 3000..3020 {
        proc_entry(p, pid, 1, pid, 70 + u64::from(pid), (1, 0));
    }
    t.set_caps(1, 100);
    assert!(t.sample(t0 + Duration::from_secs(1)).truncated);
    // A long-lived descendant appears; once uncapped it is discovered with lifetime totals.
    proc_entry(p, 1002, 1000, 1000, 90, (90_000, 0));
    io(p, 1002, 1 << 30, 1 << 30);
    t.set_caps(100_000, 100);
    let c = t.sample(t0 + Duration::from_secs(2));
    assert!(!c.truncated);
    assert_eq!(c.cpu_cores, None, "baseline reset after truncation");
    assert_eq!(c.read_bytes_per_sec, None);
    assert_eq!(c.write_bytes_per_sec, None);
    proc_entry(p, 1002, 1000, 1000, 90, (90_100, 0));
    let d = t.sample(t0 + Duration::from_secs(3));
    let cores = d.cpu_cores.unwrap();
    assert!((cores - 1.0).abs() < 1e-6, "{cores}");
}

// Exactly representable: 10 * 2^30.
const TEN_GIB_F: f64 = 10_737_418_240.0;

#[test]
fn large_io_rate_is_not_clamped_to_u32() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path();
    proc_entry(p, 1000, 1, 1000, 50, (1, 0));
    io(p, 1000, 0, 0);
    let mut t = Tracker::with_proc_root(p, 1000).unwrap();
    let t0 = Instant::now();
    t.sample(t0);
    let ten_gib: u64 = 10 * 1024 * 1024 * 1024;
    io(p, 1000, ten_gib, 2 * ten_gib);
    let b = t.sample(t0 + Duration::from_secs(2));
    let r = b.read_bytes_per_sec.unwrap();
    assert!((r - TEN_GIB_F / 2.0).abs() < 1.0, "{r}");
    assert!((b.write_bytes_per_sec.unwrap() - TEN_GIB_F).abs() < 1.0);
}

#[cfg(not(target_os = "linux"))]
#[test]
fn process_accounting_is_explicitly_unavailable_off_linux() {
    assert_eq!(
        multiview_webengine::service::Accounting::current(),
        multiview_webengine::service::Accounting::Unavailable
    );
    assert!(multiview_webengine::resource::Tracker::new(std::process::id()).is_none());
}
