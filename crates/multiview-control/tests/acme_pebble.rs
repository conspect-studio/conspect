//! Full ACME flows against Pebble + pebble-challtestsrv (ADR-0107 §5a tests).
//!
//! Every test is `#[ignore]`d: they need the Pebble stack that the
//! `acme-pebble` CI job (and `ci/pebble/run-local.sh start`) brings up, and
//! read its endpoints from the environment:
//! `PEBBLE_DIRECTORY`, `PEBBLE_EAB_DIRECTORY`, `PEBBLE_NONCE_DIRECTORY`,
//! `PEBBLE_ROOT`, `CHALLTESTSRV_MGMT`, `CHALLTESTSRV_DNS`, and optionally
//! `PEBBLE_TLS_PORT` (Pebble's `tlsPort`, default 5001).
//! Run with `--ignored --test-threads=1` (TLS-ALPN-01 binds `[::]:<tlsPort>`).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::too_many_lines
)]

mod acme_support;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use acme_support::{
    cf_router, cf_state, fixture_pki, serve_https, write_root, CfAuth, CfFault, SharedCf,
};
use multiview_config::{AcmeChallenge, CertificateAuthority, DnsProviderConfig, NetworkTlsConfig};
use multiview_control::acme::cloudflare::{CloudflareCredential, CloudflareDns};
use multiview_control::acme::engine::parse_cert_id;
use multiview_control::acme::store::Ledger;
use multiview_control::acme::{
    alarm_for, AcmeEngine, AcmeError, AcmeService, DnsFuture, DnsProvider, TxtHandle,
};
use multiview_control::certificate_source::{
    bootstrap_certificate, AcmeCertificateSource, CertificateAlarm, CertificateSource,
    ChallengeResolver, TlsState,
};
use zeroize::Zeroizing;

const ZONE: &str = "acme.test";
const ZONE_ID: &str = "a1b2c3d4e5f60718293a4b5c6d7e8f90";
const IGNORE: &str = "needs Pebble; run by the acme-pebble CI job";

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} not set ({IGNORE})"))
}

fn dns_addr() -> SocketAddr {
    env("CHALLTESTSRV_DNS").parse().unwrap()
}

/// A unique label so reruns against one Pebble never collide.
fn unique(prefix: &str) -> String {
    format!(
        "{prefix}-{}",
        &uuid::Uuid::new_v4().simple().to_string()[..10]
    )
}

struct Harness {
    _tmp: tempfile::TempDir,
    state_dir: PathBuf,
    cf: SharedCf,
    cf_port: u16,
    cf_root: PathBuf,
    source: Arc<AcmeCertificateSource>,
    resolver: Arc<ChallengeResolver>,
}

async fn harness(auth: CfAuth) -> Harness {
    let tmp = tempfile::tempdir().unwrap();
    let state_dir = acme_support::private_dir(tmp.path(), "state");
    let cf = cf_state(auth, &[(ZONE, ZONE_ID)], Some(env("CHALLTESTSRV_MGMT")));
    let pki = fixture_pki();
    let addr = serve_https(cf_router(cf.clone()), &pki).await;
    let cf_root = write_root(&pki, tmp.path());
    let bootstrap = bootstrap_certificate("host.acme.test").unwrap();
    let source = Arc::new(AcmeCertificateSource::new(
        bootstrap.certificate,
        bootstrap.fingerprint,
        "custom".into(),
        Vec::new(),
    ));
    let resolver = Arc::new(ChallengeResolver::new(source.clone()));
    Harness {
        _tmp: tmp,
        state_dir,
        cf,
        cf_port: addr.port(),
        cf_root,
        source,
        resolver,
    }
}

impl Harness {
    fn provider(&self, credential: CloudflareCredential) -> Arc<CloudflareDns> {
        Arc::new(
            CloudflareDns::for_tests(
                credential,
                &format!("https://[::1]:{}/client/v4", self.cf_port),
                self.cf_port,
                Some(&self.cf_root),
                vec![dns_addr()],
            )
            .unwrap(),
        )
    }

