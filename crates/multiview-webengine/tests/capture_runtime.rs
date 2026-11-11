#![allow(clippy::unwrap_used, clippy::expect_used, clippy::print_stderr)]
use multiview_webengine::{
    artifact::{ArtifactStore, Limits, Provenance},
    capture::{Background, Capture, Tools},
    runner::{run_capture, Control},
};
use std::{
    io::Read,
    path::PathBuf,
    sync::atomic::AtomicBool,
    time::{Duration, Instant},
};

/// Real-tool discovery, never hardcoded: `CONSPECT_WEB_NODE`, `CONSPECT_WEB_BROWSER`
/// and `CONSPECT_WEB_FFMPEG` override; otherwise node/ffmpeg are looked up on
/// `PATH` and the browser under `$HOME/.cache/ms-playwright/chromium_headless_shell-*`.
/// When any tool is unavailable the real-browser tests report that loudly and
/// return, unless `CONSPECT_REQUIRE_REAL_BROWSER=1`, which makes absence a failure
/// (set it in any environment that provisions the tools). Deterministic fake
/// producer coverage lives in `runner_fake.rs` and always runs.
fn which(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}
fn discover_browser() -> Option<PathBuf> {
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
}
fn tools() -> Option<Tools> {
    let var = |k: &str| std::env::var_os(k).map(PathBuf::from);
    let found = (|| {
        Some(Tools {
            node: var("CONSPECT_WEB_NODE").or_else(|| which("node"))?,
            browser: var("CONSPECT_WEB_BROWSER").or_else(discover_browser)?,
            ffmpeg: var("CONSPECT_WEB_FFMPEG").or_else(|| which("ffmpeg"))?,
        })
    })();
    if found.is_none() {
        assert!(
            std::env::var("CONSPECT_REQUIRE_REAL_BROWSER").as_deref() != Ok("1"),
            "real browser tools required but not found"
        );
        eprintln!("SKIPPED real-browser test: node/browser/ffmpeg not found");
    }
    found
}
fn capture() -> Capture {
    Capture {width:320,height:180,fps:5,frames:5,windy: None,background: Background::Opaque,
        document:"<body style='margin:0;background:black'><canvas id='c' width='320' height='180'></canvas><script>window.mvCaptureFrame=n=>{const x=c.getContext('2d');x.fillStyle=n%2?'red':'blue';x.fillRect(0,0,320,180)}</script>".to_owned()}
}
fn provenance(generation: u64) -> Provenance {
    Provenance {
        owner: "owner".into(),
        fill: "radar".into(),
        job: format!("job-{generation}"),
        generation,
    }
}

#[test]
fn actual_browser_bytes_are_committed_and_pinnable_only_after_success() {
    let Some(tools) = tools() else { return };
    let directory = tempfile::tempdir().unwrap();
    let mut store = ArtifactStore::open(
        directory.path(),
        Limits {
            bytes: 10_000_000,
            count: 20,
        },
    )
    .unwrap();
    let output = run_capture(
        &mut store,
        provenance(1),
        1_000_000,
        &tools,
        &capture(),
        Control {
            cancelled: &AtomicBool::new(false),
            deadline: Instant::now() + Duration::from_secs(30),
        },
        |_| true,
    )
    .unwrap();
    assert!(output.bytes > 100);
    let mut lease = store.pin(&output.id, "owner", |_| true).unwrap();
    let mut bytes = Vec::new();
    lease.read_to_end(&mut bytes).unwrap();
    assert!(bytes.windows(4).any(|s| s == b"ftyp"));
    assert!(bytes.windows(4).any(|s| s == b"moov"));
    assert_eq!(u64::try_from(bytes.len()).unwrap(), output.bytes);
}

#[test]
fn cancellation_and_tool_failure_never_publish_success() {
    let Some(tools) = tools() else { return };
    let directory = tempfile::tempdir().unwrap();
    let mut store = ArtifactStore::open(
        directory.path(),
        Limits {
            bytes: 10_000_000,
            count: 20,
        },
    )
    .unwrap();
    assert!(run_capture(
        &mut store,
        provenance(1),
        1_000_000,
        &tools,
        &capture(),
        Control {
            cancelled: &AtomicBool::new(true),
            deadline: Instant::now() + Duration::from_secs(30)
        },
        |_| true
    )
    .is_err());
    let mut missing = Tools {
        node: tools.node.clone(),
        browser: tools.browser.clone(),
        ffmpeg: tools.ffmpeg.clone(),
    };
    missing.browser = PathBuf::from("/no-such-browser");
    assert!(run_capture(
        &mut store,
        provenance(2),
        1_000_000,
        &missing,
        &capture(),
        Control {
            cancelled: &AtomicBool::new(false),
            deadline: Instant::now() + Duration::from_secs(5)
        },
        |_| true
    )
    .is_err());
}
