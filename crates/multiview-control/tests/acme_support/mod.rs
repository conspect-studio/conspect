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
    /// DNS-over-HTTPS queries answered by the fake `/dns-query` route.
    pub doh_queries: usize,
    /// How the fake `DoH` route answers.
    pub doh: DohMode,
}

/// How the fake `DoH` route answers TXT queries.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum DohMode {
    /// From the published records.
    #[default]
    Records,
    /// Never any TXT (NOERROR, empty answer).
    Empty,
    /// A TXT with this value instead of the published one.
    Value(String),
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
        doh_queries: 0,
        doh: DohMode::Records,
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

/// A TXT answer for one query message (`None` if `query` does not parse).
fn txt_response(query: &[u8], values: &[String]) -> Option<Vec<u8>> {
    use hickory_resolver::proto::op::{Message, MessageType, OpCode};
    use hickory_resolver::proto::rr::rdata::TXT;
    use hickory_resolver::proto::rr::{RData, Record};
    let request = Message::from_vec(query).ok()?;
    let question = request.queries.first()?.clone();
    let mut response = Message::new(request.metadata.id, MessageType::Response, OpCode::Query);
    response.metadata.recursion_desired = request.metadata.recursion_desired;
    response.metadata.recursion_available = true;
    response.add_query(question.clone());
    for value in values {
        response.add_answer(Record::from_rdata(
            question.name().clone(),
            60,
            RData::TXT(TXT::new(vec![value.clone()])),
        ));
    }
    response.to_vec().ok()
}

/// The TXT values the fake currently serves for the name asked in `query`.
fn published_values(state: &SharedCf, query: &[u8]) -> Vec<String> {
    use hickory_resolver::proto::op::Message;
    let Some(name) = Message::from_vec(query)
        .ok()
        .and_then(|m| m.queries.first().map(|q| q.name().to_ascii()))
    else {
        return Vec::new();
    };
    let name = name.trim_end_matches('.').to_ascii_lowercase();
    state
        .lock()
        .unwrap()
        .records
        .iter()
        .filter(|r| r.name.to_ascii_lowercase() == name)
        .map(|r| r.content.clone())
        .collect()
}

/// RFC 8484 GET `/dns-query?dns=<base64url>`: answers TXT per [`DohMode`].
async fn doh(
    State(s): State<SharedCf>,
    h: HeaderMap,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> Result<([(axum::http::HeaderName, &'static str); 1], Vec<u8>), StatusCode> {
    use base64::Engine as _;
    assert_eq!(
        h.get("accept").and_then(|v| v.to_str().ok()),
        Some("application/dns-message")
    );
    let wire = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(q.get("dns").ok_or(StatusCode::BAD_REQUEST)?)
        .map_err(|_| StatusCode::BAD_REQUEST)?;
    let mode = {
        let mut st = s.lock().unwrap();
        st.doh_queries += 1;
        st.doh.clone()
    };
    let values = match mode {
        DohMode::Records => published_values(&s, &wire),
        DohMode::Empty => Vec::new(),
        DohMode::Value(v) => vec![v],
    };
    let body = txt_response(&wire, &values).ok_or(StatusCode::BAD_REQUEST)?;
    Ok((
        [(axum::http::header::CONTENT_TYPE, "application/dns-message")],
        body,
    ))
}

/// How a fake port-53 server answers.
#[derive(Debug, Clone)]
pub enum FakeDns {
    /// A hijacking resolver: every query gets NOERROR, no answer, and a bogus
    /// SOA (`a.misconfigured.dns.server.invalid`) in the authority section.
    Intercepting,
    /// An honest authoritative server for the fake's published TXT records.
    Authoritative(SharedCf),
    /// Never answers.
    Silent,
    /// Like [`FakeDns::Intercepting`], but the reply carries a different id
    /// than the query (a stray datagram, not an answer to it).
    WrongId,
    /// Like [`FakeDns::Intercepting`], but the (otherwise correct) reply is
    /// sent from a different socket than the one the query went to.
    ReplyFromElsewhere,
}

/// A hijacker's answer to `request` (NOERROR, no answer, bogus SOA) with `id`.
fn hijacked_reply(request: hickory_resolver::proto::op::Message, id: u16) -> Vec<u8> {
    use hickory_resolver::proto::op::{Message, MessageType, OpCode};
    use hickory_resolver::proto::rr::rdata::SOA;
    use hickory_resolver::proto::rr::{Name, RData, Record};
    let mut m = Message::new(id, MessageType::Response, OpCode::Query);
    m.metadata.recursion_available = true;
    for q in request.queries {
        m.add_query(q);
    }
    let bogus = Name::from_ascii("a.misconfigured.dns.server.invalid.").unwrap();
    m.add_authority(Record::from_rdata(
        Name::root(),
        60,
        RData::SOA(SOA::new(bogus.clone(), bogus, 1, 3600, 600, 86400, 60)),
    ));
    m.to_vec().unwrap()
}

/// A UDP DNS server on `[::1]:0` (see [`FakeDns`]); returns its address.
/// The socket lives as long as the returned task.
pub async fn fake_dns(mode: FakeDns) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    use hickory_resolver::proto::op::Message;
    let socket = tokio::net::UdpSocket::bind("[::1]:0").await.unwrap();
    let addr = socket.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let mut buf = [0_u8; 1500];
        loop {
            let Ok((len, peer)) = socket.recv_from(&mut buf).await else {
                return;
            };
            let query = buf[..len].to_vec();
            let reply = match &mode {
                FakeDns::Silent => continue,
                FakeDns::Authoritative(state) => {
                    let values = published_values(state, &query);
                    txt_response(&query, &values).map(|wire| {
                        let mut m = Message::from_vec(&wire).unwrap();
                        m.metadata.authoritative = true;
                        m.to_vec().unwrap()
                    })
                }
                FakeDns::Intercepting | FakeDns::ReplyFromElsewhere => {
                    Message::from_vec(&query).ok().map(|request| {
                        let id = request.metadata.id;
                        hijacked_reply(request, id)
                    })
                }
                FakeDns::WrongId => Message::from_vec(&query).ok().map(|request| {
                    let id = request.metadata.id.wrapping_add(1);
                    hijacked_reply(request, id)
                }),
            };
            if let (Some(reply), FakeDns::ReplyFromElsewhere) = (&reply, &mode) {
                let other = tokio::net::UdpSocket::bind("[::1]:0").await.unwrap();
                let _ = other.send_to(reply, peer).await;
                continue;
            }
            if let Some(reply) = reply {
                let _ = socket.send_to(&reply, peer).await;
            }
        }
    });
    (addr, task)
}