    fn token_provider(&self) -> Arc<CloudflareDns> {
        self.provider(CloudflareCredential::Token(Zeroizing::new("tok".into())))
    }

    fn records(&self) -> usize {
        self.cf.lock().unwrap().records.len()
    }
}

fn config(
    directory: &str,
    state_dir: &Path,
    challenge: AcmeChallenge,
    domains: &[String],
) -> NetworkTlsConfig {
    NetworkTlsConfig {
        ca: CertificateAuthority::Custom,
        staging: false,
        challenge,
        domains: domains.to_vec(),
        contact_email: Some("ops@acme.test".into()),
        acme_profile: None,
        directory: Some(directory.into()),
        trust_anchor: Some(PathBuf::from(env("PEBBLE_ROOT"))),
        allow_private_directory: true,
        eab_kid_env: None,
        eab_hmac_env: None,
        state_dir: state_dir.to_path_buf(),
        dns: (challenge == AcmeChallenge::Dns01).then(|| DnsProviderConfig::Cloudflare {
            token_env: Some("CF_TOKEN_NAME_ONLY".into()),
            email_env: None,
            api_key_env: None,
            propagation_timeout_s: 30,
        }),
    }
}

fn engine(h: &Harness, cfg: NetworkTlsConfig, dns: Option<Arc<dyn DnsProvider>>) -> AcmeEngine {
    AcmeEngine::new(cfg, dns, h.resolver.clone(), h.source.clone()).unwrap()
}

fn leaf_sans(h: &Harness) -> Vec<String> {
    let current = h.source.current();
    let leaf = current.cert.first().unwrap();
    let (_, parsed) = x509_parser::parse_x509_certificate(leaf).unwrap();
    let mut names: Vec<String> = parsed
        .subject_alternative_name()
        .unwrap()
        .unwrap()
        .value
        .general_names
        .iter()
        .filter_map(|n| match n {
            x509_parser::extensions::GeneralName::DNSName(d) => Some((*d).to_owned()),
            _ => None,
        })
        .collect();
    names.sort();
    names
}

/// TXT values challtestsrv serves for `fqdn`.
async fn txt_at_challtestsrv(fqdn: &str) -> Vec<String> {
    use hickory_resolver::config::{ConnectionConfig, NameServerConfig, ResolverConfig};
    use hickory_resolver::proto::rr::{RData, RecordType};
    let addr = dns_addr();
    let mut udp = ConnectionConfig::udp();
    udp.port = addr.port();
    let config =
        ResolverConfig::from_name_servers(vec![NameServerConfig::new(addr.ip(), true, vec![udp])]);
    let mut builder = hickory_resolver::TokioResolver::builder_with_config(
        config,
        hickory_resolver::net::runtime::TokioRuntimeProvider::default(),
    );
    builder.options_mut().cache_size = 0;
    let resolver = builder.build().unwrap();
    match resolver.lookup(format!("{fqdn}."), RecordType::TXT).await {
        Ok(found) => found
            .answers()
            .iter()
            .filter_map(|r| match &r.data {
                RData::TXT(t) => Some(
                    t.txt_data
                        .iter()
                        .map(|p| String::from_utf8_lossy(p).into_owned())
                        .collect::<String>(),
                ),
                _ => None,
            })
            .collect(),
        Err(_) => Vec::new(),
    }
}

