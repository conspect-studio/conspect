//! Subprocess-shell tests for the `YouTube` resolver (issue #420): argument
//! vector, default-clients-first with one fallback, `--js-runtimes` handling,
//! and the operator-visible error. A fake `yt-dlp` shell script stands in for
//! the real binary, so there is no network and no real `yt-dlp`.
#![cfg(all(feature = "youtube", unix))]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use multiview_input::youtube::{
    find_on_path, resolve_args, resolve_url, ResolverConfig, YoutubeError, FALLBACK_PLAYER_CLIENTS,
};

const LIVE_JSON: &str = r#"{"live_status":"is_live","formats":[{"protocol":"m3u8_native","manifest_url":"https://manifest.googlevideo.com/api/manifest/hls_variant/expire/1893456000/index.m3u8"}]}"#;

/// Serialises the tests that write and then exec a fake binary: a file being
/// written in one test while another test forks inherits the write fd until
/// `exec`, and Linux then refuses to exec the script ("Text file busy").
static EXEC_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Write an executable fake `yt-dlp` that logs its argv (one line per call)
/// to `calls.log` and then runs `body` (POSIX sh, `$*` is the argv).
fn fake_yt_dlp(dir: &Path, body: &str) -> PathBuf {
    let path = dir.join("yt-dlp");
    let log = dir.join("calls.log");
    let script = format!(
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\n{body}\n",
        log.display()
    );
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn calls(dir: &Path) -> Vec<String> {
    std::fs::read_to_string(dir.join("calls.log"))
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

fn config(bin: &Path, node: Option<&str>) -> ResolverConfig {
    ResolverConfig::new(bin.to_str().unwrap(), Duration::from_secs(10))
        .with_js_runtime(node.map(str::to_owned))
}

#[test]
fn args_pin_no_client_by_default_and_keep_the_url_after_double_dash() {
    let args = resolve_args(None, None, "-https://evil");
    assert_eq!(args, vec!["-J", "--no-warnings", "--", "-https://evil"]);
    assert!(!args.iter().any(|a| a.contains("player_client")));
}

#[test]
fn args_carry_node_runtime_and_fallback_clients_when_asked() {
    let args = resolve_args(
        Some("/usr/bin/node"),
        Some(FALLBACK_PLAYER_CLIENTS),
        "https://www.youtube.com/watch?v=x",
    );
    assert_eq!(
        args,
        vec![
            "-J",
            "--no-warnings",
            "--js-runtimes",
            "node:/usr/bin/node",
            "--extractor-args",
            &format!("youtube:player_client={FALLBACK_PLAYER_CLIENTS}"),
            "--",
            "https://www.youtube.com/watch?v=x",
        ]
    );
    assert!(!FALLBACK_PLAYER_CLIENTS.contains("ios"));
}

#[test]
fn find_on_path_finds_only_executables_in_absolute_dirs() {
    let dir = tempfile::tempdir().unwrap();
    let exe = dir.path().join("node");
    std::fs::write(&exe, "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = std::env::join_paths(["relative/dir", dir.path().to_str().unwrap()]).unwrap();
    assert_eq!(
        find_on_path("node", Some(&path)),
        Some(exe.to_str().unwrap().to_owned())
    );
    std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(find_on_path("node", Some(&path)), None);
    assert_eq!(find_on_path("node", None), None);
}

#[tokio::test]
async fn default_clients_succeed_in_one_call() {
    let _serial = EXEC_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let bin = fake_yt_dlp(dir.path(), &format!("echo '{LIVE_JSON}'"));
    let resolved = resolve_url(&config(&bin, Some("/opt/node")), "https://youtu.be/x")
        .await
        .unwrap();
    assert!(resolved.manifest_url.contains("googlevideo"));
    let calls = calls(dir.path());
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0],
        "-J --no-warnings --js-runtimes node:/opt/node -- https://youtu.be/x"
    );
}

#[tokio::test]
async fn a_failing_default_falls_back_to_explicit_clients() {
    let _serial = EXEC_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let body = format!(
        "case \"$*\" in *player_client=*) echo '{LIVE_JSON}' ;; \
         *) echo 'ERROR: [youtube] x: No video formats found!' >&2; exit 1 ;; esac"
    );
    let bin = fake_yt_dlp(dir.path(), &body);
    let resolved = resolve_url(&config(&bin, None), "https://youtu.be/x")
        .await
        .unwrap();
    assert!(resolved.manifest_url.contains("googlevideo"));
    let calls = calls(dir.path());
    assert_eq!(calls.len(), 2);
    assert!(!calls[0].contains("player_client"));
    assert!(calls[1].contains(&format!("youtube:player_client={FALLBACK_PLAYER_CLIENTS}")));
}

#[tokio::test]
async fn both_attempts_failing_reports_both_yt_dlp_errors() {
    let _serial = EXEC_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let bin = fake_yt_dlp(
        dir.path(),
        "echo 'ERROR: [youtube] x: Sign in to confirm you are not a bot. See https://example.test/a?sig=SECRET' >&2; exit 1",
    );
    let err = resolve_url(&config(&bin, None), "https://youtu.be/x")
        .await
        .unwrap_err();
    assert!(matches!(err, YoutubeError::Resolve(_)), "{err:?}");
    let message = err.operator_message();
    assert!(message.contains("Sign in to confirm"), "{message}");
    assert!(message.contains("fallback clients"), "{message}");
    assert!(!message.contains("SECRET"), "{message}");
    assert!(message.contains("<url>"), "{message}");
    assert_eq!(calls(dir.path()).len(), 2);
}

#[tokio::test]
async fn an_old_yt_dlp_without_js_runtimes_is_retried_without_the_flag() {
    let _serial = EXEC_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let body = format!(
        "case \"$*\" in *--js-runtimes*) echo 'yt-dlp: error: no such option: --js-runtimes' >&2; exit 2 ;; \
         *) echo '{LIVE_JSON}' ;; esac"
    );
    let bin = fake_yt_dlp(dir.path(), &body);
    resolve_url(&config(&bin, Some("/opt/node")), "https://youtu.be/x")
        .await
        .unwrap();
    let calls = calls(dir.path());
    assert_eq!(calls.len(), 2);
    assert!(calls[0].contains("--js-runtimes"));
    assert!(!calls[1].contains("--js-runtimes"));
    assert!(!calls[1].contains("player_client"));
}

#[tokio::test]
async fn not_live_is_final_and_does_not_try_other_clients() {
    let _serial = EXEC_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let bin = fake_yt_dlp(
        dir.path(),
        r#"echo '{"live_status":"was_live","formats":[]}'"#,
    );
    let err = resolve_url(&config(&bin, None), "https://youtu.be/x")
        .await
        .unwrap_err();
    assert_eq!(
        err.operator_message(),
        "not a live stream (status: was_live)"
    );
    assert_eq!(calls(dir.path()).len(), 1);
}

#[tokio::test]
async fn a_hung_yt_dlp_is_killed_and_reported_as_a_timeout() {
    let _serial = EXEC_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let bin = fake_yt_dlp(dir.path(), "exec sleep 30");
    let cfg = ResolverConfig::new(bin.to_str().unwrap(), Duration::from_millis(300))
        .with_js_runtime(None);
    let started = std::time::Instant::now();
    let err = resolve_url(&cfg, "https://youtu.be/x").await.unwrap_err();
    assert!(started.elapsed() < Duration::from_secs(10));
    assert_eq!(err.operator_message(), "yt-dlp timed out");
    assert_eq!(calls(dir.path()).len(), 1);
}

/// Invariant #10: a stop during a hung `yt-dlp` ends the re-resolution loop
/// promptly (the child is killed, not awaited for its full timeout).
#[tokio::test]
async fn a_stop_during_a_hung_resolve_ends_the_reresolve_loop_promptly() {
    use multiview_input::youtube::{
        run_reresolve_loop, ProcessResolver, ReresolveConfig, ReresolveOutcome, SystemUnixClock,
    };
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    let _serial = EXEC_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let bin = fake_yt_dlp(dir.path(), "exec sleep 30");
    let resolver = ProcessResolver::new(
        ResolverConfig::new(bin.to_str().unwrap(), Duration::from_secs(25)).with_js_runtime(None),
    );
    let stop = Arc::new(AtomicBool::new(false));
    let raiser = Arc::clone(&stop);
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        raiser.store(true, Ordering::Release);
    });
    let started = std::time::Instant::now();
    let outcome = run_reresolve_loop(
        "https://youtu.be/x",
        ReresolveConfig::default(),
        &resolver,
        &SystemUnixClock,
        &stop,
        |_| panic!("a hung resolve publishes nothing"),
    )
    .await
    .unwrap();
    assert_eq!(outcome, ReresolveOutcome::Stopped);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
}

