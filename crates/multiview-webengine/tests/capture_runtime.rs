#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::print_stderr)]
use multiview_webengine::{
    artifact::{ArtifactStore, Limits, Provenance},
    capture::{Background, Capture, CaptureMode, CaptureOptions, StepTiming, Tools},
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
            gl: multiview_webengine::capture::BrowserGl::Default,
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
        document:"<body style='margin:0;background:black'><canvas id='c' width='320' height='180'></canvas><script>window.mvCaptureFrame=n=>{const x=c.getContext('2d');x.fillStyle=n%2?'red':'blue';x.fillRect(0,0,320,180)}</script>".to_owned(), capture: multiview_webengine::capture::CaptureOptions::default()}
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
        gl: multiview_webengine::capture::BrowserGl::Default,
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

/// Fake radar page exposing the `W.store` step contract the Windy template uses.
/// Six steps paint red, blue, blue, green, green, red: two consecutive duplicates.
fn fake_radar_steps() -> Capture {
    let document = "<body style='margin:0'><canvas id='c' width='320' height='180'></canvas><script>\
        const colors={1000:'red',2000:'blue',3000:'blue',4000:'green',5000:'green',6000:'red'};let ts=null;\
        window.W={store:{get:k=>k==='radarCalendar'?{timestamps:[6000,1000,2000,3000,4000,5000]}:k==='timestamp'?ts:null,\
        set:(k,v)=>{if(k!=='timestamp')return;setTimeout(()=>{const x=c.getContext('2d');x.fillStyle=colors[v];\
        x.fillRect(0,0,320,180);ts=v;},50);}}};</script>"
        .to_owned();
    Capture {
        width: 320,
        height: 180,
        fps: 10,
        frames: 2,
        windy: None,
        background: Background::Opaque,
        document,
        capture: CaptureOptions {
            mode: Some(CaptureMode::Steps),
            timing: StepTiming::Equal,
            frame_ms: 200,
            end_hold_ms: 300,
            max_steps: 12,
        },
    }
}

#[test]
fn steps_capture_drives_the_timeline_drops_duplicates_and_holds() {
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
        2_000_000,
        &tools,
        &fake_radar_steps(),
        Control {
            cancelled: &AtomicBool::new(false),
            deadline: Instant::now() + Duration::from_secs(60),
        },
        |_| true,
    )
    .unwrap();
    let mut bytes = Vec::new();
    store
        .pin(&output.id, "owner", |_| true)
        .unwrap()
        .read_to_end(&mut bytes)
        .unwrap();
    let mov = directory.path().join("steps.mov");
    std::fs::write(&mov, &bytes).unwrap();
    // Decode 1x1 RGB per frame: red, blue, green, red kept (2 frames each at
    // 200 ms / 10 fps) plus a 3-frame hold of the last step = 11 frames.
    let out = std::process::Command::new(&tools.ffmpeg)
        .args(["-nostdin", "-loglevel", "error", "-i"])
        .arg(&mov)
        .args([
            "-vf",
            "scale=1:1",
            "-f",
            "rawvideo",
            "-pix_fmt",
            "rgb24",
            "-",
        ])
        .output()
        .unwrap();
    assert!(out.status.success());
    let dominant: Vec<char> = out
        .stdout
        .chunks(3)
        .map(
            |p| match p.iter().enumerate().max_by_key(|(_, v)| **v).unwrap().0 {
                0 => 'r',
                1 => 'g',
                _ => 'b',
            },
        )
        .collect();
    assert_eq!(dominant.iter().collect::<String>(), "rrbbggrrrrr");
}

#[test]
fn steps_capture_with_a_single_distinct_frame_fails_clearly() {
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
    let mut capture = fake_radar_steps();
    capture.document = capture
        .document
        .replace("'blue'", "'red'")
        .replace("'green'", "'red'");
    assert!(run_capture(
        &mut store,
        provenance(1),
        2_000_000,
        &tools,
        &capture,
        Control {
            cancelled: &AtomicBool::new(false),
            deadline: Instant::now() + Duration::from_secs(60),
        },
        |_| true,
    )
    .is_err());
}
