//! Issue #498: the `YouTube` resolve address family and the media family the
//! signed URL demands. A fake `yt-dlp` shell script stands in for the real
//! binary (no network): it bot-checks every run without `-4`, as `YouTube` did
//! to mda001's IPv6 /64, and answers a master signed for `/ip/<v4>/` over `-4`.
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
    is_bot_check, media_family, parse_signed_ip, resolve_args_with_family, resolve_url,
    MediaFamily, ResolveFamily, ResolverConfig, YoutubeError,
};

const V4_MASTER: &str = "https://manifest.googlevideo.com/api/manifest/hls_variant/expire/1893456000/ip/103.135.99.52/id/x/index.m3u8";
const V6_MASTER: &str = "https://manifest.googlevideo.com/api/manifest/hls_variant/expire/1893456000/ip/2404%3A79c0%3A1002%3A4031%3A%3A1/id/x/index.m3u8";
const BOT: &str = "ERROR: [youtube] x: Sign in to confirm you\\'re not a bot. Use --cookies";

static EXEC_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn live_json(master: &str) -> String {
    format!(
        r#"{{"live_status":"is_live","formats":[{{"protocol":"m3u8_native","manifest_url":"{master}"}}]}}"#
    )
}

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

/// A fake that bot-checks every invocation without `-4` and serves `V4_MASTER`
/// over `-4` (mda001, 17 Nov).
fn v6_bot_checked(dir: &Path) -> PathBuf {
    let ok = live_json(V4_MASTER);
    fake_yt_dlp(
        dir,
        &format!("case \" $* \" in *' -4 '*) echo '{ok}' ;; *) printf '%s\\n' \"{BOT}\" >&2; exit 1 ;; esac"),
    )
}

fn config(bin: &Path, family: ResolveFamily) -> ResolverConfig {
    ResolverConfig::new(bin.to_str().unwrap(), Duration::from_secs(10))
        .with_js_runtime(None)
        .with_family(family)
}

#[test]
fn family_spellings_parse_and_map_to_yt_dlp_flags() {
    assert_eq!(ResolveFamily::parse("auto"), Some(ResolveFamily::Auto));
    assert_eq!(ResolveFamily::parse(" V4 "), Some(ResolveFamily::V4));
    assert_eq!(ResolveFamily::parse("ipv6"), Some(ResolveFamily::V6));
    assert_eq!(ResolveFamily::parse("v5"), None);
    assert_eq!(ResolveFamily::default(), ResolveFamily::Auto);
    assert_eq!(ResolveFamily::Auto.flag(), None);
    assert_eq!(ResolveFamily::V4.flag(), Some("-4"));
    assert_eq!(ResolveFamily::V6.flag(), Some("-6"));
}

#[test]
fn args_carry_the_family_flag_before_the_url() {
    let args = resolve_args_with_family(ResolveFamily::V4, None, None, "-x");
    assert_eq!(args, vec!["-J", "--no-warnings", "-4", "--", "-x"]);
    let args = resolve_args_with_family(ResolveFamily::Auto, None, None, "u");
    assert_eq!(args, vec!["-J", "--no-warnings", "--", "u"]);
}

#[test]
fn the_signed_address_is_read_from_the_path_or_the_query() {
    assert_eq!(
        parse_signed_ip(V4_MASTER),
        Some("103.135.99.52".parse().unwrap())
    );
    assert_eq!(
        parse_signed_ip(V6_MASTER),
        Some("2404:79c0:1002:4031::1".parse().unwrap())
    );
    assert_eq!(
        parse_signed_ip(
            "https://r1.googlevideo.com/videoplayback?expire=1&ip=2001%3Adb8%3A%3A2&id=x"
        ),
        Some("2001:db8::2".parse().unwrap())
    );
    assert_eq!(
        parse_signed_ip("https://r1.googlevideo.com/videoplayback?ip=10.0.0.1"),
        Some("10.0.0.1".parse().unwrap())
    );
    // `ip` must be a whole segment / key, and a value that is no address is ignored.
    assert_eq!(parse_signed_ip("https://h/zip/1.2.3.4/x?vip=1.2.3.4"), None);
    assert_eq!(parse_signed_ip("https://h/ip/not-an-ip/x"), None);
    assert_eq!(parse_signed_ip("https://h/index.m3u8"), None);
    assert_eq!(parse_signed_ip(""), None);
}

#[test]
fn the_media_family_follows_the_signed_url_before_the_setting() {
    assert_eq!(
        media_family(V4_MASTER, ResolveFamily::Auto),
        MediaFamily::V4
    );
    // The URL is what the CDN checks: it wins over a forced setting.
    assert_eq!(media_family(V4_MASTER, ResolveFamily::V6), MediaFamily::V4);
    assert_eq!(
        media_family(V6_MASTER, ResolveFamily::Auto),
        MediaFamily::V6
    );
    assert_eq!(
        media_family("https://h/ip/::ffff:1.2.3.4/x", ResolveFamily::Auto),
        MediaFamily::V4
    );
    // No signed address: the forced resolve family, else no pin.
    assert_eq!(
        media_family("https://h/x.m3u8", ResolveFamily::V4),
        MediaFamily::V4
    );
    assert_eq!(
        media_family("https://h/x.m3u8", ResolveFamily::V6),
        MediaFamily::V6
    );
    assert_eq!(
        media_family("https://h/x.m3u8", ResolveFamily::Auto),
        MediaFamily::Any
    );
}