async fn dns01_issue(auth: CfAuth, credential: CloudflareCredential) {
    let h = harness(auth).await;
    let name = format!("{}.{ZONE}", unique("dns"));
    let cfg = config(
        &env("PEBBLE_DIRECTORY"),
        &h.state_dir,
        AcmeChallenge::Dns01,
        std::slice::from_ref(&name),
    );
    let provider: Arc<dyn DnsProvider> = h.provider(credential);
    let e = engine(&h, cfg.clone(), Some(provider.clone()));
    let mut rx = h.source.subscribe();
    let issued = e.issue_once(None).await.unwrap();
    assert_eq!(issued.names, vec![name.clone()]);
    assert_eq!(h.source.status().state, TlsState::Serving);
    assert!(rx.has_changed().unwrap());
    rx.mark_unchanged();
    assert_eq!(leaf_sans(&h), vec![name.clone()]);
    assert_eq!(h.records(), 0, "every TXT removed");
    assert_eq!(Ledger::new(h.state_dir.clone()).load().unwrap(), Vec::new());
    assert_eq!(
        txt_at_challtestsrv(&format!("_acme-challenge.{name}")).await,
        Vec::<String>::new()
    );
    // A fresh engine on the same state reloads without contacting the CA.
    let bootstrap = bootstrap_certificate("host.acme.test").unwrap();
    let fresh = Arc::new(AcmeCertificateSource::new(
        bootstrap.certificate,
        bootstrap.fingerprint,
        "custom".into(),
        Vec::new(),
    ));
    let reload = AcmeEngine::new(cfg, Some(provider), h.resolver.clone(), fresh.clone()).unwrap();
    let meta = reload.load_existing().unwrap().unwrap();
    assert_eq!(meta.names, vec![name]);
    assert_eq!(fresh.status().state, TlsState::Serving);
    assert!(!reload.needs_reissue(&meta));
}

#[tokio::test]
#[ignore = "needs Pebble; run by the acme-pebble CI job"]
async fn dns01_token_single_name() {
    dns01_issue(
        CfAuth::Token("tok".into()),
        CloudflareCredential::Token(Zeroizing::new("tok".into())),
    )
    .await;
}

#[tokio::test]
#[ignore = "needs Pebble; run by the acme-pebble CI job"]
async fn dns01_global_key() {
    dns01_issue(
        CfAuth::GlobalKey {
            email: "ops@acme.test".into(),
            key: "gk".into(),
        },
        CloudflareCredential::GlobalKey {
            email: Zeroizing::new("ops@acme.test".into()),
            key: Zeroizing::new("gk".into()),
        },
    )
    .await;
}

#[tokio::test]
#[ignore = "needs Pebble; run by the acme-pebble CI job"]
async fn dns01_wildcard_and_apex_share_one_txt_name() {
    let h = harness(CfAuth::Token("tok".into())).await;
    let apex = format!("{}.{ZONE}", unique("w"));
    let names = vec![format!("*.{apex}"), apex.clone()];
    let cfg = config(
        &env("PEBBLE_DIRECTORY"),
        &h.state_dir,
        AcmeChallenge::Dns01,
        &names,
    );
    let e = engine(&h, cfg, Some(h.token_provider()));
    e.issue_once(None).await.unwrap();
    let mut want = names.clone();
    want.sort();
    assert_eq!(leaf_sans(&h), want);
    // Both TXT values were published on `_acme-challenge.<apex>`.
    let posts =
        h.cf.lock()
            .unwrap()
            .log
            .iter()
            .filter(|(m, p, _)| m == "POST" && p.contains("dns_records"))
            .count();
    assert_eq!(posts, 2);
    assert_eq!(h.records(), 0);
    assert_eq!(
        txt_at_challtestsrv(&format!("_acme-challenge.{apex}")).await,
        Vec::<String>::new()
    );
}

/// Serve TLS-ALPN-01 on `[::]:<PEBBLE_TLS_PORT>` through the `ChallengeResolver`.
async fn alpn_listener(resolver: Arc<ChallengeResolver>) -> tokio::task::JoinHandle<()> {
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let mut config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_cert_resolver(resolver);
    config.alpn_protocols = vec![b"acme-tls/1".to_vec(), b"http/1.1".to_vec()];
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
    let port: u16 = std::env::var("PEBBLE_TLS_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(5001);
    let listener = tokio::net::TcpListener::bind(("::", port)).await.unwrap();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                let _ = acceptor.accept(stream).await;
            });
        }
    })
}

