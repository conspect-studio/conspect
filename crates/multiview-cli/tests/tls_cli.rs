//! `multiview tls` (ADR-0107 section 5a, #291): argument parsing, the
//! `[network.tls]`-without-`acme` gate, offline status, and the boot wiring
//! against a dead CA while the real output clock runs.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use clap::Parser as _;
use multiview_cli::cli::{Cli, Command, TlsCommand};

#[test]
fn tls_subcommands_parse() {
    let parse = |args: &[&str]| Cli::try_parse_from(args).map(|c| c.command);
    match parse(&[
        "multiview",
        "tls",
        "status",
        "--json",
        "--config",
        "/c.toml",
    ])
    .unwrap()
    {
        Command::Tls(TlsCommand::Status(a)) => {
            assert!(a.json);
            assert_eq!(a.config.config, Some(std::path::PathBuf::from("/c.toml")));
        }
        other => panic!("{other:?}"),
    }
    match parse(&["multiview", "tls", "status"]).unwrap() {
        Command::Tls(TlsCommand::Status(a)) => {
            assert!(!a.json);
            // No default any more: resolved at run time (running engine,
            // else /etc/multiview/multiview.toml when it exists).
            assert_eq!(a.config.config, None);
        }
        other => panic!("{other:?}"),
    }
    assert!(matches!(
        parse(&["multiview", "tls", "setup-code"]).unwrap(),
        Command::Tls(TlsCommand::SetupCode(_))
    ));
    assert!(matches!(
        parse(&["multiview", "tls", "issue", "--config", "x.toml"]).unwrap(),
        Command::Tls(TlsCommand::Issue(_))
    ));
    match parse(&["multiview", "tls", "set-credential", "CF_TOKEN"]).unwrap() {
        Command::Tls(TlsCommand::SetCredential(a)) => assert_eq!(a.name, "CF_TOKEN"),
        other => panic!("{other:?}"),
    }
    // The value can never come from argv.
    assert!(parse(&[
        "multiview",
        "tls",
        "set-credential",
        "CF_TOKEN",
        "the-secret"
    ])
    .is_err());
    assert!(parse(&[
        "multiview",
        "tls",
        "set-credential",
        "CF_TOKEN",
        "--value",
        "x"
    ])
    .is_err());
    assert!(parse(&["multiview", "tls", "set-credential"]).is_err());
}

const BASE: &str = r##"
schema_version = 1

[canvas]
width = 320
height = 180
fps = "25/1"
pixel_format = "nv12"
background = "#101014"

[canvas.color]
profile = "sdr-bt709-limited"

[layout]
kind = "grid"
columns = ["1fr"]
rows = ["1fr"]
areas = ["a"]

[[sources]]
id = "in_a"
kind = "bars"

[[cells]]
id = "cell_a"
area = "a"
[cells.source]
input_id = "in_a"

[[outputs]]
kind = "hls"
path = "/tmp/conspect-tls-cli-test.m3u8"
codec = "mpeg2video"
segment_ms = 1000
"##;

fn tls_config(state_dir: &std::path::Path, extra: &str) -> String {
    format!(
        "{BASE}\n[network]\npublic_host = \"fd.example.org\"\n[network.tls]\nca = \"letsencrypt\"\n\
         staging = true\nchallenge = \"tls-alpn-01\"\ndomains = [\"fd.example.org\"]\n\
         state_dir = \"{}\"\n{extra}",
        state_dir.display()
    )
}

#[test]
fn network_tls_needs_the_acme_feature() {
    let dir = tempfile::tempdir().unwrap();
    let config =
        multiview_config::MultiviewConfig::load_from_toml(&tls_config(dir.path(), "")).unwrap();
    let gate = multiview_cli::tls_cli::ensure_acme_supported(&config);
    if cfg!(all(feature = "acme", unix)) {
        assert!(gate.is_ok());
    } else {
        assert!(gate.unwrap_err().contains("lacks the `acme` feature"));
    }
    let plain = multiview_config::MultiviewConfig::load_from_toml(BASE).unwrap();
    assert!(multiview_cli::tls_cli::ensure_acme_supported(&plain).is_ok());
}

#[cfg(all(feature = "acme", unix))]
mod acme {
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Arc;
    use std::time::Duration;

    use multiview_control::certificate_source::TlsState;

