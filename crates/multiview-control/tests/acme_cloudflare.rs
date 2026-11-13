//! Cloudflare provider request shapes against an in-process fake API on
//! `[::1]` (HTTPS with a throwaway fixture root), via the `_test-seams` constructor.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod acme_support;

use std::time::Duration;

use acme_support::{
    cf_router, cf_state, fake_dns, fixture_pki, serve_https, write_root, CfAuth, CfFault, DohMode,
    FakeDns,
};
use multiview_config::DnsConfirm;
use multiview_control::acme::cloudflare::{CloudflareCredential, CloudflareDns, TestConfirm};
use multiview_control::acme::{AcmeError, DnsProvider, TxtHandle};
use zeroize::Zeroizing;

const ZONE_ID: &str = "0123456789abcdef0123456789abcdef";
const SUB_ZONE_ID: &str = "fedcba9876543210fedcba9876543210";

async fn fake(auth: CfAuth) -> (acme_support::SharedCf, CloudflareDns, tempfile::TempDir) {
    let state = cf_state(
        auth.clone(),
        &[("example.org", ZONE_ID), ("deep.example.org", SUB_ZONE_ID)],
        None,
    );
    let pki = fixture_pki();
    let addr = serve_https(cf_router(state.clone()), &pki).await;
    let tmp = tempfile::tempdir().unwrap();
    let root = write_root(&pki, tmp.path());
    let credential = match auth {
        CfAuth::Token(t) => CloudflareCredential::Token(Zeroizing::new(t)),
        CfAuth::GlobalKey { email, key } => CloudflareCredential::GlobalKey {
            email: Zeroizing::new(email),
            key: Zeroizing::new(key),
        },
    };
    let dns = CloudflareDns::for_tests(
        credential,
        &format!("https://[::1]:{}/client/v4", addr.port()),
        addr.port(),
        Some(&root),
        Vec::new(),
    )
    .unwrap();
    (state, dns, tmp)
}

fn header(state: &acme_support::SharedCf, name: &str) -> Option<String> {
    state
        .lock()
        .unwrap()
        .last_headers
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.clone())
}

#[tokio::test]
async fn token_credential_sends_bearer_and_verifies_first() {
    let (state, dns, _t) = fake(CfAuth::Token("tok-abc".into())).await;
    dns.validate_credential(&["a.example.org".into(), "*.w.example.org".into()])
        .await
        .unwrap();
    assert_eq!(
        header(&state, "authorization").as_deref(),
        Some("Bearer tok-abc")
    );
    assert_eq!(header(&state, "x-auth-key"), None);
    let log = state.lock().unwrap().log.clone();
    assert_eq!(log[0].1, "/user/tokens/verify");
    assert!(log.iter().all(|(_, _, ok)| *ok));
    // Longest suffix first: a.example.org (miss) then example.org (hit).
    assert_eq!(log[1].1, "/zones?name=a.example.org");
    assert_eq!(log[2].1, "/zones?name=example.org");
    // Wildcard stripped: w.example.org, then example.org.
    assert!(log.iter().any(|(_, p, _)| p == "/zones?name=w.example.org"));
    assert!(!log.iter().any(|(_, p, _)| p.contains('*')));
}

#[tokio::test]
async fn global_key_sends_email_and_key_and_skips_token_verify() {
    let (state, dns, _t) = fake(CfAuth::GlobalKey {
        email: "ops@example.org".into(),
        key: "gk-123".into(),
    })
    .await;
    dns.validate_credential(&["a.example.org".into()])
        .await
        .unwrap();
    assert_eq!(
        header(&state, "x-auth-email").as_deref(),
        Some("ops@example.org")
    );
    assert_eq!(header(&state, "x-auth-key").as_deref(), Some("gk-123"));
    assert_eq!(header(&state, "authorization"), None);
    assert!(!state
        .lock()
        .unwrap()
        .log
        .iter()
        .any(|(_, p, _)| p.contains("tokens/verify")));
}

#[tokio::test]
async fn wrong_credential_is_a_credential_error() {
    let (state, dns, _t) = fake(CfAuth::Token("right".into())).await;
    state.lock().unwrap().auth = CfAuth::Token("other".into());
    assert_eq!(
        dns.validate_credential(&["a.example.org".into()])
            .await
            .unwrap_err(),
        AcmeError::Credential
    );
}