/// A `yt-dlp` that floods stdout is cut off at the byte cap while still
/// running (killed, not buffered to its end), and the resolve fails cleanly.
#[tokio::test]
async fn an_over_limit_output_is_refused_while_the_child_runs() {
    let _serial = EXEC_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    // ~1 GiB of output if never stopped; far past the 16 MiB cap.
    let bin = fake_yt_dlp(dir.path(), "exec head -c 1073741824 /dev/zero");
    let cfg =
        ResolverConfig::new(bin.to_str().unwrap(), Duration::from_secs(20)).with_js_runtime(None);
    let started = std::time::Instant::now();
    let err = resolve_url(&cfg, "https://youtu.be/x").await.unwrap_err();
    assert!(
        matches!(&err, YoutubeError::Resolve(m) if m.contains("exceeded")),
        "{err:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(15),
        "{:?}",
        started.elapsed()
    );
}

/// Review ABUSE-04: a URL carrying legal sub-delimiters (`,` `(` `'`) is
/// redacted whole in the operator message, never leaving its tail behind.
#[test]
fn operator_message_redacts_whole_urls_with_sub_delimiters() {
    for raw in [
        "ERROR: see https://h.test/a,b?sig=SECRET now",
        "ERROR: see https://h.test/a(b)/signature/SECRET now",
        "ERROR: 'https://u:pa(ss@h.test/x?sig=SECRET'",
    ] {
        let message = YoutubeError::Resolve(raw.to_owned()).operator_message();
        assert!(!message.contains("SECRET"), "{message}");
        assert!(!message.contains("pa(ss"), "{message}");
        assert!(message.contains("<url>"), "{message}");
    }
}