/// The fake API router under `/client/v4`, plus the fake `DoH` endpoint at `/dns-query`.
pub fn cf_router(state: SharedCf) -> Router {
    Router::new()
        .route("/dns-query", get(doh))
        .route("/client/v4/user/tokens/verify", get(verify))
        .route("/client/v4/zones", get(zones))
        .route(
            "/client/v4/zones/{zone}/dns_records",
            axum::routing::post(create),
        )
        .route("/client/v4/zones/{zone}/dns_records/{id}", delete(remove))
        .with_state(state)
}

/// Behaviour knobs of the in-process fake ACME CA ([`fake_ca`]).
#[derive(Debug, Clone)]
pub struct FakeCaMode {
    /// Challenge types the authorization offers (e.g. `["http-01"]`).
    pub offered: Vec<&'static str>,
    /// After `set_ready`, the order turns `invalid` and the authorization
    /// carries this challenge error detail.
    pub fail_detail: String,
    /// How long each deactivation request is held before it is answered
    /// (`None` = answered at once).
    pub deactivate_delay: Option<std::time::Duration>,
}

/// Counters of the fake ACME CA.
#[derive(Debug, Default)]
pub struct FakeCaState {
    pub base: String,
    pub identifier: String,
    pub accounts: usize,
    pub orders: usize,
    pub set_ready: usize,
    pub deactivations: usize,
    pub ready: bool,
}

pub type SharedCa = Arc<Mutex<FakeCaState>>;

/// A minimal RFC 8555 CA over HTTPS on `[::1]:0` (fixture PKI): directory,
/// nonce, account, ONE order for `identifier` with one authorization whose
/// challenges are `mode.offered`; `set_ready` makes the order invalid with
/// `mode.fail_detail`. JWS signatures are not verified (the engine's own
/// account handling is not under test here). Returns the counters, the
/// directory URL and the fixture PKI (its root is the trust anchor).
pub async fn fake_ca(identifier: &str, mode: FakeCaMode) -> (SharedCa, String, FixturePki) {
    let state: SharedCa = Arc::new(Mutex::new(FakeCaState {
        identifier: identifier.to_owned(),
        ..FakeCaState::default()
    }));
    let mode = Arc::new(mode);
    let pki = fixture_pki();
    let app = Router::new()
        .route("/dir", get(ca_directory))
        .route("/nonce", get(ca_nonce))
        .route("/acct", axum::routing::post(ca_account))
        .route("/new-order", axum::routing::post(ca_new_order))
        .route("/order/1", axum::routing::post(ca_order))
        .route("/authz/1", axum::routing::post(ca_authz))
        .route("/chall/{kind}", axum::routing::post(ca_challenge))
        .with_state((state.clone(), mode));
    let addr = serve_https(app, &pki).await;
    let base = format!("https://[::1]:{}", addr.port());
    let directory = format!("{base}/dir");
    state.lock().unwrap().base = base;
    (state, directory, pki)
}

type CaState = State<(SharedCa, Arc<FakeCaMode>)>;
type CaReply = (
    StatusCode,
    [(axum::http::HeaderName, String); 2],
    Json<Value>,
);

