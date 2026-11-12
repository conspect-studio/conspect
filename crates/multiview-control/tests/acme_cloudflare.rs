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

use acme_support::{cf_router, cf_state, fixture_pki, serve_https, write_root, CfAuth, CfFault};
use multiview_control::acme::cloudflare::{CloudflareCredential, CloudflareDns};
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
