//! Shared fixtures for the ACME tests: a throwaway TLS fixture CA, an HTTPS
//! server on `[::1]:0`, and a fake Cloudflare API with fault knobs.
#![allow(
    dead_code,
    unreachable_pub,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::missing_panics_doc
)]

use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use axum::extract::{Path as AxPath, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{delete, get};
use axum::{Json, Router};
use serde_json::{json, Value};

/// A private 0700 directory under a temp root.
pub fn private_dir(root: &Path, name: &str) -> PathBuf {
    let dir = root.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    dir
}

/// A self-signed fixture CA plus one leaf for `localhost` / `::1`.
pub struct FixturePki {
    pub root_pem: String,
    pub leaf: rustls::pki_types::CertificateDer<'static>,
    pub leaf_key: rustls::pki_types::PrivateKeyDer<'static>,
}

pub fn fixture_pki() -> FixturePki {
    let ca_key = rcgen::KeyPair::generate().unwrap();
    let mut ca_params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    ca_params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "acme test fixture root");
    let ca_cert = ca_params.self_signed(&ca_key).unwrap();
    let issuer = rcgen::Issuer::new(ca_params, ca_key);
    let leaf_key = rcgen::KeyPair::generate().unwrap();
    let leaf_params =
        rcgen::CertificateParams::new(vec!["localhost".to_owned(), "::1".to_owned()]).unwrap();
    let leaf = leaf_params.signed_by(&leaf_key, &issuer).unwrap();
    FixturePki {
        root_pem: ca_cert.pem(),
        leaf: leaf.der().clone(),
        leaf_key: rustls::pki_types::PrivateKeyDer::Pkcs8(leaf_key.serialize_der().into()),
    }
}

/// Write the fixture root as a PEM file and return its path.
pub fn write_root(pki: &FixturePki, dir: &Path) -> PathBuf {
    let path = dir.join("fixture-root.pem");
    std::fs::write(&path, &pki.root_pem).unwrap();
    path
}

/// Serve `app` over HTTPS on `[::1]:0`; returns the bound address.
pub async fn serve_https(app: Router, pki: &FixturePki) -> SocketAddr {
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let mut config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![pki.leaf.clone()], pki.leaf_key.clone_key())
        .unwrap();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
    let listener = tokio::net::TcpListener::bind("[::1]:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let acceptor = acceptor.clone();
            let app = app.clone();
            tokio::spawn(async move {
                let Ok(tls) = acceptor.accept(stream).await else {
                    return;
                };
                let service = hyper::service::service_fn(
                    move |req: hyper::Request<hyper::body::Incoming>| {
                        let app = app.clone();
                        async move {
                            use tower::ServiceExt;
                            app.oneshot(req.map(axum::body::Body::new)).await
                        }
                    },
                );
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(hyper_util::rt::TokioIo::new(tls), service)
                    .await;
            });
        }
    });
    addr
}

/// Fault knobs of the fake Cloudflare API.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CfFault {
    #[default]
    None,
    /// Every call answers 500.
    ServerError,
    /// Every call answers 429.
    RateLimited,
    /// Records are accepted but never written to DNS.
    AcceptButDoNotWrite,
}

/// Which credential the fake expects.
#[derive(Debug, Clone)]
pub enum CfAuth {
    Token(String),
    GlobalKey { email: String, key: String },
}

#[derive(Debug, Clone)]
pub struct CfRecord {
    pub id: String,
    pub zone: String,
    pub name: String,
    pub content: String,
}

/// Shared state of the fake API.
#[derive(Debug)]
pub struct CfState {
    pub auth: CfAuth,
    /// zone name → zone id.
    pub zones: Vec<(String, String)>,
    pub records: Vec<CfRecord>,
    pub fault: CfFault,
    pub next_id: u64,
    /// challtestsrv management URL to mirror TXT writes into, if any.
    pub challtestsrv: Option<String>,
    /// Every request seen: (method, path, auth-ok).
    pub log: Vec<(String, String, bool)>,
    /// Header pairs seen on the last request (names lowercased).
    pub last_headers: Vec<(String, String)>,
}

pub type SharedCf = Arc<Mutex<CfState>>;

pub fn cf_state(auth: CfAuth, zones: &[(&str, &str)], challtestsrv: Option<String>) -> SharedCf {
    Arc::new(Mutex::new(CfState {
        auth,
        zones: zones
            .iter()
            .map(|(n, i)| ((*n).to_owned(), (*i).to_owned()))
            .collect(),
        records: Vec::new(),
        fault: CfFault::None,
        next_id: 1,
        challtestsrv,
        log: Vec::new(),
        last_headers: Vec::new(),
    }))
}