#[tokio::test]
#[ignore = "needs Pebble; run by the acme-pebble CI job"]
async fn tls_alpn01_issues_through_the_challenge_resolver() {
    let h = harness(CfAuth::Token("unused".into())).await;
    let listener = alpn_listener(h.resolver.clone()).await;
    let name = format!("{}.{ZONE}", unique("alpn"));
    let cfg = config(
        &env("PEBBLE_DIRECTORY"),
        &h.state_dir,
        AcmeChallenge::TlsAlpn01,
        std::slice::from_ref(&name),
    );
    let e = engine(&h, cfg, None);
    let issued = e.issue_once(None).await;
    listener.abort();
    assert_eq!(issued.unwrap().names, vec![name.clone()]);
    assert_eq!(leaf_sans(&h), vec![name]);
    assert_eq!(h.source.status().state, TlsState::Serving);
}

/// Review #338 L-1: an unparseable stored account file is kept aside and a
/// fresh account is registered (the runbook's recovery), not a permanent
/// `state-storage-failed`.
#[tokio::test]
#[ignore = "needs Pebble; run by the acme-pebble CI job"]
async fn a_corrupt_account_file_is_kept_aside_and_replaced() {
    use std::os::unix::fs::PermissionsExt;
    let h = harness(CfAuth::Token("unused".into())).await;
    let listener = alpn_listener(h.resolver.clone()).await;
    let directory = env("PEBBLE_DIRECTORY");
    let account = multiview_control::acme::store::account_file_name(&directory).unwrap();
    let path = h.state_dir.join(&account);
    std::fs::write(&path, b"{ truncated").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let name = format!("{}.{ZONE}", unique("acct"));
    let cfg = config(
        &directory,
        &h.state_dir,
        AcmeChallenge::TlsAlpn01,
        std::slice::from_ref(&name),
    );
    let issued = engine(&h, cfg, None).issue_once(None).await;
    listener.abort();
    issued.unwrap();
    let fresh = std::fs::read(&path).unwrap();
    assert!(serde_json::from_slice::<serde_json::Value>(&fresh).is_ok());
    let aside: Vec<_> = std::fs::read_dir(&h.state_dir)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with(&format!("{account}.bad-"))
        })
        .collect();
    assert_eq!(aside.len(), 1);
    assert_eq!(std::fs::read(aside[0].path()).unwrap(), b"{ truncated");
}

#[tokio::test]
#[ignore = "needs Pebble; run by the acme-pebble CI job"]
async fn eab_account_binding_and_missing_eab() {
    let h = harness(CfAuth::Token("tok".into())).await;
    let name = format!("{}.{ZONE}", unique("eab"));
    let mut cfg = config(
        &env("PEBBLE_EAB_DIRECTORY"),
        &h.state_dir,
        AcmeChallenge::Dns01,
        std::slice::from_ref(&name),
    );
    // Missing EAB: the CA requires it.
    let e = engine(&h, cfg.clone(), Some(h.token_provider()));
    assert_eq!(e.issue_once(None).await.unwrap_err(), AcmeError::Eab);
    // Names configured, values absent from env and secrets: Eab before any CA call.
    cfg.eab_kid_env = Some("CONSPECT_TEST_EAB_KID".into());
    cfg.eab_hmac_env = Some("CONSPECT_TEST_EAB_HMAC".into());
    std::env::remove_var("CONSPECT_TEST_EAB_KID");
    std::env::remove_var("CONSPECT_TEST_EAB_HMAC");
    let e = engine(&h, cfg.clone(), Some(h.token_provider()));
    assert_eq!(e.issue_once(None).await.unwrap_err(), AcmeError::Eab);
    std::env::set_var("CONSPECT_TEST_EAB_KID", "kid-1");
    std::env::set_var(
        "CONSPECT_TEST_EAB_HMAC",
        "zWNDZM6eQGHWpSRTPal5eIUYFTu7EajVIoguysqZ9wG44nMEtx3MUAsUDkMTQ12W",
    );
    let e = engine(&h, cfg, Some(h.token_provider()));
    let issued = e.issue_once(None).await;
    std::env::remove_var("CONSPECT_TEST_EAB_KID");
    std::env::remove_var("CONSPECT_TEST_EAB_HMAC");
    assert_eq!(issued.unwrap().names, vec![name]);
    assert_eq!(h.records(), 0);
}

