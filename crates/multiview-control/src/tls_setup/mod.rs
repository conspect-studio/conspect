//! The front-door certificate setup surface (ADR-0107 section 5a, #291).
//!
//! * [`front_door_router`]: the normal control router behind a setup gate.
//!   It is what slice 4's TCP 443 listener mounts. In `setup` only the setup
//!   surface answers (`GET /tls-setup`, `GET /api/v1/tls/status`,
//!   `POST /api/v1/tls/setup/session`, `POST /api/v1/tls/issue`, `GET /` →
//!   redirect); everything else gets `503` `/problems/tls-setup-required` with
//!   `Retry-After: 30`. No HSTS is ever added by this layer. In `serving` the
//!   gate is a single read of the source state per request.
//! * The loopback-only admin routes (`POST /api/v1/tls/setup-code`,
//!   `POST /api/v1/tls/credential`, `POST /api/v1/tls/issue`) and
//!   `GET /api/v1/tls/status`, merged into the existing `[control] listen`
//!   router by [`crate::router`] when [`AppState::with_tls`] is set.
//!
//! Nothing here runs on, or shares a lock with, the output clock: the only
//! shared state is the certificate source's status lock (also used by TLS
//! handshakes, never by a tick) and the setup-code table.

pub mod code;

use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::extract::{ConnectInfo, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use multiview_config::{AcmeChallenge, NetworkTlsConfig};

use crate::acme::IssueTrigger;
use crate::auth::{Action, Role};
use crate::certificate_source::{
    AcmeCertificateSource, CertificateAlarm, CertificateSource, ChallengeResolver, TlsState,
    TLS_SETUP_REQUIRED,
};
use crate::problem::Problem;
use crate::state::AppState;
use crate::tls_api::{
    CredentialRequest, IssueAccepted, SetupCodeBody, SetupSessionRequest, TlsStatusBody,
};

pub use code::SetupAuth;

/// The setup page, self-contained (no external assets). It has no input for
/// any secret: only the one-time setup code.
pub const SETUP_PAGE: &str = include_str!("page.html");
/// The setup page's content security policy.
pub const SETUP_PAGE_CSP: &str =
    "default-src 'self'; script-src 'self' 'unsafe-inline'; style-src 'self' 'unsafe-inline'; \
     frame-ancestors 'none'; base-uri 'none'; form-action 'self'";
/// The setup-session cookie name.
pub const SESSION_COOKIE: &str = "conspect_tls_setup";
/// Largest accepted `POST /api/v1/tls/credential` body.
pub const CREDENTIAL_BODY_LIMIT: usize = 8 * 1024;
/// `Retry-After` on the setup-required problem, seconds.
pub const SETUP_RETRY_AFTER_S: u32 = 30;

/// Everything the TLS routes need, shared through [`AppState`].
#[derive(Clone)]
pub struct TlsContext {
    /// The ACME certificate source.
    pub source: Arc<AcmeCertificateSource>,
    /// The same source behind the front door's handshake seam.
    pub certificates: Arc<dyn CertificateSource>,
    /// The resolver slice 4 and the QUIC listener take (TLS-ALPN-01 aware).
    pub resolver: Arc<ChallengeResolver>,
    /// "Issue now" on the ACME service thread (non-blocking).
    pub issue: IssueTrigger,
    /// The `[network.tls]` configuration (names only; no secret values).
    pub config: Arc<NetworkTlsConfig>,
    /// The setup-code authority.
    pub setup: Arc<SetupAuth>,
    /// Whether the front door sits behind a reverse proxy (section 8): turns
    /// on the TLS-ALPN-01 reachability hint.
    pub behind_proxy: bool,
}

impl std::fmt::Debug for TlsContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TlsContext").finish_non_exhaustive()
    }
}

fn problem(status: u16, slug: &str, title: &str) -> Response {
    Problem::new(status, slug, title).into_response()
}

