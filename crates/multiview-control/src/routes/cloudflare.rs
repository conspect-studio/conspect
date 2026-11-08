//! Cloudflare management is separate from `MoQ` media admission. No engine callbacks.
#![allow(missing_docs)]
use crate::{
    auth::{Action, Principal},
    problem::Problem,
};
use axum::{
    extract::Path,
    response::{IntoResponse, Response},
    Extension, Json,
};
use std::sync::Arc;

#[derive(serde::Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct SaveCredential {
    pub account_id: String,
    pub kind: String,
    pub email: Option<String>,
    pub key: Option<String>,
    pub token: Option<String>,
}
#[cfg_attr(feature="openapi",utoipa::path(post,path="/api/v1/cloudflare/credentials",tag="cloudflare",request_body=SaveCredential,responses((status=200,description="Owner-bound secret reference",body=CloudflareResult))))]
pub(crate) async fn save(
    principal: Principal,
    provider: Option<Extension<Arc<dyn CloudflareManagement>>>,
    Json(request): Json<SaveCredential>,
) -> Response {
    if let Err(e) = principal
        .role
        .require(Action::Write)
        .and_then(|()| super::require_unscoped_for_whole_system(&principal))
    {
        return e.into_response();
    }
    let Some(Extension(provider)) = provider else {
        return Problem::new(
            503,
            "cloudflare-unavailable",
            "Credential store is not configured",
        )
        .into_response();
    };
    match provider.save(&principal.key_id, request) {
        Ok(reference) => (
            [("cache-control", "no-store")],
            Json(CloudflareResult {
                data: serde_json::json!({"secret_ref":reference}),
            }),
        )
            .into_response(),
        Err(_) => Problem::new(
            400,
            "credential-unavailable",
            "Credential could not be stored",
        )
        .into_response(),
    }
}
#[derive(serde::Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct CloudflareRequest {
    pub account_id: String,
    pub secret_ref: String,
    pub relay_id: Option<String>,
    pub name: Option<String>,
    pub auth_domain: Option<String>,
    pub permission_group_id: Option<String>,
    pub token_jti: Option<String>,
    /// auto creates a missing managed relay on reconcile; manual only observes.
    pub mode: Option<String>,
    /// Whole-object upstream replacement; omitted preserves remote configuration.
    pub config: Option<CloudflareRelayConfig>,
}
#[derive(serde::Deserialize, serde::Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct CloudflareRelayConfig {
    pub upstreams: Option<CloudflareUpstreams>,
}
#[derive(serde::Deserialize, serde::Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct CloudflareUpstreams {
    pub enabled: bool,
    pub upstreams: Vec<CloudflareUpstream>,
}
#[derive(serde::Deserialize, serde::Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct CloudflareUpstream {
    pub url: String,
}
#[derive(serde::Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct CloudflareResult {
    /// Secret-free result. Token values must be persisted before this is returned.
    pub data: serde_json::Value,
}
/// Provider implementation must use owner-bound vault resolution, durable
/// write-ahead journal, and bounded admission. No default provider is installed.
pub trait CloudflareManagement: Send + Sync {
    /// # Errors
    /// Returns a sanitized error if credential validation or persistence fails.
    fn save(&self, owner: &str, request: SaveCredential) -> Result<String, &'static str>;
    fn execute<'a>(
        &'a self,
        owner: &'a str,
        action: &'a str,
        request: CloudflareRequest,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<CloudflareResult, &'static str>> + Send + 'a>,
    >;
}
#[cfg_attr(feature="openapi",utoipa::path(post,path="/api/v1/cloudflare/{action}",tag="cloudflare",params(("action"=String,Path,description="audit, permissions, scoped-token, relays, provision, import, update, teardown, observe, rotate, rotation-installed, revoke, revoke-token, detach, set-mode, reconcile, recover, adopt-recovery, acknowledge-recovery, capabilities")),request_body=CloudflareRequest,responses((status=200,description="Secret-free management result",body=CloudflareResult),(status=503,description="Management unavailable"))))]
pub(crate) async fn execute(
    principal: Principal,
    provider: Option<Extension<Arc<dyn CloudflareManagement>>>,
    Path(action): Path<String>,
    Json(request): Json<CloudflareRequest>,
) -> Response {
    if let Err(e) = principal
        .role
        .require(Action::Write)
        .and_then(|()| super::require_unscoped_for_whole_system(&principal))
    {
        return e.into_response();
    }
    let Some(Extension(provider)) = provider else {
        return Problem::new(
            503,
            "cloudflare-unavailable",
            "Cloudflare management is not configured",
        )
        .into_response();
    };
    match provider.execute(&principal.key_id, &action, request).await {
        Ok(result) => ([("cache-control", "no-store")], Json(result)).into_response(),
        Err(_) => Problem::new(
            503,
            "cloudflare-operation-failed",
            "Management operation failed; inspect recovery state before retry",
        )
        .into_response(),
    }
}