#[tokio::test]
#[ignore = "needs Pebble; run by the acme-pebble CI job"]
async fn private_trust_anchor_and_the_private_directory_knob() {
    let h = harness(CfAuth::Token("tok".into())).await;
    let name = format!("{}.{ZONE}", unique("anchor"));
    let good = config(
        &env("PEBBLE_DIRECTORY"),
        &h.state_dir,
        AcmeChallenge::Dns01,
        std::slice::from_ref(&name),
    );
    // With anchor + knob: issues (the directory is https://localhost:14000).
    assert!(good.directory.as_deref().unwrap().contains("localhost"));
    engine(&h, good.clone(), Some(h.token_provider()))
        .issue_once(None)
        .await
        .unwrap();
    // A different anchor: the TLS chain no longer verifies → Egress.
    let mut wrong = good.clone();
    wrong.state_dir = acme_support::private_dir(h.state_dir.parent().unwrap(), "state-wrong");
    wrong.trust_anchor = Some(h.cf_root.clone());
    assert_eq!(
        engine(&h, wrong, Some(h.token_provider()))
            .issue_once(None)
            .await
            .unwrap_err(),
        AcmeError::Egress
    );
    // No anchor at all: refused at config validation.
    let mut none = good.clone();
    none.trust_anchor = None;
    assert!(none
        .validate()
        .unwrap_err()
        .to_string()
        .contains("trust_anchor"));
    // No knob: refused at config validation, naming the knob.
    let mut off = good;
    off.allow_private_directory = false;
    assert!(off
        .validate()
        .unwrap_err()
        .to_string()
        .contains("allow_private_directory"));
    assert_eq!(
        AcmeEngine::new(
            off,
            Some(h.token_provider()),
            h.resolver.clone(),
            h.source.clone()
        )
        .unwrap_err(),
        AcmeError::Config
    );
}

#[tokio::test]
#[ignore = "needs Pebble; run by the acme-pebble CI job"]
async fn renewal_with_ari_replaces_installs_a_new_certificate() {
    let h = harness(CfAuth::Token("tok".into())).await;
    let name = format!("{}.{ZONE}", unique("renew"));
    let cfg = config(
        &env("PEBBLE_DIRECTORY"),
        &h.state_dir,
        AcmeChallenge::Dns01,
        std::slice::from_ref(&name),
    );
    let e = engine(&h, cfg, Some(h.token_provider()));
    let first = e.issue_once(None).await.unwrap();
    let first_key = h.source.current();
    let id = first
        .ari_cert_id
        .clone()
        .expect("Pebble leaf carries an AKI");
    // ARI is offered: the renewal instant comes from the suggested window.
    let meta = e.load_existing().unwrap().unwrap();
    let (_at, recheck) = e
        .renewal_at(&meta, multiview_control::acme::engine::unix_now())
        .await;
    assert!(recheck.is_some(), "Pebble offers renewalInfo");
    let rx = h.source.subscribe();
    let second = e.issue_once(parse_cert_id(&id)).await.unwrap();
    assert!(rx.has_changed().unwrap(), "source swapped");
    assert!(!Arc::ptr_eq(&first_key, &h.source.current()));
    assert_ne!(second.ari_cert_id, first.ari_cert_id);
    assert_eq!(h.records(), 0);
}