fn setup_required() -> Response {
    let mut response = Problem::new(
        503,
        TLS_SETUP_REQUIRED.trim_start_matches("/problems/"),
        "Certificate setup required",
    )
    .with_detail(
        "the front door has no ACME certificate yet; only the certificate setup surface is \
         available (GET /tls-setup)",
    )
    .into_response();
    response
        .headers_mut()
        .insert(header::RETRY_AFTER, HeaderValue::from(SETUP_RETRY_AFTER_S));
    response
}

/// Whether a path is part of the setup surface (allowed in `setup`).
fn setup_allowed(method: &Method, path: &str) -> bool {
    matches!(
        (method.as_str(), path),
        ("GET" | "HEAD", "/tls-setup" | "/api/v1/tls/status" | "/")
            | ("POST", "/api/v1/tls/setup/session" | "/api/v1/tls/issue")
    )
}

/// The front-door router (slice 4 mounts it on TCP 443): [`crate::router`]
/// plus the setup surface, behind the setup gate.
///
/// `state` must carry a TLS context ([`AppState::with_tls`]); without one the
/// gate fails closed (every request is `503` setup-required).
pub fn front_door_router(state: AppState) -> Router {
    front_door_router_with(state, |router| router)
}

/// [`front_door_router`] with extra routes added by `extend` before the gate
/// is applied. The run uses it to mount the `/hls/{output}` directories, so
/// they sit behind the setup gate like every other route.
pub fn front_door_router_with(state: AppState, extend: impl FnOnce(Router) -> Router) -> Router {
    let tls = state.tls.clone();
    let page = Router::new()
        .route("/tls-setup", get(setup_page))
        .with_state(state.clone());
    extend(crate::router_with(state, crate::Surface::FrontDoor).merge(page))
        .layer(axum::middleware::from_fn_with_state(tls, setup_gate))
}

/// The `/api/v1/tls/*` routes for one listener.
///
/// * Admin listener (`[control] listen`): status plus the loopback-only admin
///   routes (setup code, credential, issue).
/// * Front door: status, setup-session login and the session-or-admin issue.
///   The loopback-only routes are never mounted there: behind a reverse proxy
///   the TCP peer of every request is the proxy's loopback address.
pub(crate) fn api_routes(surface: crate::Surface) -> Router<AppState> {
    let common = Router::new().route("/tls/status", get(status));
    match surface {
        crate::Surface::Admin => common
            .route("/tls/setup-code", post(admin_setup_code))
            .route("/tls/issue", post(admin_issue))
            .route(
                "/tls/credential",
                post(admin_credential)
                    .layer(axum::extract::DefaultBodyLimit::max(CREDENTIAL_BODY_LIMIT)),
            ),
        crate::Surface::FrontDoor => common
            .route("/tls/setup/session", post(create_session))
            .route("/tls/issue", post(front_issue)),
    }
}

/// The setup gate. In `serving` it is one read of the source state.
async fn setup_gate(
    State(tls): State<Option<Arc<TlsContext>>>,
    request: Request,
    next: Next,
) -> Response {
    let Some(tls) = tls else {
        return setup_required();
    };
    if tls.source.state() == TlsState::Serving {
        return next.run(request).await;
    }
    let path = request.uri().path();
    if path == "/" && matches!(*request.method(), Method::GET | Method::HEAD) {
        return (
            StatusCode::SEE_OTHER,
            [(header::LOCATION, HeaderValue::from_static("/tls-setup"))],
        )
            .into_response();
    }
    if !setup_allowed(request.method(), path) {
        return setup_required();
    }
    let mut response = next.run(request).await;
    // Belt and braces: no HSTS in setup, whatever an inner layer did.
    response
        .headers_mut()
        .remove(header::STRICT_TRANSPORT_SECURITY);
    response
}

async fn setup_page(State(state): State<AppState>) -> Response {
    let serving = state
        .tls
        .as_ref()
        .is_some_and(|tls| tls.source.state() == TlsState::Serving);
    if serving {
        return (
            StatusCode::SEE_OTHER,
            [(header::LOCATION, HeaderValue::from_static("/"))],
        )
            .into_response();
    }
    (
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CONTENT_SECURITY_POLICY, SETUP_PAGE_CSP),
            (header::CACHE_CONTROL, "no-store"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (header::REFERRER_POLICY, "no-referrer"),
        ],
        SETUP_PAGE,
    )
        .into_response()
}