fn ca_reply(status: StatusCode, location: String, body: Value) -> CaReply {
    (
        status,
        [
            (
                axum::http::HeaderName::from_static("replay-nonce"),
                uuid::Uuid::new_v4().simple().to_string(),
            ),
            (axum::http::header::LOCATION, location),
        ],
        Json(body),
    )
}

/// The decoded JWS payload of a request body (`None` for POST-as-GET).
fn jws_payload(body: &[u8]) -> Option<Value> {
    use base64::Engine as _;
    let jws: Value = serde_json::from_slice(body).ok()?;
    let payload = jws.get("payload")?.as_str()?;
    if payload.is_empty() {
        return None;
    }
    let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    serde_json::from_slice(&raw).ok()
}

async fn ca_directory(State((s, _)): CaState) -> Json<Value> {
    let base = s.lock().unwrap().base.clone();
    Json(json!({
        "newNonce": format!("{base}/nonce"),
        "newAccount": format!("{base}/acct"),
        "newOrder": format!("{base}/new-order"),
    }))
}

async fn ca_nonce() -> ([(axum::http::HeaderName, String); 1], StatusCode) {
    (
        [(
            axum::http::HeaderName::from_static("replay-nonce"),
            uuid::Uuid::new_v4().simple().to_string(),
        )],
        StatusCode::OK,
    )
}

async fn ca_account(State((s, _)): CaState) -> CaReply {
    let base = {
        let mut st = s.lock().unwrap();
        st.accounts += 1;
        st.base.clone()
    };
    ca_reply(
        StatusCode::CREATED,
        format!("{base}/acct/1"),
        json!({"status": "valid"}),
    )
}

fn order_body(st: &FakeCaState) -> Value {
    let base = &st.base;
    let mut body = json!({
        "status": if st.ready { "invalid" } else { "pending" },
        "identifiers": [{"type": "dns", "value": st.identifier}],
        "authorizations": [format!("{base}/authz/1")],
        "finalize": format!("{base}/finalize/1"),
    });
    if st.ready {
        body["error"] = json!({
            "type": "urn:ietf:params:acme:error:unauthorized",
            "detail": "Error finalizing order",
        });
    }
    body
}

async fn ca_new_order(State((s, _)): CaState) -> CaReply {
    let (base, body) = {
        let mut st = s.lock().unwrap();
        st.orders += 1;
        (st.base.clone(), order_body(&st))
    };
    ca_reply(StatusCode::CREATED, format!("{base}/order/1"), body)
}

async fn ca_order(State((s, _)): CaState) -> CaReply {
    let (base, body) = {
        let st = s.lock().unwrap();
        (st.base.clone(), order_body(&st))
    };
    ca_reply(StatusCode::OK, format!("{base}/order/1"), body)
}

fn authz_body(st: &FakeCaState, mode: &FakeCaMode, status: &str) -> Value {
    let challenges: Vec<Value> = mode
        .offered
        .iter()
        .map(|kind| {
            let mut c = json!({
                "type": kind,
                "url": format!("{}/chall/{kind}", st.base),
                "token": "tok-0123456789abcdefghijklmnopqrstuvwxyz",
                "status": if st.ready { "invalid" } else { "pending" },
            });
            if st.ready {
                c["error"] = json!({
                    "type": "urn:ietf:params:acme:error:connection",
                    "detail": mode.fail_detail,
                });
            }
            c
        })
        .collect();
    json!({
        "identifier": {"type": "dns", "value": st.identifier},
        "status": status,
        "challenges": challenges,
    })
}

async fn ca_authz(State((s, mode)): CaState, body: axum::body::Bytes) -> CaReply {
    let deactivate = jws_payload(&body)
        .and_then(|p| p.get("status").cloned())
        .is_some_and(|v| v == "deactivated");
    if deactivate {
        s.lock().unwrap().deactivations += 1;
        if let Some(delay) = mode.deactivate_delay {
            tokio::time::sleep(delay).await;
        }
    }
    let (base, reply) = {
        let st = s.lock().unwrap();
        let status = if deactivate {
            "deactivated"
        } else if st.ready {
            "invalid"
        } else {
            "pending"
        };
        (st.base.clone(), authz_body(&st, &mode, status))
    };
    ca_reply(StatusCode::OK, format!("{base}/authz/1"), reply)
}

async fn ca_challenge(State((s, _)): CaState, AxPath(kind): AxPath<String>) -> CaReply {
    let base = {
        let mut st = s.lock().unwrap();
        st.set_ready += 1;
        st.ready = true;
        st.base.clone()
    };
    ca_reply(
        StatusCode::OK,
        format!("{base}/chall/{kind}"),
        json!({
            "type": kind,
            "url": format!("{base}/chall/{kind}"),
            "token": "tok-0123456789abcdefghijklmnopqrstuvwxyz",
            "status": "processing",
        }),
    )
}