fn check(
    state: &SharedCf,
    method: &str,
    path: &str,
    headers: &HeaderMap,
) -> Result<(), (StatusCode, Json<Value>)> {
    let mut s = state.lock().unwrap();
    let ok = match &s.auth {
        CfAuth::Token(t) => {
            headers.get("authorization").and_then(|v| v.to_str().ok())
                == Some(format!("Bearer {t}").as_str())
        }
        CfAuth::GlobalKey { email, key } => {
            headers.get("x-auth-email").and_then(|v| v.to_str().ok()) == Some(email.as_str())
                && headers.get("x-auth-key").and_then(|v| v.to_str().ok()) == Some(key.as_str())
        }
    };
    s.log.push((method.to_owned(), path.to_owned(), ok));
    s.last_headers = headers
        .iter()
        .map(|(k, v)| (k.as_str().to_owned(), v.to_str().unwrap_or("").to_owned()))
        .collect();
    match s.fault {
        CfFault::ServerError => {
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"success": false})),
            ))
        }
        CfFault::RateLimited => {
            return Err((
                StatusCode::TOO_MANY_REQUESTS,
                Json(json!({"success": false})),
            ))
        }
        _ => {}
    }
    if !ok {
        return Err((
            StatusCode::FORBIDDEN,
            Json(
                json!({"success": false, "errors": [{"code": 10000, "message": "Authentication error"}]}),
            ),
        ));
    }
    Ok(())
}

async fn verify(
    State(s): State<SharedCf>,
    h: HeaderMap,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    check(&s, "GET", "/user/tokens/verify", &h)?;
    Ok(Json(
        json!({"success": true, "result": {"id": "t1", "status": "active"}}),
    ))
}

async fn zones(
    State(s): State<SharedCf>,
    h: HeaderMap,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    check(
        &s,
        "GET",
        &format!("/zones?name={}", q.get("name").cloned().unwrap_or_default()),
        &h,
    )?;
    let name = q.get("name").cloned().unwrap_or_default();
    let st = s.lock().unwrap();
    let result: Vec<Value> = st
        .zones
        .iter()
        .filter(|(n, _)| *n == name)
        .map(|(n, i)| json!({"id": i, "name": n, "status": "active"}))
        .collect();
    Ok(Json(json!({"success": true, "result": result})))
}

async fn challtestsrv_post(base: &str, path: &str, body: Value) {
    let client = reqwest::Client::new();
    let _ = client
        .post(format!("{base}{path}"))
        .json(&body)
        .send()
        .await;
}

async fn create(
    State(s): State<SharedCf>,
    h: HeaderMap,
    AxPath(zone): AxPath<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    check(&s, "POST", &format!("/zones/{zone}/dns_records"), &h)?;
    let name = body["name"].as_str().unwrap_or_default().to_owned();
    let content = body["content"].as_str().unwrap_or_default().to_owned();
    assert_eq!(body["type"], "TXT");
    assert_eq!(body["ttl"], 60);
    let (id, mirror) = {
        let mut st = s.lock().unwrap();
        if !st.zones.iter().any(|(_, i)| *i == zone) {
            return Err((StatusCode::NOT_FOUND, Json(json!({"success": false}))));
        }
        let id = format!("{:032x}", st.next_id);
        st.next_id += 1;
        st.records.push(CfRecord {
            id: id.clone(),
            zone: zone.clone(),
            name: name.clone(),
            content: content.clone(),
        });
        let mirror = if st.fault == CfFault::AcceptButDoNotWrite {
            None
        } else {
            st.challtestsrv.clone()
        };
        (id, mirror)
    };
    if let Some(base) = mirror {
        challtestsrv_post(
            &base,
            "/set-txt",
            json!({"host": format!("{name}."), "value": content}),
        )
        .await;
    }
    Ok(Json(
        json!({"success": true, "result": {"id": id, "type": "TXT", "name": name}}),
    ))
}

async fn remove(
    State(s): State<SharedCf>,
    h: HeaderMap,
    AxPath((zone, id)): AxPath<(String, String)>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    check(&s, "DELETE", &format!("/zones/{zone}/dns_records/{id}"), &h)?;
    let (removed, mirror, remaining) = {
        let mut st = s.lock().unwrap();
        let pos = st.records.iter().position(|r| r.id == id && r.zone == zone);
        let removed = pos.map(|p| st.records.remove(p));
        let remaining: Vec<CfRecord> = removed
            .as_ref()
            .map(|r| {
                st.records
                    .iter()
                    .filter(|x| x.name == r.name)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        (removed, st.challtestsrv.clone(), remaining)
    };
    let Some(record) = removed else {
        return Err((StatusCode::NOT_FOUND, Json(json!({"success": false}))));
    };
    if let Some(base) = mirror {
        // challtestsrv keeps every value per name: clear, then re-add survivors.
        challtestsrv_post(
            &base,
            "/clear-txt",
            json!({"host": format!("{}.", record.name)}),
        )
        .await;
        for r in remaining {
            challtestsrv_post(
                &base,
                "/set-txt",
                json!({"host": format!("{}.", r.name), "value": r.content}),
            )
            .await;
        }
    }
    Ok(Json(json!({"success": true, "result": {"id": record.id}})))
}

/// The fake API router under `/client/v4`.
pub fn cf_router(state: SharedCf) -> Router {
    Router::new()
        .route("/client/v4/user/tokens/verify", get(verify))
        .route("/client/v4/zones", get(zones))
        .route(
            "/client/v4/zones/{zone}/dns_records",
            axum::routing::post(create),
        )
        .route("/client/v4/zones/{zone}/dns_records/{id}", delete(remove))
        .with_state(state)
}