fn peer_ip(request_ext: &axum::http::Extensions) -> Option<IpAddr> {
    request_ext
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(addr)| addr.ip())
}

fn is_loopback(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_loopback(),
        IpAddr::V6(v6) => {
            v6.is_loopback() || v6.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback())
        }
    }
}

async fn create_session(State(state): State<AppState>, request: Request) -> Response {
    let Some(tls) = state.tls.clone() else {
        return problem(404, "tls-not-configured", "No [network.tls] configured");
    };
    if tls.source.state() != TlsState::Setup {
        return problem(409, "tls-not-in-setup", "The front door is already serving");
    }
    let Some(peer) = peer_ip(request.extensions()) else {
        return problem(400, "tls-peer-unknown", "Peer address unavailable");
    };
    let Ok(body) = axum::body::to_bytes(request.into_body(), 4096).await else {
        return problem(413, "payload-too-large", "Body too large");
    };
    let Ok(SetupSessionRequest { code }) = serde_json::from_slice(&body) else {
        return problem(400, "bad-request", "Expected {\"code\": \"...\"}");
    };
    match tls.setup.redeem(&code, peer) {
        Ok(token) => {
            tracing::info!(peer = %peer, "tls setup: setup code redeemed; session created");
            let cookie = format!(
                "{SESSION_COOKIE}={token}; Path=/; HttpOnly; Secure; SameSite=Strict; Max-Age={}",
                code::SESSION_TTL.as_secs()
            );
            let mut response = StatusCode::NO_CONTENT.into_response();
            if let Ok(value) = HeaderValue::from_str(&cookie) {
                response.headers_mut().insert(header::SET_COOKIE, value);
            }
            response
                .headers_mut()
                .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            response
        }
        Err(code::RedeemError::Locked(left)) => {
            tracing::warn!(peer = %peer, "tls setup: source locked after repeated failed setup codes");
            let mut response = problem(
                429,
                "tls-setup-locked",
                "Too many failed setup codes from this source",
            );
            response.headers_mut().insert(
                header::RETRY_AFTER,
                HeaderValue::from(left.as_secs().max(1)),
            );
            response
        }
        Err(code::RedeemError::Invalid) => {
            tracing::warn!(peer = %peer, "tls setup: wrong, expired or used setup code");
            problem(
                401,
                "tls-setup-code-invalid",
                "Wrong, expired or used setup code",
            )
        }
    }
}

fn session_cookie(headers: &HeaderMap) -> Option<&str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(name, _)| *name == SESSION_COOKIE)
        .map(|(_, value)| value)
}

fn admin_principal(parts: &axum::http::request::Parts, state: &AppState) -> bool {
    crate::routes::authenticate_principal(parts, state)
        .is_ok_and(|p| p.role == Role::Admin && p.role.can(Action::Administer))
}

fn trigger_issue(tls: &TlsContext) -> Response {
    match (tls.issue)() {
        Ok(()) => {
            tracing::info!("tls: issuance requested");
            (
                StatusCode::ACCEPTED,
                Json(IssueAccepted {
                    kind: "issue".to_owned(),
                }),
            )
                .into_response()
        }
        Err(_) => problem(503, "tls-acme-busy", "The ACME service is busy or stopped"),
    }
}

/// Front-door `POST /api/v1/tls/issue`: admin bearer, or a setup session
/// while in setup.
async fn front_issue(State(state): State<AppState>, request: Request) -> Response {
    let Some(tls) = state.tls.clone() else {
        return problem(404, "tls-not-configured", "No [network.tls] configured");
    };
    let (parts, _) = request.into_parts();
    let session_ok = tls.source.state() == TlsState::Setup
        && session_cookie(&parts.headers).is_some_and(|t| tls.setup.session_valid(t));
    if !session_ok && !admin_principal(&parts, &state) {
        return problem(
            401,
            "unauthenticated",
            "Admin credential or setup session required",
        );
    }
    trigger_issue(&tls)
}

