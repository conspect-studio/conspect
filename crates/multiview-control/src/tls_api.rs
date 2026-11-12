//! Wire types of the front-door certificate API (ADR-0107 section 5a).
//!
//! Always compiled, so the committed `OpenAPI` document describes the routes
//! regardless of build features. The handlers live in
//! `crate::tls_setup` (feature `acme`, unix). Nothing here carries a secret
//! except [`CredentialRequest::value`], which is write-only, never logged,
//! never echoed and has no `Debug`.
#![allow(clippy::doc_markdown)]

use serde::{Deserialize, Serialize};

/// Non-secret certificate status (`GET /api/v1/tls/status`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct TlsStatusBody {
    /// `setup`, `serving` or `expired` (serving an expired certificate).
    pub state: String,
    /// CA profile (`letsencrypt`, `google`, `custom`).
    pub ca: Option<String>,
    /// Whether the CA's staging directory is configured.
    pub staging: bool,
    /// `dns-01` or `tls-alpn-01`.
    pub challenge: String,
    /// Names requested on the certificate.
    pub names: Vec<String>,
    /// Expiry of the served ACME certificate, Unix seconds.
    pub expires_at: Option<i64>,
    /// When the next renewal is due, Unix seconds.
    pub renewal_due_at: Option<i64>,
    /// When the next attempt runs after a failure, Unix seconds.
    pub next_attempt_at: Option<i64>,
    /// `none`, `warning` (renewal failing, under 50 % of the lifetime left),
    /// `critical` (under 25 %) or `expired`.
    pub alarm: String,
    /// Sanitized token of the last failure (e.g. `ca-unavailable`).
    pub last_error: Option<String>,
    /// SHA-256 fingerprint of the bootstrap certificate (setup only).
    pub bootstrap_fingerprint: Option<String>,
    /// Why the engine is still in setup, e.g. `credential-missing: CLOUDFLARE_API_TOKEN`
    /// (names only, never values).
    pub reasons: Vec<String>,
    /// Operator hints (e.g. the TLS-ALPN-01 reachability note).
    pub hints: Vec<String>,
}

/// `POST /api/v1/tls/setup/session` body.
#[derive(Clone, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct SetupSessionRequest {
    /// The one-time setup code (8 words) printed on the host.
    #[cfg_attr(feature = "openapi", schema(format = Password, write_only))]
    pub code: String,
}

/// `POST /api/v1/tls/setup-code` response: the new code, returned once.
#[derive(Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct SetupCodeBody {
    /// The one-time setup code (8 words, valid one hour, single use).
    pub code: String,
    /// Seconds until it expires.
    pub expires_in: u64,
}

/// `POST /api/v1/tls/credential` body (loopback admin listener only).
#[derive(Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct CredentialRequest {
    /// One of the environment-variable NAMES the configuration references
    /// (`[network.tls]` EAB names, `[network.tls.dns]` credential names).
    pub name: String,
    /// The secret value. Write-only: never logged, never echoed.
    #[cfg_attr(feature = "openapi", schema(format = Password, write_only))]
    pub value: String,
}

/// `POST /api/v1/tls/issue` response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct IssueAccepted {
    /// Always `issue`.
    pub kind: String,
}

/// Documentation-only operation stubs: the handlers are in `tls_setup`.
#[cfg(feature = "openapi")]
#[allow(dead_code, clippy::unused_async, missing_docs)]
pub(crate) mod doc {
    #[utoipa::path(
        get,
        path = "/api/v1/tls/status",
        tag = "tls",
        responses(
            (status = 200, description = "Non-secret certificate status. Unauthenticated while the front door is in setup; Read role once serving.", body = super::TlsStatusBody),
            (status = 401, description = "Serving and no valid credential.", body = crate::problem::Problem),
            (status = 404, description = "No `[network.tls]` configured.", body = crate::problem::Problem),
        ),
    )]
    pub(crate) async fn status() {}

    #[utoipa::path(
        post,
        path = "/api/v1/tls/setup/session",
        tag = "tls",
        request_body = super::SetupSessionRequest,
        responses(
            (status = 204, description = "Setup session created: an `HttpOnly; Secure; SameSite=Strict` cookie valid one hour, honoured only in setup."),
            (status = 401, description = "Wrong, expired or used setup code.", body = crate::problem::Problem),
            (status = 409, description = "Not in setup.", body = crate::problem::Problem),
            (status = 429, description = "Five failed attempts from this source (IPv6 /64) in 15 minutes: locked for 15 minutes.", body = crate::problem::Problem),
        ),
    )]
    pub(crate) async fn setup_session() {}

    #[utoipa::path(
        post,
        path = "/api/v1/tls/issue",
        tag = "tls",
        responses(
            (status = 202, description = "Issuance attempt queued.", body = super::IssueAccepted),
            (status = 401, description = "Front door: neither an admin bearer nor a setup session.", body = crate::problem::Problem),
            (status = 403, description = "Admin listener: not loopback or not admin.", body = crate::problem::Problem),
            (status = 503, description = "The ACME service is busy or stopped.", body = crate::problem::Problem),
        ),
        security(("bearer_auth" = [])),
    )]
    pub(crate) async fn issue() {}

    #[utoipa::path(
        post,
        path = "/api/v1/tls/setup-code",
        tag = "tls",
        responses(
            (status = 200, description = "A new one-time setup code, returned once (replaces the previous code). Loopback admin listener only.", body = super::SetupCodeBody),
            (status = 403, description = "Not loopback, or not admin.", body = crate::problem::Problem),
        ),
        security(("bearer_auth" = [])),
    )]
    pub(crate) async fn setup_code() {}

    #[utoipa::path(
        post,
        path = "/api/v1/tls/credential",
        tag = "tls",
        request_body = super::CredentialRequest,
        responses(
            (status = 204, description = "Stored as `<state_dir>/secrets/<NAME>`, mode 0600. Loopback admin listener only; body at most 8 KiB."),
            (status = 400, description = "Malformed body.", body = crate::problem::Problem),
            (status = 403, description = "Not loopback, or not admin.", body = crate::problem::Problem),
            (status = 422, description = "NAME is not referenced by the configuration.", body = crate::problem::Problem),
        ),
        security(("bearer_auth" = [])),
    )]
    pub(crate) async fn credential() {}
}