#[tokio::test]
async fn zone_lookup_uses_the_longest_suffix() {
    let (state, dns, _t) = fake(CfAuth::Token("t".into())).await;
    let handle = dns
        .publish_txt("_acme-challenge.x.deep.example.org", "v1")
        .await
        .unwrap();
    assert!(handle.0.starts_with(SUB_ZONE_ID), "{handle:?}");
    let records = state.lock().unwrap().records.clone();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].zone, SUB_ZONE_ID);
    assert_eq!(records[0].name, "_acme-challenge.x.deep.example.org");
}

#[tokio::test]
async fn unknown_zone_is_a_credential_error() {
    let (_state, dns, _t) = fake(CfAuth::Token("t".into())).await;
    assert_eq!(
        dns.validate_credential(&["host.other.test".into()])
            .await
            .unwrap_err(),
        AcmeError::Credential
    );
}

#[tokio::test]
async fn two_values_on_one_name_are_two_records_and_delete_404_is_success() {
    let (state, dns, _t) = fake(CfAuth::Token("t".into())).await;
    let a = dns
        .publish_txt("_acme-challenge.w.example.org", "v1")
        .await
        .unwrap();
    let b = dns
        .publish_txt("_acme-challenge.w.example.org", "v2")
        .await
        .unwrap();
    assert_ne!(a, b);
    assert_eq!(state.lock().unwrap().records.len(), 2);
    dns.remove_txt(&a).await.unwrap();
    assert_eq!(state.lock().unwrap().records.len(), 1);
    dns.remove_txt(&b).await.unwrap();
    assert_eq!(state.lock().unwrap().records.len(), 0);
    // Already gone → 404 → counts as removed.
    dns.remove_txt(&a).await.unwrap();
    let log = state.lock().unwrap().log.clone();
    assert!(log
        .iter()
        .any(|(m, p, _)| m == "DELETE" && p.ends_with(&a.0[ZONE_ID.len() + 1..])));
}

#[tokio::test]
async fn hostile_handles_are_refused_before_any_request() {
    let (state, dns, _t) = fake(CfAuth::Token("t".into())).await;
    for bad in ["../zones", "a/b/c", "zone/../x", "z/", "/r", "noslash"] {
        assert!(
            dns.remove_txt(&TxtHandle(bad.into())).await.is_err(),
            "{bad}"
        );
    }
    assert_eq!(state.lock().unwrap().log.len(), 0);
}

#[tokio::test]
async fn rate_limit_and_server_errors_are_dns_api_errors() {
    for fault in [CfFault::RateLimited, CfFault::ServerError] {
        let (state, dns, _t) = fake(CfAuth::Token("t".into())).await;
        state.lock().unwrap().fault = fault;
        assert_eq!(
            dns.publish_txt("_acme-challenge.a.example.org", "v")
                .await
                .unwrap_err(),
            AcmeError::DnsApi,
            "{fault:?}"
        );
        assert_eq!(
            dns.validate_credential(&["a.example.org".into()])
                .await
                .unwrap_err(),
            AcmeError::DnsApi
        );
    }
}

#[tokio::test]
async fn production_constructor_refuses_a_loopback_api() {
    // The fixture is on [::1]; a provider built WITHOUT the loopback port seam
    // must refuse to dial it (the egress screen fires before any request).
    let (state, _dns, tmp) = fake(CfAuth::Token("t".into())).await;
    let pki_root = tmp.path().join("fixture-root.pem");
    let port = 1; // not the fixture port: loopback stays blocked
    let dns = CloudflareDns::for_tests(
        CloudflareCredential::Token(Zeroizing::new("t".into())),
        "https://[::1]:9/client/v4",
        port,
        Some(&pki_root),
        Vec::new(),
    )
    .unwrap();
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        dns.validate_credential(&["a.example.org".into()]),
    )
    .await
    .unwrap();
    assert_eq!(result.unwrap_err(), AcmeError::Egress);
    assert_eq!(state.lock().unwrap().log.len(), 0);
}

#[test]
fn debug_of_the_provider_is_redacted() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let (_s, dns, _t) = fake(CfAuth::GlobalKey {
            email: "SECRET@example.org".into(),
            key: "SECRETKEY".into(),
        })
        .await;
        let debug = format!("{dns:?}");
        assert!(!debug.contains("SECRET"), "{debug}");
    });
}