/// `GET /api/v1/tls/status`: unauthenticated in setup, Read once serving.
async fn status(State(state): State<AppState>, request: Request) -> Response {
    let Some(tls) = state.tls.clone() else {
        return problem(404, "tls-not-configured", "No [network.tls] configured");
    };
    let (parts, _) = request.into_parts();
    if tls.source.state() == TlsState::Serving {
        match crate::routes::authenticate_principal(&parts, &state) {
            Ok(p) if p.role.can(Action::Read) => {}
            Ok(_) => return problem(403, "forbidden", "Read role required"),
            Err(e) => return e.into_response(),
        }
    }
    (
        [(header::CACHE_CONTROL, "no-store")],
        Json(status_body(&tls)),
    )
        .into_response()
}

/// Build the non-secret status body.
#[must_use]
pub fn status_body(tls: &TlsContext) -> TlsStatusBody {
    let status = tls.source.status();
    let now = crate::acme::engine::unix_now();
    let expired =
        status.state == TlsState::Serving && status.expires_at.is_some_and(|at| at <= now);
    let state = match (status.state, expired) {
        (TlsState::Setup, _) => "setup",
        (TlsState::Serving, true) => "expired",
        (TlsState::Serving, false) => "serving",
    };
    let mut reasons = Vec::new();
    if status.state == TlsState::Setup {
        reasons.extend(missing_credentials(&tls.config));
        if let Some(error) = &status.last_error {
            reasons.push(error.clone());
        }
    }
    TlsStatusBody {
        state: state.to_owned(),
        ca: status.ca.clone(),
        staging: tls.config.staging,
        challenge: challenge_name(tls.config.challenge).to_owned(),
        names: status.names.clone(),
        expires_at: status.expires_at,
        renewal_due_at: status.renewal_due_at,
        next_attempt_at: status.next_attempt_at,
        alarm: alarm_name(status.alarm).to_owned(),
        last_error: status.last_error.clone(),
        bootstrap_fingerprint: if status.state == TlsState::Setup {
            status.bootstrap_fingerprint.clone()
        } else {
            None
        },
        reasons,
        hints: hints(&tls.config, tls.behind_proxy),
    }
}

/// `dns-01` / `tls-alpn-01`.
#[must_use]
pub const fn challenge_name(challenge: AcmeChallenge) -> &'static str {
    match challenge {
        AcmeChallenge::Dns01 => "dns-01",
        AcmeChallenge::TlsAlpn01 => "tls-alpn-01",
    }
}

/// `none` / `warning` / `critical` / `expired`.
#[must_use]
pub const fn alarm_name(alarm: CertificateAlarm) -> &'static str {
    match alarm {
        CertificateAlarm::None => "none",
        CertificateAlarm::Warning => "warning",
        CertificateAlarm::Critical => "critical",
        CertificateAlarm::Expired => "expired",
    }
}

/// Operator hints (section 5a challenge table).
#[must_use]
pub fn hints(config: &NetworkTlsConfig, behind_proxy: bool) -> Vec<String> {
    let mut out = Vec::new();
    if behind_proxy && config.challenge == AcmeChallenge::TlsAlpn01 {
        out.push("TLS-ALPN-01 needs TCP 443 to reach the engine; use DNS-01".to_owned());
    }
    out
}

/// `credential-missing: NAME` for every referenced secret that is in neither
/// the environment nor `<state_dir>/secrets/` (names only, never values).
#[must_use]
pub fn missing_credentials(config: &NetworkTlsConfig) -> Vec<String> {
    let lookup = crate::acme::cloudflare::SecretLookup::new(&config.state_dir);
    let mut out = Vec::new();
    let names = config.secret_names();
    // Cloudflare needs either the token or both key names: report only the
    // configured form, which is all `secret_names` lists anyway.
    for name in names {
        match lookup.get(name) {
            Ok(Some(_)) => {}
            Ok(None) => out.push(format!("credential-missing: {name}")),
            Err(_) => out.push(format!("credential-insecure: {name}")),
        }
    }
    out
}