#[test]
fn bot_check_detection_matches_youtubes_wording_only() {
    assert!(is_bot_check(
        "ERROR: [youtube] abc: Sign in to confirm you're not a bot. Use --cookies"
    ));
    assert!(is_bot_check("Sign in to confirm you’re NOT A BOT"));
    assert!(!is_bot_check(
        "ERROR: [youtube] abc: No video formats found!"
    ));
    assert!(!is_bot_check("HTTP Error 403: Forbidden"));
}

#[tokio::test]
async fn auto_retries_a_bot_checked_resolve_over_ipv4_and_pins_media_to_v4() {
    let _serial = EXEC_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let bin = v6_bot_checked(dir.path());
    let resolved = resolve_url(&config(&bin, ResolveFamily::Auto), "https://youtu.be/x")
        .await
        .unwrap();
    assert_eq!(resolved.manifest_url, V4_MASTER);
    assert_eq!(resolved.resolved_over, ResolveFamily::V4);
    assert_eq!(resolved.media_family(), MediaFamily::V4);
    let calls = calls(dir.path());
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert_eq!(calls[0], "-J --no-warnings -- https://youtu.be/x");
    assert_eq!(calls[1], "-J --no-warnings -4 -- https://youtu.be/x");
}

#[tokio::test]
async fn auto_does_not_retry_over_ipv4_for_a_non_bot_failure() {
    let _serial = EXEC_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let bin = fake_yt_dlp(
        dir.path(),
        "echo 'ERROR: [youtube] x: No video formats found!' >&2; exit 1",
    );
    let err = resolve_url(&config(&bin, ResolveFamily::Auto), "https://youtu.be/x")
        .await
        .unwrap_err();
    assert!(matches!(err, YoutubeError::Resolve(_)), "{err:?}");
    let calls = calls(dir.path());
    assert_eq!(calls.len(), 2, "default + fallback clients only: {calls:?}");
    assert!(calls.iter().all(|c| !c.contains(" -4 ")), "{calls:?}");
}

#[tokio::test]
async fn auto_bot_checked_on_both_families_tries_ipv4_once_then_fallback_over_ipv4() {
    let _serial = EXEC_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let bin = fake_yt_dlp(dir.path(), &format!("printf '%s\\n' \"{BOT}\" >&2; exit 1"));
    let err = resolve_url(&config(&bin, ResolveFamily::Auto), "https://youtu.be/x")
        .await
        .unwrap_err();
    assert!(err.operator_message().contains("not a bot"), "{err:?}");
    let calls = calls(dir.path());
    // default, default -4, fallback -4: the refused default family is not retried.
    assert_eq!(calls.len(), 3, "{calls:?}");
    assert!(!calls[0].contains(" -4 "));
    assert!(calls[1].contains(" -4 ") && !calls[1].contains("player_client"));
    assert!(calls[2].contains(" -4 ") && calls[2].contains("player_client"));
}

#[tokio::test]
async fn forced_v6_passes_dash_6_and_never_falls_back_to_ipv4() {
    let _serial = EXEC_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let bin = v6_bot_checked(dir.path());
    let err = resolve_url(&config(&bin, ResolveFamily::V6), "https://youtu.be/x")
        .await
        .unwrap_err();
    assert!(matches!(err, YoutubeError::Resolve(_)), "{err:?}");
    let calls = calls(dir.path());
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert!(
        calls
            .iter()
            .all(|c| c.contains(" -6 ") && !c.contains(" -4 ")),
        "{calls:?}"
    );
}

#[tokio::test]
async fn forced_v4_passes_dash_4_on_the_first_call() {
    let _serial = EXEC_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let bin = v6_bot_checked(dir.path());
    let resolved = resolve_url(&config(&bin, ResolveFamily::V4), "https://youtu.be/x")
        .await
        .unwrap();
    assert_eq!(resolved.resolved_over, ResolveFamily::V4);
    assert_eq!(resolved.media_family(), MediaFamily::V4);
    assert_eq!(
        calls(dir.path()),
        vec!["-J --no-warnings -4 -- https://youtu.be/x"]
    );
}

#[tokio::test]
async fn a_default_family_resolve_signed_for_v6_pins_media_to_v6() {
    let _serial = EXEC_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let ok = live_json(V6_MASTER);
    let bin = fake_yt_dlp(dir.path(), &format!("echo '{ok}'"));
    let resolved = resolve_url(&config(&bin, ResolveFamily::Auto), "https://youtu.be/x")
        .await
        .unwrap();
    assert_eq!(resolved.resolved_over, ResolveFamily::Auto);
    assert_eq!(resolved.media_family(), MediaFamily::V6);
}
