#![cfg(unix)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stderr,
    clippy::indexing_slicing,
    clippy::cast_possible_truncation,
    clippy::as_conversions
)]
//! Real-browser alpha proof: transparent authored HTML through `run_capture`,
//! decoded by real ffmpeg to straight RGBA, plus fail-closed validation.
use multiview_webengine::{
    artifact::{ArtifactStore, Limits, Provenance},
    capture::{Background, Capture, Tools, WindyView},
    runner::{run_capture, Control},
};
use std::{
    io::{Read, Write},
    path::PathBuf,
    process::{Command, Stdio},
    sync::atomic::AtomicBool,
    time::{Duration, Instant},
};

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
const W: usize = 320;
const H: usize = 180;
fn page(background: Background) -> Capture {
    // Left third empty, middle opaque red, right third 50% blue.
    Capture {
        width: W as u32,
        height: H as u32,
        fps: 5,
        frames: 3,
        windy: None,
        background,
        document: "<body style='margin:0;background:transparent'>\
<div style='position:absolute;left:107px;top:0;width:107px;height:180px;background:rgb(255,0,0)'></div>\
<div style='position:absolute;left:214px;top:0;width:106px;height:180px;background:rgba(0,0,255,0.5)'></div></body>"
            .to_owned(),
        capture: multiview_webengine::capture::CaptureOptions::default(),
    }
}
fn provenance() -> Provenance {
    Provenance {
        owner: "o".into(),
        fill: "f".into(),
        job: "j".into(),
        generation: 1,
    }
}
fn run(tools: &Tools, c: &Capture) -> Vec<u8> {
    let dir = tempfile::tempdir().unwrap();
    let mut store = ArtifactStore::open(
        dir.path(),
        Limits {
            bytes: 50_000_000,
            count: 4,
        },
    )
    .unwrap();
    let out = run_capture(
        &mut store,
        provenance(),
        1_000_000,
        tools,
        c,
        Control {
            cancelled: &AtomicBool::new(false),
            deadline: Instant::now() + Duration::from_secs(60),
        },
        |_| true,
    )
    .unwrap();
    let mut bytes = Vec::new();
    store
        .pin(&out.id, "o", |_| true)
        .unwrap()
        .read_to_end(&mut bytes)
        .unwrap();
    bytes
}
/// Real ffmpeg: decode MOV to raw RGBA (straight), optionally compositing over a colour.
fn decode(tools: &Tools, mov: &[u8], over: Option<&str>) -> Vec<u8> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.mov");
    std::fs::write(&path, mov).unwrap();
    let mut cmd = Command::new(&tools.ffmpeg);
    cmd.args(["-nostdin", "-loglevel", "error", "-i"])
        .arg(&path);
    if let Some(color) = over {
        cmd.args([
            "-f",
            "lavfi",
            "-i",
            &format!("color=c={color}:s={W}x{H}:r=5"),
            "-filter_complex",
            "[1:v][0:v]overlay=shortest=1:format=auto,format=rgba",
        ]);
    } else {
        cmd.args(["-vf", "format=rgba"]);
    }
    cmd.args(["-frames:v", "1", "-f", "rawvideo", "-pix_fmt", "rgba", "-"])
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    let mut child = cmd.spawn().unwrap();
    let mut raw = Vec::new();
    child.stdout.take().unwrap().read_to_end(&mut raw).unwrap();
    assert!(child.wait().unwrap().success());
    assert_eq!(raw.len(), W * H * 4);
    raw
}
fn px(raw: &[u8], x: usize, y: usize) -> [u8; 4] {
    let i = (y * W + x) * 4;
    [raw[i], raw[i + 1], raw[i + 2], raw[i + 3]]
}
fn near(a: u8, b: u8, tol: u8) -> bool {
    a.abs_diff(b) <= tol
}
fn pix_fmt(tools: &Tools, mov: &[u8]) -> String {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.mov");
    std::fs::write(&path, mov).unwrap();
    let out = Command::new(&tools.ffmpeg)
        .args(["-hide_banner", "-i"])
        .arg(&path)
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn transparent_html_roundtrips_straight_alpha_through_ffv1_mov() {
    let Some(tools) = tools() else { return };
    let mov = run(&tools, &page(Background::Transparent));
    let probe = pix_fmt(&tools, &mov);
    assert!(
        probe.contains("ffv1") && probe.contains("yuva420p"),
        "{probe}"
    );
    let raw = decode(&tools, &mov, None);
    let empty = px(&raw, 50, 90);
    let red = px(&raw, 160, 90);
    let blue = px(&raw, 270, 90);
    assert_eq!(empty[3], 0, "empty must be alpha 0");
    assert_eq!(red[3], 255, "opaque must be alpha 255");
    assert!(near(blue[3], 128, 3), "half alpha, got {}", blue[3]);
    // Straight (non-premultiplied) colour: red stays pure, blue stays ~255.
    assert!(red[0] >= 250 && red[1] <= 5 && red[2] <= 5, "{red:?}");
    assert!(
        blue[2] >= 245 && blue[0] <= 10 && blue[1] <= 10,
        "straight blue expected, got {blue:?}"
    );
    // Compositing over dark and light: result must equal a*src + (1-a)*bg once.
    let dark = decode(&tools, &mov, Some("black"));
    let light = decode(&tools, &mov, Some("white"));
    let (bd, bl) = (px(&dark, 270, 90), px(&light, 270, 90));
    assert!(near(bd[2], 128, 6) && bd[0] <= 6, "dark over: {bd:?}");
    assert!(
        near(bl[0], 127, 6) && near(bl[2], 255, 6),
        "light over: {bl:?}"
    );
    assert_eq!(px(&dark, 50, 90)[..3], [0, 0, 0]);
    assert!(px(&light, 50, 90)[..3].iter().all(|c| *c >= 250));
    assert_eq!(px(&dark, 160, 90)[..3], px(&light, 160, 90)[..3]);
}

#[test]
fn opaque_default_stays_yuv420p_without_alpha() {
    let Some(tools) = tools() else { return };
    let mov = run(&tools, &page(Background::Opaque));
    let probe = pix_fmt(&tools, &mov);
    assert!(
        probe.contains("yuv420p") && !probe.contains("yuva420p"),
        "{probe}"
    );
    let raw = decode(&tools, &mov, None);
    assert_eq!(px(&raw, 50, 90)[3], 255);
}

#[test]
fn background_defaults_opaque_and_windy_transparent_is_rejected() {
    let c: Capture =
        serde_json::from_str(r#"{"width":320,"height":180,"fps":5,"frames":3,"document":"x"}"#)
            .unwrap();
    assert!(matches!(c.background, Background::Opaque));
    let mut w = page(Background::Transparent);
    w.document.clear();
    w.width = 1280;
    w.height = 720;
    w.windy = Some(WindyView {
        lat: 0.0,
        lon: 0.0,
        zoom: 5,
        overlay: "radar".into(),
        terms_accepted: true,
        require_premium: false,
        credentials: None,
    });
    assert!(w.validate().is_err());
    w.background = Background::Opaque;
    assert!(w.validate().is_ok());
    let _ = std::io::stderr().flush();
}

#[test]
fn output_is_tagged_srgb_bt709_limited() {
    let Some(tools) = tools() else { return };
    let mov = run(&tools, &page(Background::Transparent));
    let probe = pix_fmt(&tools, &mov);
    assert!(probe.contains("bt709"), "{probe}");
    assert!(probe.contains("tv") || probe.contains("limited"), "{probe}");
}

#[test]
fn encoder_that_strips_alpha_fails_closed_and_publishes_nothing() {
    use std::os::unix::fs::PermissionsExt;
    let Some(mut tools) = tools() else { return };
    let dir = tempfile::tempdir().unwrap();
    let wrapper = dir.path().join("ffmpeg-noalpha");
    let script = format!(
        "#!/bin/sh\nfor a in \"$@\"; do shift; case \"$a\" in yuva420p) a=yuv420p;; esac; set -- \"$@\" \"$a\"; done\nexec '{}' \"$@\"\n",
        tools.ffmpeg.display()
    );
    std::fs::write(&wrapper, script).unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
    tools.ffmpeg = wrapper;
    let store_dir = tempfile::tempdir().unwrap();
    let mut store = ArtifactStore::open(
        store_dir.path(),
        Limits {
            bytes: 50_000_000,
            count: 4,
        },
    )
    .unwrap();
    let res = run_capture(
        &mut store,
        provenance(),
        1_000_000,
        &tools,
        &page(Background::Transparent),
        Control {
            cancelled: &AtomicBool::new(false),
            deadline: Instant::now() + Duration::from_secs(60),
        },
        |_| true,
    );
    assert!(res.is_err(), "alpha-stripped encode must be rejected");
}