/// Loopback peer AND admin principal, else the refusal.
fn loopback_admin(
    parts: &axum::http::request::Parts,
    state: &AppState,
) -> Result<Arc<TlsContext>, Box<Response>> {
    let tls = state.tls.clone().ok_or_else(|| {
        Box::new(problem(
            404,
            "tls-not-configured",
            "No [network.tls] configured",
        ))
    })?;
    let loopback = parts
        .extensions
        .get::<ConnectInfo<SocketAddr>>()
        .is_some_and(|ConnectInfo(addr)| is_loopback(addr.ip()));
    if !loopback {
        return Err(Box::new(problem(
            403,
            "tls-loopback-only",
            "This route is served only to a loopback peer on the admin listener",
        )));
    }
    match crate::routes::authenticate_principal(parts, state) {
        Ok(p) if p.role == Role::Admin => Ok(tls),
        Ok(_) => Err(Box::new(problem(403, "forbidden", "Admin role required"))),
        Err(e) => Err(Box::new(e.into_response())),
    }
}

async fn admin_setup_code(State(state): State<AppState>, request: Request) -> Response {
    let (parts, _) = request.into_parts();
    let tls = match loopback_admin(&parts, &state) {
        Ok(tls) => tls,
        Err(r) => return *r,
    };
    let code = tls.setup.mint();
    tracing::info!("tls setup: a new one-time setup code was minted via the admin listener");
    (
        [(header::CACHE_CONTROL, "no-store")],
        Json(SetupCodeBody {
            code,
            expires_in: code::CODE_TTL.as_secs(),
        }),
    )
        .into_response()
}

async fn admin_issue(State(state): State<AppState>, request: Request) -> Response {
    let (parts, _) = request.into_parts();
    match loopback_admin(&parts, &state) {
        Ok(tls) => trigger_issue(&tls),
        Err(r) => *r,
    }
}

async fn admin_credential(State(state): State<AppState>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let tls = match loopback_admin(&parts, &state) {
        Ok(tls) => tls,
        Err(r) => return *r,
    };
    let Ok(body) = axum::body::to_bytes(body, CREDENTIAL_BODY_LIMIT).await else {
        return problem(413, "payload-too-large", "Body larger than 8 KiB");
    };
    let body = zeroize::Zeroizing::new(body.to_vec());
    // Parse without ever formatting the body or the parse error (it can quote input).
    let Ok(CredentialRequest { name, value }) = serde_json::from_slice::<CredentialRequest>(&body)
    else {
        return problem(
            400,
            "bad-request",
            "Expected {\"name\": \"...\", \"value\": \"...\"}",
        );
    };
    let value = zeroize::Zeroizing::new(value);
    if !tls.config.secret_names().contains(&name.as_str()) {
        return problem(
            422,
            "tls-credential-unknown",
            "NAME is not referenced by [network.tls]",
        );
    }
    if value.is_empty() {
        return problem(400, "bad-request", "Empty value");
    }
    match write_secret(&tls.config.state_dir, &name, value.as_bytes()) {
        Ok(()) => {
            tracing::info!(name = %name, "tls: credential stored in the state directory (value not logged)");
            StatusCode::NO_CONTENT.into_response()
        }
        Err(_) => problem(
            500,
            "tls-credential-store-failed",
            "Could not store the credential",
        ),
    }
}

/// Write `<state_dir>/secrets/<name>` mode 0600 atomically (`state_dir` and
/// secrets/ are 0700). Also used by the CLI when the engine is not running.
///
/// # Errors
/// An error category when the directory is insecure or the write fails.
pub fn write_secret(
    state_dir: &Path,
    name: &str,
    value: &[u8],
) -> Result<(), crate::acme::AcmeError> {
    crate::acme::store::private_dir(state_dir)?;
    let dir: PathBuf = state_dir.join("secrets");
    crate::acme::store::write_private(&dir, name, value)
}