    fn private_state() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        std::fs::create_dir(&state).unwrap();
        std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o700)).unwrap();
        (dir, state)
    }

    #[test]
    fn offline_status_reads_state_dir() {
        let (_dir, state) = private_state();
        let text = super::tls_config(
            &state,
            "\n[network.tls.dns]\nprovider = \"cloudflare\"\ntoken_env = \"CONSPECT_TLS_CLI_TEST_TOKEN\"\n",
        )
        .replace("tls-alpn-01", "dns-01");
        let config = multiview_config::MultiviewConfig::load_from_toml(&text).unwrap();
        let tls = config.network.as_ref().unwrap().tls.as_ref().unwrap();
        // Nothing stored yet: setup, the missing credential named.
        let value = multiview_cli::tls_cli::offline_status(tls).unwrap();
        assert_eq!(value["source"], "offline");
        assert_eq!(value["state"], "setup");
        let reasons = value["reasons"].to_string();
        assert!(
            reasons.contains("credential-missing: CONSPECT_TLS_CLI_TEST_TOKEN"),
            "{reasons}"
        );
        // A stored (self-signed, already expired) certificate: expired, its
        // expiry and names read back from active.pem.
        let key = rcgen::KeyPair::generate().unwrap();
        let mut params = rcgen::CertificateParams::new(vec!["fd.example.org".to_owned()]).unwrap();
        params.not_before = rcgen::date_time_ymd(2020, 1, 1);
        params.not_after = rcgen::date_time_ymd(2020, 4, 1);
        let cert = params.self_signed(&key).unwrap();
        let bundle = format!("{}\n{}", key.serialize_pem(), cert.pem());
        multiview_control::acme::store::write_private(
            &state,
            multiview_control::acme::store::ACTIVE_PEM,
            bundle.as_bytes(),
        )
        .unwrap();
        let value = multiview_cli::tls_cli::offline_status(tls).unwrap();
        assert_eq!(value["state"], "expired");
        assert_eq!(value["expires_at"], 1_585_699_200);
        assert_eq!(value["names"][0], "fd.example.org");
        let table = multiview_cli::tls_cli::render_status(&value);
        assert!(table.contains("2020-04-01 00:00:00Z"), "{table}");
        assert!(table.starts_with("engine not running"));
        // The key never appears in the status.
        assert!(!value.to_string().contains("PRIVATE KEY"));
    }

    /// Boot wiring with a CA that refuses connections: the engine starts, the
    /// real output clock runs every tick, and the source stays in `setup` with
    /// the sanitized `last_error`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dead_ca_boot_keeps_setup_and_the_clock_running() {
        let (_dir, state) = private_state();
        // A closed loopback port as the custom directory: connection refused.
        let port = {
            let l = std::net::TcpListener::bind("[::1]:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let anchor = state.parent().unwrap().join("root.pem");
        let ca_key = rcgen::KeyPair::generate().unwrap();
        let mut ca = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
        ca.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        std::fs::write(&anchor, ca.self_signed(&ca_key).unwrap().pem()).unwrap();
        let text = super::tls_config(&state, "")
            .replace("ca = \"letsencrypt\"\nstaging = true", "ca = \"custom\"")
            .replace(
                "challenge = \"tls-alpn-01\"",
                &format!(
                    "challenge = \"tls-alpn-01\"\ndirectory = \"https://[::1]:{port}/dir\"\n\
                     trust_anchor = \"{}\"\nallow_private_directory = true",
                    anchor.display()
                ),
            );
        let config = multiview_config::MultiviewConfig::load_from_toml(&text).unwrap();
        config.validate().unwrap();
        let options = multiview_cli::tls_boot::BootOptions {
            egress_timeouts: None,
            attempt_timeout: Some(Duration::from_secs(5)),
        };
        let boot = multiview_cli::tls_boot::TlsBoot::start(&config, None, &options)
            .unwrap()
            .expect("[network.tls] configured");
        let source = Arc::clone(&boot.context.certificates);
        assert_eq!(source.status().state, TlsState::Setup);
        assert!(state.join("bootstrap.key").exists());
        assert_eq!(
            std::fs::metadata(state.join("bootstrap.key"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let fingerprint = source.status().bootstrap_fingerprint.unwrap();
        assert_eq!(fingerprint.len(), 32 * 3 - 1);

        // The real software engine on the realtime pacer, 50 ticks at 25 fps.
        let mut engine = multiview_cli::run::SoftwareEngine::build(&config).unwrap();
        let report = engine.run_for_realtime(50).await.unwrap();
        assert_eq!(report.ticks, 50);
        assert_eq!(report.frames, 50);
        assert!(!report.faltered);

        // The CA was refused: still setup, sanitized reason recorded.
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        loop {
            let status = source.status();
            if status.last_error.is_some() {
                assert_eq!(status.state, TlsState::Setup);
                let reason = status.last_error.unwrap();
                assert!(
                    ["ca-unavailable", "egress-refused", "attempt-timed-out"]
                        .contains(&reason.as_str()),
                    "{reason}"
                );
                assert!(status.next_attempt_at.is_some());
                break;
            }
            assert!(std::time::Instant::now() < deadline, "no attempt recorded");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(!boot.service_finished());
        // The issue trigger is non-blocking and the service accepts it.
        assert!((boot.context.issue)().is_ok());
        drop(boot);
    }
}