/// A provider whose API and `DoH` endpoint are the same fake (`state`), with
/// the given confirmation mode, probe targets and authoritative servers.
async fn fake_with_confirm(
    state: &acme_support::SharedCf,
    mode: DnsConfirm,
    probe: Vec<std::net::SocketAddr>,
    authoritative: Vec<std::net::SocketAddr>,
) -> (CloudflareDns, tempfile::TempDir) {
    let pki = fixture_pki();
    let addr = serve_https(cf_router(state.clone()), &pki).await;
    let tmp = tempfile::tempdir().unwrap();
    let root = write_root(&pki, tmp.path());
    let dns = CloudflareDns::for_tests_with_confirm(
        CloudflareCredential::Token(Zeroizing::new("t".into())),
        &format!("https://[::1]:{}/client/v4", addr.port()),
        addr.port(),
        Some(&root),
        authoritative,
        TestConfirm {
            mode,
            probe,
            doh_url: format!("https://[::1]:{}/dns-query", addr.port()),
            doh_port: addr.port(),
        },
    )
    .unwrap();
    (dns, tmp)
}

fn doh_state() -> acme_support::SharedCf {
    cf_state(CfAuth::Token("t".into()), &[("example.org", ZONE_ID)], None)
}

const FQDN: &str = "_acme-challenge.a.example.org";