#[tokio::test]
#[ignore = "needs Pebble; run by the acme-pebble CI job"]
async fn failed_renewal_keeps_the_current_certificate() {
    let h = harness(CfAuth::Token("tok".into())).await;
    let name = format!("{}.{ZONE}", unique("keep"));
    let cfg = config(
        &env("PEBBLE_DIRECTORY"),
        &h.state_dir,
        AcmeChallenge::Dns01,
        std::slice::from_ref(&name),
    );
    let issued = engine(&h, cfg.clone(), Some(h.token_provider()))
        .issue_once(None)
        .await
        .unwrap();
    let current = h.source.current();
    let dead = {
        let l = std::net::TcpListener::bind("[::1]:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let mut down = cfg;
    down.directory = Some(format!("https://localhost:{dead}/dir"));
    let e = engine(&h, down, Some(h.token_provider()));
    assert_eq!(e.issue_once(None).await.unwrap_err(), AcmeError::Authority);
    assert!(
        Arc::ptr_eq(&current, &h.source.current()),
        "current unchanged"
    );
    assert_eq!(h.source.status().state, TlsState::Serving);
    // Alarm math with an injected clock instead of sleeping.
    let (nb, na) = (issued.not_before, issued.not_after);
    let life = na - nb;
    assert_eq!(
        alarm_for(nb, na, nb + life / 10, true),
        CertificateAlarm::None
    );
    assert_eq!(
        alarm_for(nb, na, nb + life * 6 / 10, true),
        CertificateAlarm::Warning
    );
    assert_eq!(
        alarm_for(nb, na, nb + life * 8 / 10, true),
        CertificateAlarm::Critical
    );
    assert_eq!(alarm_for(nb, na, na + 1, true), CertificateAlarm::Expired);
    assert_eq!(
        alarm_for(nb, na, nb + life * 8 / 10, false),
        CertificateAlarm::None
    );
}

#[tokio::test]
#[ignore = "needs Pebble; run by the acme-pebble CI job"]
async fn nonce_rejections_are_retried() {
    let h = harness(CfAuth::Token("tok".into())).await;
    let name = format!("{}.{ZONE}", unique("nonce"));
    let cfg = config(
        &env("PEBBLE_NONCE_DIRECTORY"),
        &h.state_dir,
        AcmeChallenge::Dns01,
        std::slice::from_ref(&name),
    );
    let e = engine(&h, cfg, Some(h.token_provider()));
    let mut ok = false;
    for _ in 0..5 {
        if e.issue_once(None).await.is_ok() {
            ok = true;
            break;
        }
    }
    assert!(ok, "issued within 5 attempts despite 40% nonce rejection");
    assert_eq!(h.records(), 0);
}

/// Wrapper provider: bypass propagation confirmation, or hang in it after
/// signalling that the record is published.
struct Wrap {
    inner: Arc<CloudflareDns>,
    published: Option<tokio::sync::mpsc::Sender<()>>,
}

impl DnsProvider for Wrap {
    fn name(&self) -> &'static str {
        self.inner.name()
    }
    fn validate_credential<'a>(&'a self, names: &'a [String]) -> DnsFuture<'a, ()> {
        self.inner.validate_credential(names)
    }
    fn publish_txt<'a>(&'a self, fqdn: &'a str, value: &'a str) -> DnsFuture<'a, TxtHandle> {
        self.inner.publish_txt(fqdn, value)
    }
    fn confirm_at_authoritative<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
        _: Duration,
    ) -> DnsFuture<'a, ()> {
        Box::pin(async move {
            if let Some(tx) = &self.published {
                let _ = tx.send(()).await;
                std::future::pending::<()>().await;
            }
            Ok(())
        })
    }
    fn remove_txt<'a>(&'a self, handle: &'a TxtHandle) -> DnsFuture<'a, ()> {
        self.inner.remove_txt(handle)
    }
}

