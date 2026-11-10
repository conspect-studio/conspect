//! Deterministic fetch tests: the REAL embedded `fetch_host.cjs` under real node,
//! with fake `yt-dlp` and fake egress-wrapper executables. No network.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::print_stderr)]
use multiview_webengine::{
    artifact::{ArtifactStore, Limits, Provenance},
    fetch::{run_fetch, run_fetch_with_pending, Codec, Fetch, FetchProgress, FetchTools},
    runner::{is_commit_uncertain, Control},
};
use std::{
    io::{ErrorKind, Read},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex,
    },
    time::{Duration, Instant},
};

const TOKEN: &str = "test-token-0123456789";
// Serialises script-write + spawn so no fork inherits a writable script fd (ETXTBSY).
static SERIAL: Mutex<()> = Mutex::new(());

fn exe(path: &Path, body: &str) {
    std::fs::write(path, body).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}
fn node() -> Option<PathBuf> {
    let found = std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|d| d.join("node"))
        .find(|p| p.is_file());
    if found.is_none() {
        assert!(
            std::env::var("CONSPECT_REQUIRE_REAL_BROWSER").as_deref() != Ok("1"),
            "node required"
        );
        eprintln!("SKIPPED fetch test: node not found");
    }
    found
}
struct Env {
    dir: tempfile::TempDir,
    store: ArtifactStore,
    tools: FetchTools,
}
/// `ytdlp` is the body of a fake yt-dlp (`$*` contains argv; `-J` = metadata run).
fn env(ytdlp: &str, probe_token: &str) -> Option<Env> {
    let node = node()?;
    let _g = SERIAL.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let w = dir.path().join("w");
    std::fs::create_dir(&w).unwrap();
    let (y, f, wr) = (w.join("ytdlp"), w.join("ffmpeg"), w.join("wrapper"));
    exe(&y, &format!("#!/bin/sh\nW='{}'\n{ytdlp}\n", w.display()));
    // EXPLICIT FAKE validator (always accepts); real decode validation tests use real_ffmpeg().
    exe(&f, "#!/bin/sh\n# fake-validator\n");
    exe(
        &wr,
        &format!(
            "#!/bin/sh\nif [ \"$1\" = --probe ]; then echo 'multiview-egress-boundary/1 {probe_token}'; exit 0; fi\n[ \"$1\" = -- ] || exit 9\nshift\nexec \"$@\"\n"
        ),
    );
    let s = dir.path().join("s");
    std::fs::create_dir(&s).unwrap();
    std::fs::set_permissions(&s, std::fs::Permissions::from_mode(0o700)).unwrap();
    let store = ArtifactStore::open(
        &s,
        Limits {
            bytes: 100_000_000,
            count: 20,
        },
    )
    .unwrap();
    Some(Env {
        dir,
        store,
        tools: FetchTools {
            node,
            ytdlp: y,
            ffmpeg: f,
            egress_wrapper: wr,
            egress_token: TOKEN.into(),
        },
    })
}
fn req() -> Fetch {
    Fetch {
        url: "https://www.youtube.com/watch?v=abc".into(),
        host_allowlist: Fetch::youtube_allowlist(),
        codec: Codec::default(),
        max_height: 720,
        max_duration_secs: 600,
        extract_metadata: true,
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
fn ctl(flag: &AtomicBool, secs: u64) -> Control<'_> {
    Control {
        cancelled: flag,
        deadline: Instant::now() + Duration::from_secs(secs),
    }
}
const OK: &str = r#"
case "$*" in
 *" -J "*) printf '{"title":"T\\u0007itle","uploader":"U","duration":12.5}'; exit 0;;
esac
echo "$$" > "$W/pid"
printf 'MVPROG 5 10\n' >&2
printf 'hello-bytes'
exit 0
"#;