#[tokio::test]
async fn auto_detects_an_intercepting_resolver_and_confirms_via_doh() {
    // The "network" answers every port-53 query itself, including those to
    // the probe target and to the authoritative server: the record can never
    // be seen there.
    let (hijack, _task) = fake_dns(FakeDns::Intercepting).await;
    let state = doh_state();
    let (dns, _t) = fake_with_confirm(&state, DnsConfirm::Auto, vec![hijack], vec![hijack]).await;
    dns.publish_txt(FQDN, "the-value").await.unwrap();
    tokio::time::timeout(
        Duration::from_secs(6),
        dns.confirm_at_authoritative(FQDN, "the-value", Duration::from_secs(5)),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(state.lock().unwrap().doh_queries >= 1);
    let detail = dns.propagation_detail().unwrap();
    assert!(detail.contains("DNS over HTTPS"), "{detail}");
    assert!(detail.contains("intercepted"), "{detail}");
}

#[tokio::test]
async fn doh_that_never_shows_the_value_is_a_propagation_error() {
    for mode in [DohMode::Empty, DohMode::Value("another-value".into())] {
        let state = doh_state();
        state.lock().unwrap().doh = mode.clone();
        let (dns, _t) = fake_with_confirm(&state, DnsConfirm::Doh, Vec::new(), Vec::new()).await;
        dns.publish_txt(FQDN, "the-value").await.unwrap();
        let result = tokio::time::timeout(
            Duration::from_secs(6),
            dns.confirm_at_authoritative(FQDN, "the-value", Duration::from_secs(5)),
        )
        .await
        .unwrap();
        assert_eq!(result.unwrap_err(), AcmeError::Propagation, "{mode:?}");
        assert!(state.lock().unwrap().doh_queries >= 1, "{mode:?}");
    }
}

#[tokio::test]
async fn auto_with_a_silent_probe_uses_the_authoritative_servers() {
    let (silent, _quiet) = fake_dns(FakeDns::Silent).await;
    let state = doh_state();
    // An honest authoritative server answering from the fake's records.
    let (auth, _task) = fake_dns(FakeDns::Authoritative(state.clone())).await;
    let (dns, _t) = fake_with_confirm(&state, DnsConfirm::Auto, vec![silent], vec![auth]).await;
    dns.publish_txt(FQDN, "the-value").await.unwrap();
    tokio::time::timeout(
        Duration::from_secs(6),
        dns.confirm_at_authoritative(FQDN, "the-value", Duration::from_secs(5)),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(state.lock().unwrap().doh_queries, 0, "DoH never asked");
    let detail = dns.propagation_detail().unwrap();
    assert!(detail.contains("authoritative"), "{detail}");
}

/// Review C2: only a reply to THIS probe query (QR=response, same id) from a
/// probe target counts as interception. A stray datagram with another id, or
/// a correct-looking reply from a peer that is not a probe target, leaves the
/// verdict at "authoritative" (no `DoH` query at all).
#[tokio::test]
async fn a_probe_reply_with_the_wrong_id_or_from_elsewhere_is_not_interception() {
    for mode in [FakeDns::WrongId, FakeDns::ReplyFromElsewhere] {
        let label = format!("{mode:?}");
        let (probe, _p) = fake_dns(mode).await;
        let state = doh_state();
        let (auth, _a) = fake_dns(FakeDns::Authoritative(state.clone())).await;
        let (dns, _t) = fake_with_confirm(&state, DnsConfirm::Auto, vec![probe], vec![auth]).await;
        dns.publish_txt(FQDN, "the-value").await.unwrap();
        tokio::time::timeout(
            Duration::from_secs(6),
            dns.confirm_at_authoritative(FQDN, "the-value", Duration::from_secs(5)),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(
            state.lock().unwrap().doh_queries,
            0,
            "{label}: DoH never asked"
        );
        let detail = dns.propagation_detail().unwrap();
        assert!(detail.contains("authoritative"), "{label}: {detail}");
        assert!(!detail.contains("intercepted"), "{label}: {detail}");
    }
}

/// #374: a resolver front end that keeps answering the pre-publish negative on
/// the connection it gave it. A pooled (kept-alive) client never leaves that
/// connection and times out; every poll must use a fresh connection.
#[tokio::test]
async fn doh_polls_do_not_stay_on_a_connection_that_cached_the_negative() {
    let state = doh_state();
    state.lock().unwrap().doh = DohMode::StickyPerConnection { hidden: 1 };
    let (dns, _t) = fake_with_confirm(&state, DnsConfirm::Doh, Vec::new(), Vec::new()).await;
    dns.publish_txt(FQDN, "the-value").await.unwrap();
    tokio::time::timeout(
        Duration::from_secs(8),
        dns.confirm_at_authoritative(FQDN, "the-value", Duration::from_secs(7)),
    )
    .await
    .unwrap()
    .unwrap();
    let st = state.lock().unwrap();
    assert!(st.doh_queries >= 2, "{}", st.doh_queries);
    assert_eq!(
        st.doh_conns.len(),
        st.doh_queries,
        "one connection per poll"
    );
}

/// Every silent "not seen" now carries its reason into the failure detail.
#[tokio::test]
async fn a_doh_failure_detail_names_the_last_reason() {
    for (mode, expect) in [
        (DohMode::Empty, "rcode 0 (NOERROR), 0 answer(s), 0 TXT"),
        (
            DohMode::Value("another-value".into()),
            "1 answer(s), 1 TXT, value absent",
        ),
        (DohMode::Status(503), "HTTP status 503"),
    ] {
        let state = doh_state();
        state.lock().unwrap().doh = mode.clone();
        let (dns, _t) = fake_with_confirm(&state, DnsConfirm::Doh, Vec::new(), Vec::new()).await;
        dns.publish_txt(FQDN, "the-value").await.unwrap();
        let result = tokio::time::timeout(
            Duration::from_secs(6),
            dns.confirm_at_authoritative(FQDN, "the-value", Duration::from_secs(5)),
        )
        .await
        .unwrap();
        assert_eq!(result.unwrap_err(), AcmeError::Propagation, "{mode:?}");
        let detail = dns.propagation_detail().unwrap();
        assert!(detail.contains("last DoH answer: "), "{detail}");
        assert!(detail.contains(expect), "{mode:?}: {detail}");
        assert!(!detail.contains("the-value"), "{detail}");
    }
}

/// The `DoH` egress screen still refuses a loopback endpoint that the test
/// seam did not admit, and the detail says so.
#[tokio::test]
async fn a_screened_doh_endpoint_is_reported_as_refused() {
    let state = doh_state();
    let pki = fixture_pki();
    let addr = serve_https(cf_router(state.clone()), &pki).await;
    let tmp = tempfile::tempdir().unwrap();
    let root = write_root(&pki, tmp.path());
    let dns = CloudflareDns::for_tests_with_confirm(
        CloudflareCredential::Token(Zeroizing::new("t".into())),
        &format!("https://[::1]:{}/client/v4", addr.port()),
        addr.port(),
        Some(&root),
        Vec::new(),
        TestConfirm {
            mode: DnsConfirm::Doh,
            probe: Vec::new(),
            // Same server, but the DoH client admits loopback on another port only.
            doh_url: format!("https://[::1]:{}/dns-query", addr.port()),
            doh_port: addr.port().wrapping_add(1),
        },
    )
    .unwrap();
    dns.publish_txt(FQDN, "the-value").await.unwrap();
    let result = tokio::time::timeout(
        Duration::from_secs(6),
        dns.confirm_at_authoritative(FQDN, "the-value", Duration::from_secs(5)),
    )
    .await
    .unwrap();
    assert_eq!(result.unwrap_err(), AcmeError::Propagation);
    assert_eq!(state.lock().unwrap().doh_queries, 0, "never dialled");
    let detail = dns.propagation_detail().unwrap();
    assert!(detail.contains("egress refused (loopback)"), "{detail}");
}
