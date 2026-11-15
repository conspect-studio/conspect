#![cfg(unix)]
//! `verify_premium` against fake hosts (no browser): typed verdicts from the
//! host's final status line, and the credential travels on stdin only.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use multiview_webengine::{
    capture::{BrowserGl, Tools, WindyCredentials},
    runner::{verify_premium, Control, PremiumVerdict},
};
use std::{
    os::unix::fs::PermissionsExt,
    sync::{atomic::AtomicBool, Mutex, PoisonError},
    time::{Duration, Instant},
};

// Serialises script-write + spawn so no fork inherits a writable script fd (ETXTBSY).
static SERIAL: Mutex<()> = Mutex::new(());

const PASSWORD: &str = "pw-3d5f-on-stdin-only";

fn run(tail: &str) -> (PremiumVerdict, tempfile::TempDir) {
    let _serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
    let dir = tempfile::tempdir().unwrap();
    let w = dir.path();
    let node = w.join("node");
    std::fs::write(
        &node,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{d}/argv'\nenv > '{d}/env'\ncat > '{d}/stdin'\n{tail}\n",
            d = w.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&node, std::fs::Permissions::from_mode(0o755)).unwrap();
    let tools = Tools {
        node,
        browser: "/bin/sh".into(),
        ffmpeg: "/bin/sh".into(),
        gl: BrowserGl::Default,
    };
    let cancelled = AtomicBool::new(false);
    let verdict = verify_premium(
        &tools,
        WindyCredentials {
            username: "ops@example.com".into(),
            password: PASSWORD.into(),
        },
        Control {
            cancelled: &cancelled,
            deadline: Instant::now() + Duration::from_secs(30),
        },
    )
    .unwrap();
    (verdict, dir)
}

#[test]
fn premium_marker_is_premium_and_credential_is_on_stdin_only() {
    let (verdict, dir) = run(r#"printf '%s\n' '{"event":"premium_verify","premium":true}' >&2"#);
    assert_eq!(verdict, PremiumVerdict::Premium);
    let stdin: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.path().join("stdin")).unwrap()).unwrap();
    assert_eq!(stdin["verify"], true);
    assert_eq!(stdin["source"]["view"]["require_premium"], true);
    assert_eq!(stdin["source"]["view"]["credentials"]["password"], PASSWORD);
    for channel in ["argv", "env"] {
        let text = std::fs::read_to_string(dir.path().join(channel)).unwrap();
        assert!(!text.contains(PASSWORD) && !text.contains("ops@example.com"));
    }
}

#[test]
fn wrong_password_is_rejected() {
    let (verdict, _dir) = run("printf 'Windy login rejected\\n' >&2; exit 1");
    assert_eq!(verdict, PremiumVerdict::Rejected);
}

#[test]
fn no_premium_and_other_failures_are_typed() {
    assert_eq!(
        run("printf 'Windy account has no Premium\\n' >&2; exit 1").0,
        PremiumVerdict::NotPremium
    );
    assert_eq!(
        run("printf 'Windy login did not complete\\n' >&2; exit 1").0,
        PremiumVerdict::Unavailable
    );
    // A success exit without the verify marker (or with media output) is not Premium.
    assert_eq!(run("exit 0").0, PremiumVerdict::Unavailable);
    assert_eq!(
        run(r#"printf 'bytes'; printf '%s\n' '{"event":"premium_verify","premium":true}' >&2"#).0,
        PremiumVerdict::Unavailable
    );
}