#[test]
fn success_commits_exact_bytes_with_meta_and_progress() {
    let Some(mut e) = env(OK, TOKEN) else { return };
    let mut seen = Vec::new();
    let mut hook = None;
    let (art, report) = run_fetch_with_pending(
        &mut e.store,
        prov(),
        1_000_000,
        &e.tools,
        &req(),
        ctl(&AtomicBool::new(false), 60),
        |_| true,
        |id| {
            hook = Some(id.as_str().to_owned());
            Ok(())
        },
        |p| seen.push(p),
    )
    .unwrap();
    assert_eq!(hook.as_deref(), Some(art.id.as_str()));
    assert_eq!(art.bytes, 11);
    assert_eq!(report.bytes, 11);
    let m = report.meta.unwrap();
    assert_eq!(m.title.as_deref(), Some("Title"));
    assert_eq!(m.duration, Some(12.5));
    assert!(seen.contains(&FetchProgress {
        downloaded: Some(5),
        total: Some(10)
    }));
    let mut lease = e.store.pin(&art.id, "o", |_| true).unwrap();
    let mut b = String::new();
    lease.read_to_string(&mut b).unwrap();
    assert_eq!(b, "hello-bytes");
    assert_eq!(art.provenance.job, "j");
}

#[test]
fn missing_or_wrong_egress_wrapper_fails_closed() {
    let Some(mut e) = env(OK, "wrong-token-0123456789") else {
        return;
    };
    let err = run_fetch(
        &mut e.store,
        prov(),
        1000,
        &e.tools,
        &req(),
        ctl(&AtomicBool::new(false), 60),
        |_| true,
        |_| {},
    )
    .unwrap_err();
    assert!(err.to_string().contains("egress_required"), "{err}");
    assert!(e.store.snapshot(|_| true).is_empty());
    assert!(!e.dir.path().join("w/pid").exists());

    let Some(mut e) = env(OK, TOKEN) else { return };
    e.tools.egress_wrapper = e.dir.path().join("w/absent");
    let mut reserved = false;
    let err = run_fetch_with_pending(
        &mut e.store,
        prov(),
        1000,
        &e.tools,
        &req(),
        ctl(&AtomicBool::new(false), 60),
        |_| true,
        |_| {
            reserved = true;
            Ok(())
        },
        |_| {},
    )
    .unwrap_err();
    assert!(!reserved && e.store.snapshot(|_| true).is_empty(), "{err}");
    e.tools.egress_wrapper = PathBuf::from("relative/wrapper");
    assert!(e.tools.validate().is_err());
    e.tools.egress_wrapper = e.dir.path().join("w/wrapper");
    e.tools.egress_token = "short".into();
    assert!(e.tools.validate().is_err());
}

#[test]
fn reservation_hook_failure_runs_nothing() {
    let Some(mut e) = env(OK, TOKEN) else { return };
    let err = run_fetch_with_pending(
        &mut e.store,
        prov(),
        1000,
        &e.tools,
        &req(),
        ctl(&AtomicBool::new(false), 60),
        |_| true,
        |_| Err(std::io::Error::other("journal")),
        |_| {},
    )
    .unwrap_err();
    assert_eq!(err.to_string(), "journal");
    assert!(!e.dir.path().join("w/pid").exists());
    assert!(e.store.snapshot(|_| true).is_empty());
    assert!(!is_commit_uncertain(&err));
}

#[test]
fn url_and_allowlist_validation_precede_any_work() {
    let Some(mut e) = env(OK, TOKEN) else { return };
    for url in [
        "http://www.youtube.com/watch?v=a",
        "https://evil.example/x",
        "https://user@youtube.com/x",
        "https://youtube.com:8443/x",
        "https://127.0.0.1/x",
        "https://[::1]/x",
        "https://youtube.com.evil.com/x",
        "https://youtube.com/a b",
    ] {
        let mut r = req();
        r.url = url.into();
        assert!(r.validate().is_err(), "{url}");
        assert!(run_fetch(
            &mut e.store,
            prov(),
            1000,
            &e.tools,
            &r,
            ctl(&AtomicBool::new(false), 60),
            |_| true,
            |_| {}
        )
        .is_err());
    }
    let mut r = req();
    r.host_allowlist = vec!["example.com".into()];
    r.url = "https://example.com/x".into();
    assert!(r.validate().is_err(), "allowlist must include YouTube");
    assert!(req().validate().is_ok());
    assert!(!e.dir.path().join("w/pid").exists());
}

