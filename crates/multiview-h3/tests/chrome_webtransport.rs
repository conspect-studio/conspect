//! A real headless Chrome opens WebTransport sessions against the h3
//! endpoint (#382). Chrome's CONNECT carries `wt-available-protocols` (an
//! sf-list of sf-strings) when the page passes `protocols`; the session must
//! reach `ready` with `wt.protocol` the server's choice among those.
//!
//! Opt-in (`--features browser-integration`): needs node, the `web/`
//! dependencies (`npm --prefix web ci`) and a Chrome binary (Playwright's
//! chromium in `~/.cache/ms-playwright`, or `CONSPECT_WEB_BROWSER`).
#![cfg(feature = "browser-integration")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::print_stderr
)]

mod common;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use common::*;
use multiview_h3::{AdmitToken, BoxFuture, H3Limits, WtHandler, WtSession};
use web_transport_trait::Session as _;

/// Records what each admitted CONNECT offered and what the session got.
struct Recorder {
    protocols: &'static [&'static str],
    seen: Mutex<Vec<(Option<String>, Option<String>)>>,
}

impl WtHandler for Recorder {
    fn admit(
        &self,
        _req: &http::request::Parts,
        _peer: SocketAddr,
    ) -> Result<AdmitToken, http::StatusCode> {
        Ok(Box::new(()))
    }

    fn accept(
        &self,
        session: WtSession,
        _token: AdmitToken,
        req: http::request::Parts,
        _peer: SocketAddr,
    ) -> BoxFuture<'static, ()> {
        let offered = req
            .headers
            .get("wt-available-protocols")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        self.seen
            .lock()
            .unwrap()
            .push((offered, session.protocol().map(str::to_owned)));
        Box::pin(async move {
            let _ = session.closed().await;
        })
    }

    fn protocols(&self) -> &[&'static str] {
        self.protocols
    }
}

/// A P-256 certificate valid for 10 days: Chrome accepts a
/// `serverCertificateHashes` pin only for an ECDSA certificate valid for at
/// most 14 days.
fn short_cert() -> Cert {
    let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
    let mut params =
        rcgen::CertificateParams::new(vec!["localhost".to_owned(), "::1".to_owned()]).unwrap();
    let now = time::OffsetDateTime::now_utc();
    params.not_before = now - time::Duration::hours(1);
    params.not_after = now + time::Duration::days(10);
    let cert = params.self_signed(&key).unwrap();
    Cert {
        cert: cert.der().clone(),
        key: rustls::pki_types::PrivateKeyDer::Pkcs8(key.serialize_der().into()),
    }
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    })
}

/// Runs the probe; returns its JSON lines.
async fn probe(config: serde_json::Value) -> Vec<serde_json::Value> {
    let root = repo_root();
    let node = std::env::var_os("CONSPECT_WEB_NODE").unwrap_or_else(|| "node".into());
    let output = tokio::process::Command::new(node)
        .arg(root.join("web/scripts/wt-chrome-probe.mjs"))
        .arg(config.to_string())
        .current_dir(root.join("web"))
        .kill_on_drop(true)
        .output();
    let output = tokio::time::timeout(std::time::Duration::from_secs(120), output)
        .await
        .expect("the browser probe timed out")
        .expect("node runs");
    let stdout = String::from_utf8_lossy(&output.stdout);
    eprintln!("probe stdout:\n{stdout}");
    eprintln!("probe stderr:\n{}", String::from_utf8_lossy(&output.stderr));
    stdout
        .lines()
        .filter(|l| l.starts_with('{'))
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chrome_negotiates_an_offered_subprotocol() {
    let cert = short_cert();
    let der = hex(&cert.cert);
    let handler = Arc::new(Recorder {
        protocols: &["moqt-16", "moq-transport-16"],
        seen: Mutex::new(Vec::new()),
    });
    let server = start_with_cert(
        cert,
        TestSite::new(),
        Some(Arc::<Recorder>::clone(&handler)),
        H3Limits::default(),
    )
    .await;
    let url = format!("https://[::1]:{}/moq/?ticket=t", server.addr.port());
    let lines = probe(serde_json::json!({
        "url": url,
        "certDer": der,
        "attempts": [
            { "protocols": null },
            { "protocols": ["moq-transport-16"] },
            { "protocols": ["moqt-16"] },
            { "protocols": ["moq-lite-06", "moqt-18", "moqt-16", "moqt-15"] },
            // Nothing in common: refused with 400 before admit, which
            // Chrome reports as a handshake failure.
            { "protocols": ["moq-00"] },
        ],
    }))
    .await;
    assert_eq!(lines.len(), 5, "{lines:?}");
    for line in &lines[..4] {
        assert_eq!(line["ok"], true, "WebTransport ready: {line}");
    }
    assert_eq!(lines[4]["ok"], false, "{}", lines[4]);
    assert_eq!(lines[0]["protocol"], "");
    assert_eq!(lines[1]["protocol"], "moq-transport-16");
    assert_eq!(lines[2]["protocol"], "moqt-16");
    assert_eq!(lines[3]["protocol"], "moqt-16");
    let seen = handler.seen.lock().unwrap().clone();
    eprintln!("server saw: {seen:?}");
    assert_eq!(seen.len(), 4, "the refused CONNECT never reached accept");
    // Chrome's exact offer: an sf-list of sf-strings.
    assert_eq!(seen[1].0.as_deref(), Some("\"moq-transport-16\""));
    assert_eq!(seen[1].1.as_deref(), Some("moq-transport-16"));
    assert_eq!(seen[3].1.as_deref(), Some("moqt-16"));
    assert_eq!(
        multiview_h3::Counters::get(&server.endpoint.counters().wt_refused),
        1
    );
    server.endpoint.shutdown().await;
}