#[tokio::test]
#[ignore = "needs Pebble; run by the acme-pebble CI job"]
async fn failure_injection() {
    // Validation failure: the record is "accepted" but never reaches DNS.
    let h = harness(CfAuth::Token("tok".into())).await;
    h.cf.lock().unwrap().fault = CfFault::AcceptButDoNotWrite;
    let name = format!("{}.{ZONE}", unique("invalid"));
    let cfg = config(
        &env("PEBBLE_DIRECTORY"),
        &h.state_dir,
        AcmeChallenge::Dns01,
        std::slice::from_ref(&name),
    );
    let wrap: Arc<dyn DnsProvider> = Arc::new(Wrap {
        inner: h.token_provider(),
        published: None,
    });
    let e = engine(&h, cfg.clone(), Some(wrap));
    assert_eq!(e.issue_once(None).await.unwrap_err(), AcmeError::Challenge);
    assert_eq!(
        h.records(),
        0,
        "published TXT removed after a failed validation"
    );
    assert_eq!(h.source.status().state, TlsState::Setup);

    // CA down (refused port): Authority, fast.
    let dead = {
        let l = std::net::TcpListener::bind("[::1]:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let mut down = cfg.clone();
    down.directory = Some(format!("https://localhost:{dead}/dir"));
    let started = Instant::now();
    assert_eq!(
        engine(&h, down, Some(h.token_provider()))
            .issue_once(None)
            .await
            .unwrap_err(),
        AcmeError::Authority
    );
    assert!(started.elapsed() < Duration::from_secs(5));

    // DNS API 500 and 429: DnsApi, nothing left behind.
    for fault in [CfFault::ServerError, CfFault::RateLimited] {
        h.cf.lock().unwrap().fault = fault;
        assert_eq!(
            engine(&h, cfg.clone(), Some(h.token_provider()))
                .issue_once(None)
                .await
                .unwrap_err(),
            AcmeError::DnsApi,
            "{fault:?}"
        );
        assert_eq!(h.records(), 0);
    }
    h.cf.lock().unwrap().fault = CfFault::None;

    // Kill mid-order: abort right after publish, then a new engine cleans up.
    let name = format!("{}.{ZONE}", unique("kill"));
    let mut kill_cfg = cfg;
    kill_cfg.domains = vec![name.clone()];
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    let wrap: Arc<dyn DnsProvider> = Arc::new(Wrap {
        inner: h.token_provider(),
        published: Some(tx),
    });
    let doomed = engine(&h, kill_cfg.clone(), Some(wrap));
    let task = tokio::spawn(async move { doomed.issue_once(None).await });
    tokio::time::timeout(Duration::from_secs(60), rx.recv())
        .await
        .unwrap()
        .unwrap();
    task.abort();
    let _ = task.await;
    let fqdn = format!("_acme-challenge.{name}");
    assert_eq!(h.records(), 1, "the aborted order left its TXT");
    assert_eq!(txt_at_challtestsrv(&fqdn).await.len(), 1);
    assert_eq!(Ledger::new(h.state_dir.clone()).load().unwrap().len(), 1);
    let restarted = engine(&h, kill_cfg, Some(h.token_provider()));
    restarted.cleanup_pending().await;
    assert_eq!(h.records(), 0);
    assert_eq!(txt_at_challtestsrv(&fqdn).await, Vec::<String>::new());
    assert_eq!(Ledger::new(h.state_dir.clone()).load().unwrap(), Vec::new());
}

#[test]
#[ignore = "needs Pebble; run by the acme-pebble CI job"]
fn service_thread_issues_and_stops() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let h = rt.block_on(harness(CfAuth::Token("tok".into())));
    let name = format!("{}.{ZONE}", unique("svc"));
    let cfg = config(
        &env("PEBBLE_DIRECTORY"),
        &h.state_dir,
        AcmeChallenge::Dns01,
        std::slice::from_ref(&name),
    );
    let e = engine(&h, cfg, Some(h.token_provider()));
    let handle = AcmeService::spawn(e).unwrap();
    let deadline = Instant::now() + Duration::from_secs(90);
    while h.source.status().state != TlsState::Serving && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(200));
    }
    let status = h.source.status();
    assert_eq!(status.state, TlsState::Serving, "{status:?}");
    // Status carries the schedule once the loop re-evaluates.
    let deadline = Instant::now() + Duration::from_secs(30);
    while h.source.status().renewal_due_at.is_none() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(200));
    }
    assert!(h.source.status().renewal_due_at.is_some());
    let stopped = Instant::now();
    drop(handle);
    assert!(stopped.elapsed() <= Duration::from_secs(3));
    assert_eq!(h.records(), 0);
    drop(rt);
}