#[test]
fn cancellation_stops_group_and_publishes_nothing() {
    let Some(mut e) = env(
        r#"case "$*" in *" -J "*) exit 1;; esac
echo "$$" > "$W/pid"
printf 'MVPROG 1 10\n' >&2
printf 'partial'
sleep 60"#,
        TOKEN,
    ) else {
        return;
    };
    let flag = AtomicBool::new(false);
    let start = Instant::now();
    let err = run_fetch(
        &mut e.store,
        prov(),
        1_000_000,
        &e.tools,
        &req(),
        ctl(&flag, 60),
        |_| true,
        |_| flag.store(true, Ordering::Release),
    )
    .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Interrupted);
    assert!(start.elapsed() < Duration::from_secs(30));
    assert!(e.store.snapshot(|_| true).is_empty());
    let pid = std::fs::read_to_string(e.dir.path().join("w/pid")).unwrap();
    let mut gone = false;
    for _ in 0..100 {
        if !Path::new(&format!("/proc/{}", pid.trim())).exists() {
            gone = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(gone, "downloader survived cancellation");
}

#[test]
fn quota_overrun_and_downloader_failure_never_publish() {
    let Some(mut e) = env(
        r#"case "$*" in *" -J "*) exit 1;; esac
head -c 300000 /dev/zero"#,
        TOKEN,
    ) else {
        return;
    };
    let err = run_fetch(
        &mut e.store,
        prov(),
        1000,
        &e.tools,
        &req(),
        ctl(&AtomicBool::new(false), 60),
        |_| true,
        |_| {},
    )
    .unwrap_err();
    eprintln!("quota error: {err}");
    assert!(e.store.snapshot(|_| true).is_empty());

    let Some(mut e) = env(
        r#"case "$*" in *" -J "*) exit 1;; esac
printf 'some bytes'; exit 3"#,
        TOKEN,
    ) else {
        return;
    };
    let err = run_fetch(
        &mut e.store,
        prov(),
        1000,
        &e.tools,
        &req(),
        ctl(&AtomicBool::new(false), 60),
        |_| true,
        |_| {},
    )
    .unwrap_err();
    assert!(err.to_string().contains("downloader_failed"), "{err}");
    assert!(e.store.snapshot(|_| true).is_empty());
}

#[test]
fn noisy_unbounded_progress_stream_is_bounded_and_succeeds() {
    let Some(mut e) = env(
        r#"case "$*" in *" -J "*) exit 1;; esac
head -c 3000000 /dev/zero | tr '\0' 'x' >&2
i=0; while [ $i -lt 300 ]; do printf 'MVPROG %s 300\n' "$i" >&2; i=$((i+1)); done
printf 'ok-bytes'"#,
        TOKEN,
    ) else {
        return;
    };
    let mut n = 0;
    let (art, rep) = run_fetch(
        &mut e.store,
        prov(),
        1_000_000,
        &e.tools,
        &req(),
        ctl(&AtomicBool::new(false), 90),
        |_| true,
        |_| n += 1,
    )
    .unwrap();
    assert_eq!((art.bytes, rep.bytes), (8, 8));
    assert!(rep.meta.is_none());
    assert!(n <= 300);
}

fn real_ffmpeg() -> Option<PathBuf> {
    let found = ["/usr/bin/ffmpeg", "/usr/local/bin/ffmpeg"]
        .iter()
        .map(PathBuf::from)
        .find(|p| p.is_file());
    if found.is_none() {
        assert!(
            std::env::var("CONSPECT_REQUIRE_REAL_BROWSER").as_deref() != Ok("1"),
            "ffmpeg required"
        );
        eprintln!("SKIPPED real-validation test: ffmpeg not found");
    }
    found
}
/// Tiny local MP4 generated by the real ffmpeg (lavfi source; no network).
fn gen_mp4(ff: &Path, out: &Path) {
    let st = std::process::Command::new(ff)
        .args(["-nostdin", "-v", "error", "-y", "-f", "lavfi", "-i"])
        .arg("testsrc=size=64x64:rate=5:duration=1")
        .args(["-pix_fmt", "yuv420p", "-c:v", "mpeg4", "-f", "mp4"])
        .arg(out)
        .status()
        .unwrap();
    assert!(st.success());
}
const SERVE_FILE: &str = r#"case "$*" in *" -J "*) exit 1;; esac
cat "$W/payload""#;

#[test]
fn real_ffmpeg_validates_generated_mp4_and_publishes_exact_bytes() {
    let Some(ff) = real_ffmpeg() else { return };
    let Some(mut e) = env(SERVE_FILE, TOKEN) else {
        return;
    };
    e.tools.ffmpeg = ff.clone(); // REAL validator, not the fake
    let payload = e.dir.path().join("w/payload");
    gen_mp4(&ff, &payload);
    let want = std::fs::read(&payload).unwrap();
    let n = u64::try_from(want.len()).unwrap();
    let (art, rep) = run_fetch(
        &mut e.store,
        prov(),
        1_000_000,
        &e.tools,
        &req(),
        ctl(&AtomicBool::new(false), 60),
        |_| true,
        |_| {},
    )
    .unwrap();
    assert_eq!((art.bytes, rep.bytes), (n, n));
    let mut lease = e.store.pin(&art.id, "o", |_| true).unwrap();
    let mut got = Vec::new();
    lease.read_to_end(&mut got).unwrap();
    assert_eq!(got, want);
}

#[test]
fn real_ffmpeg_rejects_html_json_and_fake_bytes_unpublished() {
    let Some(ff) = real_ffmpeg() else { return };
    for body in [
        "<!doctype html><html><body>blocked</body></html>",
        "{\"error\":\"nope\"}",
        "FAKEBYTES-not-media-0123456789",
    ] {
        let Some(mut e) = env(SERVE_FILE, TOKEN) else {
            return;
        };
        e.tools.ffmpeg = ff.clone();
        std::fs::write(e.dir.path().join("w/payload"), body).unwrap();
        let mut hooked = 0;
        let err = run_fetch_with_pending(
            &mut e.store,
            prov(),
            1_000_000,
            &e.tools,
            &req(),
            ctl(&AtomicBool::new(false), 60),
            |_| true,
            |_| {
                hooked += 1;
                Ok(())
            },
            |_| {},
        )
        .unwrap_err();
        assert!(err.to_string().contains("invalid_media"), "{err}");
        assert!(e.store.snapshot(|_| true).is_empty(), "unpublished");
        assert_eq!(hooked, 1);
    }
}

#[test]
fn real_ffmpeg_rejects_ffconcat_playlist_pointing_at_valid_sibling_unpublished() {
    let Some(ff) = real_ffmpeg() else { return };
    let Some(mut e) = env(
        "cp \"$W/sibling.mp4\" planted.mp4; cat \"$W/payload\"",
        TOKEN,
    ) else {
        return;
    };
    e.tools.ffmpeg = ff.clone();
    let sib = e.dir.path().join("w/sibling.mp4");
    gen_mp4(&ff, &sib);
    std::fs::write(
        e.dir.path().join("w/payload"),
        "ffconcat version 1.0\nfile 'planted.mp4'\n",
    )
    .unwrap();
    let err = run_fetch_with_pending(
        &mut e.store,
        prov(),
        1_000_000,
        &e.tools,
        &req(),
        ctl(&AtomicBool::new(false), 60),
        |_| true,
        |_| Ok(()),
        |_| {},
    )
    .unwrap_err();
    assert!(err.to_string().contains("invalid_media"), "{err}");
    assert!(e.store.snapshot(|_| true).is_empty(), "unpublished");
}

#[test]
fn fake_validator_still_accepts_arbitrary_bytes_proving_real_test_is_distinct() {
    // Same arbitrary payload: fake validator (explicit) accepts, real ffmpeg rejects above.
    let Some(mut e) = env(SERVE_FILE, TOKEN) else {
        return;
    };
    std::fs::write(
        e.dir.path().join("w/payload"),
        "FAKEBYTES-not-media-0123456789",
    )
    .unwrap();
    let (art, _) = run_fetch(
        &mut e.store,
        prov(),
        1_000_000,
        &e.tools,
        &req(),
        ctl(&AtomicBool::new(false), 60),
        |_| true,
        |_| {},
    )
    .unwrap();
    assert_eq!(art.bytes, 30);
}
